# Changer ici pour basculer entre les deux apps de test :
#   "rainbow" → WS2812 arc-en-ciel GPIO10
#   "button"  → compteur bouton BOOT GPIO9
set(EMBEWI_APP "button")

if(EMBEWI_APP STREQUAL "rainbow")
    set(EMBEWI_APP_SRC "embewi_app_rainbow.c")
else()
    set(EMBEWI_APP_SRC "embewi_app_button.c")
endif()

# --- Cert fallback TLS : généré au build, jamais dans les sources ----------
# ESP-IDF parse ce fichier en mode script (-P) pour extraire les REQUIRES.
# add_custom_command n'est pas disponible en mode script → guard obligatoire.
# En mode build réel : cert EC P-256 généré dans le build dir, clé éphémère.
if(NOT CMAKE_SCRIPT_MODE_FILE)
    find_program(OPENSSL_BIN openssl REQUIRED)

    set(FALLBACK_CERT "${CMAKE_CURRENT_BINARY_DIR}/embewi_fallback.crt")
    set(FALLBACK_KEY  "${CMAKE_CURRENT_BINARY_DIR}/embewi_fallback.key")

    add_custom_command(
        OUTPUT  "${FALLBACK_CERT}" "${FALLBACK_KEY}"
        COMMAND "${OPENSSL_BIN}" ecparam -name prime256v1 -genkey -noout
                                 -out "${FALLBACK_KEY}"
        COMMAND "${OPENSSL_BIN}" req -new -x509
                                 -key "${FALLBACK_KEY}"
                                 -out "${FALLBACK_CERT}"
                                 -days 3650
                                 -subj "/CN=embewi-fallback"
        COMMENT "Génération cert fallback TLS auto-signé (EC P-256)"
        VERBATIM
    )

    set(TLS_EMBED_FILES "${FALLBACK_CERT}" "${FALLBACK_KEY}")
else()
    set(TLS_EMBED_FILES "")
endif()

# CA du Core embarquée pour vérifier les flux sortants (prod uniquement).
# En dev (CONFIG_EMBEWI_VERIFY_CORE_CERT non défini) → rien embarqué, le code
# ne référence pas le symbole. En prod → main/core_ca.pem REQUIS (build échoue
# clairement s'il manque, plutôt que d'embarquer une CA silencieusement fausse).
if(CONFIG_EMBEWI_VERIFY_CORE_CERT)
    set(CORE_CA_EMBED "core_ca.pem")
else()
    set(CORE_CA_EMBED "")
endif()

idf_component_register(
    SRCS
        "embewi_main.c"
        "embewi_config.c"
        "embewi_parse.c"
        "embewi_log.c"
        "embewi_time.c"
        "embewi_provision.c"
        "embewi_ota.c"
        "embewi_http.c"
        "embewi_tls.c"
        "embewi_selfcheck.c"
        "embewi_heartbeat.c"
        "${EMBEWI_APP_SRC}"
    INCLUDE_DIRS "."
    EMBED_TXTFILES
        ${TLS_EMBED_FILES}
        ${CORE_CA_EMBED}
    REQUIRES
        app_update          # esp_ota_* , rollback
        spi_flash           # esp_flash_get_size()
        esp_https_server    # HTTPS :443 + httpd_ssl_start
        esp_http_client     # POST heartbeat/logs vers ctrl_url (flux sortants)
        esp_ringbuf         # ring buffer pour streaming logs (embewi_log.c)
        esp-tls             # mbedTLS sous esp_https_server
        esp_wifi
        esp_event           # event loop wifi
        esp_netif           # netif STA + SNTP (esp_netif_sntp, embewi_time.c)
        lwip                # backend SNTP
        nvs_flash           # staged state + TLS NVS
        mbedtls             # sha-256 incrémental (PSA)
        esp_timer           # deadline pending_verify
        esp_system
        esp_driver_rmt      # WS2812 (rainbow)
        esp_driver_gpio     # bouton BOOT (button)
        esp_driver_tsens    # capteur température interne (heartbeat métriques §5)
)
