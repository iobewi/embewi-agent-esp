// embewi_heartbeat.c — flux sortants vers le Core (contrat §5)
//
// Device-initiated : marche même si le device n'accepte pas d'inbound.
// Invariant clé : un device en pending_verify CONTINUE d'émettre (state =
// "pending_verify", ota_validated=false). Pas de silence pendant la validation.
//
// Transport : POST HTTP(S) vers ctrl_url (provisionné au boot).
// Fallback ESP_LOGI si ctrl_url vide ou Core injoignable.

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "esp_log.h"
#include "esp_timer.h"
#include "esp_system.h"
#include "esp_wifi.h"
#include "esp_netif.h"
#include "esp_http_client.h"
#include "driver/temperature_sensor.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "embewi_agent.h"
#include "embewi_parse.h"

static const char *TAG = "embewi.hb";
#define HEARTBEAT_PERIOD_MS  5000
#define HTTP_TIMEOUT_MS      1500   // au-delà → Core injoignable, on passe

#if CONFIG_EMBEWI_VERIFY_CORE_CERT
// CA embarquée (main/core_ca.pem) qui signe le cert serveur du Core. Prod only.
extern const char core_ca_pem_start[] asm("_binary_core_ca_pem_start");
#endif

static int current_rssi(void) {
    wifi_ap_record_t ap;
    return (esp_wifi_sta_get_ap_info(&ap) == ESP_OK) ? ap.rssi : 0;
}

// IP STA courante en notation pointée. "0.0.0.0" si le netif n'est pas encore
// disponible (ne devrait pas arriver : heartbeat_start est appelé après WiFi STA).
// Incluse dans le heartbeat pour que le Core mette à jour l'EndpointSlice sans
// dépendre de l'IP source TCP — rend l'IP dynamique (DHCP) transparente (§8).
static void current_ip_str(char *out, size_t len) {
    esp_netif_t *netif = esp_netif_get_handle_from_ifkey("WIFI_STA_DEF");
    esp_netif_ip_info_t ip = {0};
    if (netif) esp_netif_get_ip_info(netif, &ip);
    snprintf(out, len, "%u.%u.%u.%u",
        (unsigned)esp_ip4_addr1_16(&ip.ip), (unsigned)esp_ip4_addr2_16(&ip.ip),
        (unsigned)esp_ip4_addr3_16(&ip.ip), (unsigned)esp_ip4_addr4_16(&ip.ip));
}

// --- Métriques télémétrie (contrat §5, champs optionnels) -------------------
static temperature_sensor_handle_t s_tsens;

// Capteur de température interne du SoC. Installé une fois au démarrage.
static void metrics_init(void) {
    temperature_sensor_config_t cfg = TEMPERATURE_SENSOR_CONFIG_DEFAULT(-10, 80);
    if (temperature_sensor_install(&cfg, &s_tsens) == ESP_OK)
        temperature_sensor_enable(s_tsens);
    else
        s_tsens = NULL;   // capteur indispo → sentinelle -127 dans le heartbeat
}

static float current_temp(void) {
    float t = -127.0f;   // sentinelle : capteur indisponible (le Core filtre)
    if (s_tsens) temperature_sensor_get_celsius(s_tsens, &t);
    return t;
}

// Plus petit high-water-mark de stack (octets restants) : détecte la task qui
// approche de l'overflow. Min sur TOUTES les tasks si la trace facility est
// activée (sdkconfig), sinon fallback sur la seule task heartbeat.
static uint32_t min_task_hwm(void) {
#if configUSE_TRACE_FACILITY
    UBaseType_t n = uxTaskGetNumberOfTasks();
    TaskStatus_t *arr = malloc(n * sizeof(TaskStatus_t));
    if (arr) {
        UBaseType_t got = uxTaskGetSystemState(arr, n, NULL);
        uint32_t mn = UINT32_MAX;
        for (UBaseType_t i = 0; i < got; i++)
            if (arr[i].usStackHighWaterMark < mn) mn = arr[i].usStackHighWaterMark;
        free(arr);
        if (got > 0) return mn;
    }
#endif
    return (uint32_t)uxTaskGetStackHighWaterMark(NULL);
}

// POST json vers ctrl_url+path. Silencieux si ctrl_url vide.
// path : "/v1alpha1/heartbeat" ou "/v1alpha1/logs"
static void emit_to(const char *path, const char *json) {
    const char *ctrl_url = embewi_rt()->ctrl_url;
    if (ctrl_url[0] == '\0') {
        ESP_LOGI(TAG, "%s", json);
        return;
    }

    // Contrat §1 : transport chiffré obligatoire. On force https:// quel que
    // soit le scheme stocké en NVS (cf. embewi_url_rebase, testé sur host).
    char url[256];
    embewi_url_rebase(ctrl_url, "https", path, url, sizeof(url));

    esp_http_client_config_t cfg = {
        .url            = url,
        .method         = HTTP_METHOD_POST,
        .timeout_ms     = HTTP_TIMEOUT_MS,
#if CONFIG_EMBEWI_VERIFY_CORE_CERT
        // PROD : authentifie le Core contre la CA embarquée. Ferme le MITM.
        .cert_pem       = core_ca_pem_start,
#else
        // DEV : chiffré mais NON authentifié (cert auto-signé / pas de PKI).
        // Transport chiffré sans vérification d'identité (cible mTLS §1).
        .skip_cert_common_name_check = true,
        .crt_bundle_attach = NULL,
#endif
    };
    esp_http_client_handle_t client = esp_http_client_init(&cfg);
    if (!client) {
        ESP_LOGW(TAG, "http_client_init failed");
        return;
    }

    esp_http_client_set_header(client, "Content-Type", "application/json");
    esp_http_client_set_post_field(client, json, strlen(json));

    esp_err_t err = esp_http_client_perform(client);
    if (err != ESP_OK)
        ESP_LOGW(TAG, "POST %s → %s", path, esp_err_to_name(err));
    else
        ESP_LOGD(TAG, "POST %s → %d", path,
                 esp_http_client_get_status_code(client));

    esp_http_client_cleanup(client);
}

static void heartbeat_task(void *arg) {
    while (true) {
        embewi_runtime_t *rt = embewi_rt();
        char ip[16];
        current_ip_str(ip, sizeof(ip));
        char body[576];
        snprintf(body, sizeof(body),
            "{\"node_id\":\"%s\",\"ip\":\"%s\",\"ts\":%lld,\"state\":\"%s\","
            "\"deployment_id\":\"%s\",\"firmware_digest\":\"%s\","
            "\"ota_validated\":%s,\"uptime_ms\":%lld,"
            "\"heap_free\":%u,\"rssi\":%d,\"config_generation\":%lu,"
            "\"temp_celsius\":%.1f,\"task_hwm_min\":%lu}",
            rt->node_id, ip,
            (long long)time(NULL),   // epoch UTC (uptime ~1970 si NTP pas encore sync)
            embewi_state_str(rt->state),
            rt->deployment_id, rt->fw_digest,
            rt->ota_validated ? "true" : "false",
            (long long)(esp_timer_get_time() / 1000),
            (unsigned)esp_get_free_heap_size(),
            current_rssi(),
            (unsigned long)rt->cfg_active_generation,
            current_temp(),
            (unsigned long)min_task_hwm());
        emit_to("/v1alpha1/heartbeat", body);
        vTaskDelay(pdMS_TO_TICKS(HEARTBEAT_PERIOD_MS));
    }
}

void embewi_heartbeat_start(void) {
    metrics_init();   // capteur de température interne (best-effort)
    // Stack augmentée : esp_http_client consomme ~4 KB de stack supplémentaires.
    xTaskCreate(heartbeat_task, "embewi_hb", 8192, NULL, 4, NULL);
}

void embewi_log_emit(const char *level, const char *msg) {
    char body[384];
    snprintf(body, sizeof(body),
        "{\"ts\":%lld,\"node\":\"%s\",\"workload\":\"%s\","
        "\"level\":\"%s\",\"msg\":\"%s\"}",
        (long long)(esp_timer_get_time() / 1000000),
        embewi_rt()->node_id, EMBEWI_FW_NAME, level, msg);
    emit_to("/v1alpha1/logs", body);
}
