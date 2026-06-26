// test_parse.c — tests host des helpers purs embewi_parse (cf. embewi_parse.h).
//
// Zéro dépendance : compile et tourne sur n'importe quel host avec gcc.
//   gcc -I../../main test_parse.c ../../main/embewi_parse.c -o test_parse && ./test_parse
// Ou via le Makefile du dossier : make
//
// Couvre exactement les fonctions où des bugs ont été trouvés/corrigés :
// le forçage de scheme (sécurité §1) et l'escape JSON borné (hook logs).

#include <assert.h>
#include <stdio.h>
#include <string.h>
#include "embewi_parse.h"

static int g_pass, g_fail;

#define CHECK_STR(got, want) do {                                      \
    if (strcmp((got), (want)) == 0) { g_pass++; }                      \
    else { g_fail++;                                                   \
        printf("  FAIL %s:%d\n    got : \"%s\"\n    want: \"%s\"\n",    \
               __func__, __LINE__, (got), (want)); }                   \
} while (0)

#define CHECK_EQ(got, want) do {                                       \
    if ((got) == (want)) { g_pass++; }                                 \
    else { g_fail++;                                                   \
        printf("  FAIL %s:%d  got=%zu want=%zu\n",                     \
               __func__, __LINE__, (size_t)(got), (size_t)(want)); }   \
} while (0)

// ── embewi_url_rebase ─────────────────────────────────────────────────────────

static void test_url_forces_scheme(void) {
    char out[128];
    // http:// provisionné → forcé en https:// (le cœur du fix sécurité)
    embewi_url_rebase("http://10.0.0.1:8080", "https", "/v1alpha1/heartbeat",
                      out, sizeof(out));
    CHECK_STR(out, "https://10.0.0.1:8080/v1alpha1/heartbeat");

    // https:// déjà bon → inchangé
    embewi_url_rebase("https://core.local", "https", "/x", out, sizeof(out));
    CHECK_STR(out, "https://core.local/x");

    // hôte nu (sans scheme) → scheme ajouté
    embewi_url_rebase("192.168.1.5:443", "wss", "/v1alpha1/logs",
                      out, sizeof(out));
    CHECK_STR(out, "wss://192.168.1.5:443/v1alpha1/logs");

    // wss depuis http → port et hôte préservés
    embewi_url_rebase("http://host:9000", "wss", "/v1alpha1/logs",
                      out, sizeof(out));
    CHECK_STR(out, "wss://host:9000/v1alpha1/logs");
}

static void test_url_truncates_safely(void) {
    char out[16];
    // buffer trop petit : doit tronquer ET rester terminé par '\0'
    embewi_url_rebase("http://averylonghost.example.com:8443", "https", "/path",
                      out, sizeof(out));
    CHECK_EQ(out[sizeof(out) - 1], '\0');
    CHECK_EQ(strncmp(out, "https://", 8), 0);

    // out_len == 0 : ne doit pas écrire (pas de crash)
    char dummy = 'Z';
    embewi_url_rebase("http://x", "https", "/y", &dummy, 0);
    CHECK_EQ(dummy, 'Z');
}

// ── embewi_json_escape ────────────────────────────────────────────────────────

static void test_json_escape(void) {
    char out[64];
    size_t n;

    n = embewi_json_escape("plain text", out, sizeof(out));
    CHECK_STR(out, "plain text");
    CHECK_EQ(n, 10);

    // guillemet échappé
    embewi_json_escape("say \"hi\"", out, sizeof(out));
    CHECK_STR(out, "say \\\"hi\\\"");

    // backslash échappé (ex. chemin Windows dans un log)
    embewi_json_escape("C:\\tmp", out, sizeof(out));
    CHECK_STR(out, "C:\\\\tmp");

    // vide
    n = embewi_json_escape("", out, sizeof(out));
    CHECK_STR(out, "");
    CHECK_EQ(n, 0);
}

static void test_json_escape_no_orphan_backslash(void) {
    // Buffer pile assez petit pour couper SUR un échappement : on ne doit
    // jamais écrire un '\' orphelin (qui produirait du JSON invalide).
    // Entrée: 3 guillemets → chacun s'escape en 2 octets.
    char out[6];   // place pour 2 paires échappées (4) + '\0', la 3e ne tient pas
    size_t n = embewi_json_escape("\"\"\"", out, sizeof(out));
    CHECK_STR(out, "\\\"\\\"");        // 2 paires complètes, pas de demi-paire
    CHECK_EQ(n, 4);
    CHECK_EQ(out[n], '\0');
    // invariant : le dernier octet écrit n'est jamais un '\' seul
    if (n > 0) CHECK_EQ(out[n - 1] != '\\' || (n >= 2 && out[n - 2] == '\\'), 1);
}

// ── embewi_json_str / embewi_json_u32 ─────────────────────────────────────────

static void test_json_str(void) {
    char out[64];
    const char *body = "{\"deployment_id\":\"wheel-1.1.0\",\"size\":983040}";

    CHECK_EQ(embewi_json_str(body, "deployment_id", out, sizeof(out)), 1);
    CHECK_STR(out, "wheel-1.1.0");

    // clé absente → false
    CHECK_EQ(embewi_json_str(body, "nope", out, sizeof(out)), 0);
    // valeur numérique (pas une string) → false
    CHECK_EQ(embewi_json_str(body, "size", out, sizeof(out)), 0);

    // troncature bornée + terminaison
    char small[5];
    CHECK_EQ(embewi_json_str(body, "deployment_id", small, sizeof(small)), 1);
    CHECK_STR(small, "whee");
    CHECK_EQ(small[4], '\0');

    // tolère espace après les deux-points
    CHECK_EQ(embewi_json_str("{\"k\" : \"v\"}", "k", out, sizeof(out)), 1);
    CHECK_STR(out, "v");
}

static void test_json_u32(void) {
    uint32_t v = 0;
    const char *body = "{\"size\":983040,\"port\":8080}";

    CHECK_EQ(embewi_json_u32(body, "size", &v), 1);  CHECK_EQ(v, 983040u);
    CHECK_EQ(embewi_json_u32(body, "port", &v), 1);  CHECK_EQ(v, 8080u);
    CHECK_EQ(embewi_json_u32(body, "absent", &v), 0);
    // valeur string → pas numérique → false
    CHECK_EQ(embewi_json_u32("{\"x\":\"abc\"}", "x", &v), 0);
}

// ── embewi_json_data_iter (parser POST /config) ──────────────────────────────

typedef struct { char k[16][32]; char v[16][64]; int n; } kv_capture_t;

static void capture_cb(const char *k, const char *v, void *ctx) {
    kv_capture_t *c = ctx;
    if (c->n < 16) {
        snprintf(c->k[c->n], 32, "%s", k);
        snprintf(c->v[c->n], 64, "%s", v);
        c->n++;
    }
}

static void test_json_data_iter(void) {
    kv_capture_t cap = {0};
    const char *body =
        "{\"data\":{\"gpio_button\":\"9\",\"gpio_ws2812\":\"48\"}}";
    CHECK_EQ(embewi_json_data_iter(body, capture_cb, &cap), 2);
    CHECK_EQ(cap.n, 2);
    CHECK_STR(cap.k[0], "gpio_button");  CHECK_STR(cap.v[0], "9");
    CHECK_STR(cap.k[1], "gpio_ws2812");  CHECK_STR(cap.v[1], "48");

    // champ "data" absent → -1
    kv_capture_t c2 = {0};
    CHECK_EQ(embewi_json_data_iter("{\"nope\":{}}", capture_cb, &c2), -1);

    // data vide → 0 paire
    kv_capture_t c3 = {0};
    CHECK_EQ(embewi_json_data_iter("{\"data\":{}}", capture_cb, &c3), 0);

    // espaces, sauts de ligne et virgules tolérés
    kv_capture_t c4 = {0};
    CHECK_EQ(embewi_json_data_iter(
        "{ \"data\" : {\n  \"a\" : \"1\" ,\n  \"b\":\"2\"\n} }",
        capture_cb, &c4), 2);
    CHECK_STR(c4.v[1], "2");
}

static void test_json_data_iter_robustness(void) {
    // Régression du bug corrigé cette session : une clé > 31 chars ne doit PAS
    // avorter le batch — juste tronquer la clé, les paires suivantes passent.
    kv_capture_t cap = {0};
    const char *body =
        "{\"data\":{\"this_key_is_way_too_long_to_fit_inside\":\"x\","
        "\"ok\":\"y\"}}";
    CHECK_EQ(embewi_json_data_iter(body, capture_cb, &cap), 2);
    CHECK_STR(cap.k[1], "ok");
    CHECK_STR(cap.v[1], "y");

    // JSON malformé (valeur non terminée) → arrêt propre, pas de crash.
    kv_capture_t c2 = {0};
    CHECK_EQ(embewi_json_data_iter(
        "{\"data\":{\"a\":\"unterminated", capture_cb, &c2), 0);
}

// ── embewi_parse_content_range (OTA reprenable) ──────────────────────────────

static void test_content_range(void) {
    uint32_t s, e, t;

    // cas nominal
    CHECK_EQ(embewi_parse_content_range("bytes 0-1023/983040", &s, &e, &t), 1);
    CHECK_EQ(s, 0u); CHECK_EQ(e, 1023u); CHECK_EQ(t, 983040u);

    // plage de reprise au milieu
    CHECK_EQ(embewi_parse_content_range("bytes 884736-983039/983040", &s, &e, &t), 1);
    CHECK_EQ(s, 884736u); CHECK_EQ(e, 983039u); CHECK_EQ(t, 983040u);

    // dernier octet : end == total-1
    CHECK_EQ(embewi_parse_content_range("bytes 0-0/1", &s, &e, &t), 1);
    CHECK_EQ(e, 0u); CHECK_EQ(t, 1u);

    // tolère espaces multiples / sans "bytes"
    CHECK_EQ(embewi_parse_content_range("0-9/100", &s, &e, &t), 1);
    CHECK_EQ(s, 0u); CHECK_EQ(e, 9u);
}

static void test_content_range_rejects(void) {
    uint32_t s, e, t;
    // end >= total (borne inclusive violée)
    CHECK_EQ(embewi_parse_content_range("bytes 0-1024/1024", &s, &e, &t), 0);
    // start > end
    CHECK_EQ(embewi_parse_content_range("bytes 500-100/1000", &s, &e, &t), 0);
    // total absent / non numérique
    CHECK_EQ(embewi_parse_content_range("bytes 0-10/*", &s, &e, &t), 0);
    CHECK_EQ(embewi_parse_content_range("bytes 0-10", &s, &e, &t), 0);
    // séparateurs manquants
    CHECK_EQ(embewi_parse_content_range("bytes 010/100", &s, &e, &t), 0);
    CHECK_EQ(embewi_parse_content_range("garbage", &s, &e, &t), 0);
}

// ── embewi_ota_plan / embewi_ota_is_final (décision de reprise OTA) ───────────

static void test_ota_plan(void) {
    // pas de Content-Range → écriture monolithique → BEGIN
    CHECK_EQ(embewi_ota_plan(false, 0, false, 0), EMBEWI_OTA_BEGIN);
    // range start==0 → nouvelle session, même si une traîne déjà → BEGIN
    CHECK_EQ(embewi_ota_plan(true, 0, true, 5000), EMBEWI_OTA_BEGIN);
    // reprise alignée : session ouverte, written == start → CONTINUE
    CHECK_EQ(embewi_ota_plan(true, 65536, true, 65536), EMBEWI_OTA_CONTINUE);
    // reprise mais aucune session ouverte → RESYNC
    CHECK_EQ(embewi_ota_plan(true, 65536, false, 0), EMBEWI_OTA_RESYNC);
    // reprise désalignée : device plus loin que demandé (plage rejouée) → RESYNC
    CHECK_EQ(embewi_ota_plan(true, 65536, true, 98304), EMBEWI_OTA_RESYNC);
    // reprise désalignée : device en retard → RESYNC
    CHECK_EQ(embewi_ota_plan(true, 65536, true, 32768), EMBEWI_OTA_RESYNC);
}

static void test_ota_is_final(void) {
    // monolithique (pas de range) → toujours final
    CHECK_EQ(embewi_ota_is_final(false, 0, 0), 1);
    // dernière plage : end == total-1
    CHECK_EQ(embewi_ota_is_final(true, 983039, 983040), 1);
    // plage intermédiaire → pas final
    CHECK_EQ(embewi_ota_is_final(true, 65535, 983040), 0);
    // image d'un seul octet [0-0]/1
    CHECK_EQ(embewi_ota_is_final(true, 0, 1), 1);
}

// ── embewi_ct_equal (comparaison de token à temps constant) ──────────────────

static void test_ct_equal(void) {
    // égalité stricte
    CHECK_EQ(embewi_ct_equal("a1b2c3d4e5f6", "a1b2c3d4e5f6"), 1);
    CHECK_EQ(embewi_ct_equal("", ""), 1);

    // même longueur, un octet différent (début / milieu / fin)
    CHECK_EQ(embewi_ct_equal("Xbcdef", "abcdef"), 0);
    CHECK_EQ(embewi_ct_equal("abcXef", "abcdef"), 0);
    CHECK_EQ(embewi_ct_equal("abcdeX", "abcdef"), 0);

    // longueurs différentes (préfixe d'un secret plus long ne doit pas matcher)
    CHECK_EQ(embewi_ct_equal("abc", "abcdef"), 0);
    CHECK_EQ(embewi_ct_equal("abcdef", "abc"), 0);
    CHECK_EQ(embewi_ct_equal("x", ""), 0);
    CHECK_EQ(embewi_ct_equal("", "x"), 0);
}

int main(void) {
    printf("embewi_parse host tests\n");
    test_url_forces_scheme();
    test_url_truncates_safely();
    test_json_escape();
    test_json_escape_no_orphan_backslash();
    test_json_str();
    test_json_u32();
    test_json_data_iter();
    test_json_data_iter_robustness();
    test_content_range();
    test_content_range_rejects();
    test_ota_plan();
    test_ota_is_final();
    test_ct_equal();

    printf("\n%d passed, %d failed\n", g_pass, g_fail);
    return g_fail == 0 ? 0 : 1;
}
