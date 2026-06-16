# Embewi Agent — squelette ESP-IDF

Implémentation du côté **device** du contrat `embewi-contract-v2.md`. Le device
reste bête : il reçoit des octets, les flashe en A/B, s'auto-valide ou rollback,
et émet heartbeat/logs. Toute la logique OCI/ORAS/K8s vit côté Core.

## Build

```bash
idf.py set-target esp32
idf.py build flash monitor
# pour 8 MB : éditer sdkconfig.defaults (partitions_8mb.csv + FLASHSIZE_8MB)
idf.py size            # vérifier que l'app tient dans le slot
```

## Arborescence

```text
embewi-agent/
├── CMakeLists.txt
├── sdkconfig.defaults        # rollback + secure boot (commenté) + WDT
├── partitions_4mb.csv        # A/B sans factory, app ≤ 1.28 MB (tight)
├── partitions_8mb.csv        # A/B confortable (RECOMMANDÉE)
└── main/
    ├── embewi_agent.h        # états, staged, runtime, prototypes
    ├── embewi_main.c         # app_main : détecte PENDING_VERIFY au boot
    ├── embewi_selfcheck.c    # validation bornée → mark_valid / rollback
    ├── embewi_ota.c          # prepare/write(SHA incrémental)/activate + NVS
    ├── embewi_http.c         # /info /health /ota/* + token auth
    └── embewi_heartbeat.c    # heartbeat + logs sortants
```

## Mapping contrat → code

| Contrat v2                              | Où                              |
| --------------------------------------- | ------------------------------- |
| §2 états                                | `embewi_agent.h` enum + `_str`  |
| §3 PENDING_VERIFY + deadline + watchdog | `embewi_main.c` + `_selfcheck.c`|
| §3 mark_valid / rollback                | `embewi_selfcheck.c`            |
| §4 /info avec staged                    | `embewi_http.c` h_info          |
| §4 /ota/write digest incrémental        | `embewi_ota.c` write_chunk/finish|
| §5 heartbeat + ota_validated            | `embewi_heartbeat.c`            |
| §6 staged NVS (reprise)                 | `embewi_ota.c` staged_*         |
| §1 token auth inbound                   | `embewi_http.c` authorized()    |

## La séquence qui peut tuer le projet (à tester en priorité)

```text
flash ota_1 → reboot → boot PENDING_VERIFY
  → embewi_main détecte l'état → embewi_selfcheck_start()
  → deadline esp_timer armée (15 s)
  → run_checks()
      OK  → esp_ota_mark_app_valid_cancel_rollback() → RUNNING, ota_validated=true
      KO  → esp_ota_mark_app_invalid_rollback_and_reboot()
      HANG→ deadline_cb → esp_restart() → bootloader rollback (PENDING non confirmé)
```

Test de non-régression à écrire d'abord : flasher une image dont `run_checks()`
renvoie `false`, vérifier que le device revient SEUL sur l'ancienne image.

## TODO avant que ce soit un vrai agent

```text
[ ] wifi_connect() dans app_main (mode push : device joignable)
[ ] parser JSON des bodies /ota/prepare et /ota/activate (cJSON)
[ ] token par node provisionné en NVS/efuse (pas en dur dans le code)
[ ] passer httpd → esp_https_server + cert serveur
[ ] run_checks() réels : capteurs I2C/SPI, montage littlefs, control loop
[ ] socket réel vers collector dans emit_payload()
[ ] reprise activate après reboot process : recharger s_target depuis staged NVS
[ ] anti-rollback efuse (secure_version) quand le schéma de version est figé
[ ] hardware watchdog (TWDT) explicite autour du self-check en plus du timer
```

## Décision 4 MB vs 8 MB

4 MB **tient** pour l'agent seul (app ~1.28 MB/slot, littlefs 1.44 MB), mais
devient serré dès que Secure Boot v2 + flash encryption + TLS embarqué grossissent
le binaire. Recommandation : **prototyper sur 4 MB, cibler 8 MB pour le terrain.**
C'est un choix de BOM — à trancher avant de souder.
