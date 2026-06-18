# Embewi — Référence API Agent

> Documentation technique de l'agent ESP32 (famille : C3, C6, S3, H6…). Décrit exactement ce qui est
> implémenté dans `embewi_http.c` (inbound) et `embewi_heartbeat.c` (outbound).
> Le contrat de référence reste `embewi-contract-v2.md`.

Version protocole : `v1alpha1`  
Transport inbound : **HTTPS :443** (TLS, cert auto-signé fallback ou CA-signé via NVS)  
Transport outbound : **HTTP POST** vers `ctrl_url` (provisionné au portail captif)

---

## Authentification

Tous les endpoints inbound exigent un header `Authorization` :

```
Authorization: Bearer <token>
```

Le token est généré aléatoirement à la première configuration (portail captif) et
affiché une unique fois dans la page de confirmation. Il est stocké en NVS
(`embewi_prov` / clé `token`). Il peut aussi être fourni manuellement dans le
formulaire de provisioning.

Réponse si token absent ou invalide :

```
HTTP 401 Unauthorized
{"error": "unauthorized"}
```

---

## Portail de provisioning (premier boot)

Au premier boot (NVS vide), l'agent démarre en mode AP WiFi (`embewi-XXXX`) et
sert un portail captif sur `http://192.168.4.1`. Après soumission du formulaire,
la configuration est sauvegardée en NVS (`embewi_prov`) et le device redémarre.

**Champs du formulaire :**

| Champ | Obligatoire | Clé NVS | Description |
|-------|-------------|---------|-------------|
| `node_id` | Oui | `node_id` | Identifiant unique du nœud — pré-rempli `embewi-AABBCC` (3 derniers octets MAC)¹ |
| `ssid` | Oui | `ssid` | SSID du réseau WiFi cible |
| `pass` | Non | `pass` | Mot de passe WiFi |
| `ctrl_url` | Oui | `ctrl_url` | URL du Runtime Core (`http://IP:port`) |
| `ip` | Non² | `ip` | IP statique du device (vide = DHCP) |
| `mask` | Non² | `mask` | Masque réseau (ex: `255.255.255.0`) |
| `gw` | Non² | `gw` | Passerelle — pré-remplie avec l'IP du contrôleur³ |
| `dns` | Non | `dns` | DNS — pré-rempli avec la passerelle³ |
| `token` | Non | `token` | Token Bearer — généré aléatoirement si vide⁴ |

> ¹ `node_id` est modifiable librement (format libre, ex: `embewi-moteur-gauche`). Si absent de NVS au boot (flash directe sans portail), un ID temporaire MAC-based est utilisé en RAM.  
> ² `ip`, `mask` et `gw` sont tous obligatoires ensemble si IP statique.  
> ³ Valeurs déduites automatiquement dans le formulaire (éditables).  
> ⁴ Le token généré est affiché **une seule fois** dans la page de confirmation —
>   le noter immédiatement.

**Logique réseau :**

```
DHCP (défaut)
  → aucun champ IP requis, le réseau fournit l'adresse

IP statique (ip + mask renseignés)
  gw  vide → IP extraite de ctrl_url  (ex: http://192.168.10.1:8080 → 192.168.10.1)
  dns vide → copie de gw
```

**Page de confirmation :**

Après soumission réussie, le token est affiché clairement pendant 15 secondes
avant le reboot automatique. C'est la seule occasion de le récupérer.

```
Configuration enregistrée ✓

Copiez ce token maintenant :
┌─────────────────────────────────────┐
│ a3f7c1b2e8d09441f6bc3e7a2c504d8f   │
└─────────────────────────────────────┘
Le device redémarre dans 15 secondes…
```

**Reprovisioning :** effacer les clés NVS (`idf.py erase-flash` ou endpoint futur
`/v1alpha1/reset`) force un retour au portail captif au prochain boot.

---

## Endpoints inbound (Core → ESP)

### `GET /v1alpha1/info`

Identité matérielle, firmware courant et slot stagé (clé de l'idempotence OTA).

**Réponse 200 :**

```json
{
  "node_id": "embewi-a1b2c3",
  "chip": "esp32c3",
  "idf_version": "v6.0.0",
  "flash_size": 4194304,
  "ram_size": 401408,
  "partition_layout": "embewi-ab-v1",
  "active_slot": "ota_0",
  "firmware": {
    "name": "wheel-controller",
    "version": "1.0.0",
    "digest": "sha256:a3f7c1..."
  },
  "staged": {
    "state": "none",
    "slot": "",
    "digest": "",
    "deployment_id": ""
  },
  "state": "running",
  "app_port": 8080
}
```

| Champ | Type | Description |
|-------|------|-------------|
| `node_id` | string | Identifiant du nœud (NVS `embewi_prov`/`node_id`, défaut `embewi-AABBCC` depuis la MAC) |
| `chip` | string | Cible IDF (`CONFIG_IDF_TARGET`) |
| `idf_version` | string | Version ESP-IDF (`IDF_VER`) |
| `flash_size` | uint | Taille flash SPI en octets |
| `ram_size` | uint | RAM interne totale allouable en octets |
| `active_slot` | string | Partition active : `factory` \| `ota_0` \| `ota_1` |
| `firmware.digest` | string | `sha256:<hex>` calculé à chaque boot depuis la flash (vide si jamais passé par OTA) |
| `staged.state` | string | `none` \| `written` \| `activating` |
| `state` | string | État agent (voir §2 contrat) |
| `app_port` | uint | Port du service applicatif (NVS, défaut 8080) |

---

### `GET /v1alpha1/health`

État de santé local. Utilisé par Kubernetes pour piloter `EndpointSlice.ready`.

**Réponse 200 :**

```json
{
  "status": "ok",
  "state": "running",
  "checks": {
    "app":     "ok",
    "sensors": "ok",
    "storage": "ok"
  }
}
```

| `status` | Condition |
|----------|-----------|
| `ok` | `state == running` |
| `degraded` | `state == degraded` |
| `fail` | tout autre état |

| Check | Valeurs |
|-------|---------|
| `app` | `ok` \| `fail` — résultat de `embewi_app_selfcheck()` |
| `sensors` | `ok` \| `fail` — TODO: ping capteurs I2C |
| `storage` | `ok` \| `fail` — TODO: filesystem montable |

---

### `POST /v1alpha1/ota/prepare`

Le Core annonce le firmware avant tout transfert. L'ESP valide la compatibilité
matérielle et réserve le slot inactif.

**Corps de la requête :**

```json
{
  "deployment_id": "wheel-controller-1.1.0",
  "digest":        "sha256:b9e4f2...",
  "size":          983040,
  "chip":          "esp32c3",
  "idf_version":   "v6.0.0",
  "partition_layout": "embewi-ab-v1"
}
```

**Réponse 200 — accepté :**

```json
{ "accepted": true, "target_slot": "ota_1", "reason": null }
```

**Réponse 200 — refusé :**

```json
{ "accepted": false, "target_slot": null, "reason": "chip_mismatch" }
```

| `reason` | Cause |
|----------|-------|
| `chip_mismatch` | `chip` != `CONFIG_IDF_TARGET` |
| `layout_mismatch` | `partition_layout` != `embewi-ab-v1` |
| `idf_incompatible` | version IDF incompatible |
| `size_too_large` | `size` > taille partition inactive |
| `busy` | OTA déjà en cours |
| `unknown` | erreur interne |

---

### `PUT /v1alpha1/ota/write`

Stream du binaire `.bin` brut. L'ESP calcule le SHA-256 en incrémental à
l'écriture et compare au digest annoncé en header.

**Headers requis :**

```
Content-Type: application/octet-stream
Content-Length: <taille exacte>
X-Embewi-Digest: sha256:<hex>
Authorization: Bearer <token>
```

**Corps :** binaire `.bin` brut (pas base64, pas multipart).

**Réponse 200 — succès :**

```json
{ "written": 983040, "digest": "sha256:b9e4f2...", "status": "written" }
```

**Réponse 200 — erreur digest :**

```json
{ "status": "digest_mismatch" }
```

**Réponse 200 — erreur écriture :**

```json
{ "status": "write_failed" }
```

> Le digest est calculé en streaming pendant la réception — pas de relecture de
> la partition flash. Si `X-Embewi-Digest` est absent, la vérification est sautée.

---

### `POST /v1alpha1/ota/activate`

Configure le prochain boot sur le slot écrit et redémarre. La réponse est
envoyée **avant** le reboot.

**Corps de la requête :**

```json
{ "deployment_id": "wheel-controller-1.1.0", "reboot": true }
```

**Réponse 200 (avant reboot) :**

```json
{ "status": "rebooting", "target_slot": "ota_1" }
```

> Après le reboot, l'agent est en `pending_verify`. Il émet un heartbeat avec
> `state: "pending_verify"` et `ota_validated: false` pendant la fenêtre de
> validation (15 s par défaut). Si le self-check passe → `running`. Sinon →
> rollback automatique par le bootloader.

---

### `POST /v1alpha1/app/port`

Reconfigure dynamiquement le port du service applicatif sans reflash.
Stoppe le service, sauvegarde en NVS, relance sur le nouveau port.

**Corps de la requête :**

```json
{ "port": 9090 }
```

Contrainte : `1024 ≤ port ≤ 65535`.

**Réponse 200 :**

```json
{ "app_port": 9090 }
```

**Réponse 400 :**

```json
{ "error": "port must be 1024-65535" }
```

---

### `POST /v1alpha1/tls/cert`

Livre un certificat TLS signé CA pour remplacer le fallback auto-signé.
Sauvegardé en NVS — effectif au prochain démarrage du serveur HTTPS.

**Corps de la requête :**

```json
{
  "cert": "-----BEGIN CERTIFICATE-----\n...\n-----END CERTIFICATE-----\n",
  "key":  "-----BEGIN EC PRIVATE KEY-----\n...\n-----END EC PRIVATE KEY-----\n"
}
```

Les `\n` dans les valeurs JSON sont des séquences échappées (`\\n`) — l'agent
les déséchape avant validation.

**Réponse 200 :**

```json
{ "status": "saved", "note": "effective_after_reboot" }
```

**Réponse 400 :**

```json
{ "error": "invalid_pem" }
```

**Réponse 413 :** body > 6144 octets.

---

## Flux sortants (ESP → Core)

L'ESP initie ces requêtes vers `ctrl_url` (ex: `http://192.168.1.10:8080`).
Si `ctrl_url` est vide ou le Core injoignable, l'agent continue de fonctionner
— les émissions sont loggées localement en `ESP_LOGW`.

---

### `POST /v1alpha1/heartbeat`

Émis toutes les **5 secondes** par la task `embewi_hb`.

```json
{
  "node_id":         "embewi-a1b2c3",
  "ts":              1710000000,
  "state":           "running",
  "deployment_id":   "wheel-controller-1.1.0",
  "firmware_digest": "sha256:a3f7c1...",
  "ota_validated":   true,
  "uptime_ms":       120034,
  "heap_free":       82344,
  "rssi":            -61
}
```

| Champ | Type | Description |
|-------|------|-------------|
| `ts` | int | Uptime en secondes depuis le boot (pas epoch) |
| `state` | string | État courant de l'agent |
| `ota_validated` | bool | `true` uniquement après `mark_valid` — distingue `pending_verify` de `running` |
| `heap_free` | uint | RAM libre en octets (`esp_get_free_heap_size()`) |
| `rssi` | int | Signal WiFi en dBm (`esp_wifi_sta_get_ap_info()`) |

> Règle critique : un agent en `pending_verify` **continue d'émettre** avec
> `ota_validated: false`. Le silence est indistinguable d'un crash pour le Core.

---

### `POST /v1alpha1/logs`

Émis à chaque événement significatif via `embewi_log_emit(level, msg)`.

```json
{
  "ts":       1710000000,
  "node":     "embewi-a1b2c3",
  "workload": "wheel-controller",
  "level":    "info",
  "msg":      "embewi agent up"
}
```

| `level` | Usage |
|---------|-------|
| `info`  | Événements normaux (boot, OTA validé) |
| `error` | Self-check KO, rollback déclenché |
| `fatal` | Rollback impossible → état `failed` |

---

## Codes d'erreur HTTP

| Code | Signification |
|------|---------------|
| 200 | Succès (y compris les refus métier avec `accepted: false`) |
| 400 | Body malformé ou champ manquant |
| 401 | Token absent ou invalide |
| 413 | Body trop grand |
| 500 | Erreur interne ESP (NVS, OTA) |

---

## Limites MVP

| Limite | Valeur |
|--------|--------|
| Body max (JSON) | 512 octets (sauf `/tls/cert` : 6144) |
| Token | 32 hex chars (16 octets aléatoires) |
| Timeout HTTP outbound | 1500 ms |
| Période heartbeat | 5000 ms |
| Fenêtre pending_verify | 15 000 ms |
| Chunks OTA | 1024 octets |
