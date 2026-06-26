// embewi_time.c — synchronisation NTP (SNTP) de l'horloge murale.
//
// Pourquoi c'est critique, pas cosmétique :
//   1. Les timestamps (heartbeat, logs) doivent être en EPOCH UTC pour être
//      corrélables côté Kubernetes/Loki. Sans NTP, l'horloge ESP démarre à 1970
//      et `ts` ne vaut que l'uptime (repart à 0 à chaque reboot).
//   2. La vérif du cert du Core (prod, CONFIG_EMBEWI_VERIFY_CORE_CERT) contrôle
//      les dates de validité. Horloge à 1970 → mbedTLS juge le cert « pas encore
//      valide » → handshake TLS échoue. NTP est donc un PRÉ-REQUIS du TLS
//      authentifié en prod, pas un confort.
//
// Source du serveur : clé McuConfigMap `ntp_server` (poussée par le Core), sinon
// défaut `pool.ntp.org`. Le NTP fourni par DHCP est aussi utilisé (LAN managé).

#include <string.h>
#include <time.h>
#include "esp_log.h"
#include "esp_netif_sntp.h"
#include "freertos/FreeRTOS.h"
#include "embewi_agent.h"

static const char *TAG = "embewi.time";

void embewi_time_sync_start(void) {
    // static : lwip SNTP conserve un POINTEUR sur la chaîne (pas de copie) →
    // elle doit survivre au retour de cette fonction.
    static char server[64];
    if (!embewi_cfg_get("ntp_server", server, sizeof(server)))
        strlcpy(server, "pool.ntp.org", sizeof(server));

    esp_sntp_config_t cfg = ESP_NETIF_SNTP_DEFAULT_CONFIG(server);
    cfg.start                      = true;
    cfg.server_from_dhcp           = true;   // NTP fourni par DHCP (LAN managé)
    cfg.renew_servers_after_new_IP = true;

    esp_err_t err = esp_netif_sntp_init(&cfg);
    if (err != ESP_OK)
        ESP_LOGW(TAG, "sntp init échec: %s", esp_err_to_name(err));
    else
        ESP_LOGI(TAG, "SNTP démarré (serveur=%s + DHCP)", server);
}

bool embewi_time_is_set(void) {
    // > 2023-11-14 → l'horloge a forcément été synchronisée (boot = 1970).
    return time(NULL) > 1700000000;
}

bool embewi_time_wait(int timeout_ms) {
    if (esp_netif_sntp_sync_wait(pdMS_TO_TICKS(timeout_ms)) == ESP_OK)
        return true;
    return embewi_time_is_set();   // déjà synchronisé par un cycle antérieur ?
}
