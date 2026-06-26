// embewi_log.c — streaming de tous les ESP_LOGx vers le Core via WebSocket
//
// Capture transparente via esp_log_set_vprintf : aucune modification des
// appels ESP_LOGx existants. Comportement UART conservé intégralement.
//
// Flux :
//   ESP_LOGx (toute task)
//       │
//   log_hook()  ← esp_log_set_vprintf
//       ├── vprintf original  → UART (inchangé)
//       └── xRingbufferSend() → ring buffer 4 KB (non-bloquant, drop si plein)
//                                        │
//                               drain_task()  → WebSocket → Core /v1alpha1/logs
//
// Récursion : la drain task est identifiée par son handle ; le hook l'ignore.
// Buffer plein : drop silencieux — on préfère perdre des logs que bloquer.
//
// Best-effort : si le WS est déconnecté, la drain task vide quand même le ring
// buffer et JETTE les messages (pas de buffering jusqu'à reconnexion). Choix
// assumé : on garde toujours les logs RÉCENTS une fois reconnecté, plutôt que
// de retenir un vieux backlog. Les logs émis pendant une coupure sont perdus.

#include <string.h>
#include <stdio.h>
#include <time.h>
#include "esp_log.h"
#include "esp_websocket_client.h"
#include "esp_timer.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "freertos/ringbuf.h"
#include "embewi_agent.h"
#include "embewi_parse.h"

static const char *TAG = "embewi.log";

#define LOG_RINGBUF_SIZE  (4 * 1024)  // ~25 lignes de 160 chars
#define LOG_MSG_MAX       192
#define DRAIN_TIMEOUT_MS  2000        // attente max ring buffer avant wake-up

static RingbufHandle_t   s_rbuf  = NULL;
static vprintf_like_t    s_orig  = NULL;
static TaskHandle_t      s_drain = NULL;
static esp_websocket_client_handle_t s_ws = NULL;

#if CONFIG_EMBEWI_VERIFY_CORE_CERT
// CA embarquée (main/core_ca.pem) qui signe le cert serveur du Core. Prod only.
extern const char core_ca_pem_start[] asm("_binary_core_ca_pem_start");
#endif

// ── Hook vprintf ──────────────────────────────────────────────────────────────
// Appelé par TOUS les ESP_LOGx. Ne fait que des opérations rapides et
// non-bloquantes. va_copy indispensable : la va_list est consommée par s_orig.

static int log_hook(const char *fmt, va_list args) {
    va_list args2;
    va_copy(args2, args);

    // NULL-guard : fenêtre de course à l'installation du hook (s_orig affecté
    // APRÈS esp_log_set_vprintf). Un log concurrent dans cette fenêtre ne doit
    // pas déréférencer NULL — on saute juste l'UART pour cette ligne.
    int r = s_orig ? s_orig(fmt, args) : 0;

    if (s_rbuf && xTaskGetCurrentTaskHandle() != s_drain) {
        char line[LOG_MSG_MAX];
        int n = vsnprintf(line, sizeof(line), fmt, args2);
        // Strip trailing \r\n (format standard ESP-IDF)
        while (n > 0 && (line[n-1] == '\n' || line[n-1] == '\r')) line[--n] = '\0';
        if (n > 0)
            xRingbufferSend(s_rbuf, line, (size_t)n + 1, 0);  // drop si plein
    }
    va_end(args2);
    return r;
}

// ── Drain task ────────────────────────────────────────────────────────────────

static void drain_task(void *arg) {
    while (true) {
        size_t item_size;
        char *line = xRingbufferReceive(s_rbuf, &item_size,
                                         pdMS_TO_TICKS(DRAIN_TIMEOUT_MS));
        if (!line) continue;

        // Vider tout le disponible en une passe pour absorber les rafales
        do {
            if (s_ws && esp_websocket_client_is_connected(s_ws)) {
                const embewi_runtime_t *rt = embewi_rt();

                // Escape JSON (\ et ") — borné, testé sur host.
                char esc[LOG_MSG_MAX * 2];
                embewi_json_escape(line, esc, sizeof(esc));

                // worst-case frame : JSON fixed(53) + ts(20) + node_id(63)
                //   + workload(64) + esc(LOG_MSG_MAX*2) + null → 584
                char frame[640];
                snprintf(frame, sizeof(frame),
                    "{\"ts\":%lld,\"node\":\"%s\",\"workload\":\"%s\","
                    "\"level\":\"raw\",\"msg\":\"%s\"}",
                    (long long)time(NULL),   // epoch UTC (cf. embewi_time.c)
                    rt->node_id, EMBEWI_FW_NAME, esc);

                esp_websocket_client_send_text(s_ws, frame, (int)strlen(frame),
                                               pdMS_TO_TICKS(500));
            }
            vRingbufferReturnItem(s_rbuf, line);
            line = xRingbufferReceive(s_rbuf, &item_size, 0);  // non-bloquant
        } while (line);
    }
}

// ── WebSocket events ──────────────────────────────────────────────────────────

static void ws_event(void *arg, esp_event_base_t base,
                     int32_t id, void *data) {
    switch (id) {
        case WEBSOCKET_EVENT_CONNECTED:
            ESP_LOGI(TAG, "WS connecté — streaming logs actif");  break;
        case WEBSOCKET_EVENT_DISCONNECTED:
            ESP_LOGW(TAG, "WS déconnecté — logs perdus jusqu'à reconnexion"); break;
        case WEBSOCKET_EVENT_ERROR:
            ESP_LOGE(TAG, "WS erreur");                           break;
        default: break;
    }
}

// ── Point d'entrée public ─────────────────────────────────────────────────────

void embewi_log_start(void) {
    const char *ctrl = embewi_rt()->ctrl_url;
    if (ctrl[0] == '\0') {
        ESP_LOGW(TAG, "ctrl_url vide — streaming logs désactivé");
        return;
    }

    s_rbuf = xRingbufferCreate(LOG_RINGBUF_SIZE, RINGBUF_TYPE_NOSPLIT);
    if (!s_rbuf) { ESP_LOGE(TAG, "ring buffer alloc failed"); return; }

    char ws_url[180];
    embewi_url_rebase(ctrl, "wss", "/v1alpha1/logs", ws_url, sizeof(ws_url));

    const esp_websocket_client_config_t ws_cfg = {
        .uri = ws_url,
#if CONFIG_EMBEWI_VERIFY_CORE_CERT
        .cert_pem = core_ca_pem_start,         // PROD : authentifie le Core (MITM fermé)
#else
        .skip_cert_common_name_check = true,   // DEV : chiffré, non authentifié
#endif
    };
    s_ws = esp_websocket_client_init(&ws_cfg);
    if (!s_ws) {
        ESP_LOGE(TAG, "websocket init failed");
        vRingbufferDelete(s_rbuf); s_rbuf = NULL;
        return;
    }
    esp_websocket_register_events(s_ws, WEBSOCKET_EVENT_ANY, ws_event, NULL);
    esp_websocket_client_start(s_ws);

    // Task créée AVANT l'installation du hook :
    // xTaskCreate remplit s_drain avant que la task s'exécute,
    // garantissant que le handle est valide dès le premier appel du hook.
    xTaskCreate(drain_task, "embewi_log", 8192, NULL, 3, &s_drain);

    s_orig = esp_log_set_vprintf(log_hook);
    ESP_LOGI(TAG, "streaming logs → %s (ring %u B)", ws_url, LOG_RINGBUF_SIZE);
}
