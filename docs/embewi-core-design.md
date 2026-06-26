# Embewi — Notes de conception Core (hors frontière agent)

> **Portée.** Ce document décrit des fonctionnalités **purement côté Core**
> (runtime Kubernetes, repo séparé). Elles ne touchent **pas** le contrat
> Core ↔ Agent (`embewi-contract-v2.md`) : l'agent embarqué les ignore. Elles
> sont documentées ici pour la continuité de conception et pour être reprises
> dans le repo Core.
>
> Convention : **[CORE-INTERNE]** = design interne au Core, pas une contrainte
> de la frontière. Les références `§N` pointent vers `embewi-contract-v2.md`.

---

## 1. Rollout de flotte — `McuDeploymentSet` **[CORE-INTERNE]**

### Problème

Le contrat fige `1 McuDeployment = 1 device physique` (§7, affinité matérielle).
C'est correct au niveau atomique, mais une flotte de N devices **identiquement
câblés** (ex. 50 `wheel-controller`) ne peut pas se mettre à jour device par
device à la main : il faut un objet d'orchestration au-dessus, façon
`DaemonSet`/`Deployment`, qui génère et pilote les `McuDeployment` unitaires.

### Ressource

```yaml
apiVersion: embewi.io/v1alpha1
kind: McuDeploymentSet
metadata:
  name: wheel-controllers
spec:
  selector:
    matchLabels:
      role: wheel-controller        # matche des McuNode (pas du scheduling : filtrage)
  firmware: registry.local/embewi/wheel-controller:v1.2.0
  configMapRef: wheel-gpio          # optionnel (§7a)
  strategy:
    type: RollingUpdate             # RollingUpdate | Canary
    rollingUpdate:
      maxUnavailable: 20%           # devices simultanément hors-ready tolérés
      maxSurge: 0                   # toujours 0 : pas de device "en plus" (HW fixe)
    canary:
      steps:                        # utilisé si type=Canary
        - setWeight: 1              # 1 device d'abord (nombre absolu OU %)
          pause: { duration: 5m }   # observation avant d'élargir
        - setWeight: 25%
          pause: { duration: 10m }
        - setWeight: 100%
  rollback:
    autoOnFailureRate: 30%          # si >30% des devices du step échouent → halt + rollback
    failureWindow: 10m
status:
  desiredReplicas: 50
  updatedReplicas: 12
  readyReplicas: 11
  unavailableReplicas: 1
  phase: Progressing                # Progressing | Paused | Healthy | Degraded | RollingBack
  currentStep: 1
  conditions: [...]
```

`maxSurge` est **toujours 0** : contrairement aux Pods, on ne peut pas créer un
device « en plus » pour absorber la transition — le matériel est fixe. Le rollout
joue donc uniquement sur `maxUnavailable`.

### Boucle d'orchestration

Le contrôleur `McuDeploymentSet` **ne parle jamais au device** : il manipule des
`McuDeployment` et lit leur statut, qui dérive lui-même des signaux agent du
contrat (§3, §8).

```text
reconcile(set):
  nodes      = listMcuNodes(set.spec.selector)
  desired    = set.spec.firmware (+ configMapRef)
  batch      = selectNextBatch(nodes, strategy)   # respecte maxUnavailable / step

  for node in batch:
    ensureMcuDeployment(node, desired)   # crée/MAJ le McuDeployment unitaire

  # avancement = somme des McuDeployment unitaires
  ready   = count(dep.status == Ready && dep.digest == desired.digest)
  failed  = count(dep.status == Failed)   # via timeout négatif §3 (Core CONSTATE)

  if failed / len(batch) > rollback.autoOnFailureRate:
      set.phase = RollingBack
      revertBatchToPreviousFirmware()     # ré-applique l'ancienne image
  elif ready == len(batch):
      advanceToNextStep()                 # ou Healthy si dernier step
```

### Réutilisation du contrat existant — rien à ajouter côté agent

| Besoin rollout                | Signal agent déjà au contrat            |
|-------------------------------|------------------------------------------|
| « le device a fini son OTA ?» | heartbeat `state=running` + `ota_validated=true` + `deployment_id` (§3, §5) |
| « l'OTA a échoué ?»           | timeout négatif → `Failed` (§3) ; le Core **constate**, l'agent a déjà rollback |
| « le device est routable ?»   | EndpointSlice `ready` piloté par heartbeat (§8) |
| « quelle version tourne ?»    | `firmware_digest` heartbeat + `GET /info` (§4, §5) |

Le rollback automatique de flotte = le Core arrête d'avancer et **ré-pousse
l'ancienne image** sur les devices déjà migrés. Ce n'est **pas** le rollback
bootloader (§3) : celui-là est local et instantané par device. Les deux
coexistent — bootloader = filet par device, `McuDeploymentSet` = politique de
flotte.

### Points de vigilance

- **Idempotence** (§6) : un device peut déjà être à la bonne version (re-réconcile
  après crash Core) → ne pas le recompter comme « en cours de mise à jour ».
- **Canary sur device unique** : un step `setWeight: 1` choisit **quel** device en
  premier — privilégier un device de test étiqueté (`canary: "true"`), pas un
  aléatoire en production.
- **Devices NATés** (annexe contrat, OTA pull) : un device injoignable en push ne
  peut pas être migré → le compter en `unavailable`, pas en `failed`.

---

## 2. Webhook de validation `McuConfigMap` **[CORE-INTERNE]**

### Pourquoi un webhook et pas le contrôleur

Le contrat §9 attribue au Core « valide la sémantique des valeurs config » et
§4a précise « l'agent ne valide pas, il stocke et expose ». Faire cette
validation dans le **contrôleur** de réconciliation est trop tard : une valeur
invalide est déjà persistée dans l'`etcd` et un `kubectl apply` réussit sans
erreur visible. Un **ValidatingAdmissionWebhook** rejette à l'admission — l'erreur
remonte directement à l'utilisateur de `kubectl`.

```text
kubectl apply McuConfigMap
      │
      ▼
ValidatingAdmissionWebhook   ← REJETTE ici si invalide (avant etcd)
      │ admis
      ▼
etcd → contrôleur réconcilie → POST /config (§4a)  ← ne voit que du valide
```

### Configuration

```yaml
apiVersion: admissionregistration.k8s.io/v1
kind: ValidatingWebhookConfiguration
metadata:
  name: embewi-configmap-validator
webhooks:
  - name: validate.mcuconfigmap.embewi.io
    rules:
      - apiGroups:   ["embewi.io"]
        apiVersions: ["v1alpha1"]
        operations:  ["CREATE", "UPDATE"]
        resources:   ["mcuconfigmaps"]
    failurePolicy: Fail            # config non validable = rejet (pas de bypass silencieux)
    sideEffects: None
    admissionReviewVersions: ["v1"]
    clientConfig:
      service: { name: embewi-webhook, namespace: embewi-system, path: /validate-configmap }
```

### Règles de validation

L'agent traite toutes les valeurs comme des **chaînes opaques** (§4a) ; c'est le
webhook qui porte la connaissance sémantique. Validation par profil de SoC, car
les GPIO valides dépendent de la cible :

```text
Clés standardisées (§4a) :
  gpio_button, gpio_ws2812 :
    - doivent parser en entier
    - dans la plage GPIO valide DU SoC cible du déploiement référent
      (ESP32-C3 : 0–21 sauf strapping ; ESP32 : 0–39 ; etc.)
    - pas un GPIO réservé (flash SPI : 6–11 sur la plupart des cibles)
    - pas de collision : gpio_button != gpio_ws2812

Clés app-spécifiques :
  - nom : [a-z0-9_]{1,31}  (l'agent tronque au-delà — cf. parser §4 POST /config)
  - valeur : longueur ≤ 63 octets (limite buffer agent)
  - le webhook ne connaît pas leur sémantique → valide seulement forme + taille
```

> **Dépendance au SoC cible.** La plage GPIO dépend du device visé. Le webhook
> doit résoudre `configMapRef` → `McuDeployment` → `McuNode.spec.chip` pour
> choisir le bon profil. Si le `McuConfigMap` est **partagé** entre plusieurs
> déploiements de SoC différents, valider contre **l'intersection** des plages
> (ou refuser le partage cross-SoC — décision Core).

### Limites volontaires (alignées sur les buffers agent)

Ces bornes ne sont pas arbitraires : elles reflètent les tailles de buffer de
l'agent (`embewi_http.c`, parser `json_data_iter`). Le webhook **doit** les faire
respecter sinon l'agent tronque silencieusement :

| Contrainte        | Limite | Origine                                  |
|-------------------|--------|------------------------------------------|
| Longueur clé      | 31     | `k[32]` dans le parser POST /config      |
| Longueur valeur   | 63     | `v[64]` dans le parser POST /config      |
| Taille du batch   | ~1 KB  | `BODY_MAX` du serveur HTTP de l'agent    |

> Tout changement de ces buffers côté agent **doit** être répercuté ici — c'est
> le seul couplage implicite entre ce design Core et le firmware. À surveiller en
> revue croisée des deux repos.

---

## Voir aussi

- `embewi-contract-v2.md` — contrat normatif de la frontière Core ↔ Agent.
- Réserve contrat (annexe) : `McuConfigMap` versionné, multi-workload, préemption,
  Virtual Kubelet provider — autant de features Core à détailler ici le moment venu.
