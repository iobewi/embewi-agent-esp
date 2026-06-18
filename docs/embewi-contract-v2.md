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
  "state": "running"
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

> **[RÉSERVE]** Write chunké reprenable (`Content-Range` + offset). Hors MVP : un
> `PUT` monolithique n'est pas reprenable, une coupure wifi à 90 % refait tout.
> À ajouter avant tout déploiement sur réseau instable.

### `POST /v1alpha1/ota/activate`

L'ESP configure le prochain boot (`esp_ota_set_boot_partition`) et redémarre.

```json
{ "deployment_id": "wheel-controller-1.1.0", "reboot": true }
```

Réponse avant reboot :

```json
{ "status": "rebooting", "target_slot": "ota_1" }
```

---

## 5. Flux sortants (ESP → Core / collector) **[NORMATIF]**

### Heartbeat

Device-initiated (compatible avec un device qui n'accepte pas d'inbound).

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
  "rssi": -61
}
```

`ota_validated` distingue `pending_verify` (false) de `running` (true) même si
le digest est déjà celui de la nouvelle image.

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

Flux MVP : `ESP → TCP/UDP collector → Vector/Loki`.

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
timeout négatif → Failed             heartbeat + logs sortants
pilote EndpointSlice ready           (ne connaît ni OCI ni Kubernetes)
```

Le device reste bête. Toute la complexité « cloud native » (OCI, ORAS, K8s,
confiance amont) reste côté Core. C'est le principe de façade appliqué au plan
de données.

---

## 10. Ordre de réalisation (rappel)

```text
0. Nom projet : Embewi                          ✔ figé
1. Contrat Core ↔ Agent v2                       ← CE DOCUMENT
2. Machine d'état OTA sécurisée (§2 §3)           prochaine étape code
3. Partition table 4 MB / 8 MB
4. Squelette ESP-IDF
5. Runtime Core minimal
```

---

## Annexe — Hors MVP **[RÉSERVE]**

```text
- OTA pull (device-initiated) pour devices NATés
- write chunké reprenable (Content-Range)
- Pod IP logique + dataplane proxy/NAT
- multi-workload par ESP
- préemption sur conflit de binding
- flash encryption (si pas activé dès le MVP)
- Virtual Kubelet provider (exposer les ESP comme vrais Nodes)
```
