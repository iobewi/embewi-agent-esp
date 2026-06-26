// embewi_parse.c — helpers purs (cf. embewi_parse.h). Aucune dépendance ESP-IDF.
#include "embewi_parse.h"
#include <string.h>
#include <stdio.h>
#include <stdlib.h>

void embewi_url_rebase(const char *ctrl_url, const char *scheme,
                       const char *path, char *out, size_t out_len) {
    if (out_len == 0) return;
    const char *host = ctrl_url;
    if      (strncmp(ctrl_url, "https://", 8) == 0) host = ctrl_url + 8;
    else if (strncmp(ctrl_url, "http://",  7) == 0) host = ctrl_url + 7;
    // snprintf garantit la terminaison '\0' et la troncature bornée.
    snprintf(out, out_len, "%s://%s%s", scheme, host, path);
}

size_t embewi_json_escape(const char *in, char *out, size_t out_len) {
    if (out_len == 0) return 0;
    size_t o = 0;
    // Réserve 1 pour le '\0' ; réserve 2 par caractère échappé pour ne jamais
    // écrire un '\' orphelin si le buffer se remplit pile sur un échappement.
    for (size_t i = 0; in[i] != '\0'; i++) {
        char c = in[i];
        if (c == '\\' || c == '"') {
            if (o + 2 > out_len - 1) break;
            out[o++] = '\\';
            out[o++] = c;
        } else {
            if (o + 1 > out_len - 1) break;
            out[o++] = c;
        }
    }
    out[o] = '\0';
    return o;
}

bool embewi_json_str(const char *body, const char *key,
                     char *out, size_t out_len) {
    char needle[64];
    snprintf(needle, sizeof(needle), "\"%s\"", key);
    const char *p = strstr(body, needle);
    if (!p) return false;
    p += strlen(needle);
    while (*p == ' ' || *p == ':') p++;
    if (*p != '"') return false;
    p++;
    size_t i = 0;
    while (*p && *p != '"' && i < out_len - 1) out[i++] = *p++;
    out[i] = '\0';
    return true;
}

bool embewi_json_u32(const char *body, const char *key, uint32_t *out) {
    char needle[64];
    snprintf(needle, sizeof(needle), "\"%s\"", key);
    const char *p = strstr(body, needle);
    if (!p) return false;
    p += strlen(needle);
    while (*p == ' ' || *p == ':') p++;
    if (*p < '0' || *p > '9') return false;
    *out = (uint32_t)strtoul(p, NULL, 10);
    return true;
}

int embewi_json_data_iter(const char *body, embewi_kv_cb cb, void *ctx) {
    const char *p = strstr(body, "\"data\"");
    if (!p) return -1;
    p += 6;
    while (*p == ' ' || *p == '\t' || *p == ':') p++;
    if (*p != '{') return -1;
    p++;

    int count = 0;
    while (*p) {
        while (*p == ' ' || *p == '\t' || *p == '\n' || *p == '\r' || *p == ',') p++;
        if (*p == '}' || *p == '\0') break;
        if (*p != '"') break;
        p++;
        char k[32] = {0}; size_t ki = 0;
        while (*p && *p != '"') { if (ki < sizeof(k) - 1) k[ki++] = *p; p++; }
        if (*p != '"') break;   // chaîne non terminée → JSON malformé, on stoppe
        p++;
        while (*p == ' ' || *p == '\t' || *p == ':') p++;
        if (*p != '"') break;
        p++;
        char v[64] = {0}; size_t vi = 0;
        while (*p && *p != '"') { if (vi < sizeof(v) - 1) v[vi++] = *p; p++; }
        if (*p != '"') break;
        p++;
        if (k[0]) { cb(k, v, ctx); count++; }
    }
    return count;
}

bool embewi_parse_content_range(const char *hdr, uint32_t *start,
                                uint32_t *end, uint32_t *total) {
    const char *p = hdr;
    while (*p == ' ') p++;
    if (strncmp(p, "bytes", 5) == 0) p += 5;
    while (*p == ' ') p++;

    char *e;
    if (*p < '0' || *p > '9') return false;
    unsigned long s  = strtoul(p, &e, 10);
    if (*e != '-') return false;
    p = e + 1;
    if (*p < '0' || *p > '9') return false;
    unsigned long en = strtoul(p, &e, 10);
    if (*e != '/') return false;
    p = e + 1;
    if (*p < '0' || *p > '9') return false;
    unsigned long t  = strtoul(p, &e, 10);

    // Bornes inclusives : end max = total-1. Incohérence → rejet.
    if (s > en || en >= t) return false;
    *start = (uint32_t)s; *end = (uint32_t)en; *total = (uint32_t)t;
    return true;
}

embewi_ota_action_t embewi_ota_plan(bool has_range, uint32_t start,
                                    bool in_progress, uint32_t written) {
    if (!has_range || start == 0)            return EMBEWI_OTA_BEGIN;
    if (!in_progress || written != start)    return EMBEWI_OTA_RESYNC;
    return EMBEWI_OTA_CONTINUE;
}

bool embewi_ota_is_final(bool has_range, uint32_t end, uint32_t total) {
    return !has_range || (end + 1 == total);
}

bool embewi_ct_equal(const char *a, const char *b) {
    size_t la = strlen(a), lb = strlen(b);
    // diff démarre non nul si les longueurs diffèrent ; volatile pour empêcher
    // le compilateur de réintroduire un court-circuit.
    volatile unsigned char diff = (la != lb);
    size_t n = (la > lb) ? la : lb;
    for (size_t i = 0; i < n; i++) {
        unsigned char ca = (i < la) ? (unsigned char)a[i] : 0;
        unsigned char cb = (i < lb) ? (unsigned char)b[i] : 0;
        diff |= (unsigned char)(ca ^ cb);   // accumule TOUTES les différences
    }
    return diff == 0;
}
