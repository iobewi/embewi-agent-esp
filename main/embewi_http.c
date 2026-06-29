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
#include "embewi_parse.h"
#if CONFIG_EMBEWI_ENABLE_IP_FILTER
#include <lwip/sockets.h>
#endif

static const char *TAG = "embewi.http";
static httpd_handle_t s_srv = NULL;


// Parseurs JSON (json_str/json_u32/json_data_iter) : extraits dans
// embewi_parse.{c,h} — purs, testés sur host (test/host/test_parse.c).

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
    const char *tok = embewi_rt()->token;
    if (tok[0] == '\0') return false;   // token absent → tout refuser (§1)
    char hdr[160];
    if (httpd_req_get_hdr_value_str(req, "Authorization", hdr, sizeof(hdr)) != ESP_OK)
        return false;
    const char *p = "Bearer ";
    if (strncmp(hdr, p, strlen(p)) != 0) return false;
    // Comparaison à temps constant : pas de fuite du token octet par octet (§1).
    return embewi_ct_equal(hdr + strlen(p), tok);
}

static esp_err_t deny(httpd_req_t *req) {
    httpd_resp_set_status(req, "401 Unauthorized");
    httpd_resp_sendstr(req, "{\"error\":\"unauthorized\"}");
    return ESP_OK;
}

// --- Filtrage IP inbound (contrat §1, opt-in CONFIG_EMBEWI_ENABLE_IP_FILTER) ---
// open_fn appelée une fois par session TCP, avant tout handler. Retour != ESP_OK
// → socket fermée immédiatement (avant la couche applicative).
// Source autorisée : clé McuConfigMap "allowed_cidr" (CIDR explicite) ou hôte
// de ctrl_url en /32 implicite. Hostname non-IPv4 → filtre désactivé + warning.
#if CONFIG_EMBEWI_ENABLE_IP_FILTER
static uint32_t s_filter_ip   = 0;
static uint32_t s_filter_mask = 0;   // 0 = filtre désactivé

static void ip_filter_init(void) {
    char cidr[32] = {0};
    if (!embewi_cfg_get("allowed_cidr", cidr, sizeof(cidr))) {
        char host[48] = {0};
        embewi_parse_url_host(embewi_rt()->ctrl_url, host, sizeof(host));
        snprintf(cidr, sizeof(cidr), "%s/32", host);
    }
    if (!embewi_parse_cidr(cidr, &s_filter_ip, &s_filter_mask)) {
        ESP_LOGW(TAG, "ip_filter: '%s' n'est pas une IPv4 → filtre désactivé", cidr);
        return;
    }
    ESP_LOGI(TAG, "ip_filter actif : autorisé %s (mask 0x%08lx)",
             cidr, (unsigned long)s_filter_mask);
}

static esp_err_t ip_filter_open_fn(httpd_handle_t hd, int sockfd) {
    (void)hd;
    if (s_filter_mask == 0) return ESP_OK;
    struct sockaddr_in addr = {0};
    socklen_t len = sizeof(addr);
    if (getpeername(sockfd, (struct sockaddr *)&addr, &len) < 0) return ESP_OK;
    uint32_t peer = ntohl(addr.sin_addr.s_addr);
    if ((peer & s_filter_mask) != (s_filter_ip & s_filter_mask)) {
        ESP_LOGW(TAG, "ip_filter: refus %lu.%lu.%lu.%lu",
                 (unsigned long)(peer >> 24 & 0xFF), (unsigned long)(peer >> 16 & 0xFF),
                 (unsigned long)(peer >>  8 & 0xFF), (unsigned long)(peer & 0xFF));
        return ESP_FAIL;
    }
    return ESP_OK;
}
#endif /* CONFIG_EMBEWI_ENABLE_IP_FILTER */

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

    char body[720];
    snprintf(body, sizeof(body),
        "{\"node_id\":\"%s\",\"chip\":\"" CONFIG_IDF_TARGET "\",\"idf_version\":\"" IDF_VER "\","
        "\"flash_size\":%u,\"ram_size\":%u,"
        "\"partition_layout\":\"embewi-ab-v1\",\"active_slot\":\"%s\","
        "\"firmware\":{\"name\":\"%s\",\"version\":\"%s\",\"digest\":\"%s\"},"
        "\"staged\":{\"state\":\"%s\",\"slot\":\"%s\",\"digest\":\"%s\","
        "\"deployment_id\":\"%s\"},"
        "\"state\":\"%s\",\"config_generation\":%lu,\"app_port\":%u}",
        rt->node_id, (unsigned)flash_size, (unsigned)ram_size, rt->active_slot,
        EMBEWI_FW_NAME, rt->fw_version,
        rt->fw_digest, stage_str, st.slot, st.digest, st.deployment_id,
        embewi_state_str(rt->state), (unsigned long)rt->cfg_generation,
        (unsigned)rt->app_http_port);
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
    embewi_json_str(buf, "deployment_id",    p.deployment_id,    sizeof(p.deployment_id));
    embewi_json_str(buf, "digest",           p.digest,           sizeof(p.digest));
    embewi_json_str(buf, "chip",             p.chip,             sizeof(p.chip));
    embewi_json_str(buf, "idf_version",      p.idf_version,      sizeof(p.idf_version));
    embewi_json_str(buf, "partition_layout", p.partition_layout, sizeof(p.partition_layout));
    embewi_json_u32(buf, "size",             &p.size);

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
// Content-Range optionnel = reprise (§4). Absent → écriture monolithique (legacy).
// Le Core chunke l'image en plusieurs plages ; sur coupure wifi, il reprend à
// l'offset que le device rapporte (416 + written). Handle OTA + SHA survivent à
// la déconnexion TCP (statiques), donc la reprise est gratuite côté flash.
static esp_err_t h_ota_write(httpd_req_t *req) {
    if (!authorized(req)) return deny(req);

    char expected[72] = {0};
    httpd_req_get_hdr_value_str(req, "X-Embewi-Digest", expected, sizeof(expected));

    // Header contrat v2 : tag le slot stagé avec le deployment_id dès l'écriture,
    // pour que GET /info expose staged.deployment_id avant même l'activate (§4 §6).
    char dep_id[64] = {0};
    httpd_req_get_hdr_value_str(req, "X-Embewi-Deployment-Id", dep_id, sizeof(dep_id));

    char range_hdr[64] = {0};
    bool has_range = httpd_req_get_hdr_value_str(req, "Content-Range",
                        range_hdr, sizeof(range_hdr)) == ESP_OK;
    uint32_t start = 0, end = 0, total = 0;
    if (has_range &&
        !embewi_parse_content_range(range_hdr, &start, &end, &total)) {
        httpd_resp_set_status(req, "400 Bad Request");
        send_json(req, "{\"error\":\"bad_content_range\"}");
        return ESP_OK;
    }

    // Décision de reprise (logique pure, testée sur host).
    switch (embewi_ota_plan(has_range, start,
                            embewi_ota_in_progress(), embewi_ota_written())) {
    case EMBEWI_OTA_BEGIN:
        if (embewi_ota_write_begin() != ESP_OK) {
            httpd_resp_set_status(req, "500 Internal Server Error");
            send_json(req, "{\"status\":\"ota_begin_failed\"}");
            return ESP_OK;
        }
        break;
    case EMBEWI_OTA_RESYNC: {
        httpd_resp_set_status(req, "416 Range Not Satisfiable");
        char body[64];
        snprintf(body, sizeof(body),
            "{\"error\":\"range_mismatch\",\"written\":%u}",
            (unsigned)embewi_ota_written());
        send_json(req, body);
        return ESP_OK;
    }
    case EMBEWI_OTA_CONTINUE:
        break;   // session alignée : on écrit directement la plage
    }

    uint8_t buf[1024];
    int remaining = req->content_len, r;
    while (remaining > 0) {
        r = httpd_req_recv(req, (char *)buf, sizeof(buf) < (size_t)remaining
                                              ? sizeof(buf) : remaining);
        // Coupure en cours de plage : on NE ferme PAS la session. Les octets déjà
        // écrits restent comptés (s_written) ; le Core reprendra à cet offset.
        if (r <= 0) return ESP_FAIL;
        if (embewi_ota_write_chunk(buf, r) != ESP_OK) {
            httpd_resp_set_status(req, "500 Internal Server Error");
            send_json(req, "{\"status\":\"write_failed\"}");
            return ESP_OK;
        }
        remaining -= r;
    }

    // Transfert terminé ? (logique pure, testée sur host)
    if (!embewi_ota_is_final(has_range, end, total)) {
        char body[64];
        snprintf(body, sizeof(body),
            "{\"status\":\"partial\",\"written\":%u}",
            (unsigned)embewi_ota_written());
        send_json(req, body);
        return ESP_OK;
    }

    uint32_t written = 0; char digest[72] = {0};
    esp_err_t err = embewi_ota_write_finish(expected[0] ? expected : NULL,
                                            dep_id[0] ? dep_id : NULL,
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
    if (!embewi_json_u32(buf, "port", &port) || port < 1024 || port > 65535) {
        httpd_resp_set_status(req, "400 Bad Request");
        send_json(req, "{\"error\":\"port must be 1024-65535\"}");
        return ESP_OK;
    }

    embewi_runtime_t *rt = embewi_rt();
    embewi_app_service_stop();
    embewi_app_port_save((uint16_t)port);
    rt->app_http_port = (uint16_t)port;
    embewi_app_service_start((uint16_t)port);

    // Format contrat v2 ({status,port}) + alias app_port (rétrocompat lecteurs existants).
    char body[72];
    snprintf(body, sizeof(body),
        "{\"status\":\"saved\",\"port\":%u,\"app_port\":%u}",
        (unsigned)port, (unsigned)port);
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

    // Champs du contrat v2 : cert_pem / key_pem. On accepte aussi les anciens
    // noms cert / key (rétrocompat des intégrations existantes) en repli.
    if (!embewi_json_str(buf, "cert_pem", cert, 4096))
        embewi_json_str(buf, "cert", cert, 4096);
    if (!embewi_json_str(buf, "key_pem", key, 2048))
        embewi_json_str(buf, "key",  key,  2048);
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

// --- POST /token : rotation du token Bearer par node (contrat §4) -----------
// Authentifié au token COURANT. Le nouveau token est commité en NVS AVANT la
// réponse : si l'écriture échoue, l'ancien token reste seul valide (pas de
// fenêtre sans auth). Effectif immédiatement, sans reboot.
static esp_err_t h_token(httpd_req_t *req) {
    if (!authorized(req)) return deny(req);
    char buf[BODY_MAX];
    if (read_body(req, buf, sizeof(buf)) < 0) return ESP_OK;

    char new_token[65] = {0};
    if (!embewi_json_str(buf, "token", new_token, sizeof(new_token))) {
        httpd_resp_set_status(req, "400 Bad Request");
        send_json(req, "{\"error\":\"missing_token\"}");
        return ESP_OK;
    }
    // Token vide refusé : on ne désactive pas l'auth par rotation (§4).
    size_t len = strlen(new_token);
    if (len < 8 || len > 64) {
        httpd_resp_set_status(req, "400 Bad Request");
        send_json(req, "{\"error\":\"token must be 8-64 chars\"}");
        return ESP_OK;
    }

    // Commit NVS d'abord ; l'ancien token reste actif tant que ça n'a pas réussi.
    if (embewi_token_save(new_token) != ESP_OK) {
        httpd_resp_set_status(req, "500 Internal Server Error");
        send_json(req, "{\"error\":\"nvs_write_failed\"}");
        return ESP_OK;
    }
    // Bascule runtime : authorized() exige le nouveau token dès maintenant.
    strlcpy(embewi_rt()->token, new_token, sizeof(embewi_rt()->token));

    send_json(req, "{\"status\":\"rotated\"}");
    return ESP_OK;
}

// --- GET /config : génération + dump NVS ------------------------------------
static esp_err_t h_config_get(httpd_req_t *req) {
    if (!authorized(req)) return deny(req);
    embewi_runtime_t *rt = embewi_rt();
    // active = snapshot figé au boot (§4) ; nvs = NVS courant (peut diverger
    // après un POST /config sans reboot → generation > active_generation).
    char active_json[384], nvs_json[384];
    embewi_cfg_json_active(active_json, sizeof(active_json));
    embewi_cfg_json_nvs(nvs_json, sizeof(nvs_json));
    char body[896];
    snprintf(body, sizeof(body),
        "{\"generation\":%lu,\"active_generation\":%lu,"
        "\"active\":%s,\"nvs\":%s}",
        (unsigned long)rt->cfg_generation,
        (unsigned long)rt->cfg_active_generation,
        active_json, nvs_json);
    send_json(req, body);
    return ESP_OK;
}

// Contexte du callback de POST /config (cf. embewi_json_data_iter).
typedef struct { int count; esp_err_t err; } cfg_post_ctx_t;

static void cfg_set_cb(const char *k, const char *v, void *ctx) {
    cfg_post_ctx_t *c = ctx;
    if (c->err != ESP_OK) return;
    if (k[0] == '_') return;   // clés internes ignorées silencieusement
    c->err = embewi_cfg_write(k, v);
    if (c->err == ESP_OK) c->count++;
}

// --- POST /config : push merge-on-key --------------------------------------
static esp_err_t h_config_post(httpd_req_t *req) {
    if (!authorized(req)) return deny(req);
    char buf[BODY_MAX];
    if (read_body(req, buf, sizeof(buf)) < 0) return ESP_OK;

    cfg_post_ctx_t ctx = {0};
    int found = embewi_json_data_iter(buf, cfg_set_cb, &ctx);
    if (found < 0) {
        httpd_resp_set_status(req, "400 Bad Request");
        send_json(req, "{\"error\":\"missing_data_field\"}");
        return ESP_OK;
    }
    if (ctx.err != ESP_OK) {
        httpd_resp_set_status(req, "500 Internal Server Error");
        send_json(req, "{\"error\":\"nvs_write_failed\"}");
        return ESP_OK;
    }
    if (ctx.count > 0) embewi_cfg_bump_generation();

    char body[96];
    snprintf(body, sizeof(body),
        "{\"status\":\"saved\",\"generation\":%lu,\"note\":\"effective_after_reboot\"}",
        (unsigned long)embewi_rt()->cfg_generation);
    send_json(req, body);
    return ESP_OK;
}

// --- POST /reboot : reboot contrôlé sans cycle OTA -------------------------
static esp_err_t h_reboot(httpd_req_t *req) {
    if (!authorized(req)) return deny(req);
    send_json(req, "{\"status\":\"rebooting\"}");
    embewi_log_emit("info", "reboot requested by Core");
    vTaskDelay(pdMS_TO_TICKS(200));
    esp_restart();
    return ESP_OK;
}

// --- POST /ota/activate -----------------------------------------------------
static esp_err_t h_ota_activate(httpd_req_t *req) {
    if (!authorized(req)) return deny(req);
    char buf[BODY_MAX];
    if (read_body(req, buf, sizeof(buf)) < 0) return ESP_OK;

    char dep[64] = {0};
    embewi_json_str(buf, "deployment_id", dep, sizeof(dep));
    if (dep[0] == '\0') {
        httpd_resp_set_status(req, "400 Bad Request");
        send_json(req, "{\"error\":\"missing deployment_id\"}");
        return ESP_OK;
    }

    // 1. Prépare le boot (set_boot_partition) AVANT de répondre.
    //    Si le slot n'est pas prêt, on renvoie une erreur au lieu d'un faux
    //    "rebooting" (embewi_ota_activate restaure s_target depuis NVS si absent).
    esp_err_t ret = embewi_ota_activate(dep);
    if (ret != ESP_OK) {
        httpd_resp_set_status(req, "409 Conflict");
        send_json(req, "{\"error\":\"slot_not_ready\"}");
        return ESP_OK;
    }

    // 2. Slot configuré : le slot cible vient du NVS staged (mis à jour par activate).
    embewi_staged_t st;
    embewi_staged_load(&st);
    char body[128];
    snprintf(body, sizeof(body),
        "{\"status\":\"rebooting\",\"target_slot\":\"%s\"}", st.slot);
    send_json(req, body);

    // 3. Reboot APRÈS la réponse (le client a le temps de la recevoir).
    embewi_log_emit("info", "ota activate, rebooting");
    vTaskDelay(pdMS_TO_TICKS(200));
    esp_restart();
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
#if CONFIG_EMBEWI_ENABLE_IP_FILTER
    ip_filter_init();
    cfg.httpd.open_fn = ip_filter_open_fn;
#endif

    if (httpd_ssl_start(&s_srv, &cfg) != ESP_OK) return ESP_FAIL;

    const httpd_uri_t routes[] = {
        { .uri = EMBEWI_API_PREFIX "/info",         .method = HTTP_GET,  .handler = h_info },
        { .uri = EMBEWI_API_PREFIX "/health",       .method = HTTP_GET,  .handler = h_health },
        { .uri = EMBEWI_API_PREFIX "/config",       .method = HTTP_GET,  .handler = h_config_get },
        { .uri = EMBEWI_API_PREFIX "/config",       .method = HTTP_POST, .handler = h_config_post },
        { .uri = EMBEWI_API_PREFIX "/reboot",       .method = HTTP_POST, .handler = h_reboot },
        { .uri = EMBEWI_API_PREFIX "/ota/prepare",  .method = HTTP_POST, .handler = h_ota_prepare },
        { .uri = EMBEWI_API_PREFIX "/ota/write",    .method = HTTP_PUT,  .handler = h_ota_write },
        { .uri = EMBEWI_API_PREFIX "/ota/activate", .method = HTTP_POST, .handler = h_ota_activate },
        { .uri = EMBEWI_API_PREFIX "/app/port",     .method = HTTP_POST, .handler = h_app_port },
        { .uri = EMBEWI_API_PREFIX "/tls/cert",     .method = HTTP_POST, .handler = h_tls_cert },
        { .uri = EMBEWI_API_PREFIX "/token",        .method = HTTP_POST, .handler = h_token },
    };
    for (size_t i = 0; i < sizeof(routes) / sizeof(routes[0]); i++)
        httpd_register_uri_handler(s_srv, &routes[i]);

    ESP_LOGI(TAG, "https up :443, %d routes sous %s",
             (int)(sizeof(routes)/sizeof(routes[0])), EMBEWI_API_PREFIX);
    return ESP_OK;
}
