# Architecture

## Séquence de boot

```{mermaid}
flowchart TD
    A([nvs_flash_init]) --> B["embewi_cfg_boot_init\nSnapshot configuration figé au boot"]
    B --> C["fill_running_partition_info\nSlot actif · SHA-256 du binaire"]
    C --> D{NVS vide ?}
    D -->|"Premier démarrage"| E(["AP embewi-XXXX · Portail HTTPS\nbloquant → reboot après configuration"])
    D -->|"Démarrages suivants"| F["STA · ctrl_url chargé\nembewi_time_sync_start — SNTP"]
    F --> G[embewi_app_init]
    G --> H{PENDING_VERIFY ?}
    H -->|"oui — boot post-activate"| I["selfcheck_task lancée\nen tâche de fond · priorité 5"]
    H -->|"non — image déjà validée"| J["state = RUNNING\nota_validated = true"]
    I --> K["embewi_http_start — HTTPS :443\n(lancé même en PENDING_VERIFY)"]
    J --> K
    K --> L["Attente NTP max 8 s\nHeartbeat · Logs WSS"]
    L --> M([Boucle principale])

    I -.->|"deadline esp_timer 15 s"| SC{"Selfcheck OK ?"}
    SC -->|"OK"| N["mark_valid\nstate = RUNNING · ota_validated = true"]
    SC -->|"KO / timeout"| O(["reset → rollback bootloader"])
```

## Machine d'état (contrat §2)

```{mermaid}
stateDiagram-v2
    [*] --> booting
    booting --> running       : image validée / ota_validated=true
    booting --> pending_verify: PENDING_VERIFY — boot post-activate

    pending_verify --> running : selfcheck OK · mark_valid
    pending_verify --> rollback: KO ou deadline 15 s

    running --> degraded : check KO
    rollback --> failed  : rollback impossible

    note right of pending_verify
        Heartbeat émis en continu —
        le silence est indistinguable
        d'un crash pour le Core
    end note

    note right of failed
        Ne reboot pas :
        reste joignable pour que
        le Core repousse une image saine
    end note
```

**Invariants clés :**
- `ota_validated` passe à `true` **uniquement** après
  `esp_ota_mark_app_valid_cancel_rollback()` — un seul endroit dans le code
  (`selfcheck_task`).
- `FAILED` : le device ne reboot pas — il reste joignable pour que le Core
  puisse repousser une image saine.

## Séquence OTA (contrat §3)

```{mermaid}
sequenceDiagram
    participant Core
    participant Agent
    participant Bootloader

    Core->>Agent: POST /ota/prepare {chip, size, digest}
    Agent-->>Core: {accepted: true, target_slot: "ota_1"}

    loop Transfert par chunks (Content-Range)
        Core->>Agent: PUT /ota/write
        Agent-->>Core: {status: "partial", written: N}
    end
    Core->>Agent: PUT /ota/write — dernier chunk
    Agent-->>Core: {status: "written", digest: "sha256:..."}

    Core->>Agent: POST /ota/activate
    Agent->>Bootloader: set_boot_partition
    Agent-->>Core: {status: "rebooting"}
    Agent->>Agent: esp_restart()

    Note over Agent,Bootloader: Selfcheck borné 15 s (esp_timer haute résolution)

    alt Selfcheck OK
        Agent->>Agent: mark_valid · staged = none (NVS)
        Agent-->>Core: heartbeat state=running
    else KO ou timeout
        Bootloader->>Agent: rollback vers image précédente
        Agent-->>Core: heartbeat state=rollback
    end
```

**Reprise après coupure (`Content-Range`)** — protocole in-session :

| Situation | Action agent |
|---|---|
| Header absent ou `start=0` | Nouvelle session (BEGIN) |
| `start` aligné sur `written` | Continue (CONTINUE) |
| `start` désaligné | `416` + `{"written": N}` pour resync |
| `end+1 == total` | Finalise + vérifie SHA |

**Idempotence** — le Core reprend un reconcile interrompu via `GET /info` :

| `staged.state` | Comportement Core |
|---|---|
| `none` | Repartir de `/ota/prepare` |
| `written` + bon digest | Sauter write, aller à `/ota/activate` |
| `written` + digest périmé | Re-préparer + ré-écrire |
| `activating` | Attendre le heartbeat (timeout négatif §3) |

## Flux sortants

```{mermaid}
flowchart LR
    Logs["ESP_LOGx\ntoutes tâches"] --> Hook["log_hook\nesp_log_set_vprintf"]
    Hook --> UART[UART]
    Hook --> Ring["Ring buffer 4 KB\nnon-bloquant · drop si plein"]
    Ring --> Drain[drain_task]
    Drain -->|best-effort| WSS["WSS /v1alpha1/logs"]

    Emit["embewi_log_emit()"] -->|"événements OTA / lifecycle"| HTTPS["HTTPS POST\n/v1alpha1/logs"]
    HB["heartbeat_task\ntoutes les 5 s"] --> HBPost["HTTPS POST\n/v1alpha1/heartbeat"]
```

Le scheme est toujours forcé en `https://` / `wss://` quel que soit le scheme
stocké dans `ctrl_url` (contrat §1).

**Champs du heartbeat (§5) :**

| Champ | Description |
|---|---|
| `node_id` | Identifiant du device (NVS provisioning) |
| `ip` | IP Wi-Fi courante — le Core met à jour l'EndpointSlice K8s à chaque réception (§8) |
| `ts` | Epoch UTC (0 si NTP pas encore synchronisé) |
| `state` | État courant (`running`, `pending_verify`, `degraded`…) |
| `ota_validated` | `true` uniquement après `mark_valid` — pilote le `ready` de l'EndpointSlice |
| `deployment_id` | Identifiant du déploiement validé courant |
| `firmware_digest` | Digest SHA-256 du binaire actif |
| `uptime_ms` | Uptime en millisecondes (`esp_timer`) |
| `heap_free` | Heap interne libre (octets) |
| `rssi` | Niveau Wi-Fi (dBm) |
| `config_generation` | Génération McuConfigMap active au boot |
| `temp_celsius` | Température interne du SoC (`-127.0` si capteur indisponible) |
| `task_hwm_min` | Minimum high-water-mark de stack sur toutes les tâches (octets libres restants) |

Le champ `ip` est la source de vérité que le Core utilise pour patcher
`endpoints[].addresses` de l'EndpointSlice — l'IP peut changer entre deux
reboots (DHCP) sans action manuelle.

### Streaming WebSocket — fonctionnement détaillé

**Connexion outbound device-initiated.** L'agent ouvre la connexion WS en
client vers `wss://<ctrl_url_host>:<port>/v1alpha1/logs` — c'est le device qui
initie, pas le Core. Compatible NAT et pare-feu entrant restrictif. Le scheme
est toujours forcé en `wss://` (contrat §1), quel que soit le scheme de
`ctrl_url`.

**Séquence d'initialisation (`embewi_log_start`) :**

```{mermaid}
sequenceDiagram
    participant Main as app_main
    participant Log as embewi_log_start
    participant WS as esp_websocket_client
    participant Hook as log_hook (vprintf)

    Main->>Log: embewi_log_start()
    Log->>Log: xRingbufferCreate(4 KB)
    Log->>WS: esp_websocket_client_init(wss://...)
    Log->>WS: esp_websocket_client_start()
    Note over WS: connexion TLS + handshake WS<br/>(asynchrone, en arrière-plan)
    Log->>Log: xTaskCreate(drain_task) → s_drain rempli
    Log->>Hook: esp_log_set_vprintf(log_hook)
    Note over Hook: hook actif — UART conservé
```

**Ordre critique** : la `drain_task` est créée et son handle (`s_drain`) est
affecté **avant** l'installation du hook. Cela garantit que le hook peut
comparer `xTaskGetCurrentTaskHandle() != s_drain` dès le premier appel.

**Pipeline complet :**

| Étape | Composant | Comportement |
|---|---|---|
| 1 | `ESP_LOGx` (toute tâche) | Appel vprintf normal |
| 2 | `log_hook` | Écrit sur l'UART original **et** pousse dans le ring buffer — non-bloquant (`pdMS_TO_TICKS(0)`), drop silencieux si plein |
| 3 | Ring buffer 4 KB | ~25 lignes de 160 chars. La `drain_task` est ignorée par le hook (guard via handle) pour éviter la récursion infinie |
| 4 | `drain_task` | Tire les lignes, les encode en JSON et les envoie via `esp_websocket_client_send_text` si le WS est connecté |
| 5 | WebSocket | Reconnexion automatique gérée par `esp_websocket_client` (bibliothèque Espressif) |

**Comportement en cas de déconnexion WS :** la `drain_task` continue de
consommer le ring buffer et **jette** les messages. Les logs ne s'accumulent
pas — à la reconnexion, seuls les logs récents (ceux dans le ring buffer à
l'instant de la reconnexion) sont diffusés. Choix assumé : fraîcheur plutôt que
complétude. Les événements OTA et lifecycle critiques passent par
`embewi_log_emit()` (HTTPS POST, garanti délivré ou erreur loggée).

**Récursion :** la `drain_task` génère elle-même des `ESP_LOGx` (connexion,
erreur…). Le hook détecte son propre handle et saute l'écriture dans le ring
buffer pour cette tâche uniquement — l'UART reçoit quand même ces logs.

**Garanties de livraison :**

| Canal | Garantie | Usage |
|---|---|---|
| WebSocket `wss` | Best-effort, drop si buffer plein ou WS déconnecté | Tous les `ESP_LOGx` en streaming |
| HTTPS POST `/v1alpha1/logs` via `embewi_log_emit()` | Tentative unique, erreur loggée | Événements OTA et lifecycle |

**TLS :**
- Dev (`CONFIG_EMBEWI_VERIFY_CORE_CERT=n`) : connexion chiffrée, cert serveur non vérifié.
- Prod (`CONFIG_EMBEWI_VERIFY_CORE_CERT=y`) : cert du Core vérifié contre la CA embarquée (`main/core_ca.pem`).

## Tâches FreeRTOS

| Tâche | Stack | Priorité | Rôle |
|---|---|---|---|
| `embewi_hb` | 8 192 B | 4 | Heartbeat HTTPS (esp_http_client exige ~4 KB) |
| `embewi_log` | 8 192 B | 3 | Drain ring buffer → WebSocket |
| `embewi_selfchk` | 4 096 B | 5 | Selfcheck borné post-OTA |
| httpd (HTTPS) | ~10 240 B | — | Serveur HTTPS :443 (mbedTLS inclus) |
