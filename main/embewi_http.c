// embewi_http.c — endpoints inbound (contrat §4)
//
// MVP : HTTPS + token par node (cible mTLS). Auth vérifiée sur CHAQUE endpoint
// inbound — sinon /ota/activate est une primitive de reboot ouverte sur le LAN.
//
// NB: skeleton en httpd clair ici pour lisibilité. Le build sécurisé passe par
// esp_https_server (CONFIG_ESP_HTTPS_SERVER_ENABLE) + cert serveur ; le handler
// de token ci-dessous est identique.

#include <string.h>
#include <stdlib.h>
#include "esp_log.h"
#include "esp_https_server.h"
#include "esp_ota_ops.h"
#include "esp_flash.h"
#include "esp_heap_caps.h"
#include "embewi_agent.h"
#include "embewi_app.h"

static const char *TAG = "embewi.http";
static httpd_handle_t s_srv = NULL;


// Extrait la valeur d'une clé JSON string : "key":"value" → value dans out.
// Retourne false si la clé est absente. Pas un vrai parseur : MVP uniquement.
static bool json_str(const char *body, const char *key, char *out, size_t out_len) {
    char needle[64];
    snprintf(needle, sizeof(needle), "\"%s\"", key);
    const char *p = strstr(body, needle);
    if (!p) return false;
    p += strlen(needle);
    while (*p == ' ' || *p == ':' || *p == ' ') p++;
    if (*p != '"') return false;
    p++;
    size_t i = 0;
    while (*p && *p != '"' && i < out_len - 1) out[i++] = *p++;
    out[i] = '\0';
    return true;
}

// Extrait la valeur d'une clé JSON number : "key":12345 → out.
static bool json_u32(const char *body, const char *key, uint32_t *out) {
    char needle[64];
    snprintf(needle, sizeof(needle), "\"%s\"", key);
    const char *p = strstr(body, needle);
    if (!p) return false;
    p += strlen(needle);
    while (*p == ' ' || *p == ':' || *p == ' ') p++;
    if (*p < '0' || *p > '9') return false;
    *out = (uint32_t)strtoul(p, NULL, 10);
    return true;
}

// Lit le body complet d'une requête dans un buffer alloué sur la stack.
// Retourne la longueur lue, ou -1 si trop grand ou erreur réseau.
#define BODY_MAX 512
static int read_body(httpd_req_t *req, char *buf, size_t buf_len) {
    if (req->content_len == 0) { buf[0] = '\0'; return 0; }
    if (req->content_len >= buf_len) {
        httpd_resp_set_status(req, "413 Request Entity Too Large");
        httpd_resp_sendstr(req, "{\"error\":\"body_too_large\"}");
        return -1;
    }
    int r = httpd_req_recv(req, buf, req->content_len);
    if (r <= 0) return -1;
    buf[r] = '\0';
    return r;
}

static bool authorized(httpd_req_t *req) {
    char hdr[160];
    if (httpd_req_get_hdr_value_str(req, "Authorization", hdr, sizeof(hdr)) != ESP_OK)
        return false;
    const char *p = "Bearer ";
    if (strncmp(hdr, p, strlen(p)) != 0) return false;
    return strcmp(hdr + strlen(p), embewi_rt()->token) == 0;
}

static esp_err_t deny(httpd_req_t *req) {
    httpd_resp_set_status(req, "401 Unauthorized");
    httpd_resp_sendstr(req, "{\"error\":\"unauthorized\"}");
    return ESP_OK;
}

static void send_json(httpd_req_t *req, const char *json) {
    httpd_resp_set_type(req, "application/json");
    httpd_resp_sendstr(req, json);
}

// --- GET /info (avec staged, contrat §4 §6) ---------------------------------
static esp_err_t h_info(httpd_req_t *req) {
    if (!authorized(req)) return deny(req);
    embewi_runtime_t *rt = embewi_rt();
    embewi_staged_t st; embewi_staged_load(&st);

    const char *stage_str = st.stage == EMBEWI_STAGE_WRITTEN    ? "written"
                          : st.stage == EMBEWI_STAGE_ACTIVATING ? "activating"
                                                                : "none";
    uint32_t flash_size = 0;
    esp_flash_get_size(NULL, &flash_size);
    size_t ram_size = heap_caps_get_total_size(MALLOC_CAP_INTERNAL);

    char body[680];
    snprintf(body, sizeof(body),
        "{\"node_id\":\"%s\",\"chip\":\"" CONFIG_IDF_TARGET "\",\"idf_version\":\"" IDF_VER "\","
        "\"flash_size\":%u,\"ram_size\":%u,"
        "\"partition_layout\":\"embewi-ab-v1\",\"active_slot\":\"%s\","
        "\"firmware\":{\"name\":\"%s\",\"version\":\"%s\",\"digest\":\"%s\"},"
        "\"staged\":{\"state\":\"%s\",\"slot\":\"%s\",\"digest\":\"%s\","
        "\"deployment_id\":\"%s\"},"
        "\"state\":\"%s\",\"app_port\":%u}",
        rt->node_id, (unsigned)flash_size, (unsigned)ram_size, rt->active_slot,
        EMBEWI_FW_NAME, rt->fw_version,
        rt->fw_digest, stage_str, st.slot, st.digest, st.deployment_id,
        embewi_state_str(rt->state), (unsigned)rt->app_http_port);
    send_json(req, body);
    return ESP_OK;
}

// --- GET /health (local, pas juste réseau) ----------------------------------
static esp_err_t h_health(httpd_req_t *req) {
    if (!authorized(req)) return deny(req);
    embewi_runtime_t *rt = embewi_rt();
    const char *status = (rt->state == EMBEWI_RUNNING) ? "ok"
                       : (rt->state == EMBEWI_DEGRADED) ? "degraded" : "fail";
    char body[256];
    snprintf(body, sizeof(body),
        "{\"status\":\"%s\",\"state\":\"%s\",\"checks\":{"
        "\"app\":\"%s\",\"sensors\":\"%s\",\"storage\":\"%s\"}}",
        status, embewi_state_str(rt->state),
        rt->chk_app ? "ok" : "fail", rt->chk_sensors ? "ok" : "fail",
        rt->chk_storage ? "ok" : "fail");
    send_json(req, body);
    return ESP_OK;
}

// --- POST /ota/prepare ------------------------------------------------------
static esp_err_t h_ota_prepare(httpd_req_t *req) {
    if (!authorized(req)) return deny(req);
    char buf[BODY_MAX];
    if (read_body(req, buf, sizeof(buf)) < 0) return ESP_OK;

    embewi_ota_prepare_t p = {0};
    json_str(buf, "deployment_id",    p.deployment_id,    sizeof(p.deployment_id));
    json_str(buf, "digest",           p.digest,           sizeof(p.digest));
    json_str(buf, "chip",             p.chip,             sizeof(p.chip));
    json_str(buf, "idf_version",      p.idf_version,      sizeof(p.idf_version));
    json_str(buf, "partition_layout", p.partition_layout, sizeof(p.partition_layout));
    json_u32(buf, "size",             &p.size);

    char slot[8]; const char *reason = NULL;
    if (embewi_ota_prepare(&p, slot, sizeof(slot), &reason) == ESP_OK) {
        char body[128];
        snprintf(body, sizeof(body),
            "{\"accepted\":true,\"target_slot\":\"%s\",\"reason\":null}", slot);
        send_json(req, body);
    } else {
        char body[128];
        snprintf(body, sizeof(body),
            "{\"accepted\":false,\"target_slot\":null,\"reason\":\"%s\"}",
            reason ? reason : "unknown");
        send_json(req, body);
    }
    return ESP_OK;
}

// --- PUT /ota/write : stream chunké → write incrémental ---------------------
static esp_err_t h_ota_write(httpd_req_t *req) {
    if (!authorized(req)) return deny(req);

    char expected[72] = {0};
    httpd_req_get_hdr_value_str(req, "X-Embewi-Digest", expected, sizeof(expected));

    if (embewi_ota_write_begin() != ESP_OK) {
        httpd_resp_set_status(req, "500 Internal Server Error");
        httpd_resp_sendstr(req, "{\"status\":\"ota_begin_failed\"}");
        return ESP_OK;
    }

    uint8_t buf[1024];
    int remaining = req->content_len, r;
    while (remaining > 0) {
        r = httpd_req_recv(req, (char *)buf, sizeof(buf) < (size_t)remaining
                                              ? sizeof(buf) : remaining);
        if (r <= 0) { return ESP_FAIL; }
        if (embewi_ota_write_chunk(buf, r) != ESP_OK) {
            httpd_resp_set_status(req, "500 Internal Server Error");
            httpd_resp_sendstr(req, "{\"status\":\"write_failed\"}");
            return ESP_OK;
        }
        remaining -= r;
    }

    uint32_t written = 0; char digest[72] = {0};
    esp_err_t err = embewi_ota_write_finish(expected[0] ? expected : NULL,
                                            &written, digest, sizeof(digest));
    if (err == ESP_ERR_INVALID_CRC) {
        send_json(req, "{\"status\":\"digest_mismatch\"}");
        return ESP_OK;
    }
    if (err != ESP_OK) {
        send_json(req, "{\"status\":\"write_failed\"}");
        return ESP_OK;
    }
    char body[160];
    snprintf(body, sizeof(body),
        "{\"written\":%u,\"digest\":\"%s\",\"status\":\"written\"}",
        (unsigned)written, digest);
    send_json(req, body);
    return ESP_OK;
}

// --- POST /app/port : reconfigure dynamiquement le port du service app ------
static esp_err_t h_app_port(httpd_req_t *req) {
    if (!authorized(req)) return deny(req);
    char buf[BODY_MAX];
    if (read_body(req, buf, sizeof(buf)) < 0) return ESP_OK;

    uint32_t port = 0;
    if (!json_u32(buf, "port", &port) || port < 1024 || port > 65535) {
        httpd_resp_set_status(req, "400 Bad Request");
        send_json(req, "{\"error\":\"port must be 1024-65535\"}");
        return ESP_OK;
    }

    embewi_runtime_t *rt = embewi_rt();
    embewi_app_service_stop();
    embewi_app_port_save((uint16_t)port);
    rt->app_http_port = (uint16_t)port;
    embewi_app_service_start((uint16_t)port);

    char body[48];
    snprintf(body, sizeof(body), "{\"app_port\":%u}", (unsigned)port);
    send_json(req, body);
    return ESP_OK;
}

// Déséchappement minimal du JSON : \n → newline, \r, \\, \".
// Nécessaire car les PEM en JSON ont leurs newlines échappés en \n littéral.
static void unescape_json(char *s) {
    char *r = s, *w = s;
    while (*r) {
        if (*r == '\\' && *(r + 1)) {
            r++;
            switch (*r) {
                case 'n':  *w++ = '\n'; break;
                case 'r':  *w++ = '\r'; break;
                case '\\': *w++ = '\\'; break;
                case '"':  *w++ = '"';  break;
                case 't':  *w++ = '\t'; break;
                default:   *w++ = '\\'; *w++ = *r; break;
            }
        } else {
            *w++ = *r;
        }
        r++;
    }
    *w = '\0';
}

// --- POST /tls/cert : livraison cert+key depuis le Core --------------------
// Le Core peut pousser un cert signé CA sans reflash.
// Sauvé en NVS → actif au prochain démarrage du serveur HTTPS.
static esp_err_t h_tls_cert(httpd_req_t *req) {
    if (!authorized(req)) return deny(req);
    if (req->content_len == 0 || req->content_len >= 6144) {
        httpd_resp_set_status(req, "413 Request Entity Too Large");
        send_json(req, "{\"error\":\"body_too_large\"}");
        return ESP_OK;
    }
    char *buf = malloc(req->content_len + 1);
    if (!buf) {
        httpd_resp_set_status(req, "500 Internal Server Error");
        send_json(req, "{\"error\":\"oom\"}");
        return ESP_OK;
    }
    int r = httpd_req_recv(req, buf, req->content_len);
    if (r <= 0) { free(buf); return ESP_FAIL; }
    buf[r] = '\0';

    char *cert = calloc(1, 4096);
    char *key  = calloc(1, 2048);
    if (!cert || !key) {
        free(buf); free(cert); free(key);
        httpd_resp_set_status(req, "500 Internal Server Error");
        send_json(req, "{\"error\":\"oom\"}");
        return ESP_OK;
    }

    json_str(buf, "cert", cert, 4096);
    json_str(buf, "key",  key,  2048);
    free(buf);

    unescape_json(cert);
    unescape_json(key);

    if (strncmp(cert, "-----BEGIN", 10) != 0 ||
        strncmp(key,  "-----BEGIN", 10) != 0) {
        free(cert); free(key);
        httpd_resp_set_status(req, "400 Bad Request");
        send_json(req, "{\"error\":\"invalid_pem\"}");
        return ESP_OK;
    }

    esp_err_t err = embewi_tls_save(cert, key);
    free(cert); free(key);

    if (err != ESP_OK) {
        httpd_resp_set_status(req, "500 Internal Server Error");
        send_json(req, "{\"error\":\"nvs_save_failed\"}");
        return ESP_OK;
    }

    // Sauvé en NVS — effectif au prochain reboot.
    // Le Core déclenche le reboot via OTA activate (même firmware) si nécessaire.
    send_json(req, "{\"status\":\"saved\",\"note\":\"effective_after_reboot\"}");
    return ESP_OK;
}

// --- POST /ota/activate -----------------------------------------------------
static esp_err_t h_ota_activate(httpd_req_t *req) {
    if (!authorized(req)) return deny(req);
    char buf[BODY_MAX];
    if (read_body(req, buf, sizeof(buf)) < 0) return ESP_OK;

    char dep[64] = {0};
    json_str(buf, "deployment_id", dep, sizeof(dep));
    if (dep[0] == '\0') {
        httpd_resp_set_status(req, "400 Bad Request");
        send_json(req, "{\"error\":\"missing deployment_id\"}");
        return ESP_OK;
    }

    // Réponse AVANT reboot (le client doit la recevoir).
    char body[128];
    const esp_partition_t *next = esp_ota_get_next_update_partition(NULL);
    snprintf(body, sizeof(body),
        "{\"status\":\"rebooting\",\"target_slot\":\"%s\"}",
        next ? next->label : "ota_?");
    send_json(req, body);
    embewi_ota_activate(dep);   // set_boot + reboot (ne retourne pas)
    return ESP_OK;
}

esp_err_t embewi_http_stop(void) {
    if (s_srv) { httpd_ssl_stop(s_srv); s_srv = NULL; }
    return ESP_OK;
}

esp_err_t embewi_http_start(void) {
    const uint8_t *cert; size_t cert_len;
    const uint8_t *key;  size_t key_len;
    embewi_tls_load(&cert, &cert_len, &key, &key_len);

    httpd_ssl_config_t cfg = HTTPD_SSL_CONFIG_DEFAULT();
    cfg.httpd.uri_match_fn = httpd_uri_match_wildcard;
    cfg.servercert     = cert;
    cfg.servercert_len = cert_len;
    cfg.prvtkey_pem    = key;
    cfg.prvtkey_len    = key_len;
    // port_secure = 443 par défaut dans HTTPD_SSL_CONFIG_DEFAULT()

    if (httpd_ssl_start(&s_srv, &cfg) != ESP_OK) return ESP_FAIL;

    const httpd_uri_t routes[] = {
        { .uri = EMBEWI_API_PREFIX "/info",         .method = HTTP_GET,  .handler = h_info },
        { .uri = EMBEWI_API_PREFIX "/health",       .method = HTTP_GET,  .handler = h_health },
        { .uri = EMBEWI_API_PREFIX "/ota/prepare",  .method = HTTP_POST, .handler = h_ota_prepare },
        { .uri = EMBEWI_API_PREFIX "/ota/write",    .method = HTTP_PUT,  .handler = h_ota_write },
        { .uri = EMBEWI_API_PREFIX "/ota/activate", .method = HTTP_POST, .handler = h_ota_activate },
        { .uri = EMBEWI_API_PREFIX "/app/port",     .method = HTTP_POST, .handler = h_app_port },
        { .uri = EMBEWI_API_PREFIX "/tls/cert",     .method = HTTP_POST, .handler = h_tls_cert },
    };
    for (size_t i = 0; i < sizeof(routes) / sizeof(routes[0]); i++)
        httpd_register_uri_handler(s_srv, &routes[i]);

    ESP_LOGI(TAG, "https up :443, %d routes sous %s",
             (int)(sizeof(routes)/sizeof(routes[0])), EMBEWI_API_PREFIX);
    return ESP_OK;
}
