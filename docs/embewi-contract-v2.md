# Embewi — Contrat Core ↔ Agent (v2)

> Spec de référence figée. Toute implémentation (Runtime Core en Go, Agent en
> ESP-IDF/C) doit s'y conformer. Les sections marquées **[NORMATIF]** sont des
> contraintes ; les sections **[RÉSERVE]** sont hors MVP et documentées pour ne
> pas recoder l'API plus tard.

Version protocole : `v1alpha1`
Préfixe des endpoints inbound : `/v1alpha1/...` (versionner dès le départ — les
devices sur le terrain survivront à plusieurs versions de Core).

---

## 0. Périmètre et hypothèses réseau **[NORMATIF]**

```text
MVP network mode : OTA PUSH
Hypothèse        : le subnet de management des ESP est L3-joignable
                   depuis les nodes du cluster Embewi Core.
Mode futur       : OTA PULL (device-initiated) pour devices NATés / non joignables.
```

Conséquence de l'asymétrie :

```text
Inbound  (Core → ESP) : /info /health /ota/prepare /ota/write /ota/activate
Outbound (ESP → Core) : heartbeat, logs
```

Le jour où un device n'est plus joignable en inbound (NAT), le mode PUSH
s'effondre : c'est une bifurcation d'architecture (mode PULL), pas un correctif.
Elle est nommée ici, pas découverte en phase 4.

---

## 1. Modèle de sécurité **[NORMATIF]**

Principe directeur :

```text
Core verifies for efficiency.
Bootloader verifies for trust.
```

La vérification de signature n'est **pas** « Core OU ESP ». C'est **les deux**,
avec autorité finale au bootloader — le seul composant qui ne peut pas mentir.

| Couche          | Rôle de sécurité                                                        | Qui porte la confiance |
| --------------- | ----------------------------------------------------------------------- | ---------------------- |
| Core            | Pull OCI/ORAS, vérifie digest + signature + compat avant de transférer  | Efficacité (pas confiance) |
| Transport       | Endpoint OTA authentifié (mTLS, ou token par node au minimum)           | Empêche l'inbound anonyme |
| ESP — réception | N'accepte un OTA que d'un Core autorisé (auth transport)                | Filtre l'appelant      |
| Bootloader      | **Secure Boot v2** : refuse de booter toute image non/ mal signée       | **Racine de confiance** |
| eFuse           | **Anti-rollback** : bloque le downgrade vers image signée plus ancienne | Matériel, irréversible |
| Flash           | **Flash encryption** : si vol physique du device plausible              | Confidentialité at-rest |
| Runtime         | Rollback validé **localement** après self-check (cf. §3)                | Intégrité fonctionnelle |

Pourquoi le Core ne peut pas être racine de confiance : un ESP qui reçoit des
octets sur `/ota/write` n'a aucun moyen cryptographique de savoir que ces octets
sont ceux que le Core a vérifiés. Sans Secure Boot, quiconque atteint l'IP de
management peut écrire une image arbitraire. **Sans auth inbound, `/ota/activate`
est une primitive de reboot offerte au premier venu sur le LAN.**

Transport — décision MVP explicite :

```text
MVP security transport    : per-node token over HTTPS
Target security transport : mTLS
```

Justification : mTLS sur ESP-IDF est faisable, mais entraîne immédiatement
gestion de CA, distribution + rotation de certificats, horloge fiable (validation
de validité) et empreinte mémoire. Pour un premier prototype, HTTPS + token par
node + Secure Boot v2 est un socle cohérent et défendable. mTLS est la cible, pas
le point de départ.

Exigences minimales MVP :
- `CONFIG_SECURE_BOOT_V2_ENABLED` — sinon le projet n'est pas défendable en revue sécu.
- Auth sur tous les endpoints inbound : **token par node sur HTTPS** (cible mTLS).
- Anti-rollback eFuse activé dès qu'un schéma de version de sécurité est en place.
- Flash encryption : optionnel MVP, obligatoire si le device peut être volé.

> **Dev vs Prod.** Ces exigences sont des opérations eFuse **irréversibles** :
> elles ne s'activent **pas en dev**. Elles sont portées par un profil de build
> séparé et opt-in (`sdkconfig.defaults.prod` + `CONFIG_EMBEWI_VERIFY_CORE_CERT`
> pour l'auth des flux sortants). Procédure complète, gestion de la clé de
> signature et de la CA du Core : **`embewi-prod-security.md`**. Le flux de dev
> reste chiffré-mais-non-authentifié et librement reflashable.

---

## 1a. Enrôlement et identité du device **[NORMATIF]**

Un device n'est pas « ajouté » par le Core : il **se présente**. L'identité et le
secret d'authentification sont établis **hors-bande au provisioning**, pas
négociés en ligne (pas d'AC, pas de CSR en MVP — cf. §1).

```text
Provisioning (portail captif, premier boot, AP "embewi-XXXX") :
  admin saisit : node_id, ctrl_url, [token]
  token vide   → généré aléatoirement par le device (128 bits hex)
  token affiché UNE SEULE FOIS sur la page de confirmation
  persisté en NVS (namespace embewi_prov)
```

**Durcissement du provisioning (l'AP est le moment le plus faible — aucun secret
partagé n'existe encore) :**

```text
- AP ouvert PAR NÉCESSITÉ (œuf/poule : pas de clé pré-partageable avec un device
  vierge). Un mot de passe dérivé du SSID serait du théâtre (déchiffrable).
- Portail servi en HTTPS (cert auto-signé embarqué) → le mot de passe WiFi et le
  token sont chiffrés au niveau applicatif même sur l'AP ouvert. Bloque le
  sniffing PASSIF. Résiduel : MITM actif (rogue AP) — accepté pour le MVP.
  Coût : l'utilisateur ouvre https://192.168.4.1 manuellement (avertissement cert).
- Fenêtre AP bornée (10 min sans config → reboot) : évite un AP ouvert wedgé
  indéfiniment. Hypothèse de déploiement : provisioning en zone de confiance.
- Durcissement supérieur (hors MVP) : ESP-IDF wifi_provisioning security2
  (X25519 + proof-of-possession imprimé) pour résister au MITM actif.
```

Le couple `(node_id, token)` est l'identité du device. Le token est le secret
partagé : l'admin le recopie dans le Core (typiquement un `McuSecret`) au moment
où il crée le `McuNode` correspondant. **Le Core et le device connaissent le
token ; le réseau ne le voit jamais en clair (HTTPS, §1).**

Découverte côté Core — deux modèles compatibles :

```text
1. Pré-déclaré (recommandé) :
   l'admin crée McuNode{node_id, tokenRef} AVANT le boot du device.
   Au premier heartbeat, le Core matche node_id + valide le token → Ready.

2. Auto-enrôlement (TOFU, Trust On First Use) :
   le Core accepte un node_id inconnu au premier heartbeat et crée un
   McuNode en état "Pending" jusqu'à approbation manuelle (kubectl approve).
   Le token reçu devient le secret de référence à l'approbation.
```

Invariants côté agent (ce que ce repo garantit) :

```text
- le device émet son node_id dans CHAQUE heartbeat (auto-annonce continue).
- aucun appel inbound n'est accepté tant que le token NVS est vide (401, §1).
- node_id absent en NVS → ID temporaire dérivé de la MAC (embewi-AABBCC),
  marqué comme tel : à reprovisionner (pas une identité stable).
- ré-enrôlement = repasser par le portail captif (efface/réécrit NVS prov).
```

Le Core est responsable de refuser un `node_id` en double (cf. §7,
`AmbiguousBinding`) — l'agent ne déduplique pas, il s'annonce.

---

## 2. États de l'agent **[NORMATIF]**

```text
booting        → démarrage, avant que l'app n'ait pris la main
pending_verify → image fraîchement flashée, en attente de validation locale
running        → image validée (mark_valid effectué), nominal
degraded       → app tourne mais un check est KO (capteur/storage)
rollback       → bascule en cours vers l'image précédente
failed         → échec terminal (ni validation ni rollback possibles)
```

Transitions clés :

```text
booting --------> pending_verify      (boot d'une image tout juste activée)
booting --------> running             (boot d'une image déjà validée)
pending_verify -> running             (self-check OK → mark_valid)
pending_verify -> rollback            (self-check KO → mark_invalid_and_reboot)
pending_verify -> rollback            (deadline dépassée → watchdog → reset)  ← §3
running -------> degraded             (un check passe KO en cours de route)
rollback ------> running              (reboot sur l'image précédente, déjà validée)
* -------------> failed               (rollback impossible, image précédente absente)
```

Règle d'or côté heartbeat : **un device en `pending_verify` continue d'émettre
un heartbeat** portant `state: "pending_verify"`. Pas de silence pendant la
fenêtre de validation (le silence est indistinguable d'un crash).

---

## 3. Séquence OTA critique **[NORMATIF]**

C'est le cœur dur du projet — le bout qui peut l'invalider. Le rollback réel
n'est **pas** piloté par le Core : c'est le bootloader ESP-IDF qui le fait.

```text
Core: POST /ota/prepare        → ESP valide compat, réserve target_slot
Core: PUT  /ota/write          → ESP écrit le slot inactif (ota_1)
Core: POST /ota/activate       → ESP: esp_ota_set_boot_partition + reboot
   ↓
boot ota_1  →  state = pending_verify   (image en PENDING_VERIFY côté bootloader)
   ↓
self-check local BORNÉ PAR WATCHDOG (deadline T)
   ├─ OK   avant T : esp_ota_mark_app_valid_cancel_rollback()  → state=running
   │                 PUIS SEULEMENT : heartbeat repart en "running"
   ├─ KO   avant T : esp_ota_mark_app_invalid_rollback_and_reboot() → rollback
   └─ HANG (pas de mark avant T) : Task Watchdog (TWDT) → reset forcé
                                   → le bootloader revient seul sur ota_0
```

Deux garde-fous **obligatoires**, l'un local, l'autre côté Core :

```text
côté ESP  : self-check borné par un hardware watchdog (TWDT).
            CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE actif.
            sans mark_valid avant deadline T → reset → rollback bootloader.
            => un pending_verify ne peut JAMAIS rester coincé indéfiniment.

côté Core : timeout négatif explicite.
            si la confirmation (state=running + bon deployment_id) n'arrive pas
            dans N secondes → déploiement = Failed.
            le Core ne déclenche pas le rollback : il le CONSTATE
            (le device a déjà rollback tout seul).
```

Le signal de santé sert **deux maîtres** : il valide l'image localement
(`mark_valid`) ET il ouvre le routage côté cluster (EndpointSlice ready). Si on
rate ça, on obtient des ESP qui annoncent « ready » sur une image qui se fera
revert au prochain reset.

Confirmation de fin de déploiement émise par l'agent (cf. §5, ré-écho du
`deployment_id` — pas seulement le digest, car deux déploiements peuvent
partager un digest) :

```json
{
  "state": "running",
  "ota_validated": true,
  "deployment_id": "wheel-controller-1.1.0",
  "firmware": { "version": "1.1.0", "digest": "sha256:..." }
}
```

---

## 4. Endpoints inbound (Core → ESP) **[NORMATIF]**

### `GET /v1alpha1/info`

Identité matérielle **+ slot stagé** (clé de l'idempotence, cf. §6).

```json
{
  "node_id": "embewi-a1b2c3",
  "chip": "esp32",
  "idf_version": "5.2",
  "flash_size": 4194304,
  "ram_size": 409600,
  "partition_layout": "embewi-ab-v1",
  "active_slot": "ota_0",
  "firmware": {
    "name": "wheel-controller",
    "version": "1.0.0",
    "digest": "sha256:..."
  },
  "staged": {
    "slot": "ota_1",
    "digest": "sha256:...",
    "deployment_id": "wheel-controller-1.1.0",
    "state": "written"
  },
  "state": "running",
  "config_generation": 2,
  "app_port": 8080
}
```

`staged.state` ∈ `none | written | activating`. Quand aucun slot n'est stagé :
`"staged": { "state": "none" }`.

### `GET /v1alpha1/health`

Health **local**, pas seulement réseau.

```json
{
  "status": "ok",
  "state": "running",
  "checks": { "app": "ok", "sensors": "ok", "storage": "ok" }
}
```

`status` ∈ `ok | degraded | fail`.

### `POST /v1alpha1/ota/prepare`

Le Core annonce le firmware avant transfert. L'ESP vérifie la compat **avant**
tout octet (un binaire `esp32-s3` flashé sur `esp32` ne boote pas).

```json
{
  "deployment_id": "wheel-controller-1.1.0",
  "artifact": "registry.local/embewi/wheel-controller:v1.1.0",
  "digest": "sha256:...",
  "size": 983040,
  "chip": "esp32",
  "idf_version": "5.2",
  "partition_layout": "embewi-ab-v1"
}
```

Réponse (accept) :

```json
{ "accepted": true, "target_slot": "ota_1", "reason": null }
```

Réponse (refus) — `reason` ∈ `chip_mismatch | layout_mismatch | idf_incompatible | busy | size_too_large` :

```json
{ "accepted": false, "target_slot": null, "reason": "chip_mismatch" }
```

### `PUT /v1alpha1/ota/write`

Le Core streame le `.bin` brut. L'ESP ne connaît jamais OCI/ORAS.

Headers :

```text
X-Embewi-Deployment-Id: wheel-controller-1.1.0
X-Embewi-Digest: sha256:...
Content-Length: 983040
Authorization: Bearer <per-node-token>   (HTTPS + token MVP ; mTLS en cible)
```

Réponse :

```json
{ "written": 983040, "digest": "sha256:...", "status": "written" }
```

L'ESP calcule le digest **en incrémental, au fil de l'écriture** (SHA-256
streaming sur chaque chunk reçu via `esp_ota_write`), et compare au header en fin
de transfert. Pas de relecture de la partition flash après coup. Rejet si mismatch
(`status: "digest_mismatch"`). Le Core a vérifié pour l'efficacité ; l'ESP
revérifie ce qu'il a réellement écrit, sans coût de relecture.

#### Write reprenable — `Content-Range` (in-session) **[NORMATIF]**

Le `PUT` accepte un header optionnel pour reprendre après une coupure réseau
sans retransférer tout le binaire :

```text
Content-Range: bytes <start>-<end>/<total>     (bornes inclusives, octets)
```

Protocole :

```text
- Header absent       → écriture monolithique (1 PUT = image entière). Legacy,
                        toujours supporté.
- start == 0          → nouvelle session : le slot est (ré)initialisé.
- start > 0           → reprise. Doit correspondre à l'offset déjà écrit côté
                        device. Sinon → 416 + l'offset réel (resync).
- end + 1 == total    → dernière plage : finalisation + vérif digest.
- sinon               → 200 {"status":"partial","written":<offset>}.
```

Le device garde la session d'écriture ouverte (handle OTA + état SHA-256) **entre
les requêtes** : une déconnexion TCP n'avorte rien. Le Core **chunke** l'image en
plusieurs plages et, sur échec, reprend à l'offset rapporté par le device.

Réponse de resynchronisation (offset demandé ≠ offset réel) :

```text
HTTP 416 Range Not Satisfiable
{ "error": "range_mismatch", "written": 884736 }
```

Réponse de plage intermédiaire :

```text
HTTP 200
{ "status": "partial", "written": 65536 }
```

`Content-Range` malformé → `400 {"error":"bad_content_range"}`.

> **Reprise inter-reboot** (le device redémarre au milieu du transfert) reste
> **[RÉSERVE]** : l'état SHA et le handle OTA sont en RAM. Le Core repart alors
> de `start=0`. La reprise in-session couvre le cas courant (coupure wifi).

### `POST /v1alpha1/ota/activate`

L'ESP configure le prochain boot (`esp_ota_set_boot_partition`) et redémarre.

```json
{ "deployment_id": "wheel-controller-1.1.0", "reboot": true }
```

Réponse avant reboot :

```json
{ "status": "rebooting", "target_slot": "ota_1" }
```

### `GET /v1alpha1/config`

Lit l'état de la configuration de l'agent : valeurs actives (chargées au dernier
boot) et valeurs NVS courantes (peut diverger si un `POST /config` a été fait
sans reboot depuis).

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

`generation > active_generation` → config poussée mais reboot en attente.
Quand NVS est vide (aucun push depuis le flash) : `"nvs": {}`, les deux
générations valent `0`.

### `POST /v1alpha1/config`

Pousse un jeu de clés/valeurs vers le NVS de l'agent. Sémantique
**merge-on-key** : seules les clés citées sont écrites, les autres sont
inchangées. Les valeurs sont des **chaînes UTF-8** ; l'app interprète le
type (`atoi`, `strtof`, etc.).

```json
{
  "data": {
    "gpio_button": "9",
    "gpio_ws2812": "48"
  }
}
```

Réponse :

```json
{
  "status": "saved",
  "generation": 3,
  "note": "effective_after_reboot"
}
```

`generation` est incrémenté à chaque `POST /config`. Pour **réinitialiser**
une clé au défaut build, pousser la valeur `""` (chaîne vide) — l'agent
efface la clé NVS.

### `POST /v1alpha1/app/port`

Reconfigure le port TCP du service applicatif embarqué (ex. serveur REST de
l'application métier). Pris en compte au prochain démarrage de l'app service.

```json
{ "port": 9090 }
```

Réponse :

```json
{ "status": "saved", "port": 9090 }
```

Contrainte : `1024 ≤ port ≤ 65535`. Valeur persistée en NVS, active au reboot.

### `POST /v1alpha1/tls/cert`

Pousse un certificat TLS et sa clé privée PEM pour le serveur HTTPS de l'agent.
Permet la rotation de certificat sans cycle OTA. Effectif au prochain
`embewi_http_start()` (i.e. après reboot).

```json
{
  "cert_pem": "-----BEGIN CERTIFICATE-----\n...",
  "key_pem":  "-----BEGIN EC PRIVATE KEY-----\n..."
}
```

Réponse :

```json
{ "status": "saved" }
```

Fallback : si aucun certificat n'a été poussé, l'agent utilise un certificat
auto-signé EC P-256 généré au build (non secret, pour démarrage sans provisioning
TLS préalable).

### `POST /v1alpha1/token`

Rotation du token Bearer par node **sans repasser par le portail captif**
(qui exige un accès physique). Authentifié avec le token **courant** ; en cas de
succès, le nouveau token remplace l'ancien en NVS et devient seul valide.

```json
{ "token": "<nouveau-token>" }
```

Réponse :

```json
{ "status": "rotated" }
```

Protocole de rotation sans coupure (responsabilité Core) :

```text
1. Core génère newToken, met à jour le McuSecret.
2. Core: POST /token (Authorization: Bearer <oldToken>) {"token":"<newToken>"}
3. device persiste newToken, répond 200, puis n'accepte plus que newToken.
4. Core bascule sur newToken pour tous les appels suivants.
```

Contrainte : `8 ≤ len(token) ≤ 64`. Atomicité : l'écriture NVS est commitée
**avant** la réponse ; si le commit échoue, l'ancien token reste actif et la
réponse est `500` (`{"error":"nvs_write_failed"}`) — pas de fenêtre où aucun
token n'est valide. Un token vide (`""`) est **refusé** (`400`) : on ne
désactive pas l'auth par rotation (utiliser le portail pour un reset complet).

> **[RÉSERVE]** Rotation à double token (overlap window où ancien ET nouveau
> sont acceptés) pour tolérer un crash Core entre l'étape 3 et 4. Hors MVP : le
> Core ré-applique simplement la rotation si le heartbeat reste authentifié à
> l'ancien token au-delà d'un délai.

### `POST /v1alpha1/reboot`

Déclenche un reboot contrôlé, authentifié comme tous les endpoints inbound.
Le Core l'utilise pour appliquer un `POST /config` sans cycle OTA complet.
Si un OTA est planifié dans la même réconciliation, **ne pas appeler `/reboot`
séparément** : `POST /ota/activate` couvre déjà le reboot.

```json
{}
```

Réponse avant reboot :

```json
{ "status": "rebooting" }
```

---

## 4a. Modèle de config en couches **[NORMATIF]**

```text
Priorité (haute → basse) :
  1. NVS runtime config  → poussé par Core via POST /config (McuConfigMap)
  2. Défaut build        → CONFIG_EMBEWI_* baked dans le binaire (Kconfig)
```

L'agent lit la config NVS **une seule fois au boot** (`embewi_app_init`).
Toute modification nécessite un reboot pour prendre effet — explicite via
`POST /reboot` ou implicite via `POST /ota/activate`.

**L'agent ne valide pas la sémantique des valeurs.** Il stocke et expose.
La validation (plage GPIO valide, fréquence cohérente…) est responsabilité
du Core au moment du push (cf. §9). Mécanisme recommandé : un
ValidatingAdmissionWebhook côté Core — voir `embewi-core-design.md` §2 (inclut
les limites de taille clé/valeur à faire respecter, couplées aux buffers agent).

Clés standardisées (convention inter-apps, non imposées par l'agent) :

| Clé           | Type | Description                                   |
|---------------|------|-----------------------------------------------|
| `gpio_button` | int  | GPIO du bouton de test (BOOT sur eval boards) |
| `gpio_ws2812` | int  | GPIO de la LED WS2812B (app rainbow)          |
| `ntp_server`  | str  | serveur NTP (défaut `pool.ntp.org` ; DHCP aussi utilisé) |

Clés spécifiques à une app : documentées dans le `McuConfigMap` du
déploiement, opaques pour l'agent.

---

## 4b. Vocabulaire des codes d'erreur **[NORMATIF]**

Codes stables et exhaustifs émis par l'agent. Le Core **doit** les mapper en
Kubernetes Events / conditions plutôt que de parser des messages libres (qui
peuvent changer). Tout nouveau code passe par une révision de ce contrat.

**`reason` de `POST /ota/prepare`** (champ `reason`, HTTP 200, `accepted:false`) :

| `reason`           | Cause                                              | Event Core suggéré      |
|--------------------|----------------------------------------------------|-------------------------|
| `chip_mismatch`    | binaire pour un autre SoC que celui du device      | `OTARejectedChip`       |
| `layout_mismatch`  | `partition_layout` incompatible                    | `OTARejectedLayout`     |
| `idf_incompatible` | version IDF du binaire non supportée               | `OTARejectedIdf`        |
| `size_too_large`   | image > taille du slot inactif                     | `OTARejectedSize`       |
| `busy`             | un OTA est déjà en cours (slot réservé)            | `OTABusy`               |

**`status` de `PUT /ota/write`** (HTTP 200 sauf mention) :

| `status`/`error`  | Cause                                          | Event Core suggéré    |
|-------------------|------------------------------------------------|-----------------------|
| `written`         | succès, digest vérifié                          | `OTAWritten`          |
| `partial`         | plage écrite, transfert non terminé (reprise)   | — (interne)           |
| `digest_mismatch` | SHA-256 écrit ≠ `X-Embewi-Digest`              | `OTADigestMismatch`   |
| `write_failed`    | échec flash (`esp_ota_write`)                   | `OTAWriteFailed`      |
| `ota_begin_failed`| `esp_ota_begin` KO (HTTP 500)                   | `OTABeginFailed`      |
| `range_mismatch`  | offset Content-Range ≠ offset device (HTTP 416) | — (resync, attendu)   |
| `bad_content_range`| header Content-Range malformé (HTTP 400)       | `OTABadRange`         |

**Erreurs de `POST /config`** (champ `error`) :

| `error`              | HTTP | Cause                                    |
|----------------------|------|------------------------------------------|
| `missing_data_field` | 400  | objet `data` absent du corps             |
| `nvs_write_failed`   | 500  | échec d'écriture NVS                      |

**Erreurs transverses** (tout endpoint inbound) :

| `error`          | HTTP | Cause                                        |
|------------------|------|----------------------------------------------|
| `unauthorized`   | 401  | token absent / invalide (§1)                 |
| `body_too_large` | 413  | corps de requête > limite                     |
| `nvs_write_failed` | 500 | échec NVS (rotation token, port, cert…)      |

Règle : un refus métier légitime (`prepare` rejeté, `digest_mismatch`) répond
**HTTP 200** avec le code dans le corps — ce n'est pas une erreur de transport.
Les `4xx/5xx` sont réservés aux erreurs de protocole/ressource.

---

## 5. Flux sortants (ESP → Core / collector) **[NORMATIF]**

### Heartbeat

Device-initiated (compatible avec un device qui n'accepte pas d'inbound).
Transport : HTTPS — le scheme est forcé en `https://` indépendamment du scheme
stocké dans `ctrl_url` (contrat §1).

```json
{
  "node_id": "embewi-a1b2c3",
  "ts": 1710000000,
  "state": "running",
  "deployment_id": "wheel-controller-1.1.0",
  "firmware_digest": "sha256:...",
  "ota_validated": true,
  "uptime_ms": 120034,
  "heap_free": 82344,
  "rssi": -61,
  "config_generation": 2,
  "temp_celsius": 41.2,
  "task_hwm_min": 1536
}
```

`ota_validated` distingue `pending_verify` (false) de `running` (true) même si
le digest est déjà celui de la nouvelle image.

**Horloge / `ts` (NORMATIF).** `ts` est un **epoch UNIX en secondes, UTC**,
synchronisé par **SNTP** au boot (serveur via la clé config `ntp_server` + NTP
DHCP). Tant que la synchro n'a pas eu lieu, l'horloge ESP démarre à 1970 et `ts`
ne vaut que l'uptime (petite valeur) — le Core détecte ce cas par `ts` très
faible. **Dépendance prod** : la vérification du certificat du Core (TLS sortant
authentifié, §1) contrôle les dates de validité du cert ; sans horloge juste, le
handshake TLS échoue. L'agent synchronise donc l'heure **avant** d'ouvrir ses
flux sortants. NTP injoignable en prod = pas de heartbeat/log TLS tant que
l'heure n'est pas posée (fail-closed assumé).

**Schéma extensible (télémétrie → métriques).** Le heartbeat est un objet JSON
**ouvert** : le Core doit ignorer les champs inconnus (forward-compatibility).
Les champs de télémétrie au-delà du minimum requis sont **optionnels** et
exposables côté Core en métriques (`kubectl top mcunode`, Prometheus).

Champs **requis** : `node_id`, `ts`, `state`, `ota_validated`,
`config_generation`. Tout le reste est best-effort.

| Champ              | Statut    | Métrique Core suggérée            |
|--------------------|-----------|-----------------------------------|
| `heap_free`        | présent   | `mcunode_heap_free_bytes`         |
| `rssi`             | présent   | `mcunode_wifi_rssi_dbm`           |
| `uptime_ms`        | présent   | `mcunode_uptime_seconds`          |
| `temp_celsius`     | présent   | `mcunode_temperature_celsius`     |
| `task_hwm_min`     | présent   | `mcunode_task_stack_hwm_bytes`    |
| `flash_wear_pct`   | réservé   | `mcunode_flash_wear_ratio`        |

`temp_celsius` : capteur interne du SoC, **°C**. Sentinelle **`-127.0`** si le
capteur est indisponible (le Core doit filtrer cette valeur).

`task_hwm_min` : plus petit high-water-mark de stack restant, en **octets** (plus
c'est proche de 0, plus une task approche de l'overflow). Min sur toutes les tasks
si `CONFIG_FREERTOS_USE_TRACE_FACILITY` est actif, sinon HWM de la task heartbeat.

`flash_wear_pct` reste **réservé** : pas d'API ESP-IDF pour lire l'usure des
partitions OTA/app. Le nom est figé pour le jour où une mesure existera.

### Logs

Une ligne JSON par message :

```json
{
  "ts": 1710000000,
  "node": "embewi-a1b2c3",
  "workload": "wheel-controller",
  "level": "info",
  "msg": "control loop started"
}
```

Flux MVP : `ESP → HTTPS POST → Core /v1alpha1/logs`.

Transport : le scheme est forcé en `https://` côté agent quel que soit le
scheme provisionné en NVS — un `ctrl_url` en `http://` est une erreur de
configuration, non une option. Même politique que le heartbeat.

### Streaming de logs (WebSocket — outbound) **[NORMATIF]**

L'agent ouvre une connexion WebSocket **cliente** vers
`wss://ctrl_url_host:port/v1alpha1/logs` — le scheme est toujours `wss://`
(contrat §1 : transport chiffré obligatoire), quel que soit le scheme de `ctrl_url`. Tous les messages `ESP_LOGx` — composants IDF, WiFi, OTA, code
applicatif, bibliothèques tierces — sont capturés via `esp_log_set_vprintf` et
streamés en temps réel sans modification des appels existants.

Format par frame WebSocket (JSON, `level="raw"` — parsing du préfixe IDF hors
MVP) :

```json
{
  "ts": 1719392051,
  "node": "embewi-a1b2c3",
  "workload": "wheel-controller",
  "level": "raw",
  "msg": "I (10352) embewi.ota: write OK 983040 octets slot=ota_1"
}
```

Garantie de livraison : **best-effort, sans buffering inter-reconnexion**. Un
ring buffer 4 KB (~25 lignes) absorbe les rafales tant que le WS est connecté.
Si le buffer sature alors que le WS est connecté, les nouveaux messages sont
silencieusement abandonnés — **on préfère perdre des logs plutôt que bloquer le
système**.

En cas de **déconnexion WS**, les messages émis pendant la coupure sont
**perdus** (la tâche de drain continue de vider le buffer et les jette). Choix
assumé pour l'observabilité : à la reconnexion, on diffuse les logs **récents**
plutôt qu'un vieux backlog accumulé. Les logs critiques OTA/lifecycle passent de
toute façon par `embewi_log_emit()` (HTTP POST, §5 « Logs »), pas par ce canal.

Connexion **device-initiated** (outbound, compatible NAT). Complémentaire à
`embewi_log_emit()` qui reste utilisé pour les événements OTA/lifecycle via
HTTP POST (envoi immédiat critique).

---

## 6. Idempotence et reprise **[NORMATIF]**

Le reconcile du Core **doit** être reprenable. Scénario garanti : le Core crashe
entre `/ota/write` et `/ota/activate`.

```text
Au redémarrage, le Core lit GET /info :
  staged.state == "none"
     → repartir de /ota/prepare
  staged.state == "written" ET staged.digest == digest désiré
     → SAUTER write, aller direct à /ota/activate
  staged.state == "written" ET staged.digest != digest désiré
     → ré-préparer + ré-écrire (slot stagé périmé)
  staged.state == "activating"
     → attendre le heartbeat ; appliquer le timeout négatif (§3)
```

Sans le champ `staged`, le Core re-transfère 1 MB inutilement à chaque reprise.

**Idempotence config :**

```text
Au redémarrage, le Core lit GET /config :
  nvs == desired config ET active_generation == generation
     → config à jour et active : rien à faire
  nvs != desired config
     → POST /config (push)
     → si firmware aussi à changer → OTA (reboot inclus)
     → sinon → POST /reboot
  nvs == desired config ET active_generation < generation
     → config poussée mais device pas encore rebooté
     → POST /reboot (ou attendre le prochain OTA)
```

Config et OTA peuvent coexister dans la même réconciliation. Ordre canonique :
**POST /config d'abord, OTA ensuite** — un seul reboot couvre les deux.

---

## 7. Politique de binding **[NORMATIF]**

Un `McuDeployment` est lié à un **device physique** (le `wheel-controller` est
câblé à un moteur précis). Le placement n'est pas du scheduling fongible : c'est
une **résolution d'affinité matérielle**.

```text
1 McuDeployment doit résoudre EXACTEMENT 1 McuNode.
0 match           → erreur (NoDeviceMatched)
>1 match          → erreur (AmbiguousBinding)   ← pas du load-balancing : un bug
node déjà occupé  → erreur (DeviceBusy)         ← contrainte 1 node = 1 workload
```

On privilégie le pin explicite :

```yaml
spec:
  nodeName: embewi-a1b2c3
```

plutôt que le sélecteur qui peut matcher plusieurs devices :

```yaml
spec:
  nodeSelector:
    role: motor          # toléré seulement s'il résout à exactement 1 node
```

Conflit (deux McuDeployment visant le même ESP) : **first-bound wins**, le second
part en erreur `DeviceBusy`. Pas de préemption en MVP.

> **Orchestration de flotte.** La mise à jour coordonnée de N devices identiques
> (rolling/canary, rollback sur taux d'échec) est portée par un
> `McuDeploymentSet` **purement côté Core** — voir `embewi-core-design.md` §1.
> Cet objet génère des `McuDeployment` unitaires et n'ajoute aucune surface
> côté agent.

---

## 7a. McuConfigMap **[NORMATIF]**

Ressource Kubernetes portée par le Core, indépendante du binaire. Découple
le câblage matériel (GPIOs, fréquences, adresses I²C…) de l'image OTA.

```yaml
apiVersion: embewi.io/v1alpha1
kind: McuConfigMap
metadata:
  name: wheel-left-gpio
data:
  gpio_button: "9"
  gpio_ws2812: "10"
  # clés arbitraires — opaques pour l'agent, interprétées par l'app
```

Référence depuis un `McuDeployment` (optionnel — absent = défauts build) :

```yaml
apiVersion: embewi.io/v1alpha1
kind: McuDeployment
spec:
  nodeName: embewi-a1b2c3
  firmware: registry.local/embewi/wheel-controller:v1.1.0
  configMapRef: wheel-left-gpio
```

**Règles de binding McuConfigMap → device :**

```text
configMapRef absent        → aucun push config ; défauts build actifs
configMapRef présent       → le Core réconcilie McuConfigMap.data vs GET /config
McuConfigMap inexistant    → erreur ConfigMapNotFound (bloque le déploiement)
```

Un `McuConfigMap` peut être partagé entre plusieurs `McuDeployment` ciblant des
devices **identiquement câblés**. La modification du `McuConfigMap` déclenche
une réconciliation sur tous les devices qui y référencent.

---

## 8. Effets Kubernetes **[NORMATIF]**

Service **selectorless** + EndpointSlice géré à la main par le contrôleur.
L'endpoint pointe **directement sur l'IP de management de l'ESP** (pas de Pod IP
logique en MVP — elle reviendrait seulement avec un vrai proxy/NAT).

```yaml
endpoints:
  - addresses: ["192.168.10.42"]
    conditions:
      ready: true        # piloté par le heartbeat ET ota_validated, jamais statique
ports:
  - port: 8080
```

Pilotage de `ready` :

```text
heartbeat OK + state=running + ota_validated=true  → ready=true
state=pending_verify                               → ready=false (image pas figée)
heartbeat perdu (> seuil)                          → ready=false
state ∈ {degraded selon politique, rollback, failed}→ ready=false
```

Le Core **ne marque jamais** un déploiement `Ready` avant la confirmation agent
(`state=running` + `ota_validated=true` + bon `deployment_id`). Le healthcheck
devient routage, gratuitement.

---

## 9. Découpage des responsabilités **[NORMATIF]**

```text
MCU Runtime Core                     ESP Agent
─────────────────                    ─────────
pull ORAS                            receive bytes (auth)
verify signature (efficacité)        recompute + verify digest écrit
verify digest                        write OTA partition (slot inactif)
verify chip / layout compat          set_boot_partition + reboot
stream .bin brut (HTTP)              self-check borné watchdog
gère idempotence (staged)            mark_valid / mark_invalid_rollback
timeout négatif → Failed             heartbeat + logs sortants (config_generation inclus)
pilote EndpointSlice ready           (ne connaît ni OCI ni Kubernetes)
réconcilie McuConfigMap → POST /config  stocke clés/valeurs en NVS (opaque)
valide sémantique des valeurs config    applique au boot suivant (§4a)
gère McuNode + enrôlement (token ref)   s'auto-annonce (node_id) au heartbeat (§1a)
rotation McuSecret → POST /token        persiste le nouveau token, atomique (§4)
mappe reason/error → Events K8s         émet des codes stables (§4b)
expose télémétrie heartbeat → métriques émet un heartbeat extensible (§5)
```

Le device reste bête. Toute la complexité « cloud native » (OCI, ORAS, K8s,
confiance amont) reste côté Core. C'est le principe de façade appliqué au plan
de données.

---

## 10. Ordre de réalisation (rappel)

```text
0. Nom projet : Embewi                          ✔ figé
1. Contrat Core ↔ Agent v2                       ✔ CE DOCUMENT
2. Machine d'état OTA sécurisée (§2 §3)          ✔ implémenté
3. Partition table 4 MB / 8 MB                   ✔ implémenté
4. Squelette ESP-IDF                             ✔ implémenté
5. McuConfigMap — config runtime (§4a §7a)        ✔ implémenté
   5a. Agent : embewi_config.c + GET/POST /config + POST /reboot  ✔
   5b. Agent : apps lisent NVS au boot (gpio_button, gpio_ws2812)  ✔
   5c. Core  : McuConfigMap CRD + réconciliation                  ← prochaine étape
6. Streaming logs ESP_LOGx → WebSocket /v1alpha1/logs            ✔ implémenté
7. Enrôlement + identité (§1a)                                   ✔ provisioning fait
                                                                   (modèle Core à figer)
8. Rotation de token POST /token (§4)                            ✔ implémenté
9. Vocabulaire reason/error (§4b)                                ✔ codes déjà émis
10. Heartbeat extensible / métriques (§5)                        ✔ temp + task_hwm
                                                                    émis ; flash_wear réservé
11. OTA write reprenable — Content-Range (§4)                    ✔ in-session
12. Horloge NTP/SNTP → ts epoch UTC (§5)                         ✔ implémenté
13. Durcissement sécurité (embewi-prod-security.md) :
    - Secure Boot v2 + Flash Enc + NVS flash-enc (profil prod)   ✔ build-validé
    - portail provisioning HTTPS + fenêtre AP bornée (§1a)       ✔ implémenté
    - vérif cert Core sortant + token constant-time (§1)         ✔ implémenté
14. Runtime Core minimal                                         ← prochaine étape
```

> **Côté agent : tout le contrat MVP est implémenté.** Les sections restantes
> (`McuConfigMap` CRD, `McuDeploymentSet`, webhook de validation, runtime Core)
> sont portées par le repo Core — voir `embewi-core-design.md`.

---

## Annexe — Hors MVP **[RÉSERVE]**

```text
- OTA pull (device-initiated) pour devices NATés
- write reprenable INTER-REBOOT (in-session via Content-Range : ✔ implémenté §4)
- Pod IP logique + dataplane proxy/NAT
- multi-workload par ESP
- préemption sur conflit de binding
- flash encryption (si pas activé dès le MVP)
- Virtual Kubelet provider (exposer les ESP comme vrais Nodes)
- DELETE /config/{key} (reset clé individuelle au défaut build)
- hot-reload config sans reboot (clés ne nécessitant pas de re-init hardware)
- McuConfigMap versionné (rollback config indépendant du rollback firmware)
```
