# Architecture

## Séquence de boot

```text
nvs_flash_init
      │
embewi_cfg_boot_init()   ← snapshot génération config (figé jusqu'au reboot)
      │
fill_running_partition_info()
      │  ├─ slot actif (ota_0 / ota_1), version firmware
      │  └─ SHA-256 du binaire courant depuis la flash (sur fw_size exact)
      │
embewi_wifi_start()
      │  ├─ NVS vide (premier boot) → AP "embewi-XXXX" + portail HTTPS
      │  │    (bloquant, reboot après config)
      │  └─ NVS rempli → STA + ctrl_url chargé
      │
embewi_time_sync_start()   ← SNTP en tâche de fond (pré-requis TLS prod)
      │
embewi_app_init()          ← workload lit sa config NVS ici
      │
      ├─ image PENDING_VERIFY ? ──────────────────────────────────┐
      │   (boot juste après /ota/activate)                        │
      │                                                           ▼
      │                                              embewi_selfcheck_start()
      │                                                    (tâche priorité 5)
      └─ image déjà validée ──► état RUNNING / DEGRADED          │
           ota_validated = true                                   │
                                                     ┌───────────┴──────────┐
                                                     │  self-check borné    │
                                                     │  deadline 15 s       │
                                                     │  (esp_timer haute    │
                                                     │  résolution)         │
                                                     └─────────┬────────────┘
                                                       OK      │    KO / hang
                                              ┌────────────────┴──┐
                                              ▼                   ▼
                                         mark_valid         mark_invalid
                                         RUNNING            → reboot
                                         ota_validated=true → bootloader
                                                              rollback
      │
embewi_http_start()    ← HTTPS :443 (lancé même en PENDING_VERIFY, §2)
embewi_app_service_start()
      │
attente NTP max 8 s    ← avant tout TLS sortant
      │
embewi_heartbeat_start()   ← POST HTTPS /v1alpha1/heartbeat toutes les 5 s
embewi_log_start()         ← ring buffer 4 KB → WSS /v1alpha1/logs
embewi_log_emit("info", "embewi agent up")
      │
      └─ boucle principale vTaskDelay(1 s)
```

## Machine d'état (contrat §2)

```text
                    ┌──────────┐
                    │ booting  │
                    └────┬─────┘
          image déjà     │      image PENDING_VERIFY
          validée        │      (boot post-activate)
              ┌──────────┴──────────┐
              ▼                     ▼
         ┌─────────┐        ┌───────────────┐
         │ running │        │pending_verify │
         └────┬────┘        └───────┬───────┘
              │               OK    │    KO / deadline
              │         ┌───────────┴──┐
              │         ▼              ▼
              │    ┌─────────┐   ┌──────────┐
              │    │ running │   │ rollback │
              │    └─────────┘   └────┬─────┘
              │                       │ rollback impossible
         check KO                     ▼
              │                  ┌────────┐
              ▼                  │ failed │  ← ne reboot pas,
         ┌──────────┐            └────────┘    reste joignable
         │ degraded │
         └──────────┘
```

**Invariants clés :**
- Un device en `pending_verify` **continue d'émettre le heartbeat** (le silence
  est indistinguable d'un crash pour le Core).
- `ota_validated` passe à `true` **uniquement** après
  `esp_ota_mark_app_valid_cancel_rollback()` — un seul endroit dans le code
  (`selfcheck_task`).
- `FAILED` : le device ne reboot pas — il reste joignable pour que le Core
  puisse repousser une image saine.

## Séquence OTA (contrat §3)

```text
Core                                    Agent
────                                    ─────
POST /ota/prepare ──────────────────►  valide chip/layout/size
                  ◄── {accepted, slot} réserve slot inactif

PUT /ota/write ─────────────────────►  écriture chunkée + SHA-256 incrémental
(répétable sur coupure réseau,          (handle OTA + état SHA statiques,
 Content-Range in-session)              survivent à la déconnexion TCP)
                  ◄── {written, digest} vérifie SHA en fin de transfert

POST /ota/activate ─────────────────►  set_boot_partition
                  ◄── {rebooting}       staged = activating (NVS)
                                        reboot

                         [boot sur le nouveau slot]
                                        self-check borné 15 s
                         ┌──────────────────────────────┐
                         │ OK → mark_valid              │
                         │      staged = none (NVS)     │
                         │      heartbeat: running      │
                         │                              │
                         │ KO / hang → reset            │
                         │      bootloader rollback     │
                         │      heartbeat: rollback     │
                         └──────────────────────────────┘
```

**Reprise après coupure (`Content-Range`)** — protocole in-session :

| Situation | Action agent |
|---|---|
| Header absent ou `start=0` | Nouvelle session (BEGIN) |
| `start` aligné sur `written` | Continue (CONTINUE) |
| `start` désaligné | `416` + `{"written": N}` pour resync |
| `end+1 == total` | Finalise + vérifie SHA |

**Idempotence** — le Core peut reprendre un reconcile interrompu via `GET /info` :

| `staged.state` | Comportement Core |
|---|---|
| `none` | Repartir de `/ota/prepare` |
| `written` + bon digest | Sauter write, aller à `/ota/activate` |
| `written` + digest périmé | Re-préparer + ré-écrire |
| `activating` | Attendre le heartbeat (timeout négatif §3) |

## Flux sortants

```text
ESP_LOGx (toutes tasks)
    │
log_hook (esp_log_set_vprintf)
    ├─ UART (inchangé)
    └─ ring buffer 4 KB (non-bloquant, drop si plein)
              │
         drain_task ──► WSS /v1alpha1/logs  (level="raw", best-effort)

embewi_log_emit() ───────► HTTPS POST /v1alpha1/logs  (événements critiques OTA/lifecycle)
heartbeat_task ──────────► HTTPS POST /v1alpha1/heartbeat  (toutes les 5 s)
```

Le scheme est toujours forcé en `https://` / `wss://` quel que soit le scheme
stocké dans `ctrl_url` (contrat §1).

## Tâches FreeRTOS

| Tâche | Stack | Priorité | Rôle |
|---|---|---|---|
| `embewi_hb` | 8 192 B | 4 | Heartbeat HTTPS (esp_http_client exige ~4 KB) |
| `embewi_log` | 8 192 B | 3 | Drain ring buffer → WebSocket |
| `embewi_selfchk` | 4 096 B | 5 | Self-check borné post-OTA |
| httpd (HTTPS) | ~10 240 B | — | Serveur HTTPS :443 (mbedTLS inclus) |
