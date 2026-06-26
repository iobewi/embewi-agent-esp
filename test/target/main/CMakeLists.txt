# Compile les fichiers du firmware SOUS TEST directement dans l'app de test,
# avec des test doubles pour leurs dépendances (cf. test_config.c).
# WHOLE_ARCHIVE : sinon le linker élague les TEST_CASE (auto-enregistrés).
set(EMBEWI_MAIN "${CMAKE_CURRENT_LIST_DIR}/../../../main")

idf_component_register(
    SRCS
        "test_config.c"
        "${EMBEWI_MAIN}/embewi_config.c"   # code sous test (NVS uniquement)
    INCLUDE_DIRS
        "${EMBEWI_MAIN}"
    REQUIRES unity nvs_flash
    WHOLE_ARCHIVE
)
