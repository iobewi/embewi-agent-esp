# CLAUDE.md — Embewi Agent (firmware ESP-IDF)

Firmware **device** ESP32 implémentant le contrat [`embewi`](https://github.com/iobewi/embewi)
(submodule `contract/`). Le device est piloté par un contrôleur
Kubernetes ([embewi-core](https://github.com/iobewi/embewi-core)) : OTA A/B,
self-check + rollback, heartbeat/logs sortants, config runtime poussée.

Langue du projet : **français** (commentaires, doc, messages). Garder cette langue.

## Contrat de référence (`contract/`)

Spec normative : **`contract/docs/embewi-contract-v2.md`** (« Contrat v2 »,
protocole `v1alpha1`). C'est la **source de vérité** Core ↔ Agent — l'agent
l'épingle par submodule. `git submodule update --init` si `contract/` est vide.

Les commentaires du firmware référencent les `§` de cette spec :
§1 sécurité, §1a enrôlement/identité, §2 états, §3 séquence OTA critique,
§4 endpoints inbound, §4a config en couches, §4b codes d'erreur stables,
§5 flux sortants (heartbeat/logs/WS), §6 idempotence, §7/§7a binding + McuConfigMap,
§8 effets Kubernetes, §9 découpage des responsabilités. Conserver ces renvois lors d'une modif.

Sections **[RÉSERVE]** (hors MVP, ne pas implémenter sans décision) : OTA pull
device-initiated, reprise OTA inter-reboot, double-token overlap, hot-reload
config sans reboot, `DELETE /config/{key}`. Côté agent, tout le MVP du contrat
est implémenté (cf. §10 de la spec).

## Build & test

```bash
# Dev — choisir la cible selon la puce (esp32c3 / esp32c6 / esp32s3 / esp32h2…)
idf.py set-target esp32c3
idf.py build
idf.py -p /dev/ttyUSB0 flash monitor

# Prod (Secure Boot v2 + Flash Enc, opt-in — voir docs/embewi-prod-security.md)
idf.py -B build-prod -DSDKCONFIG=build-prod/sdkconfig.prod \
       -DSDKCONFIG_DEFAULTS="sdkconfig.defaults;sdkconfig.defaults.prod" build

# Tests logique pure (host, sans hardware, ~85 assertions) ← lancer en priorité
cd test/host && make

# Tests ESP-couplés (NVS…) via Unity sur device/QEMU — voir test/target/README.md
idf.py -C test/target set-target esp32c3
```

- IDF >= 5.3 ; dépendance externe `espressif/esp_websocket_client` (`main/idf_component.yml`).
- Devcontainer « ESP-IDF QEMU » (`.devcontainer/`), privileged, extensions Espressif.
- Build sélectionne l'app workload via `EMBEWI_APP` dans `main/CMakeLists.txt`
  (`"button"` par défaut, ou `"rainbow"`).
- Le cert TLS fallback (EC P-256 auto-signé) est **généré au build par openssl**,
  jamais commité. `openssl` doit être dans le PATH.

## Architecture — flux de boot (`embewi_main.c::app_main`)

1. `nvs_flash_init` → `embewi_cfg_boot_init()` (snapshot génération NVS).
2. `fill_running_partition_info()` : slot actif, version, **recalcul SHA-256 du
   binaire courant** depuis la flash (sur `fw_size` exact, sans padding 0xFF).
3. `embewi_wifi_start()` : 1er boot sans NVS → **portail captif AP HTTPS** (bloquant,
   reboot après config) ; boots suivants → **STA** + `ctrl_url`.
4. `embewi_time_sync_start()` (SNTP, tâche de fond).
5. `embewi_app_init()`.
6. **Détection `ESP_OTA_IMG_PENDING_VERIFY`** (cœur du rollback) :
   - PENDING → `embewi_selfcheck_start()` (validation bornée).
   - sinon → image déjà validée → `RUNNING`/`DEGRADED`, `ota_validated=true`.
7. `embewi_http_start()` (HTTPS :443), service app, attente NTP (8 s), heartbeat, logs WS.

## Machine d'état (`embewi_agent.h`, contrat §2/§3)

`booting → pending_verify → running` | `degraded` | `rollback` → `failed`.
- `ota_validated` passe à `true` **uniquement** dans `selfcheck_task` après
  `esp_ota_mark_app_valid_cancel_rollback()`.
- Un device en `pending_verify` **continue d'émettre le heartbeat** (jamais de silence).
- `FAILED` (rollback impossible) : **on ne reboot pas** — rester joignable pour que
  le Core repousse une image saine.

## OTA — séquence reprenable (contrat §3/§4/§6)

`prepare` → `write` (chunké) → `activate` → reboot → self-check borné → `mark_valid`|rollback.

- **SHA-256 incrémental** au fil de l'eau (`embewi_ota.c`, PSA crypto), jamais de
  relecture flash pendant l'écriture.
- **Reprise après coupure** via header `Content-Range` : `embewi_ota_plan()`
  (logique pure) décide BEGIN/CONTINUE/RESYNC. Désaligné → `416` + `written`.
  Handle OTA + état SHA sont statiques → survivent à la déconnexion TCP.
- **Idempotence** : état `staged` persisté en NVS (`none|written|activating`),
  permet au Core de reprendre un reconcile interrompu.
- **Self-check borné** (`embewi_selfcheck.c`) : deadline `esp_timer` 15 s
  (`EMBEWI_PENDING_DEADLINE_MS`). Si pas de `mark_valid` → reset → le bootloader
  (`CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE`) revient seul sur l'image précédente.
  `storage_check()` = canary NVS write→commit→read→erase réel.

## API HTTPS inbound (`embewi_http.c`, préfixe `/v1alpha1`)

`/info` `/health` `/config`(GET+POST) `/reboot` `/ota/prepare` `/ota/write`(PUT)
`/ota/activate` `/app/port` `/tls/cert` `/token`.

- **Auth sur CHAQUE endpoint** : token Bearer par node, comparé en **temps constant**
  (`embewi_ct_equal`). Token absent → tout refusé (401). Sinon `/ota/activate`
  serait un reboot ouvert sur le LAN.
- Parseurs JSON minimalistes (`embewi_parse.c`) : objet plat, clés uniques, pas
  d'échappement dans les valeurs. Suffisant pour le control-plane, **pas un vrai parseur**.

## Flux sortants

- **Heartbeat** : POST HTTPS / 5 s → `ctrl_url/v1alpha1/heartbeat`
  (state, RSSI, heap, uptime, `ota_validated`, `config_generation`, temp °C, stack HWM min).
- **Logs** : `esp_log_set_vprintf` capture **tous** les `ESP_LOGx` → ring buffer 4 KB
  → **WebSocket wss** vers le Core (`embewi_log.c`). Best-effort : drop si plein,
  pas de backlog si WS coupé. `embewi_log_emit()` = events OTA/lifecycle via HTTPS.
- **Forçage de scheme** (`embewi_url_rebase`) : `https://`/`wss://` forcés quel que
  soit le scheme stocké en NVS (contrat §1, testé sur host).

## Config runtime — McuConfigMap (`embewi_config.c`, §4a/§7a)

Découple la config hardware (GPIO, `ntp_server`…) du binaire OTA. NVS `embewi_cfg`,
clés internes préfixées `_` (`_gen`). `POST /config` = merge-on-key + bump génération ;
**effectif au prochain reboot** (`cfg_active_generation` figé au boot).

## Sécurité

- Dev : TLS **chiffré mais non authentifié** (`skip_cert_common_name_check`).
- Prod : `CONFIG_EMBEWI_VERIFY_CORE_CERT=y` → CA du Core embarquée (`main/core_ca.pem`,
  requise au build) vérifie le serveur Core. Secure Boot v2 + Flash Encryption +
  anti-rollback = profil **opt-in** `sdkconfig.defaults.prod` (eFuse **IRRÉVERSIBLE**).
- NTP est un **pré-requis du TLS authentifié** (horloge à 1970 → cert « pas encore valide »).
- Token : généré au provisioning, **affiché une seule fois** (page de confirmation),
  rotation via `POST /token` (commit NVS avant réponse → pas de fenêtre sans auth).

## NVS (namespaces)

| Namespace     | Contenu                                                   |
| ------------- | --------------------------------------------------------- |
| `embewi_prov` | WiFi (ssid/pass), `ctrl_url`, `node_id`, `token`, IP statique |
| `embewi_tls`  | certificat TLS + clé privée                               |
| `embewi`      | `fw_size`, `app_port`, `staged` (idempotence OTA)         |
| `embewi_cfg`  | McuConfigMap (clés runtime + `_gen`)                      |

## Layout fichiers `main/`

| Fichier | Rôle |
| --- | --- |
| `embewi_main.c` | `app_main`, orchestration boot, détection PENDING_VERIFY, digest |
| `embewi_provision.c` | WiFi STA/AP, portail captif HTTPS, node_id, token, IP statique |
| `embewi_selfcheck.c` | validation bornée, rollback, storage check (canary NVS) |
| `embewi_ota.c` | prepare/write/activate, SHA incrémental, staged NVS, fw_size, app_port |
| `embewi_http.c` | serveur HTTPS :443, tous les endpoints inbound, auth |
| `embewi_config.c` | McuConfigMap (config runtime NVS, génération) |
| `embewi_heartbeat.c` | heartbeat + `embewi_log_emit` (HTTPS), métriques (temp, HWM) |
| `embewi_log.c` | streaming ESP_LOGx → ring buffer → WebSocket |
| `embewi_time.c` | synchro NTP/SNTP |
| `embewi_tls.c` | cert/clé depuis NVS, fallback auto-signé embarqué |
| `embewi_parse.{c,h}` | helpers **PURS** (URL, JSON, content-range, OTA plan, constant-time) — testés sur host |
| `embewi_app.h` | interface workload (contrat des 4 fonctions à implémenter) |

Les workloads d'exemple vivent dans `apps/` à la racine du dépôt :

| Chemin | Workload |
| --- | --- |
| `apps/button/main.c` | Compteur bouton BOOT, expose `GET /sensors` |
| `apps/rainbow/main.c` | Arc-en-ciel WS2812B via RMT, expose `GET /status` |

`main/CMakeLists.txt` sélectionne le workload via `EMBEWI_APP` et référence
`../apps/<nom>/main.c` dans `SRCS`.

## Spécificités ESP-IDF / SoC

### Tâches FreeRTOS

| Nom (`pcName`) | Stack | Priorité | Pourquoi cette taille |
|---|---|---|---|
| `embewi_hb` | 8 192 B | 4 | `esp_http_client` consomme ~4 KB à lui seul |
| `embewi_log` | 8 192 B | 3 | `esp_websocket_client` + JSON formatting |
| `embewi_selfchk` | 4 096 B | 5 | Logique pure + NVS, pas de TLS |
| httpd (HTTPS) | ~10 240 B | — | `HTTPD_SSL_CONFIG_DEFAULT` (mbedTLS inclus) |

Règle : toute nouvelle tâche faisant du TLS ou `esp_http_client` → **minimum 8 192 B**.

### Crypto — PSA API (IDF 5.x)

SHA-256 incrémental via `psa_hash_*` (pas mbedTLS direct) : `psa_crypto_init()` requis
une fois, puis `psa_hash_setup / update / finish`. Utilisé dans deux endroits :
- `embewi_ota.c` : digest en écriture OTA (statique, survit aux déconnexions TCP).
- `embewi_main.c` : recalcul digest depuis la flash au boot (sur `fw_size` exact).

### RNG matériel

`esp_fill_random()` = RNG hardware du SoC. Utilisé pour la génération du token
de provisioning (128 bits / 32 hex chars). **Ne pas remplacer par `rand()`.**

### Identité MAC

`esp_read_mac(mac, ESP_MAC_BASE)` fournit les 6 octets de la MAC de base du SoC.
Utilisé à deux endroits :
- SSID AP : `embewi-XXYY` (octets 4-5)
- node_id fallback : `embewi-XXYYZZ` (octets 3-4-5)

### Capteur de température interne

`driver/temperature_sensor.h` — plage configurée `-10 °C / +80 °C`.
**Non disponible sur tous les chips** : `temperature_sensor_install` retourne une
erreur sur certains variants → `s_tsens = NULL` → sentinelle **`-127.0`** dans le
heartbeat. Le Core doit filtrer cette valeur (§5).

### Contraintes GPIO par périphérique

| Périphérique | Driver | Contrainte |
|---|---|---|
| Bouton BOOT | `esp_driver_gpio` | GPIO9 sur C3/C6/H2, GPIO0 sur ESP32/S2/S3. Configurable via McuConfigMap `gpio_button`. |
| LED WS2812B | `esp_driver_rmt` (RMT TX, 10 MHz) | Tous GPIO sur C3/S3 ; GPIO limités sur ESP32 classique (vérifier compat RMT TX). Configurable via `gpio_ws2812`. |

### OTA — détails flash

`esp_ota_begin(s_target, OTA_SIZE_UNKNOWN, &handle)` : pas de pre-erase du slot —
l'écriture est streamed et le slot est effacé au fil de l'eau par `esp_ota_write`.
Conséquence : une session OTA interrompue laisse le slot partiellement écrit mais
le `staged.state` en NVS reste `written` uniquement si `write_finish` a réussi.

Limites binaires par layout :

| Partitions | Slot size | Binaire actuel | Marge |
|---|---|---|---|
| `partitions_4mb.csv` (dev) | 1,28 MB (0x140000) | ~972 KB | ~336 KB |
| `partitions_8mb.csv` (prod) | 2,5 MB (0x280000) | ~972 KB | ~1,56 MB |

### Timer haute résolution

La deadline `pending_verify` (15 s) utilise `esp_timer` (timer haute résolution,
µs), pas un timer FreeRTOS. Raison : `esp_timer` survit aux tâches — même si la
selfcheck_task hang, la deadline tire quand même → `esp_restart()` garanti.

### QEMU (tests target)

Le devcontainer embarque QEMU ESP32. Seuls les targets **esp32** et **esp32c3**
sont supportés par QEMU ESP-IDF en pratique. Les tests `test/target` (NVS Unity)
tournent sur esp32c3 QEMU.

## Conventions

- **Logique testable extraite dans `embewi_parse.{c,h}`** (zéro dépendance ESP-IDF) →
  ajouter les nouveaux helpers purs ici et couvrir dans `test/host/test_parse.c`.
- Buffers JSON formés via `snprintf` borné sur la stack ; tailles calculées au pire cas.
- Toute opération NVS = `nvs_open`/get-set/`nvs_commit`/`nvs_close` ; valeur vide = effacer la clé.
- Les commentaires référencent les `§` du contrat — les conserver lors d'une modif.
- Partitions A/B `embewi-ab-v1` ; offsets **vides** dans les CSV (auto-placés) pour
  s'adapter à `CONFIG_PARTITION_TABLE_OFFSET` (0x8000 dev / 0x10000 prod).

## Alignement spec (v2) ↔ implémentation

Le firmware a été aligné sur la forme cible du contrat v2, en gardant les apports
utiles du firmware (best of both) :

- **`GET /config`** : émet `active` (snapshot figé au boot) **+** `nvs` (NVS courant)
  comme la spec, **et conserve** `generation`/`active_generation`. Le snapshot
  `active` vient de `embewi_cfg_json_active()` (figé dans `embewi_cfg_boot_init`).
- **`POST /tls/cert`** : lit `cert_pem`/`key_pem` (spec) avec **repli** sur les
  anciens noms `cert`/`key` (rétrocompat). Réponse garde `note:"effective_after_reboot"`.
- **`POST /app/port`** : renvoie `{status:"saved", port}` (spec) **+** alias `app_port`.
- **`PUT /ota/write`** : lit le header `X-Embewi-Deployment-Id` et le persiste dans
  `staged.deployment_id` dès l'écriture (`embewi_ota_write_finish` a un param
  `deployment_id`) → `GET /info` expose `staged.deployment_id` avant l'`activate`.

## Pièges connus

- `CONFIG_FREERTOS_USE_TRACE_FACILITY` requis pour le `task_hwm_min` global du
  heartbeat ; sinon fallback sur la seule task heartbeat. Ne s'applique qu'aux
  **nouveaux** sdkconfig (effacer `sdkconfig` pour l'adopter).
- `core_ca.pem` absent en prod → build échoue (volontaire, pas de CA silencieuse).
- Convention de réponse (§4b) : un refus **métier** (prepare rejeté,
  `digest_mismatch`) répond **HTTP 200** avec le code dans le corps ; les `4xx/5xx`
  sont réservés aux erreurs de protocole/ressource. Émettre les codes stables de §4b
  (le Core les mappe en Events K8s — ne pas changer un libellé sans réviser le contrat).
</content>
</invoke>
