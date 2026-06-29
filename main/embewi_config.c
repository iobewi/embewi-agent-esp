// embewi_config.c — config runtime via NVS (McuConfigMap §4a §7a)
//
// Découple la configuration hardware (GPIO, fréquences…) du binaire OTA.
// Le Core pousse des clés/valeurs via POST /config ; l'app les lit au boot.
// Namespace NVS "embewi_cfg". Clés internes préfixées '_' (ex : _gen).

#include <string.h>
#include <stdlib.h>
#include <stdio.h>
#include "esp_log.h"
#include "nvs.h"
#include "nvs_flash.h"
#include "embewi_agent.h"

static const char *TAG = "embewi.cfg";

#define NVS_NS   "embewi_cfg"
#define KEY_GEN  "_gen"

static uint32_t s_gen;        // génération NVS, mis à jour à chaque bump
static uint32_t s_active_gen; // snapshot au boot, fixe jusqu'au prochain reboot
// Snapshot JSON des clés config au boot : ce que les apps ont RÉELLEMENT chargé
// (embewi_cfg_get au boot). Reste figé jusqu'au reboot, même si POST /config
// modifie le NVS entre-temps → GET /config peut comparer "active" vs "nvs" (§4).
static char     s_active_json[384];   // même taille que le nvs_json de h_config_get

// Appelé une fois au boot, après nvs_flash_init et memset(s_rt).
// Lit la génération NVS et l'adopte comme génération active de ce boot.
void embewi_cfg_boot_init(void) {
    s_gen = 0;
    nvs_handle_t h;
    if (nvs_open(NVS_NS, NVS_READONLY, &h) == ESP_OK) {
        nvs_get_u32(h, KEY_GEN, &s_gen);
        nvs_close(h);
    }
    s_active_gen = s_gen;
    embewi_rt()->cfg_generation        = s_gen;
    embewi_rt()->cfg_active_generation = s_active_gen;
    // Fige l'état NVS courant comme config "active" de ce boot.
    embewi_cfg_json_nvs(s_active_json, sizeof(s_active_json));
    ESP_LOGI(TAG, "boot init gen=%lu", (unsigned long)s_gen);
}

// Config "active" = snapshot figé au boot (cf. s_active_json). À distinguer de
// embewi_cfg_json_nvs() qui lit le NVS courant (peut diverger après POST /config).
void embewi_cfg_json_active(char *buf, size_t buf_len) {
    strlcpy(buf, s_active_json, buf_len);
}

// Lit une valeur string depuis NVS. Retourne false si clé absente ou vide.
bool embewi_cfg_get(const char *key, char *out, size_t len) {
    nvs_handle_t h;
    if (nvs_open(NVS_NS, NVS_READONLY, &h) != ESP_OK) return false;
    size_t sz = len;
    bool found = (nvs_get_str(h, key, out, &sz) == ESP_OK) && out[0] != '\0';
    nvs_close(h);
    return found;
}

// Lit un entier depuis NVS. Retourne default_val si clé absente ou non parseable.
int embewi_cfg_get_int(const char *key, int default_val) {
    char buf[16];
    if (!embewi_cfg_get(key, buf, sizeof(buf))) return default_val;
    char *end;
    long v = strtol(buf, &end, 10);
    return (end != buf) ? (int)v : default_val;
}

// Écrit une clé en NVS. Valeur vide → efface la clé (retour au défaut build).
// N'incrémente PAS la génération — appeler embewi_cfg_bump_generation() après le batch.
esp_err_t embewi_cfg_write(const char *key, const char *value) {
    nvs_handle_t h;
    esp_err_t err = nvs_open(NVS_NS, NVS_READWRITE, &h);
    if (err != ESP_OK) return err;
    if (value[0] == '\0') {
        err = nvs_erase_key(h, key);
        if (err == ESP_ERR_NVS_NOT_FOUND) err = ESP_OK;
    } else {
        err = nvs_set_str(h, key, value);
    }
    if (err == ESP_OK) err = nvs_commit(h);
    nvs_close(h);
    return err;
}

// Incrémente la génération NVS et met à jour le runtime.
// À appeler une fois après un batch de embewi_cfg_write().
esp_err_t embewi_cfg_bump_generation(void) {
    s_gen++;
    nvs_handle_t h;
    esp_err_t err = nvs_open(NVS_NS, NVS_READWRITE, &h);
    if (err != ESP_OK) { s_gen--; return err; }
    err = nvs_set_u32(h, KEY_GEN, s_gen);
    if (err == ESP_OK) err = nvs_commit(h);
    nvs_close(h);
    if (err == ESP_OK)
        embewi_rt()->cfg_generation = s_gen;
    else
        s_gen--;
    return err;
}

// Sérialise toutes les clés user (hors '_*') en objet JSON : {"k":"v",...}
// buf doit être assez large pour le résultat (512 octets recommandé).
void embewi_cfg_json_nvs(char *buf, size_t buf_len) {
    buf[0] = '{'; buf[1] = '\0';
    size_t pos = 1;
    bool first = true;

    nvs_handle_t h = 0;
    bool h_open = (nvs_open(NVS_NS, NVS_READONLY, &h) == ESP_OK);

    nvs_iterator_t it = NULL;
    esp_err_t res = nvs_entry_find("nvs", NVS_NS, NVS_TYPE_STR, &it);
    while (res == ESP_OK) {
        nvs_entry_info_t info;
        nvs_entry_info(it, &info);
        if (info.key[0] != '_' && h_open) {
            char val[64] = {0};
            size_t vl = sizeof(val);
            nvs_get_str(h, info.key, val, &vl);
            int n = snprintf(buf + pos, buf_len - pos,
                             "%s\"%s\":\"%s\"",
                             first ? "" : ",", info.key, val);
            if (n > 0 && (size_t)n < buf_len - pos) { pos += n; first = false; }
        }
        res = nvs_entry_next(&it);
    }
    nvs_release_iterator(it);
    if (h_open) nvs_close(h);

    if (pos < buf_len - 1) { buf[pos++] = '}'; buf[pos] = '\0'; }
}
