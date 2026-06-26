// embewi_ota.c — réception OTA + idempotence (contrat §3 §4 §6)
//
// L'ESP ne connaît ni OCI ni ORAS : il reçoit des octets bruts, les écrit dans
// le slot inactif, calcule le SHA-256 EN INCRÉMENTAL (pas de relecture flash).

#include <string.h>
#include "esp_log.h"
#include "esp_ota_ops.h"
#include "esp_partition.h"
#include "esp_system.h"
#include "psa/crypto.h"
#include "nvs.h"
#include "nvs_flash.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "embewi_agent.h"

static const char *TAG = "embewi.ota";
#define NVS_NS  "embewi"
#define NVS_KEY_STAGED "staged"

// --- État de session d'écriture (une seule à la fois) -----------------------
static esp_ota_handle_t   s_ota_handle;
static const esp_partition_t *s_target;
static psa_hash_operation_t s_sha;
static uint32_t           s_written;
static bool               s_writing;

// --- Staged NVS (idempotence reconcile) -------------------------------------
esp_err_t embewi_staged_load(embewi_staged_t *out) {
    memset(out, 0, sizeof(*out));
    out->stage = EMBEWI_STAGE_NONE;
    nvs_handle_t h;
    if (nvs_open(NVS_NS, NVS_READONLY, &h) != ESP_OK) return ESP_OK;
    size_t len = sizeof(*out);
    esp_err_t err = nvs_get_blob(h, NVS_KEY_STAGED, out, &len);
    nvs_close(h);
    if (err != ESP_OK) { out->stage = EMBEWI_STAGE_NONE; }
    return ESP_OK;
}

esp_err_t embewi_staged_save(const embewi_staged_t *s) {
    nvs_handle_t h;
    esp_err_t err = nvs_open(NVS_NS, NVS_READWRITE, &h);
    if (err != ESP_OK) return err;
    err = nvs_set_blob(h, NVS_KEY_STAGED, s, sizeof(*s));
    if (err == ESP_OK) err = nvs_commit(h);
    nvs_close(h);
    return err;
}

esp_err_t embewi_staged_clear(void) {
    embewi_staged_t none = { .stage = EMBEWI_STAGE_NONE };
    return embewi_staged_save(&none);
}

// --- Taille image courante (persistée après mark_valid, pour recalcul digest) -
#define NVS_KEY_FW_SIZE "fw_size"

esp_err_t embewi_fw_size_save(uint32_t size) {
    nvs_handle_t h;
    esp_err_t err = nvs_open(NVS_NS, NVS_READWRITE, &h);
    if (err != ESP_OK) return err;
    err = nvs_set_u32(h, NVS_KEY_FW_SIZE, size);
    if (err == ESP_OK) err = nvs_commit(h);
    nvs_close(h);
    return err;
}

esp_err_t embewi_fw_size_load(uint32_t *out) {
    *out = 0;
    nvs_handle_t h;
    if (nvs_open(NVS_NS, NVS_READONLY, &h) != ESP_OK) return ESP_OK;
    nvs_get_u32(h, NVS_KEY_FW_SIZE, out);
    nvs_close(h);
    return ESP_OK;
}

// --- Port du service applicatif (NVS, défaut 8080) --------------------------
#define NVS_KEY_APP_PORT "app_port"
#define APP_PORT_DEFAULT 8080

esp_err_t embewi_app_port_load(uint16_t *port) {
    *port = APP_PORT_DEFAULT;
    nvs_handle_t h;
    if (nvs_open(NVS_NS, NVS_READONLY, &h) != ESP_OK) return ESP_OK;
    nvs_get_u16(h, NVS_KEY_APP_PORT, port);
    nvs_close(h);
    return ESP_OK;
}

esp_err_t embewi_app_port_save(uint16_t port) {
    nvs_handle_t h;
    esp_err_t err = nvs_open(NVS_NS, NVS_READWRITE, &h);
    if (err != ESP_OK) return err;
    err = nvs_set_u16(h, NVS_KEY_APP_PORT, port);
    if (err == ESP_OK) err = nvs_commit(h);
    nvs_close(h);
    return err;
}

// --- /ota/prepare : compat AVANT tout octet ---------------------------------
esp_err_t embewi_ota_prepare(const embewi_ota_prepare_t *req,
                             char *out_slot, size_t slot_len,
                             const char **out_reason) {
    *out_reason = NULL;

    // Compat puce / layout — CONFIG_IDF_TARGET est défini au build ("esp32c3", etc.)
    if (strcmp(req->chip, CONFIG_IDF_TARGET) != 0) {
        *out_reason = "chip_mismatch"; return ESP_FAIL;
    }
    if (strcmp(req->partition_layout, "embewi-ab-v1") != 0) {
        *out_reason = "layout_mismatch"; return ESP_FAIL;
    }

    const esp_partition_t *next = esp_ota_get_next_update_partition(NULL);
    if (!next) { *out_reason = "busy"; return ESP_FAIL; }
    if (req->size > next->size) { *out_reason = "size_too_large"; return ESP_FAIL; }

    strlcpy(out_slot, next->label, slot_len);
    ESP_LOGI(TAG, "prepare OK dep=%s → slot=%s", req->deployment_id, next->label);
    return ESP_OK;
}

// --- /ota/write : flux + SHA-256 incrémental --------------------------------
// Offset déjà écrit dans la session courante (0 si aucune). Sert à la reprise
// Content-Range : le Core resynchronise sur cette valeur après une coupure.
uint32_t embewi_ota_written(void)     { return s_written; }
bool     embewi_ota_in_progress(void) { return s_writing; }

esp_err_t embewi_ota_write_begin(void) {
    // Une session incomplète traîne (coupure puis Core qui recommence à 0) :
    // on l'avorte proprement avant d'en rouvrir une — sinon handle/SHA fuités.
    if (s_writing) {
        esp_ota_abort(s_ota_handle);
        psa_hash_abort(&s_sha);
        s_writing = false;
    }
    s_target = esp_ota_get_next_update_partition(NULL);
    if (!s_target) return ESP_FAIL;
    esp_err_t err = esp_ota_begin(s_target, OTA_SIZE_UNKNOWN, &s_ota_handle);
    if (err != ESP_OK) return err;
    psa_crypto_init();
    s_sha = psa_hash_operation_init();
    psa_hash_setup(&s_sha, PSA_ALG_SHA_256);
    s_written = 0;
    s_writing = true;
    return ESP_OK;
}

esp_err_t embewi_ota_write_chunk(const uint8_t *data, size_t len) {
    if (!s_writing) return ESP_ERR_INVALID_STATE;
    esp_err_t err = esp_ota_write(s_ota_handle, data, len);
    if (err != ESP_OK) return err;
    psa_hash_update(&s_sha, data, len);
    s_written += len;
    return ESP_OK;
}

esp_err_t embewi_ota_write_finish(const char *expected_digest,
                                  uint32_t *out_written,
                                  char *out_digest, size_t digest_len) {
    if (!s_writing) return ESP_ERR_INVALID_STATE;
    s_writing = false;

    unsigned char raw[32];
    size_t raw_len;
    psa_hash_finish(&s_sha, raw, sizeof(raw), &raw_len);
    s_sha = psa_hash_operation_init();

    char hex[72] = "sha256:";
    for (int i = 0; i < 32; i++) sprintf(hex + 7 + i * 2, "%02x", raw[i]);

    // Vérifie ce qu'on a RÉELLEMENT écrit (le Core a vérifié pour l'efficacité).
    if (expected_digest && strcmp(hex, expected_digest) != 0) {
        ESP_LOGE(TAG, "digest_mismatch attendu=%s calc=%s", expected_digest, hex);
        esp_ota_abort(s_ota_handle);
        return ESP_ERR_INVALID_CRC;   // mappé en status "digest_mismatch" côté HTTP
    }

    esp_err_t err = esp_ota_end(s_ota_handle);   // valide aussi la signature image
    if (err != ESP_OK) return err;

    if (out_written) *out_written = s_written;
    if (out_digest)  strlcpy(out_digest, hex, digest_len);

    // Persiste l'état stagé : reconcile reprenable (contrat §6).
    embewi_staged_t staged = { .stage = EMBEWI_STAGE_WRITTEN };
    strlcpy(staged.slot, s_target->label, sizeof(staged.slot));
    strlcpy(staged.digest, hex, sizeof(staged.digest));
    staged.size = s_written;
    embewi_staged_save(&staged);
    ESP_LOGI(TAG, "write OK %u octets slot=%s → staged=written",
             (unsigned)s_written, s_target->label);
    return ESP_OK;
}

// --- /ota/activate : set boot + stage=activating + reboot -------------------
esp_err_t embewi_ota_activate(const char *deployment_id) {
    if (!s_target) {
        // reprise possible : recharger le slot stagé depuis NVS si process relancé
        return ESP_ERR_INVALID_STATE;
    }
    embewi_staged_t staged;
    embewi_staged_load(&staged);
    staged.stage = EMBEWI_STAGE_ACTIVATING;
    strlcpy(staged.deployment_id, deployment_id, sizeof(staged.deployment_id));
    embewi_staged_save(&staged);

    esp_err_t err = esp_ota_set_boot_partition(s_target);
    if (err != ESP_OK) return err;

    ESP_LOGW(TAG, "activate dep=%s → reboot sur %s", deployment_id, s_target->label);
    embewi_log_emit("info", "ota activate, rebooting");
    vTaskDelay(pdMS_TO_TICKS(200));   // laisse partir la réponse HTTP + log
    esp_restart();
    return ESP_OK;   // non atteint
}
