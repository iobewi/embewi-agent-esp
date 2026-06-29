# Embewi Agent — ESP-IDF firmware

Implémentation **device** du contrat
[`embewi`](https://github.com/iobewi/embewi) (`v1alpha1`),
rattaché ici en submodule sous `contract/`.

L'agent tourne sur la famille **ESP32** (testé sur C3, compatible C6, S3, H2…).
Il expose une API HTTPS, s'auto-valide après OTA, et émet heartbeat + logs (TLS)
vers le [embewi-core](https://github.com/iobewi/embewi-core) (contrôleur Kubernetes).

📖 **Documentation de l'agent** : **<https://iobewi.github.io/embewi-agent-esp/>**.
Contrat & doc système (hub) : **<https://iobewi.github.io/embewi/>**.

> Cloner avec le contrat : `git clone --recursive …` (ou `git submodule update --init`).

## Fonctionnalités

- **Provisioning** : portail captif Wi-Fi **HTTPS** (AP `embewi-XXXX`, fenêtre bornée 10 min), IP statique ou DHCP, token affiché une seule fois
- **API HTTPS:443** : `/info`, `/health`, `/config` (GET+POST), `/reboot`, `/ota/prepare`, `/ota/write`, `/ota/activate`, `/app/port`, `/tls/cert`, `/token`
- **OTA A/B** : écriture chunkée, digest SHA-256 incrémental, **reprise après coupure** (`Content-Range` in-session), idempotence `staged` NVS
- **Self-check** : validation bornée (15 s) → `mark_valid` ou rollback ; check storage réel (canary NVS) ; état `FAILED` si rollback impossible
- **McuConfigMap** : config runtime poussée par le Core (NVS), découplée du binaire OTA — GPIO, `ntp_server`, etc. (`embewi_config.c`)
- **Heartbeat** : POST HTTPS / 5 s (état, RSSI, heap, uptime, `ota_validated`, `config_generation`, **temp °C**, **stack HWM min**)
- **Streaming logs** : tous les `ESP_LOGx` capturés (`esp_log_set_vprintf`) → **WebSocket `wss`** vers le Core ; `embewi_log_emit()` HTTPS pour les événements OTA/lifecycle
- **Horloge** : synchro **NTP/SNTP** au boot → `ts` en epoch UTC ; pré-requis du TLS authentifié (dates de cert)
- **Sécurité** : token Bearer par nœud (**comparaison à temps constant**), TLS sortant authentifié en prod (CA du Core embarquée), profil **Secure Boot v2 + Flash Encryption** opt-in (`sdkconfig.defaults.prod`)

## Build

```bash
# Dev (target adapté au chip : esp32c3, esp32c6, esp32s3…)
idf.py set-target esp32c3
idf.py build
idf.py -p /dev/ttyUSB0 flash monitor

# Prod (Secure Boot + Flash Enc — voir docs/embewi-prod-security.md)
idf.py -B build-prod \
       -DSDKCONFIG=build-prod/sdkconfig.prod \
       -DSDKCONFIG_DEFAULTS="sdkconfig.defaults;sdkconfig.defaults.prod" build
```

## Tests

```bash
# Logique pure (parsing, URL, décision OTA, constant-time) — host, sans hardware
cd test/host && make            # 85 assertions

# Code ESP-couplé (config NVS…) — Unity sur device ou QEMU
idf.py -C test/target set-target esp32c3   # voir test/target/README.md
```

## Partitions

| Fichier              | Flash | Slot app | Usage             |
| -------------------- | ----- | -------- | ----------------- |
| `partitions_4mb.csv` | 4 MB  | 1.28 MB  | Développement     |
| `partitions_8mb.csv` | 8 MB  | ~3 MB    | Terrain           |

Offsets **vides** (auto-placés après la table) → le layout s'adapte à
`CONFIG_PARTITION_TABLE_OFFSET` (0x8000 dev / 0x10000 prod, bootloader signé plus
gros). Partition `nvs_keys` pour le chiffrement NVS en prod.

## NVS

| Namespace     | Contenu                                              |
| ------------- | ---------------------------------------------------- |
| `embewi_prov` | WiFi, `ctrl_url`, `node_id`, `token`, IP statique    |
| `embewi_tls`  | certificat TLS + clé privée                          |
| `embewi`      | `fw_size`, `app_port`, `staged` (idempotence OTA)    |
| `embewi_cfg`  | McuConfigMap (clés runtime + `_gen` génération)      |

## Sécurité production

Secure Boot v2 + Flash Encryption + anti-rollback : profil **opt-in**
(`sdkconfig.defaults.prod`), opérations eFuse **irréversibles**. Le profil prod
**compile et signe** (validé), reste la validation eFuse sur device.
Procédure complète, gestion des clés : **`docs/embewi-prod-security.md`**.

## Licence

MIT — voir [LICENSE](LICENSE).
