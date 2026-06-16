# Embewi Agent — ESP-IDF firmware

Implémentation **device** du contrat `embewi-contract-v2.md`.

L'agent tourne sur la famille **ESP32** (testé sur C3, compatible C6, S3, H6…).
Il expose une API HTTPS, s'auto-valide après OTA, émet heartbeat et logs vers le
[embewi-core](https://github.com/iobewi/embewi-core) (contrôleur Kubernetes).

## Fonctionnalités

- **Provisioning** : portail captif Wi-Fi (AP `embewi-XXXX`), configuration IP statique ou DHCP, token affiché une seule fois à la validation
- **API HTTPS:443** : `/info`, `/health`, `/ota/prepare`, `/ota/write`, `/ota/activate`, `/app/port`, `/tls/cert`
- **OTA A/B** : écriture par chunks avec digest SHA-256 incrémental, reprise après coupure (staged NVS)
- **Self-check** : validation bornée (15 s) → `mark_valid` ou rollback automatique ; état `FAILED` si rollback impossible
- **Heartbeat** : POST toutes les 5 s vers le core (état, RSSI, heap, uptime, `ota_validated`)
- **Logs sortants** : POST `/v1alpha1/logs` vers le core à chaque événement
- **Port applicatif** : second port configurable (défaut 8080) pour l'app embarquée
- **TLS** : certificat et clé privée provisionnés en NVS (`embewi_tls`)
- **Authentification** : Bearer token par nœud, stocké en NVS (`embewi_prov`)

## Build

```bash
# Adapter le target au chip utilisé : esp32c3, esp32c6, esp32s3…
idf.py set-target esp32c3
idf.py build
idf.py -p /dev/ttyUSB0 flash monitor
```

Flash complète (NVS effacée, reprovisionnement) :

```bash
idf.py -p /dev/ttyUSB0 erase-flash flash monitor
```

Vérifier la taille du binaire (doit tenir dans le slot OTA) :

```bash
idf.py size
```

## Partitions

| Fichier                | Flash   | Slot app | Usage recommandé         |
| ---------------------- | ------- | -------- | ------------------------ |
| `partitions_4mb.csv`   | 4 MB    | ~1.28 MB | Développement, tight     |
| `partitions_8mb.csv`   | 8 MB    | ~3 MB    | Terrain, recommandé      |

Sélection dans `sdkconfig.defaults` ou via `idf.py menuconfig`.

## Arborescence

```text
embewi/
├── CMakeLists.txt
├── sdkconfig.defaults          # rollback, WDT, Secure Boot commenté
├── partitions_4mb.csv
├── partitions_8mb.csv
├── docs/
│   ├── embewi-api.md           # documentation complète de l'API
│   └── embewi-contract-v2.md  # contrat device ↔ core
└── main/
    ├── embewi_agent.h          # états, runtime, prototypes
    ├── embewi_main.c           # app_main : boot, provisioning, démarrage tasks
    ├── embewi_provision.c      # portail captif, NVS WiFi + IP + token
    ├── embewi_selfcheck.c      # validation OTA, rollback, état FAILED
    ├── embewi_ota.c            # prepare / write (SHA incrémental) / activate
    ├── embewi_http.c           # serveur HTTPS, tous les endpoints inbound
    ├── embewi_heartbeat.c      # heartbeat + logs sortants vers le core
    ├── embewi_tls.c            # chargement cert/clé depuis NVS
    ├── embewi_app.h            # interface app embarquée
    ├── embewi_app_button.c     # exemple app : bouton
    └── embewi_app_rainbow.c    # exemple app : LED rainbow
```

## NVS

| Namespace      | Clés                                      | Contenu                        |
| -------------- | ----------------------------------------- | ------------------------------ |
| `embewi_prov`  | `ssid`, `pass`, `ctrl_url`, `token`       | WiFi + URL core + token auth   |
| `embewi_prov`  | `ip`, `mask`, `gw`, `dns`                 | IP statique (vide = DHCP)      |
| `embewi_tls`   | `cert`, `key`                             | Certificat TLS + clé privée    |
| `embewi`       | `fw_size`, `app_port`                     | Taille OTA en cours, port app  |

## Mapping contrat v2 → code

| Section contrat                         | Fichier                              |
| --------------------------------------- | ------------------------------------ |
| §2 états                                | `embewi_agent.h` enum                |
| §3 PENDING_VERIFY + deadline + watchdog | `embewi_main.c` + `embewi_selfcheck.c` |
| §3 mark_valid / rollback / FAILED       | `embewi_selfcheck.c`                 |
| §4 /info (flash_size, ram_size…)        | `embewi_http.c` `h_info()`           |
| §4 /ota/write digest incrémental        | `embewi_ota.c` `write_chunk()`       |
| §5 heartbeat + ota_validated            | `embewi_heartbeat.c`                 |
| §6 staged NVS (reprise OTA)             | `embewi_ota.c` `staged_*()`          |
| §1 token auth                           | `embewi_http.c` `authorized()`       |

## Secure Boot V2

Commenté dans `sdkconfig.defaults` — nécessite un burn eFuse **irréversible**.
À activer uniquement en production sur matériel validé.

## Licence

MIT — voir [LICENSE](LICENSE).
