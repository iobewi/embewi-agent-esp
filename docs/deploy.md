# Déployer un workload

Cette page couvre la construction de l'artefact OTA et son déploiement sur
les devices via le Core Kubernetes. Pour écrire le code du workload, voir
[Développer un workload](workload).

---

## Construire l'artefact OTA

L'artefact OTA est le binaire complet produit par `idf.py build` — firmware
agent + workload statiquement liés. C'est exactement ce que le Core pousse via
`PUT /ota/write`.

### Sélectionner le workload

Dans `main/CMakeLists.txt` :

```cmake
set(EMBEWI_APP "mon-app")   # ← changer ici
```

### Build dev (non signé)

```bash
# 1. Cible SoC (une fois par workspace)
idf.py set-target esp32c3

# 2. Compiler
idf.py build
# → build/embewi_agent.bin
```

### Build prod (signé Secure Boot v2)

```bash
idf.py -B build-prod \
       -DSDKCONFIG=build-prod/sdkconfig.prod \
       -DSDKCONFIG_DEFAULTS="sdkconfig.defaults;sdkconfig.defaults.prod" \
       build
# → build-prod/embewi_agent.bin  (signé RSA-3072)
```

> Voir [Sécurité production](embewi-prod-security) pour la gestion des clés
> de signature et la procédure de premier flash.

---

## Extraire taille et digest

Le Core a besoin de ces deux valeurs pour `POST /ota/prepare` :

```bash
# Sur le binaire dev
wc -c < build/embewi_agent.bin
sha256sum build/embewi_agent.bin | awk '{print "sha256:" $1}'

# Sur le binaire prod signé
wc -c < build-prod/embewi_agent.bin
sha256sum build-prod/embewi_agent.bin | awk '{print "sha256:" $1}'
```

> Toujours calculer `size` et `digest` sur l'artefact **effectivement transmis**
> au Core. Le binaire signé est plus grand que le brut (signature RSA-3072
> annexée) — leurs digests diffèrent.

Corps de `POST /ota/prepare` :

```json
{
  "deployment_id": "mon-app-1.2.0",
  "chip": "esp32c3",
  "idf_version": "v5.3",
  "partition_layout": "embewi-ab-v1",
  "size": 983040,
  "digest": "sha256:abc123..."
}
```

---

## McuConfigMap — préparer la configuration

Avant de déployer, créer (ou mettre à jour) le `McuConfigMap` avec les clés
nécessaires au workload :

```yaml
apiVersion: embewi.io/v1alpha1
kind: McuConfigMap
metadata:
  name: mon-app-config
data:
  gpio_button: "9"
  vitesse_max: "120"
```

Le référencer dans le `McuDeployment` :

```yaml
apiVersion: embewi.io/v1alpha1
kind: McuDeployment
spec:
  nodeName: embewi-a1b2c3
  firmware: registry.local/embewi/mon-app:v1.2.0
  configMapRef: mon-app-config
```

Le Core réconciliera automatiquement `McuConfigMap.data` contre l'état NVS du
device via `POST /config`, puis déclenchera un reboot si nécessaire.

---

## Séquence complète Core → device

```text
1. CI  → idf.py build  →  embewi_agent.bin  (workload inclus, signé si prod)
2. CI  → size + digest calculés et publiés avec l'artefact
3. Core : POST /ota/prepare  {deployment_id, chip, size, digest, ...}
          ← {accepted: true, target_slot: "ota_1"}
4. Core : PUT  /ota/write    <binaire>   X-Embewi-Digest: sha256:...
          ← {written: N, digest: "sha256:...", status: "written"}
5. Core : POST /ota/activate {deployment_id}
          ← {status: "rebooting", target_slot: "ota_1"}
6. Agent : reboot → pending_verify
           embewi_app_init()
           embewi_app_selfcheck()  × N  (fenêtre 15 s)
7. OK  → mark_valid, heartbeat state=running, EndpointSlice ready=true
   KO  → rollback bootloader, heartbeat state=rollback
```

En cas d'interruption réseau entre les étapes, le Core reprend le reconcile
depuis `GET /info` (`staged.state` + `staged.deployment_id`). Voir
[API inbound](api) pour la logique de reprise `Content-Range`.
