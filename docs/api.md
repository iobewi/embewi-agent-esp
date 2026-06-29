# API inbound (Core → Agent)

Tous les endpoints sont sous le préfixe `/v1alpha1` et servis en **HTTPS :443**.
Chaque appel doit porter un header `Authorization: Bearer <token>` — le token est
établi au provisioning et stocké en NVS. Un token absent ou invalide retourne
**401** sur **tous** les endpoints sans exception (y compris `/ota/activate`).

La comparaison du token est effectuée en **temps constant** (pas de court-circuit
au premier octet différent) pour résister aux attaques temporelles.

---

## `GET /v1alpha1/info`

Identité matérielle, firmware courant et slot stagé (clé de l'idempotence).

```json
{
  "node_id": "embewi-a1b2c3",
  "chip": "esp32c3",
  "idf_version": "v5.3",
  "flash_size": 4194304,
  "ram_size": 327680,
  "partition_layout": "embewi-ab-v1",
  "active_slot": "ota_0",
  "firmware": {
    "name": "wheel-controller",
    "version": "1.0.0",
    "digest": "sha256:abc123..."
  },
  "staged": {
    "state": "written",
    "slot": "ota_1",
    "digest": "sha256:def456...",
    "deployment_id": "wheel-controller-1.1.0"
  },
  "state": "running",
  "config_generation": 2,
  "app_port": 8080
}
```

`staged.state` ∈ `none | written | activating`. `staged.deployment_id` est
renseigné dès l'écriture (`PUT /ota/write` avec header `X-Embewi-Deployment-Id`),
avant même l'`activate`.

---

## `GET /v1alpha1/health`

Health **local** (pas seulement connectivité réseau).

```json
{
  "status": "ok",
  "state": "running",
  "checks": {
    "app": "ok",
    "sensors": "ok",
    "storage": "ok"
  }
}
```

`status` ∈ `ok | degraded | fail`. `storage` est un canary NVS réel
(write → commit → read → erase) — pas un flag en RAM.

---

## `POST /v1alpha1/ota/prepare`

Annonce le firmware avant transfert. L'agent vérifie la compatibilité **avant**
tout octet. En cas de refus, retourne HTTP 200 avec `accepted: false` et un
`reason` stable (§4b — ne pas parser les messages libres).

Corps :

```json
{
  "deployment_id": "wheel-controller-1.1.0",
  "digest": "sha256:...",
  "size": 983040,
  "chip": "esp32c3",
  "idf_version": "v5.3",
  "partition_layout": "embewi-ab-v1"
}
```

Réponse accept :

```json
{ "accepted": true, "target_slot": "ota_1", "reason": null }
```

Réponse refus :

```json
{ "accepted": false, "target_slot": null, "reason": "chip_mismatch" }
```

Codes `reason` stables :

| `reason` | Cause |
|---|---|
| `chip_mismatch` | Binaire pour un autre SoC |
| `layout_mismatch` | `partition_layout` incompatible (seul `embewi-ab-v1` accepté) |
| `size_too_large` | Image > taille du slot inactif |
| `busy` | Slot introuvable (OTA en cours ?) |

---

## `PUT /v1alpha1/ota/write`

Streame le `.bin` brut. L'agent calcule le SHA-256 **en incrémental** (PSA
crypto, pas de relecture flash après coup).

Headers :

```text
Authorization: Bearer <token>
X-Embewi-Deployment-Id: wheel-controller-1.1.0   (optionnel, persisté dans staged)
X-Embewi-Digest: sha256:...                       (optionnel, vérifié en fin)
Content-Length: 983040
Content-Range: bytes 0-65535/983040              (optionnel, reprise in-session)
```

### Reprise après coupure (`Content-Range`)

Le handle OTA et l'état SHA-256 sont **statiques** — ils survivent à une
déconnexion TCP. Le Core chunke l'image et reprend à l'offset rapporté.

| Situation | Réponse agent |
|---|---|
| Header absent ou `start=0` | Nouvelle session, HTTP 200 |
| `start` aligné sur `written` | Continue, HTTP 200 |
| `start` désaligné | HTTP 416 + `{"error":"range_mismatch","written":N}` |
| Header malformé | HTTP 400 + `{"error":"bad_content_range"}` |

Réponse plage intermédiaire : `{"status":"partial","written":N}`

Réponse finale (dernière plage ou monolithique) :

```json
{ "written": 983040, "digest": "sha256:...", "status": "written" }
```

Codes `status` stables :

| `status` | HTTP | Cause |
|---|---|---|
| `written` | 200 | Succès, digest vérifié |
| `partial` | 200 | Plage intermédiaire reçue |
| `digest_mismatch` | 200 | SHA-256 écrit ≠ `X-Embewi-Digest` |
| `write_failed` | 500 | Échec flash |
| `ota_begin_failed` | 500 | `esp_ota_begin` KO |

---

## `POST /v1alpha1/ota/activate`

Configure le prochain boot et redémarre. L'agent effectue `set_boot_partition`
**avant** de répondre — si le slot n'est pas prêt (session non initialisée),
répond `409` sans rebooter (pas de faux "rebooting").

Corps :

```json
{ "deployment_id": "wheel-controller-1.1.0" }
```

Réponse (succès) :

```json
{ "status": "rebooting", "target_slot": "ota_1" }
```

Réponse (slot non prêt) :

```text
HTTP 409
{ "error": "slot_not_ready" }
```

> **Reprise après reboot de l'agent** : si l'agent redémarre entre `write` et
> `activate`, il restaure le `s_target` depuis le NVS stagé (`staged.state ==
> written`). Le Core peut relancer `POST /ota/activate` normalement.

---

## `GET /v1alpha1/config`

État de la configuration en deux vues :
- `active` : snapshot figé au boot (ce que les apps ont chargé).
- `nvs` : NVS courant (peut diverger si un `POST /config` a eu lieu sans reboot).

```json
{
  "generation": 3,
  "active_generation": 2,
  "active": {
    "gpio_button": "9",
    "gpio_ws2812": "10"
  },
  "nvs": {
    "gpio_button": "9",
    "gpio_ws2812": "48"
  }
}
```

`generation > active_generation` → config poussée, reboot en attente.

---

## `POST /v1alpha1/config`

Pousse des clés/valeurs vers le NVS. Sémantique **merge-on-key** : seules les
clés citées sont modifiées. Valeur `""` = effacer la clé (retour au défaut build).

Corps :

```json
{ "data": { "gpio_ws2812": "48" } }
```

Réponse :

```json
{ "status": "saved", "generation": 3, "note": "effective_after_reboot" }
```

Codes d'erreur :

| `error` | HTTP | Cause |
|---|---|---|
| `missing_data_field` | 400 | Objet `data` absent |
| `nvs_write_failed` | 500 | Échec écriture NVS |

---

## `POST /v1alpha1/reboot`

Reboot contrôlé sans OTA. À utiliser pour appliquer un `POST /config`.
Si un OTA est planifié dans la même réconciliation, ne pas appeler `/reboot`
séparément — `POST /ota/activate` couvre déjà le reboot.

Corps : `{}`

Réponse : `{ "status": "rebooting" }`

---

## `POST /v1alpha1/app/port`

Reconfigure le port TCP du service applicatif (persisté en NVS).

Corps : `{ "port": 9090 }`

Réponse : `{ "status": "saved", "port": 9090, "app_port": 9090 }`

Contrainte : `1024 ≤ port ≤ 65535`.

---

## `POST /v1alpha1/tls/cert`

Pousse un certificat TLS et sa clé privée PEM pour le serveur HTTPS de l'agent.
Effectif au prochain reboot. Utiliser les noms de champs `cert_pem` / `key_pem`
(spécification v2) — les anciens noms `cert` / `key` restent acceptés.

Corps :

```json
{
  "cert_pem": "-----BEGIN CERTIFICATE-----\n...",
  "key_pem":  "-----BEGIN EC PRIVATE KEY-----\n..."
}
```

Réponse : `{ "status": "saved", "note": "effective_after_reboot" }`

---

## `POST /v1alpha1/token`

Rotation du token Bearer sans repasser par le provisioning physique.
Authentifié au token **courant**. Le nouveau token est commité en NVS
**avant** la réponse — si le commit échoue, l'ancien token reste actif.

Corps : `{ "token": "<nouveau-token>" }`

Réponse : `{ "status": "rotated" }`

Contrainte : `8 ≤ len(token) ≤ 64`. Un token vide est refusé (`400`).

---

## Erreurs transverses

| `error` | HTTP | Cause |
|---|---|---|
| `unauthorized` | 401 | Token absent ou invalide |
| `body_too_large` | 413 | Corps > limite (512 B général, 6 KB pour `/tls/cert`) |

> **Convention §4b** : un refus **métier** (`prepare` rejeté, `digest_mismatch`)
> répond HTTP **200** avec le code dans le corps. Les `4xx/5xx` sont réservés aux
> erreurs de protocole ou de ressource. Les codes sont stables — le Core les
> mappe en Kubernetes Events.
