// embewi_heartbeat.c — flux sortants vers le Core (contrat §5)
//
// Device-initiated : marche même si le device n'accepte pas d'inbound.
// Invariant clé : un device en pending_verify CONTINUE d'émettre (state =
// "pending_verify", ota_validated=false). Pas de silence pendant la validation.
//
// Transport : POST HTTP(S) vers ctrl_url (provisionné au boot).
// Fallback ESP_LOGI si ctrl_url vide ou Core injoignable.

#include <stdio.h>
#include <string.h>
#include "esp_log.h"
#include "esp_timer.h"
#include "esp_system.h"
#include "esp_wifi.h"
#include "esp_http_client.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "embewi_agent.h"

static const char *TAG = "embewi.hb";
#define HEARTBEAT_PERIOD_MS  5000
#define HTTP_TIMEOUT_MS      1500   // au-delà → Core injoignable, on passe

static int current_rssi(void) {
    wifi_ap_record_t ap;
    return (esp_wifi_sta_get_ap_info(&ap) == ESP_OK) ? ap.rssi : 0;
}

// POST json vers ctrl_url+path. Silencieux si ctrl_url vide.
// path : "/v1alpha1/heartbeat" ou "/v1alpha1/logs"
static void emit_to(const char *path, const char *json) {
    const char *ctrl_url = embewi_rt()->ctrl_url;
    if (ctrl_url[0] == '\0') {
        ESP_LOGI(TAG, "%s", json);
        return;
    }

    char url[256];
    snprintf(url, sizeof(url), "%s%s", ctrl_url, path);

    esp_http_client_config_t cfg = {
        .url            = url,
        .method         = HTTP_METHOD_POST,
        .timeout_ms     = HTTP_TIMEOUT_MS,
        // MVP : pas de vérification CA côté Core (cert auto-signé ou cert-manager
        // interne). Le canal est sur le LAN management ; l'auth inbound reste le
        // token Bearer sur le serveur HTTPS de l'ESP.
        .skip_cert_common_name_check = true,
        .crt_bundle_attach = NULL,
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
        char body[384];
        snprintf(body, sizeof(body),
            "{\"node_id\":\"%s\",\"ts\":%lld,\"state\":\"%s\","
            "\"deployment_id\":\"%s\",\"firmware_digest\":\"%s\","
            "\"ota_validated\":%s,\"uptime_ms\":%lld,"
            "\"heap_free\":%u,\"rssi\":%d}",
            EMBEWI_NODE_ID,
            (long long)(esp_timer_get_time() / 1000000),
            embewi_state_str(rt->state),
            rt->deployment_id, rt->fw_digest,
            rt->ota_validated ? "true" : "false",
            (long long)(esp_timer_get_time() / 1000),
            (unsigned)esp_get_free_heap_size(),
            current_rssi());
        emit_to("/v1alpha1/heartbeat", body);
        vTaskDelay(pdMS_TO_TICKS(HEARTBEAT_PERIOD_MS));
    }
}

void embewi_heartbeat_start(void) {
    // Stack augmentée : esp_http_client consomme ~4 KB de stack supplémentaires.
    xTaskCreate(heartbeat_task, "embewi_hb", 8192, NULL, 4, NULL);
}

void embewi_log_emit(const char *level, const char *msg) {
    char body[384];
    snprintf(body, sizeof(body),
        "{\"ts\":%lld,\"node\":\"%s\",\"workload\":\"%s\","
        "\"level\":\"%s\",\"msg\":\"%s\"}",
        (long long)(esp_timer_get_time() / 1000000),
        EMBEWI_NODE_ID, EMBEWI_FW_NAME, level, msg);
    emit_to("/v1alpha1/logs", body);
}
