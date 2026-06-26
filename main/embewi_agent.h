// embewi_agent.h — types partagés, machine d'état, contrat interne
#pragma once
#include <stdbool.h>
#include <stdint.h>
#include "esp_err.h"

#define EMBEWI_API_PREFIX        "/v1alpha1"
#define EMBEWI_FW_NAME           "wheel-controller"
#define EMBEWI_FW_VERSION        "1.0.0"

// Fenêtre de validation : si pas de mark_valid avant ça, reset → rollback (§3).
#define EMBEWI_PENDING_DEADLINE_MS   (15 * 1000)

// --- États de l'agent (contrat §2) -----------------------------------------
typedef enum {
    EMBEWI_BOOTING,
    EMBEWI_PENDING_VERIFY,
    EMBEWI_RUNNING,
    EMBEWI_DEGRADED,
    EMBEWI_ROLLBACK,
    EMBEWI_FAILED,
} embewi_state_t;

const char *embewi_state_str(embewi_state_t s);

// --- État stagé persisté en NVS (contrat §6, idempotence) -------------------
// staged.state ∈ none|written|activating. Permet au Core de reprendre un
// reconcile interrompu sans re-transférer le binaire.
typedef enum {
    EMBEWI_STAGE_NONE,
    EMBEWI_STAGE_WRITTEN,
    EMBEWI_STAGE_ACTIVATING,
} embewi_stage_state_t;

typedef struct {
    embewi_stage_state_t stage;
    char     slot[8];                 // "ota_0" | "ota_1"
    char     digest[72];              // "sha256:<hex>"
    char     deployment_id[64];
    uint32_t size;                    // octets écrits (pour recalcul digest au boot)
} embewi_staged_t;

esp_err_t embewi_staged_load(embewi_staged_t *out);   // lit NVS (none si absent)
esp_err_t embewi_staged_save(const embewi_staged_t *s);
esp_err_t embewi_staged_clear(void);

// --- État courant en RAM, lu par /info /health /heartbeat -------------------
typedef struct {
    embewi_state_t state;
    bool   ota_validated;             // true seulement après mark_valid
    char   active_slot[8];
    char   fw_version[16];
    char   fw_digest[72];
    char   deployment_id[64];         // déploiement courant validé
    char   node_id[64];               // identifiant du nœud (NVS prov, défaut embewi-AABBCC)
    char   ctrl_url[129];             // URL contrôleur Kubernetes (depuis NVS prov)
    char   token[65];                 // token Bearer par node (NVS prov, généré si absent)
    uint16_t app_http_port;           // port du service applicatif (NVS, défaut 8080)
    uint32_t cfg_generation;          // génération NVS courante (bumped par POST /config)
    uint32_t cfg_active_generation;   // génération chargée au boot (fixe jusqu'au reboot)
    // checks self-check
    bool   chk_app;
    bool   chk_sensors;
    bool   chk_storage;
} embewi_runtime_t;

embewi_runtime_t *embewi_rt(void);    // singleton process-wide

// --- OTA (embewi_ota.c) -----------------------------------------------------
typedef struct {
    char     deployment_id[64];
    char     digest[72];
    uint32_t size;
    char     chip[16];
    char     idf_version[16];
    char     partition_layout[24];
} embewi_ota_prepare_t;

// /ota/prepare : valide compat, réserve le slot inactif. reason rempli si refus.
esp_err_t embewi_ota_prepare(const embewi_ota_prepare_t *req,
                             char *out_slot, size_t slot_len,
                             const char **out_reason);

// /ota/write : feed incrémental. Le digest SHA-256 est calculé au fil de l'eau.
esp_err_t embewi_ota_write_begin(void);
esp_err_t embewi_ota_write_chunk(const uint8_t *data, size_t len);
// Reprise Content-Range (§4) : offset déjà écrit + session active ?
uint32_t  embewi_ota_written(void);
bool      embewi_ota_in_progress(void);
// finalize : compare digest calculé vs attendu, ferme la partition.
esp_err_t embewi_ota_write_finish(const char *expected_digest,
                                  uint32_t *out_written,
                                  char *out_digest, size_t digest_len);

// /ota/activate : set_boot_partition + persiste stage=activating, puis reboot.
esp_err_t embewi_ota_activate(const char *deployment_id);

// --- Self-check borné watchdog (embewi_selfcheck.c) -------------------------
// Démarre la task de validation. À appeler au boot SI on est en PENDING_VERIFY.
void embewi_selfcheck_start(void);

// --- Node ID + Token par node (embewi_provision.c) ---------------------------
// node_id : chargé depuis NVS ; si absent, ID temporaire depuis la MAC base.
void embewi_node_id_load(char *out, size_t len);
// token : chargé depuis NVS ; absent = tous les appels inbound refusés (401).
void embewi_token_load(char *out, size_t len);
esp_err_t embewi_token_save(const char *token);

// --- WiFi + provisioning (embewi_provision.c) --------------------------------
// Premier boot sans NVS → AP "embewi-XXXX" + portail captif (bloquant jusqu'au
// reboot post-config). Boots suivants → STA avec credentials NVS.
// out_ctrl_url reçoit l'URL du contrôleur Kubernetes (vide si wifi KO).
void embewi_wifi_start(char *out_ctrl_url, size_t url_len);

// --- Taille image courante persistée en NVS (embewi_ota.c) ------------------
// Sauvée après mark_valid. Permet de recalculer le digest depuis la flash au boot
// sur exactement les bons octets (sans inclure le padding 0xFF de la partition).
// 0 si l'image n'a jamais transité par un cycle OTA complet (premier flash direct).
esp_err_t embewi_fw_size_save(uint32_t size);
esp_err_t embewi_fw_size_load(uint32_t *out);

// --- Port NVS du service applicatif (embewi_ota.c) --------------------------
esp_err_t embewi_app_port_load(uint16_t *port);   // défaut 8080 si absent
esp_err_t embewi_app_port_save(uint16_t port);

// --- TLS : cert + clé pour le serveur HTTPS (embewi_tls.c) -----------------
// load : NVS en priorité, fallback auto-signé embarqué si absent.
// save : persiste cert+key PEM en NVS ; actif au prochain embewi_http_start().
esp_err_t embewi_tls_load(const uint8_t **cert, size_t *cert_len,
                           const uint8_t **key,  size_t *key_len);
esp_err_t embewi_tls_save(const char *cert_pem, const char *key_pem);

// --- HTTP/HTTPS server (embewi_http.c) --------------------------------------
esp_err_t embewi_http_start(void);
esp_err_t embewi_http_stop(void);

// --- Config runtime McuConfigMap (embewi_config.c) §4a §7a ------------------
// boot_init : snapshot de la génération NVS au boot. Appeler avant embewi_app_init().
void      embewi_cfg_boot_init(void);
// get / get_int : lecture NVS ; retourne false / default_val si clé absente.
bool      embewi_cfg_get(const char *key, char *out, size_t len);
int       embewi_cfg_get_int(const char *key, int default_val);
// write : écrit une clé (vide = effacer). Pas d'incrément de génération.
esp_err_t embewi_cfg_write(const char *key, const char *value);
// bump_generation : incrémente la génération NVS après un batch de write().
esp_err_t embewi_cfg_bump_generation(void);
// json_nvs : sérialise toutes les clés user en objet JSON (pour GET /config).
void      embewi_cfg_json_nvs(char *buf, size_t buf_len);

// --- Heartbeat / logs sortants (embewi_heartbeat.c) -------------------------
void embewi_heartbeat_start(void);
void embewi_log_emit(const char *level, const char *msg);

// --- Streaming logs ESP_LOGx via WebSocket (embewi_log.c) -------------------
// À appeler après embewi_heartbeat_start() (ctrl_url + réseau requis).
void embewi_log_start(void);

// --- Synchronisation horloge NTP (embewi_time.c) ----------------------------
// sync_start : démarre SNTP (réseau requis). À appeler dès le WiFi connecté.
// wait : bloque jusqu'à synchro ou timeout. Pré-requis du TLS authentifié prod.
void embewi_time_sync_start(void);
bool embewi_time_wait(int timeout_ms);
bool embewi_time_is_set(void);
