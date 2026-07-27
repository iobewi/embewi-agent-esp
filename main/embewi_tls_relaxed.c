// embewi_tls_relaxed.c — canal de détresse NTP (contrat §5, "clock_unsynced")
//
// Tant que SNTP n'a pas convergé, l'horloge ESP est à 1970 et le cert du Core
// paraît "pas encore valide". esp_http_client (esp-tls) ne permet PAS de ne
// relâcher QUE la validité temporelle (notBefore/notAfter) d'une CA : dès
// qu'un cacert est configuré, esp-tls force MBEDTLS_SSL_VERIFY_REQUIRED
// (esp_tls_mbedtls.c:set_ca_cert dans le composant esp-tls) et TOUT badcert
// flag est fatal au handshake — chaîne, CN et date sont tout ou rien.
//
// On descend donc au niveau mbedTLS brut pour ce cas précis : authmode
// OPTIONAL (le handshake aboutit même avec des badcert flags), puis
// inspection manuelle du résultat — seuls BADCERT_EXPIRED/BADCERT_FUTURE
// sont tolérés, tout le reste (chaîne non fiable, CN, révocation...) reste
// bloquant, comme l'exige le contrat.
//
// Utilisé uniquement en prod (CONFIG_EMBEWI_VERIFY_CORE_CERT) : en dev il n'y
// a de toute façon aucune vérification de certificat sortant.
//
// Best-effort, comme emit_to() (embewi_heartbeat.c) : toute erreur est
// loggée puis abandonnée, sans retry — le prochain tick heartbeat réessaiera.

#include "sdkconfig.h"

#if CONFIG_EMBEWI_VERIFY_CORE_CERT

#include <string.h>
#include <stdio.h>
#include "esp_log.h"
#include "mbedtls/ssl.h"
#include "mbedtls/net_sockets.h"
#include "mbedtls/x509_crt.h"
#include "embewi_agent.h"

static const char *TAG = "embewi.tls_relaxed";

// CA embarquée (main/core_ca.pem) qui signe le cert serveur du Core.
extern const char core_ca_pem_start[] asm("_binary_core_ca_pem_start");

void embewi_tls_relaxed_post(const char *host, uint16_t port,
                             const char *path, const char *json) {
    mbedtls_net_context    server_fd;
    mbedtls_ssl_context    ssl;
    mbedtls_ssl_config     conf;
    mbedtls_x509_crt       cacert;
    int ret;

    mbedtls_net_init(&server_fd);
    mbedtls_ssl_init(&ssl);
    mbedtls_ssl_config_init(&conf);
    mbedtls_x509_crt_init(&cacert);

    // Pas de mbedtls_ssl_conf_rng ici : sur cet IDF (mbedTLS 4.x /
    // TF-PSA-Crypto), le RNG est fourni en interne par PSA — il suffit que
    // psa_crypto_init() ait été appelé une fois (fait au boot dans
    // embewi_main.c pour le SHA-256 incrémental, avant tout flux sortant).
    //
    // +1 : mbedtls_x509_crt_parse veut le NUL terminal dans la longueur pour
    // détecter un buffer PEM (vs DER).
    if ((ret = mbedtls_x509_crt_parse(&cacert, (const unsigned char *)core_ca_pem_start,
                                      strlen(core_ca_pem_start) + 1)) < 0) {
        ESP_LOGW(TAG, "x509_crt_parse CA: -0x%04x", (unsigned)-ret);
        goto cleanup;
    }

    char port_str[6];
    snprintf(port_str, sizeof(port_str), "%u", port ? port : 443);
    if ((ret = mbedtls_net_connect(&server_fd, host, port_str,
                                   MBEDTLS_NET_PROTO_TCP)) != 0) {
        ESP_LOGW(TAG, "net_connect %s:%s: -0x%04x", host, port_str, (unsigned)-ret);
        goto cleanup;
    }

    if ((ret = mbedtls_ssl_config_defaults(&conf, MBEDTLS_SSL_IS_CLIENT,
                    MBEDTLS_SSL_TRANSPORT_STREAM, MBEDTLS_SSL_PRESET_DEFAULT)) != 0) {
        ESP_LOGW(TAG, "ssl_config_defaults: -0x%04x", (unsigned)-ret);
        goto cleanup;
    }
    mbedtls_ssl_conf_ca_chain(&conf, &cacert, NULL);
    // OPTIONAL : le handshake n'échoue pas sur un badcert flag — la décision
    // (quels flags tolérer) se prend APRÈS, sur le résultat de vérification.
    mbedtls_ssl_conf_authmode(&conf, MBEDTLS_SSL_VERIFY_OPTIONAL);

    if ((ret = mbedtls_ssl_setup(&ssl, &conf)) != 0) {
        ESP_LOGW(TAG, "ssl_setup: -0x%04x", (unsigned)-ret);
        goto cleanup;
    }
    // CN/SAN toujours vérifiés (mbedtls_ssl_set_hostname) : seule la fenêtre
    // temporelle est relâchée, jamais l'identité du serveur.
    if ((ret = mbedtls_ssl_set_hostname(&ssl, host)) != 0) {
        ESP_LOGW(TAG, "ssl_set_hostname: -0x%04x", (unsigned)-ret);
        goto cleanup;
    }
    mbedtls_ssl_set_bio(&ssl, &server_fd, mbedtls_net_send, mbedtls_net_recv, NULL);

    while ((ret = mbedtls_ssl_handshake(&ssl)) != 0) {
        if (ret != MBEDTLS_ERR_SSL_WANT_READ && ret != MBEDTLS_ERR_SSL_WANT_WRITE) {
            ESP_LOGW(TAG, "ssl_handshake: -0x%04x", (unsigned)-ret);
            goto cleanup;
        }
    }

    // Cœur de la relaxation (§5, NORMATIF) : seuls EXPIRED/FUTURE sont
    // tolérés. Tout autre flag (NOT_TRUSTED, CN_MISMATCH, révocation...) fait
    // échouer la connexion — le canal de détresse ne dispense JAMAIS de la
    // vérification de chaîne/CN.
    {
        uint32_t flags = mbedtls_ssl_get_verify_result(&ssl);
        if (flags & ~(uint32_t)(MBEDTLS_X509_BADCERT_EXPIRED | MBEDTLS_X509_BADCERT_FUTURE)) {
            char vrfy_buf[128];
            mbedtls_x509_crt_verify_info(vrfy_buf, sizeof(vrfy_buf), "  ! ", flags);
            ESP_LOGW(TAG, "cert Core rejeté (au-delà de la fenêtre temporelle): %s", vrfy_buf);
            goto close_notify;
        }
    }

    {
        char req[640];
        int n = snprintf(req, sizeof(req),
            "POST %s HTTP/1.1\r\nHost: %s\r\nContent-Type: application/json\r\n"
            "Content-Length: %d\r\nConnection: close\r\n\r\n%s",
            path, host, (int)strlen(json), json);
        if (n < 0 || (size_t)n >= sizeof(req)) {
            ESP_LOGW(TAG, "requête trop grande pour le buffer (%d)", n);
            goto close_notify;
        }
        int off = 0;
        while (off < n) {
            ret = mbedtls_ssl_write(&ssl, (const unsigned char *)req + off, (size_t)(n - off));
            if (ret == MBEDTLS_ERR_SSL_WANT_READ || ret == MBEDTLS_ERR_SSL_WANT_WRITE)
                continue;
            if (ret < 0) {
                ESP_LOGW(TAG, "ssl_write: -0x%04x", (unsigned)-ret);
                goto close_notify;
            }
            off += ret;
        }
        ESP_LOGD(TAG, "POST %s (clock_unsynced, cert non authentifié en date) → envoyé", path);
    }

close_notify:
    mbedtls_ssl_close_notify(&ssl);
cleanup:
    mbedtls_net_free(&server_fd);
    mbedtls_x509_crt_free(&cacert);
    mbedtls_ssl_free(&ssl);
    mbedtls_ssl_config_free(&conf);
}

#endif /* CONFIG_EMBEWI_VERIFY_CORE_CERT */
