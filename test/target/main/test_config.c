// test_config.c — tests cible Unity de embewi_config.c (couche McuConfigMap NVS).
//
// Exécuté sur device réel ou QEMU (NVS requis). Voir test/target/README.md.
//
// Pattern : on compile embewi_config.c dans l'app de test et on fournit des
// TEST DOUBLES pour ses dépendances externes — ici, le seul lien vers le reste
// du firmware est embewi_rt(), qu'on remplace par un faux runtime.

#include <string.h>
#include "unity.h"
#include "nvs_flash.h"
#include "embewi_agent.h"

// ── Test double ───────────────────────────────────────────────────────────────
// embewi_config.c appelle embewi_rt() (normalement défini dans embewi_main.c).
// On fournit un singleton local pour isoler la couche config.
static embewi_runtime_t s_fake_rt;
embewi_runtime_t *embewi_rt(void) { return &s_fake_rt; }

// ── Isolation : NVS propre + génération resync avant CHAQUE test ──────────────
#define CFG_NS "embewi_cfg"

static void clear_cfg_ns(void) {
    nvs_handle_t h;
    if (nvs_open(CFG_NS, NVS_READWRITE, &h) == ESP_OK) {
        nvs_erase_all(h);
        nvs_commit(h);
        nvs_close(h);
    }
}

void setUp(void) {
    memset(&s_fake_rt, 0, sizeof(s_fake_rt));
    clear_cfg_ns();
    embewi_cfg_boot_init();   // resync la génération statique avec NVS vidée (=0)
}

void tearDown(void) {
    clear_cfg_ns();
}

// ── Tests ─────────────────────────────────────────────────────────────────────

TEST_CASE("write/read round-trip", "[config]") {
    char out[32];
    TEST_ASSERT_EQUAL(ESP_OK, embewi_cfg_write("k", "hello"));
    TEST_ASSERT_TRUE(embewi_cfg_get("k", out, sizeof(out)));
    TEST_ASSERT_EQUAL_STRING("hello", out);
}

TEST_CASE("get_int parses and falls back to default", "[config]") {
    TEST_ASSERT_EQUAL(42, embewi_cfg_get_int("missing", 42));

    embewi_cfg_write("gpio_button", "9");
    TEST_ASSERT_EQUAL(9, embewi_cfg_get_int("gpio_button", 42));

    // valeur non numérique → on retombe sur le défaut
    embewi_cfg_write("bad", "notanumber");
    TEST_ASSERT_EQUAL(7, embewi_cfg_get_int("bad", 7));
}

TEST_CASE("empty value erases key (retour au défaut build)", "[config]") {
    embewi_cfg_write("gpio_button", "9");
    TEST_ASSERT_EQUAL(9, embewi_cfg_get_int("gpio_button", 0));

    embewi_cfg_write("gpio_button", "");   // chaîne vide = effacement (contrat §4)
    char out[16];
    TEST_ASSERT_FALSE(embewi_cfg_get("gpio_button", out, sizeof(out)));
    TEST_ASSERT_EQUAL(0, embewi_cfg_get_int("gpio_button", 0));
}

TEST_CASE("generation bump persiste et met à jour le runtime", "[config]") {
    uint32_t before = embewi_rt()->cfg_generation;   // 0 après setUp
    TEST_ASSERT_EQUAL(ESP_OK, embewi_cfg_bump_generation());
    TEST_ASSERT_EQUAL(before + 1, embewi_rt()->cfg_generation);

    // persistance : un nouveau boot relit la même génération depuis NVS
    embewi_cfg_boot_init();
    TEST_ASSERT_EQUAL(before + 1, embewi_rt()->cfg_generation);
    TEST_ASSERT_EQUAL(before + 1, embewi_rt()->cfg_active_generation);
}

TEST_CASE("json_nvs sérialise les clés user et exclut les internes", "[config]") {
    embewi_cfg_write("gpio_button", "9");
    embewi_cfg_write("gpio_ws2812", "48");
    embewi_cfg_bump_generation();   // écrit la clé interne "_gen"

    char buf[256];
    embewi_cfg_json_nvs(buf, sizeof(buf));

    TEST_ASSERT_NOT_NULL(strstr(buf, "\"gpio_button\":\"9\""));
    TEST_ASSERT_NOT_NULL(strstr(buf, "\"gpio_ws2812\":\"48\""));
    TEST_ASSERT_NULL(strstr(buf, "_gen"));   // clé interne jamais exposée
    TEST_ASSERT_EQUAL('{', buf[0]);          // objet JSON bien formé
}

// ── Point d'entrée : menu Unity piloté par pytest-embedded ───────────────────
void app_main(void) {
    ESP_ERROR_CHECK(nvs_flash_init());
    unity_run_menu();
}
