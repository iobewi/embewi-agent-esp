// embewi_parse.h — helpers de parsing/formatage PURS (zéro dépendance ESP-IDF).
//
// Isolés ici exprès : testables sur host (gcc) sans hardware ni ESP-IDF.
// Voir test/host/test_parse.c.
#pragma once
#include <stddef.h>
#include <stdbool.h>
#include <stdint.h>

// Reconstruit une URL en forçant le scheme : extrait l'hôte (+port) de
// ctrl_url (quel que soit son scheme http:// / https:// / nu) et produit
// "<scheme>://<host><path>" dans out. Tronque proprement si out est trop petit.
//
// Contrat §1 : sert à garantir https:// / wss:// sur les flux sortants quel que
// soit le scheme provisionné en NVS.
//   embewi_url_rebase("http://10.0.0.1:8080", "https", "/v1alpha1/heartbeat", …)
//     → "https://10.0.0.1:8080/v1alpha1/heartbeat"
void embewi_url_rebase(const char *ctrl_url, const char *scheme,
                       const char *path, char *out, size_t out_len);

// Échappe \ et " pour insertion dans une chaîne JSON. Borné par out_len
// (toujours terminé par '\0', jamais de demi-échappement en fin de buffer).
// Retourne le nombre d'octets écrits (hors '\0' final).
size_t embewi_json_escape(const char *in, char *out, size_t out_len);

// --- Parseurs JSON minimalistes (MVP, pas un vrai parseur) ------------------
// Cherchent "key" puis lisent la valeur. Hypothèses : clés uniques, pas
// d'échappement dans les valeurs, objet plat. Suffisant pour le contrôle-plane.

// "key":"value" → value dans out (borné). false si clé absente / pas une string.
bool embewi_json_str(const char *body, const char *key, char *out, size_t out_len);

// "key":12345 → *out. false si clé absente ou valeur non numérique.
bool embewi_json_u32(const char *body, const char *key, uint32_t *out);

// Itère les paires "k":"v" de l'objet {"data":{...}} et appelle cb pour chacune.
// Retourne le nombre de paires émises, ou -1 si le champ "data" est absent/malformé.
// S'arrête proprement au premier JSON malformé (chaîne non terminée).
typedef void (*embewi_kv_cb)(const char *key, const char *value, void *ctx);
int embewi_json_data_iter(const char *body, embewi_kv_cb cb, void *ctx);

// Parse un header HTTP "Content-Range: bytes <start>-<end>/<total>" (bornes
// inclusives, octets). Remplit start/end/total. Retourne false si malformé ou
// incohérent (start>end, ou end>=total). Sert à l'OTA write reprenable (§4).
//   "bytes 0-1023/983040" → start=0 end=1023 total=983040
bool embewi_parse_content_range(const char *hdr, uint32_t *start,
                                uint32_t *end, uint32_t *total);

// --- Décision de reprise OTA write (§4) — logique pure, testée sur host ------
typedef enum {
    EMBEWI_OTA_BEGIN,     // (re)démarrer une session puis écrire
    EMBEWI_OTA_CONTINUE,  // session alignée sur l'offset → écrire la plage
    EMBEWI_OTA_RESYNC,    // offset désaligné → répondre 416 + written
} embewi_ota_action_t;

// Action AVANT écriture selon l'état de session et la plage demandée.
//   pas de range OU start==0          → BEGIN
//   start>0, session absente/désalignée → RESYNC
//   start>0, session alignée           → CONTINUE
embewi_ota_action_t embewi_ota_plan(bool has_range, uint32_t start,
                                    bool in_progress, uint32_t written);

// La plage [.. end] termine-t-elle le transfert ? (monolithique = toujours oui)
bool embewi_ota_is_final(bool has_range, uint32_t end, uint32_t total);

// Égalité de chaînes à TEMPS CONSTANT (pas de court-circuit au premier octet
// différent) — défense contre les attaques temporelles sur le token (§1).
// La longueur reste observable par timing (acceptable : ce n'est pas le secret).
bool embewi_ct_equal(const char *a, const char *b);
