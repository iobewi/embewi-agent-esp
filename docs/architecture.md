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

## Tâches FreeRTOS

| Tâche | Stack | Priorité | Rôle |
|---|---|---|---|
| `embewi_hb` | 8 192 B | 4 | Heartbeat HTTPS (esp_http_client exige ~4 KB) |
| `embewi_log` | 8 192 B | 3 | Drain ring buffer → WebSocket |
| `embewi_selfchk` | 4 096 B | 5 | Selfcheck borné post-OTA |
| httpd (HTTPS) | ~10 240 B | — | Serveur HTTPS :443 (mbedTLS inclus) |
