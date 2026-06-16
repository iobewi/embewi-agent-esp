// embewi_tls.c — certificat TLS agent (NVS + fallback auto-signé embarqué)
//
// Priorité : NVS "embewi_tls" (cert signé CA livré par le Core via POST /tls/cert)
// Fallback : cert EC P-256 auto-signé généré offline, embarqué au build.
//            Fournit le chiffrement transport dès le premier boot.
//            Le Core doit faire du certificate pinning en mode fallback.

#include <string.h>
#include "esp_log.h"
#include "nvs.h"
#include "nvs_flash.h"
#include "embewi_agent.h"

static const char *TAG = "embewi.tls";

#define NVS_NS_TLS   "embewi_tls"
#define NVS_KEY_CERT "cert"
#define NVS_KEY_KEY  "key"

// Buffers statiques — durée de vie = celle du serveur HTTPS.
// Taille : RSA-4096 PEM ≈ 3 KB cert, EC P-256 ≈ 300 B key.
// On surdimensionne pour supporter des certs CA-signés RSA si besoin.
#define CERT_BUF_MAX 4096
#define KEY_BUF_MAX  2048
static uint8_t s_cert[CERT_BUF_MAX];
static uint8_t s_key[KEY_BUF_MAX];
static size_t  s_cert_len;
static size_t  s_key_len;

// Cert + clé auto-signés embarqués via EMBED_TXTFILES (CMakeLists).
// ESP-IDF ajoute un '\0' terminal → la taille inclut le null-terminator,
// ce qu'attend mbedTLS pour du PEM.
extern const uint8_t embewi_fallback_crt_start[] asm("_binary_embewi_fallback_crt_start");
extern const uint8_t embewi_fallback_crt_end[]   asm("_binary_embewi_fallback_crt_end");
extern const uint8_t embewi_fallback_key_start[]  asm("_binary_embewi_fallback_key_start");
extern const uint8_t embewi_fallback_key_end[]    asm("_binary_embewi_fallback_key_end");

esp_err_t embewi_tls_load(const uint8_t **cert, size_t *cert_len,
                           const uint8_t **key,  size_t *key_len) {
    bool from_nvs = false;
    nvs_handle_t h;

    if (nvs_open(NVS_NS_TLS, NVS_READONLY, &h) == ESP_OK) {
        size_t clen = CERT_BUF_MAX, klen = KEY_BUF_MAX;
        if (nvs_get_str(h, NVS_KEY_CERT, (char *)s_cert, &clen) == ESP_OK &&
            nvs_get_str(h, NVS_KEY_KEY,  (char *)s_key,  &klen) == ESP_OK) {
            s_cert_len = clen;   // inclut le '\0' terminal
            s_key_len  = klen;
            from_nvs   = true;
            ESP_LOGI(TAG, "cert chargé NVS (%u B)", (unsigned)clen);
        }
        nvs_close(h);
    }

    if (!from_nvs) {
        s_cert_len = embewi_fallback_crt_end - embewi_fallback_crt_start;
        s_key_len  = embewi_fallback_key_end  - embewi_fallback_key_start;
        memcpy(s_cert, embewi_fallback_crt_start, s_cert_len);
        memcpy(s_key,  embewi_fallback_key_start,  s_key_len);
        ESP_LOGW(TAG, "NVS vide → fallback auto-signé embarqué");
    }

    *cert = s_cert; *cert_len = s_cert_len;
    *key  = s_key;  *key_len  = s_key_len;
    return ESP_OK;
}

esp_err_t embewi_tls_save(const char *cert_pem, const char *key_pem) {
    nvs_handle_t h;
    esp_err_t err = nvs_open(NVS_NS_TLS, NVS_READWRITE, &h);
    if (err != ESP_OK) return err;
    err = nvs_set_str(h, NVS_KEY_CERT, cert_pem);
    if (err == ESP_OK) err = nvs_set_str(h, NVS_KEY_KEY, key_pem);
    if (err == ESP_OK) err = nvs_commit(h);
    nvs_close(h);
    if (err == ESP_OK) ESP_LOGI(TAG, "cert+key sauvés NVS → actifs au prochain boot");
    return err;
}
