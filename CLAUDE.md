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
| `embewi_app*.{c,h}` | interface workload + apps démo (button, rainbow) |

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
