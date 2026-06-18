// embewi_provision.c — WiFi avec portail captif au premier boot
//
// Logique :
//   NVS vide (premier boot ou reset conf) → AP "embewi-XXXX" + page web
//     → user soumet SSID / password / URL contrôleur → NVS → reboot
//   NVS rempli → STA avec credentials stockés, bloquant jusqu'à IP ou timeout.
//
// Namespace NVS : "embewi_prov"   clés : ssid / pass / ctrl_url

#include <string.h>
#include <stdio.h>
#include "esp_log.h"
#include "esp_wifi.h"
#include "esp_event.h"
#include "esp_netif.h"
#include "esp_http_server.h"
#include "esp_system.h"
#include "esp_mac.h"
#include "esp_random.h"
#include "nvs.h"
#include "nvs_flash.h"
#include "lwip/inet.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "freertos/event_groups.h"
#include "embewi_agent.h"

static const char *TAG = "embewi.prov";

#define PROV_NVS_NS    "embewi_prov"
#define PROV_KEY_SSID  "ssid"
#define PROV_KEY_PASS  "pass"
#define PROV_KEY_URL   "ctrl_url"
#define PROV_KEY_NODE_ID "node_id"
#define PROV_KEY_TOKEN   "token"
#define PROV_KEY_IP      "ip"
#define PROV_KEY_MASK  "mask"
#define PROV_KEY_GW    "gw"
#define PROV_KEY_DNS   "dns"

#define WIFI_CONNECTED_BIT BIT0
#define WIFI_FAIL_BIT      BIT1
#define WIFI_MAX_RETRY     5

static EventGroupHandle_t s_wifi_eg;
static int s_retry = 0;

// ── NVS helpers ──────────────────────────────────────────────────────────────

typedef struct {
    char node_id[64];
    char ssid[33];
    char pass[65];
    char ctrl_url[129];
    char ip[16];    // vide = DHCP
    char mask[16];
    char gw[16];
    char dns[16];   // vide = même que gw
} prov_cfg_t;

static bool prov_load(prov_cfg_t *cfg) {
    memset(cfg, 0, sizeof(*cfg));
    nvs_handle_t h;
    if (nvs_open(PROV_NVS_NS, NVS_READONLY, &h) != ESP_OK) return false;

    size_t l;
    l = sizeof(cfg->node_id);  nvs_get_str(h, PROV_KEY_NODE_ID, cfg->node_id, &l);
    l = sizeof(cfg->ssid);     nvs_get_str(h, PROV_KEY_SSID,    cfg->ssid,    &l);
    l = sizeof(cfg->pass);     nvs_get_str(h, PROV_KEY_PASS,    cfg->pass,    &l);
    l = sizeof(cfg->ctrl_url); nvs_get_str(h, PROV_KEY_URL,     cfg->ctrl_url,&l);
    l = sizeof(cfg->ip);       nvs_get_str(h, PROV_KEY_IP,      cfg->ip,      &l);
    l = sizeof(cfg->mask);     nvs_get_str(h, PROV_KEY_MASK,    cfg->mask,    &l);
    l = sizeof(cfg->gw);       nvs_get_str(h, PROV_KEY_GW,      cfg->gw,      &l);
    l = sizeof(cfg->dns);      nvs_get_str(h, PROV_KEY_DNS,     cfg->dns,     &l);
    nvs_close(h);

    if (cfg->ssid[0] == '\0') return false;
    ESP_LOGI(TAG, "prov: ssid=%s ctrl=%s ip=%s",
             cfg->ssid, cfg->ctrl_url, cfg->ip[0] ? cfg->ip : "dhcp");
    return true;
}

static esp_err_t prov_save(const prov_cfg_t *cfg) {
    nvs_handle_t h;
    esp_err_t err = nvs_open(PROV_NVS_NS, NVS_READWRITE, &h);
    if (err != ESP_OK) return err;
    err = nvs_set_str(h, PROV_KEY_NODE_ID, cfg->node_id);
    if (err == ESP_OK) err = nvs_set_str(h, PROV_KEY_SSID, cfg->ssid);
    if (err == ESP_OK) err = nvs_set_str(h, PROV_KEY_PASS, cfg->pass);
    if (err == ESP_OK) err = nvs_set_str(h, PROV_KEY_URL,  cfg->ctrl_url);
    // IP statique — sauvés seulement si renseignés
    if (err == ESP_OK && cfg->ip[0])   err = nvs_set_str(h, PROV_KEY_IP,   cfg->ip);
    if (err == ESP_OK && cfg->mask[0]) err = nvs_set_str(h, PROV_KEY_MASK, cfg->mask);
    if (err == ESP_OK && cfg->gw[0])   err = nvs_set_str(h, PROV_KEY_GW,   cfg->gw);
    if (err == ESP_OK && cfg->dns[0])  err = nvs_set_str(h, PROV_KEY_DNS,  cfg->dns);
    if (err == ESP_OK) err = nvs_commit(h);
    nvs_close(h);
    return err;
}

// ── URL-decode (formulaire HTML) ─────────────────────────────────────────────

static void url_decode(const char *src, char *dst, size_t dst_len) {
    size_t i = 0;
    while (*src && i < dst_len - 1) {
        if (*src == '%' && src[1] && src[2]) {
            char hex[3] = { src[1], src[2], '\0' };
            dst[i++] = (char)strtol(hex, NULL, 16);
            src += 3;
        } else if (*src == '+') {
            dst[i++] = ' ';
            src++;
        } else {
            dst[i++] = *src++;
        }
    }
    dst[i] = '\0';
}

// Extrait l'hôte (IP ou nom) d'une URL : "http://192.168.1.1:8080/..." → "192.168.1.1"
static void url_host(const char *url, char *out, size_t out_len) {
    out[0] = '\0';
    const char *p = strstr(url, "://");
    p = p ? p + 3 : url;
    size_t i = 0;
    while (*p && *p != ':' && *p != '/' && i < out_len - 1)
        out[i++] = *p++;
    out[i] = '\0';
}

static bool form_field(const char *body, const char *key,
                       char *out, size_t out_len) {
    char needle[48];
    snprintf(needle, sizeof(needle), "%s=", key);
    const char *p = strstr(body, needle);
    if (!p) return false;
    p += strlen(needle);
    char raw[256] = {0};
    size_t i = 0;
    while (*p && *p != '&' && i < sizeof(raw) - 1) raw[i++] = *p++;
    url_decode(raw, out, out_len);
    return true;
}

// ── Page HTML de provisioning ─────────────────────────────────────────────────

// HTML coupé en deux pour injecter le node_id par défaut sans snprintf sur %
static const char SETUP_HTML_A[] =
    "<!DOCTYPE html><html><head><meta charset='utf-8'>"
    "<meta name='viewport' content='width=device-width,initial-scale=1'>"
    "<title>Embewi Setup</title>"
    "<style>"
    "body{font-family:sans-serif;max-width:440px;margin:40px auto;padding:0 16px}"
    "label{display:block;margin:10px 0 3px;font-size:.95em}"
    "input:not([type=checkbox]){width:100%;box-sizing:border-box;padding:8px;font-size:1em}"
    "input[readonly]{background:#f0f0f0;color:#666}"
    ".cb{display:flex;align-items:center;gap:8px;margin:10px 0 3px;font-size:.9em;color:#444}"
    ".cb label{margin:0;cursor:pointer}"
    "hr{margin:18px 0;border:0;border-top:1px solid #ddd}"
    "button{margin-top:20px;width:100%;padding:10px;background:#1a73e8;"
    "color:#fff;border:0;font-size:1em;cursor:pointer;border-radius:4px}"
    "</style></head><body>"
    "<h2>Embewi &#8212; Configuration</h2>"
    "<form method='POST' action='/setup' onsubmit='prep()'>"
    "<label>Identifiant du n&#339;ud</label>"
    "<input name='node_id' maxlength='63' required value='";
// ← node_id par défaut injecté ici par h_get via send_chunk →
static const char SETUP_HTML_B[] =
    "' autocomplete='off'>"
    "<small style='color:#666'>Unique par device &mdash; format libre (ex&nbsp;: embewi-moteur-gauche)</small>"
    "<hr>"
    "<label>WiFi SSID</label>"
    "<input name='ssid' maxlength='32' required autocomplete='off'>"
    "<label>WiFi Mot de passe</label>"
    "<input name='pass' type='password' maxlength='64' autocomplete='off'>"
    "<label>URL du contr&#244;leur Kubernetes</label>"
    "<input name='ctrl_url' id='cu' maxlength='128'"
    " placeholder='http://192.168.1.10:8080' required oninput='sync()'>"

    "<hr>"
    "<div class='cb'>"
    "<input type='checkbox' id='dhcp' checked onchange='togIp()'>"
    "<label for='dhcp'>DHCP &mdash; IP automatique</label>"
    "</div>"

    "<div id='ips' style='display:none'>"
    "<label>Adresse IP du device</label>"
    "<input name='ip' id='ip' maxlength='15' placeholder='192.168.1.100'>"
    "<label>Masque r&#233;seau</label>"
    "<input name='mask' id='mask' maxlength='15' value='255.255.255.0'>"
    "<div class='cb'>"
    "<input type='checkbox' id='gwa' checked onchange='togGw()'>"
    "<label for='gwa'>Passerelle = IP du contr&#244;leur</label>"
    "</div>"
    "<input name='gw' id='gw' maxlength='15' readonly placeholder='auto'>"
    "<div class='cb'>"
    "<input type='checkbox' id='dnsa' checked onchange='togDns()'>"
    "<label for='dnsa'>DNS = passerelle</label>"
    "</div>"
    "<input name='dns' id='dns' maxlength='15' readonly placeholder='auto'>"
    "</div>"

    "<hr>"
    "<label>Token (vide = auto-g&#233;n&#233;r&#233;)</label>"
    "<input name='token' maxlength='64' autocomplete='off' placeholder='auto'>"
    "<button type='submit'>Enregistrer et red&#233;marrer</button>"
    "</form>"

    "<script>"
    "function host(u){var m=u.match(/[a-z]+:\\/\\/([^:\\/]+)/i);return m?m[1]:'';}"
    "function fillGw(){document.getElementById('gw').value=host(document.getElementById('cu').value);}"
    "function fillDns(){document.getElementById('dns').value=document.getElementById('gw').value;}"
    "function sync(){"
    "if(document.getElementById('gwa').checked)fillGw();"
    "if(document.getElementById('dnsa').checked)fillDns();}"
    "function togIp(){"
    "var on=!document.getElementById('dhcp').checked;"
    "document.getElementById('ips').style.display=on?'block':'none';"
    "if(on)sync();}"
    "function togGw(){"
    "var a=document.getElementById('gwa').checked;"
    "document.getElementById('gw').readOnly=a;"
    "if(a)fillGw();}"
    "function togDns(){"
    "var a=document.getElementById('dnsa').checked;"
    "document.getElementById('dns').readOnly=a;"
    "if(a)fillDns();}"
    "document.getElementById('gw').addEventListener('input',function(){"
    "if(document.getElementById('dnsa').checked)fillDns();});"
    "function prep(){"
    "if(document.getElementById('dhcp').checked)"
    "['ip','mask','gw','dns'].forEach(function(i){document.getElementById(i).value='';});}"
    "</script>"
    "</body></html>";

// OK_HTML est généré dynamiquement pour y inclure le token (voir h_post).
#define OK_HTML_FMT \
    "<!DOCTYPE html><html><head><meta charset='utf-8'>" \
    "<title>Embewi Setup</title>" \
    "<style>body{font-family:sans-serif;max-width:420px;margin:40px auto;padding:0 16px}" \
    ".token{background:#f1f3f4;border:1px solid #ccc;padding:10px;font-family:monospace;" \
    "word-break:break-all;font-size:1.1em;margin:8px 0}" \
    ".warn{color:#c00;font-weight:bold}</style></head><body>" \
    "<h2>Configuration enregistr&#233;e &#10003;</h2>" \
    "<p class='warn'>Copiez ce token maintenant&nbsp;&mdash; il ne sera plus affich&#233;&nbsp;:</p>" \
    "<div class='token'>%s</div>" \
    "<p>&#128274; Conservez-le en lieu s&#251;r : il authentifie le Core sur ce device.</p>" \
    "<p>Le device red&#233;marre dans 15&nbsp;secondes&#8230;</p>" \
    "</body></html>"

// ── Handlers HTTP du portail ──────────────────────────────────────────────────

static esp_err_t h_get(httpd_req_t *req) {
    uint8_t mac[6];
    esp_read_mac(mac, ESP_MAC_BASE);
    char def_id[24];
    snprintf(def_id, sizeof(def_id), "embewi-%02x%02x%02x", mac[3], mac[4], mac[5]);

    httpd_resp_set_type(req, "text/html");
    httpd_resp_send_chunk(req, SETUP_HTML_A, HTTPD_RESP_USE_STRLEN);
    httpd_resp_send_chunk(req, def_id, HTTPD_RESP_USE_STRLEN);
    httpd_resp_send_chunk(req, SETUP_HTML_B, HTTPD_RESP_USE_STRLEN);
    httpd_resp_send_chunk(req, NULL, 0);
    return ESP_OK;
}

static esp_err_t h_post(httpd_req_t *req) {
    if (req->content_len == 0 || req->content_len > 600) {
        httpd_resp_set_status(req, "400 Bad Request");
        httpd_resp_sendstr(req, "body manquant ou trop grand");
        return ESP_OK;
    }
    char buf[601] = {0};
    int r = httpd_req_recv(req, buf, req->content_len);
    if (r <= 0) return ESP_FAIL;
    buf[r] = '\0';

    prov_cfg_t cfg = {0};
    char token[65] = {0};
    if (!form_field(buf, "ssid",     cfg.ssid,     sizeof(cfg.ssid))     || cfg.ssid[0] == '\0' ||
        !form_field(buf, "ctrl_url", cfg.ctrl_url, sizeof(cfg.ctrl_url)) || cfg.ctrl_url[0] == '\0' ||
        !form_field(buf, "node_id",  cfg.node_id,  sizeof(cfg.node_id))  || cfg.node_id[0] == '\0') {
        httpd_resp_set_status(req, "400 Bad Request");
        httpd_resp_sendstr(req, "ssid, ctrl_url et node_id sont obligatoires");
        return ESP_OK;
    }
    form_field(buf, "pass",  cfg.pass,  sizeof(cfg.pass));
    form_field(buf, "ip",    cfg.ip,    sizeof(cfg.ip));
    form_field(buf, "mask",  cfg.mask,  sizeof(cfg.mask));
    form_field(buf, "gw",    cfg.gw,    sizeof(cfg.gw));
    form_field(buf, "dns",   cfg.dns,   sizeof(cfg.dns));
    form_field(buf, "token", token,     sizeof(token));

    // IP statique : gateway vide → IP du contrôleur (extrait de ctrl_url)
    if (cfg.ip[0] && cfg.gw[0] == '\0')
        url_host(cfg.ctrl_url, cfg.gw, sizeof(cfg.gw));

    // IP partielle → erreur explicite
    if ((cfg.ip[0] || cfg.mask[0] || cfg.gw[0]) &&
        !(cfg.ip[0] && cfg.mask[0] && cfg.gw[0])) {
        httpd_resp_set_status(req, "400 Bad Request");
        httpd_resp_sendstr(req, "ip et mask sont obligatoires si IP statique");
        return ESP_OK;
    }
    // DNS vide → passerelle par défaut
    if (cfg.ip[0] && cfg.dns[0] == '\0')
        strlcpy(cfg.dns, cfg.gw, sizeof(cfg.dns));

    // Token vide → génération aléatoire ici, avant le reboot, pour l'afficher.
    if (token[0] == '\0') {
        uint8_t rnd[16];
        esp_fill_random(rnd, sizeof(rnd));
        for (int i = 0; i < 16; i++) snprintf(token + i * 2, 3, "%02x", rnd[i]);
        token[32] = '\0';
    }

    esp_err_t err = prov_save(&cfg);
    if (err == ESP_OK) err = embewi_token_save(token);
    if (err != ESP_OK) {
        ESP_LOGE(TAG, "NVS save failed: %s", esp_err_to_name(err));
        httpd_resp_set_status(req, "500 Internal Server Error");
        httpd_resp_sendstr(req, "erreur NVS");
        return ESP_OK;
    }

    // Page de confirmation avec le token en clair — unique occasion de l'afficher.
    char ok_page[900];
    snprintf(ok_page, sizeof(ok_page), OK_HTML_FMT, token);
    httpd_resp_set_type(req, "text/html");
    httpd_resp_sendstr(req, ok_page);
    ESP_LOGI(TAG, "config enregistrée ssid=%s ctrl=%s → reboot dans 15s", cfg.ssid, cfg.ctrl_url);
    // 15 s pour que l'admin copie le token avant le reboot.
    vTaskDelay(pdMS_TO_TICKS(15000));
    esp_restart();
    return ESP_OK;
}

// ── Mode AP + portail captif ──────────────────────────────────────────────────

static void run_ap_portal(void) {
    uint8_t mac[6];
    esp_wifi_get_mac(WIFI_IF_AP, mac);
    char ap_ssid[24];
    snprintf(ap_ssid, sizeof(ap_ssid), "embewi-%02x%02x", mac[4], mac[5]);

    wifi_config_t ap_cfg = {
        .ap = {
            .max_connection = 2,
            .authmode = WIFI_AUTH_OPEN,
        },
    };
    strlcpy((char *)ap_cfg.ap.ssid, ap_ssid, sizeof(ap_cfg.ap.ssid));
    ap_cfg.ap.ssid_len = strlen(ap_ssid);

    ESP_ERROR_CHECK(esp_wifi_set_mode(WIFI_MODE_AP));
    ESP_ERROR_CHECK(esp_wifi_set_config(WIFI_IF_AP, &ap_cfg));
    ESP_ERROR_CHECK(esp_wifi_start());

    ESP_LOGW(TAG, "AP démarré : SSID=%s — connectez-vous et ouvrez http://192.168.4.1",
             ap_ssid);

    httpd_handle_t srv = NULL;
    httpd_config_t cfg = HTTPD_DEFAULT_CONFIG();
    cfg.uri_match_fn = httpd_uri_match_wildcard;
    ESP_ERROR_CHECK(httpd_start(&srv, &cfg));

    const httpd_uri_t routes[] = {
        { .uri = "/",      .method = HTTP_GET,  .handler = h_get  },
        { .uri = "/setup", .method = HTTP_GET,  .handler = h_get  },
        { .uri = "/setup", .method = HTTP_POST, .handler = h_post },
        // Réponse générique pour les sondes de portail captif des OS
        { .uri = "/*",     .method = HTTP_GET,  .handler = h_get  },
    };
    for (size_t i = 0; i < sizeof(routes) / sizeof(routes[0]); i++)
        httpd_register_uri_handler(srv, &routes[i]);

    // Bloque indéfiniment : h_post fait esp_restart() après sauvegarde.
    while (true) vTaskDelay(pdMS_TO_TICKS(1000));
}

// ── Mode STA avec credentials NVS ────────────────────────────────────────────

static void wifi_event_handler(void *arg, esp_event_base_t base,
                               int32_t id, void *data) {
    if (base == WIFI_EVENT && id == WIFI_EVENT_STA_DISCONNECTED) {
        if (s_retry < WIFI_MAX_RETRY) {
            esp_wifi_connect();
            s_retry++;
        } else {
            xEventGroupSetBits(s_wifi_eg, WIFI_FAIL_BIT);
        }
    } else if (base == IP_EVENT && id == IP_EVENT_STA_GOT_IP) {
        ip_event_got_ip_t *e = (ip_event_got_ip_t *)data;
        ESP_LOGI(TAG, "wifi connecté ip=" IPSTR, IP2STR(&e->ip_info.ip));
        s_retry = 0;
        xEventGroupSetBits(s_wifi_eg, WIFI_CONNECTED_BIT);
    }
}

static void connect_sta(const prov_cfg_t *cfg) {
    s_wifi_eg = xEventGroupCreate();

    esp_event_handler_instance_t h_wifi, h_ip;
    ESP_ERROR_CHECK(esp_event_handler_instance_register(
        WIFI_EVENT, ESP_EVENT_ANY_ID, wifi_event_handler, NULL, &h_wifi));
    ESP_ERROR_CHECK(esp_event_handler_instance_register(
        IP_EVENT, IP_EVENT_STA_GOT_IP, wifi_event_handler, NULL, &h_ip));

    esp_netif_t *netif = esp_netif_create_default_wifi_sta();

    // IP statique : désactiver DHCP et configurer manuellement avant la connexion.
    if (cfg->ip[0]) {
        ESP_ERROR_CHECK(esp_netif_dhcpc_stop(netif));

        esp_netif_ip_info_t ip_info = {0};
        ip_info.ip.addr      = inet_addr(cfg->ip);
        ip_info.netmask.addr = inet_addr(cfg->mask);
        ip_info.gw.addr      = inet_addr(cfg->gw);
        ESP_ERROR_CHECK(esp_netif_set_ip_info(netif, &ip_info));

        const char *dns_str = cfg->dns[0] ? cfg->dns : cfg->gw;
        esp_netif_dns_info_t dns = {0};
        dns.ip.u_addr.ip4.addr = inet_addr(dns_str);
        dns.ip.type = IPADDR_TYPE_V4;
        ESP_ERROR_CHECK(esp_netif_set_dns_info(netif, ESP_NETIF_DNS_MAIN, &dns));

        ESP_LOGI(TAG, "IP statique : %s/%s gw=%s dns=%s",
                 cfg->ip, cfg->mask, cfg->gw, dns_str);
    }

    wifi_config_t wcfg = {0};
    strlcpy((char *)wcfg.sta.ssid,     cfg->ssid, sizeof(wcfg.sta.ssid));
    strlcpy((char *)wcfg.sta.password, cfg->pass,  sizeof(wcfg.sta.password));
    wcfg.sta.threshold.authmode = cfg->pass[0] ? WIFI_AUTH_WPA2_PSK : WIFI_AUTH_OPEN;

    ESP_ERROR_CHECK(esp_wifi_set_mode(WIFI_MODE_STA));
    ESP_ERROR_CHECK(esp_wifi_set_config(WIFI_IF_STA, &wcfg));
    ESP_ERROR_CHECK(esp_wifi_start());
    esp_wifi_connect();

    EventBits_t bits = xEventGroupWaitBits(s_wifi_eg,
        WIFI_CONNECTED_BIT | WIFI_FAIL_BIT, pdFALSE, pdFALSE,
        pdMS_TO_TICKS(15000));
    if (!(bits & WIFI_CONNECTED_BIT)) {
        ESP_LOGW(TAG, "wifi non disponible — mode dégradé réseau");
    }
}

// ── Node ID + Token par node ──────────────────────────────────────────────────

void embewi_node_id_load(char *out, size_t len) {
    out[0] = '\0';
    nvs_handle_t h;
    if (nvs_open(PROV_NVS_NS, NVS_READONLY, &h) == ESP_OK) {
        size_t sz = len;
        nvs_get_str(h, PROV_KEY_NODE_ID, out, &sz);
        nvs_close(h);
    }
    if (out[0] != '\0') {
        ESP_LOGI(TAG, "node_id: %s", out);
    } else {
        uint8_t mac[6];
        esp_read_mac(mac, ESP_MAC_BASE);
        snprintf(out, len, "embewi-%02x%02x%02x", mac[3], mac[4], mac[5]);
        ESP_LOGW(TAG, "node_id absent NVS → ID temporaire %s (reprovisionner)", out);
    }
}

void embewi_token_load(char *out, size_t len) {
    out[0] = '\0';
    nvs_handle_t h;
    if (nvs_open(PROV_NVS_NS, NVS_READONLY, &h) == ESP_OK) {
        size_t tok_len = len;
        nvs_get_str(h, PROV_KEY_TOKEN, out, &tok_len);
        nvs_close(h);
    }
    if (out[0] != '\0') {
        ESP_LOGI(TAG, "token chargé depuis NVS");
    } else {
        // Cas rare : flash direct sans portail. Repasser par le portail captif
        // (effacer NVS ou laisser ssid vide) pour obtenir un token affiché.
        ESP_LOGE(TAG, "AUCUN TOKEN — tous les appels inbound seront refusés (401)."
                      " Reprovisionner via le portail captif.");
    }
}

esp_err_t embewi_token_save(const char *token) {
    nvs_handle_t h;
    esp_err_t err = nvs_open(PROV_NVS_NS, NVS_READWRITE, &h);
    if (err != ESP_OK) return err;
    err = nvs_set_str(h, PROV_KEY_TOKEN, token);
    if (err == ESP_OK) err = nvs_commit(h);
    nvs_close(h);
    return err;
}

// ── Point d'entrée public ─────────────────────────────────────────────────────

void embewi_wifi_start(char *out_ctrl_url, size_t url_len) {
    ESP_ERROR_CHECK(esp_netif_init());
    ESP_ERROR_CHECK(esp_event_loop_create_default());

    wifi_init_config_t wcfg = WIFI_INIT_CONFIG_DEFAULT();
    ESP_ERROR_CHECK(esp_wifi_init(&wcfg));

    prov_cfg_t cfg;
    if (prov_load(&cfg)) {
        ESP_LOGI(TAG, "config NVS trouvée → STA ssid=%s", cfg.ssid);
        connect_sta(&cfg);
        if (out_ctrl_url) strlcpy(out_ctrl_url, cfg.ctrl_url, url_len);
    } else {
        ESP_LOGW(TAG, "aucune config NVS → portail captif AP");
        esp_netif_create_default_wifi_ap();
        run_ap_portal();   // ne retourne pas (reboot après submit)
    }
}
