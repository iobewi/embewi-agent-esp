// embewi_main.c — point d'entrée + orchestration du boot
//
// Rôle clé au boot : détecter si l'image courante est en PENDING_VERIFY.
// Si oui → on vient d'être flashés/activés, il FAUT lancer le self-check borné
// (qui appellera mark_valid ou rollback). Si non → image déjà validée, RUNNING.

#include <string.h>
#include <stdio.h>
#include "esp_log.h"
#include "esp_ota_ops.h"
#include "esp_partition.h"
#include "esp_system.h"
#include "nvs_flash.h"
#include "psa/crypto.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "embewi_agent.h"
#include "embewi_app.h"

static const char *TAG = "embewi";

static embewi_runtime_t s_rt;
embewi_runtime_t *embewi_rt(void) { return &s_rt; }

const char *embewi_state_str(embewi_state_t s) {
    switch (s) {
        case EMBEWI_BOOTING:        return "booting";
        case EMBEWI_PENDING_VERIFY: return "pending_verify";
        case EMBEWI_RUNNING:        return "running";
        case EMBEWI_DEGRADED:       return "degraded";
        case EMBEWI_ROLLBACK:       return "rollback";
        case EMBEWI_FAILED:         return "failed";
        default:                    return "unknown";
    }
}

// Recalcule le SHA-256 du binaire courant en lisant la partition flash.
// fw_size doit être la taille exacte du .bin (sans padding 0xFF de la partition).
// Si fw_size == 0 (premier flash direct, pas via OTA), fw_digest reste "".
static void compute_fw_digest(const esp_partition_t *part, uint32_t fw_size) {
    if (!part || fw_size == 0) return;

    psa_crypto_init();
    psa_hash_operation_t sha = psa_hash_operation_init();
    if (psa_hash_setup(&sha, PSA_ALG_SHA_256) != PSA_SUCCESS) return;

    uint8_t chunk[512];
    uint32_t offset = 0, remaining = fw_size;
    while (remaining > 0) {
        uint32_t to_read = remaining < sizeof(chunk) ? remaining : sizeof(chunk);
        if (esp_partition_read(part, offset, chunk, to_read) != ESP_OK) {
            psa_hash_abort(&sha);
            return;
        }
        psa_hash_update(&sha, chunk, to_read);
        offset += to_read;
        remaining -= to_read;
    }

    uint8_t raw[32]; size_t raw_len;
    if (psa_hash_finish(&sha, raw, sizeof(raw), &raw_len) != PSA_SUCCESS) return;

    char hex[72] = "sha256:";
    for (int i = 0; i < 32; i++) sprintf(hex + 7 + i * 2, "%02x", raw[i]);
    strlcpy(s_rt.fw_digest, hex, sizeof(s_rt.fw_digest));
    ESP_LOGI(TAG, "fw_digest=%s", s_rt.fw_digest);
}

static void fill_running_partition_info(void) {
    const esp_partition_t *run = esp_ota_get_running_partition();
    if (run) strlcpy(s_rt.active_slot, run->label, sizeof(s_rt.active_slot));
    esp_app_desc_t desc;
    if (run && esp_ota_get_partition_description(run, &desc) == ESP_OK) {
        strlcpy(s_rt.fw_version, desc.version, sizeof(s_rt.fw_version));
        ESP_LOGI(TAG, "running app version=%s", desc.version);
    }
    uint32_t fw_size = 0;
    embewi_fw_size_load(&fw_size);
    compute_fw_digest(run, fw_size);
}

void app_main(void) {
    ESP_ERROR_CHECK(nvs_flash_init());

    memset(&s_rt, 0, sizeof(s_rt));
    embewi_cfg_boot_init();   // snapshot génération NVS avant tout appel app
    s_rt.state = EMBEWI_BOOTING;
    strlcpy(s_rt.fw_version, EMBEWI_FW_VERSION, sizeof(s_rt.fw_version));
    fill_running_partition_info();

    // --- Détection PENDING_VERIFY (cœur du rollback, contrat §3) ------------
    const esp_partition_t *run = esp_ota_get_running_partition();
    esp_ota_img_states_t ota_state = ESP_OTA_IMG_UNDEFINED;
    if (run && esp_ota_get_state_partition(run, &ota_state) == ESP_OK)
        ESP_LOGI(TAG, "ota img state = %d", (int)ota_state);

    // Premier boot → portail captif AP (bloquant + reboot).
    // Boots suivants → STA, ctrl_url disponible pour le heartbeat sortant.
    char ctrl_url[129] = {0};
    embewi_wifi_start(ctrl_url, sizeof(ctrl_url));
    strlcpy(s_rt.ctrl_url, ctrl_url, sizeof(s_rt.ctrl_url));
    embewi_node_id_load(s_rt.node_id, sizeof(s_rt.node_id));
    embewi_token_load(s_rt.token, sizeof(s_rt.token));

    // Démarre la synchro NTP dès le réseau dispo : elle s'effectue en tâche de
    // fond pendant l'init suivante (app, self-check). Pré-requis du TLS prod.
    embewi_time_sync_start();

    uint16_t app_port = 8080;
    embewi_app_port_load(&app_port);
    s_rt.app_http_port = app_port;

    embewi_app_init();

    if (ota_state == ESP_OTA_IMG_PENDING_VERIFY) {
        s_rt.state = EMBEWI_PENDING_VERIFY;
        s_rt.ota_validated = false;
        ESP_LOGW(TAG, "image PENDING_VERIFY → self-check borné (deadline %d ms)",
                 EMBEWI_PENDING_DEADLINE_MS);
        embewi_selfcheck_start();
    } else {
        // Image déjà validée — on vérifie quand même le hardware applicatif.
        s_rt.chk_sensors = true;
        s_rt.chk_storage = true;
        s_rt.chk_app     = embewi_app_selfcheck();
        s_rt.state        = s_rt.chk_app ? EMBEWI_RUNNING : EMBEWI_DEGRADED;
        s_rt.ota_validated = true;
        embewi_staged_t staged;
        if (embewi_staged_load(&staged) == ESP_OK &&
            staged.stage != EMBEWI_STAGE_NONE)
            strlcpy(s_rt.deployment_id, staged.deployment_id,
                    sizeof(s_rt.deployment_id));
    }

    // un device en pending_verify DOIT continuer à émettre un heartbeat (§2).
    ESP_ERROR_CHECK(embewi_http_start());
    embewi_app_service_start(s_rt.app_http_port);
    // Attend la synchro NTP avant d'émettre/ouvrir du TLS : timestamps en epoch
    // ET, en prod, horloge valide pour la vérif des dates du cert Core.
    if (embewi_time_wait(8000))
        ESP_LOGI(TAG, "horloge NTP synchronisée");
    else
        ESP_LOGW(TAG, "NTP non synchronisé — ts en uptime ; TLS authentifié (prod) "
                      "échouera tant que l'horloge n'est pas posée");

    embewi_heartbeat_start();
    embewi_log_start();   // streaming ESP_LOGx → WS → Core (après ctrl_url + réseau)
    embewi_log_emit("info", "embewi agent up");

    while (true) vTaskDelay(pdMS_TO_TICKS(1000));
}
