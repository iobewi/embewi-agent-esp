# Changelog

Format inspiré de [Keep a Changelog](https://keepachangelog.com/fr/).

## [1.0.0] - 2026-07-28

Première version publique. Implémente le contrat `v1alpha1`
([`embewi`](https://github.com/iobewi/embewi)) côté device, vérifié croisé
contre [`embewi-core`](https://github.com/iobewi/embewi-core).

### Ajouté

**Provisioning et identité**
- Portail captif Wi-Fi HTTPS (AP `embewi-XXXX`, fenêtre bornée 10 min), token
  Bearer affiché une seule fois, `node_id` configurable (défaut dérivé de la
  MAC si NVS absent).

**API inbound (HTTPS :443, `/v1alpha1`)**
- `GET /info`, `GET /health`, `GET`/`POST /config`, `POST /reboot`,
  `POST /ota/prepare`, `PUT /ota/write`, `POST /ota/activate`,
  `POST /app/port`, `POST /tls/cert`, `POST /token`.
- Authentification Bearer par node, comparaison à temps constant.
- `api_versions` dans `GET /info` (découverte de version d'API, repli
  implicite `v1alpha1`).

**OTA et self-check**
- Écriture chunkée avec reprise `Content-Range` in-session (digest SHA-256
  incrémental, survit aux déconnexions TCP).
- Validation compat firmware avant transfert : `chip_mismatch`,
  `layout_mismatch`, `idf_incompatible` (comparaison du major ESP-IDF),
  `size_too_large`, `busy`.
- Self-check borné (15 s) post-activation → `mark_valid` ou rollback
  bootloader ; état `FAILED` si le rollback lui-même est impossible.
- Idempotence via `staged` NVS (reprise d'un reconcile Core interrompu sans
  re-transfert du binaire).

**Config runtime (McuConfigMap)**
- Lecture/écriture NVS merge-on-key, séparée du binaire OTA.
- Génération (`generation`/`active_generation`) exposée pour détecter une
  config poussée mais pas encore active après reboot.

**Flux sortants**
- Heartbeat HTTPS toutes les 5 s (état, RSSI, heap, uptime, température
  SoC, stack HWM, `ota_validated`, `config_generation`).
- Streaming `ESP_LOGx` vers WebSocket `wss://.../v1alpha1/logs` ; événements
  OTA/lifecycle critiques doublés en HTTPS (`POST /logs`).
- IP émise à chaque heartbeat (source de vérité pour l'`EndpointSlice` côté
  Core, indépendante de l'IP source TCP).
- Canal de détresse NTP : `reason=clock_unsynced` tant que l'horloge n'est
  pas synchronisée, bascule TLS relâchée (validité temporelle uniquement,
  chaîne/CN toujours vérifiés) pour ne jamais rester silencieux.

**Sécurité (profil production, opt-in)**
- Secure Boot v2, Flash Encryption (mode RELEASE), anti-rollback eFuse,
  chiffrement NVS.
- Vérification du certificat Core sortant (CA embarquée + validité
  temporelle).
- Filtrage IP inbound par CIDR, avant tout handler.

**Workload SDK**
- Interface à 4 fonctions (`embewi_app_init`, `embewi_app_selfcheck`,
  `embewi_app_service_start/stop`) pour le code métier compilé
  statiquement avec l'agent.
- Deux workloads de référence (`apps/button`, `apps/rainbow`) démontrant
  lecture GPIO, driver RMT, config custom, service HTTP applicatif.

**Tests et documentation**
- Suite de logique pure sur host (`test/host`, 123 assertions — parsing,
  décision OTA, comparaison à temps constant).
- Harnais de tests ESP-couplés (`test/target`, Unity + pytest-embedded,
  QEMU-compatible).
- Documentation Sphinx complète (architecture, API, configuration,
  sécurité de production, SDK workload).
