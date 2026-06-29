# Workloads

Un **workload** est le code métier embarqué dans l'agent. Il est compilé
statiquement avec le firmware et sélectionné au build. Le binaire résultant
est le **même artefact** que celui poussé en OTA par le Core.

## Interface à implémenter

Toute app workload doit implémenter les quatre fonctions déclarées dans
`main/embewi_app.h` :

```c
void embewi_app_init(void);
bool embewi_app_selfcheck(void);
void embewi_app_service_start(uint16_t port);
void embewi_app_service_stop(void);
```

| Fonction | Appelée par | Rôle |
|---|---|---|
| `embewi_app_init` | `app_main` au boot, avant le self-check | Init hardware, lecture config NVS |
| `embewi_app_selfcheck` | `selfcheck_task` pendant `pending_verify` | Retourne `true` si l'app est saine |
| `embewi_app_service_start(port)` | `app_main` après NTP | Démarre le service sur `port` |
| `embewi_app_service_stop` | `app_main` sur changement de port | Arrête le service |

### Contrainte sur `embewi_app_init`

**Interdit d'écrire dans le namespace `embewi_cfg` depuis `embewi_app_init`.**
Le snapshot `active` de `GET /config` est figé **avant** cet appel
(`embewi_cfg_boot_init` en tête de boot). Toute écriture NVS ici serait
invisible dans `active` mais visible dans `nvs`, créant une divergence trompeuse
que le Core interpréterait comme une config en attente de reboot.

Pour lire la config courante :

```c
void embewi_app_init(void) {
    int gpio = embewi_cfg_get_int("gpio_button", CONFIG_EMBEWI_BUTTON_GPIO);
    // ...
}
```

### Selfcheck

`embewi_app_selfcheck()` est appelé pendant la fenêtre de **15 secondes** qui
suit un boot post-OTA. Si elle retourne `false` (ou si la fenêtre expire),
le bootloader rollback sur l'image précédente.

Règle : ne tester que ce qui peut réellement planter le service — driver
initialisé, peripheral répondant. Pas de logique applicative.

---

## Workloads fournis

Les deux workloads dans `apps/` servent de **référence d'implémentation**.

### `apps/button/main.c` — compteur bouton BOOT

- GPIO configuré via McuConfigMap `gpio_button` (défaut : `CONFIG_EMBEWI_BUTTON_GPIO`)
- Tâche polling 50 ms, incrémente un compteur sur front descendant
- Service HTTP sur le port app : `GET /sensors` → `{"counter": N}`
- Selfcheck : `gpio_get_level(s_gpio) == 1` (bouton relâché = hardware OK)

### `apps/rainbow/main.c` — LED WS2812B arc-en-ciel

- GPIO configuré via McuConfigMap `gpio_ws2812` (défaut : `CONFIG_EMBEWI_WS2812_GPIO`)
- Pilotage RMT TX à 10 MHz (100 ns/tick), protocole WS2812B ordre GRB, MSB en premier
- Cycle HSV 0-255 toutes les ~5 s, luminosité limitée à 60/255
- Service HTTP sur le port app : `GET /status` → `{"running": true|false}`
- Selfcheck : canal RMT et encoder initialisés (`s_chan != NULL && s_encoder != NULL`)

---

## Créer son workload

1. Créer le dossier `apps/<mon-app>/` et y écrire `main.c`.
2. Implémenter les 4 fonctions de `embewi_app.h`.
3. Dans `main/CMakeLists.txt`, déclarer le nouveau nom et pointer vers le fichier :

```cmake
set(EMBEWI_APP "mon-app")

if(EMBEWI_APP STREQUAL "rainbow")
    set(EMBEWI_APP_SRC "../apps/rainbow/main.c")
elseif(EMBEWI_APP STREQUAL "mon-app")
    set(EMBEWI_APP_SRC "../apps/mon-app/main.c")
else()
    set(EMBEWI_APP_SRC "../apps/button/main.c")
endif()
```

Si le workload a plusieurs fichiers sources, les lister tous dans `EMBEWI_APP_SRC`
(liste CMake séparée par `;`) ou ajouter directement dans `SRCS` de
`idf_component_register`.

Les headers de `main/` (`embewi_app.h`, `embewi_agent.h`) sont accessibles
sans chemin supplémentaire — `INCLUDE_DIRS "."` du composant couvre `main/`.

---

## Construire l'artefact OTA

L'artefact OTA est le **binaire complet** produit par `idf.py build` — firmware
agent + workload statiquement liés. C'est exactement ce que le Core pousse via
`PUT /ota/write`.

### Build dev (non signé)

```bash
# 1. Sélectionner la cible SoC (une seule fois par workspace)
idf.py set-target esp32c3

# 2. Sélectionner le workload dans main/CMakeLists.txt
#    set(EMBEWI_APP "button")   # ou "rainbow" ou "mon-app"

# 3. Compiler
idf.py build
```

L'artefact est `build/embewi_agent.bin`.

### Extraire taille et digest

Le Core a besoin de ces deux valeurs pour `POST /ota/prepare` :

```bash
# Taille en octets
wc -c < build/embewi_agent.bin

# Digest SHA-256 (format attendu par /ota/prepare : "sha256:<hex>")
sha256sum build/embewi_agent.bin | awk '{print "sha256:" $1}'
```

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

### Build prod (signé Secure Boot v2)

Avec le profil prod, le binaire est **signé au build**. Le digest doit être
calculé sur le binaire signé (pas sur le binaire brut).

```bash
# Build prod (isole les artefacts dans build-prod/)
idf.py -B build-prod \
       -DSDKCONFIG=build-prod/sdkconfig.prod \
       -DSDKCONFIG_DEFAULTS="sdkconfig.defaults;sdkconfig.defaults.prod" \
       build

# Artefact signé
ls -la build-prod/embewi_agent.bin

# Taille et digest du binaire SIGNÉ
wc -c < build-prod/embewi_agent.bin
sha256sum build-prod/embewi_agent.bin | awk '{print "sha256:" $1}'
```

> Le binaire signé est légèrement plus grand que le binaire brut (signature
> RSA-3072 annexée). Toujours calculer `size` et `digest` sur l'artefact
> **effectivement transmis** au Core, pas sur le binaire intermédiaire.

### `fw_size` — rôle à l'initialisation

Au premier flash (hors OTA), l'agent stocke la taille du binaire en NVS
(`fw_size`). Au boot, il recalcule le SHA-256 de la flash sur exactement
`fw_size` octets (sans padding `0xFF`). Si ce digest correspond à celui de
l'image OTA reçue, `GET /info` expose `firmware.digest` avec la bonne valeur.

En pratique : ne pas chercher à pré-renseigner `fw_size` manuellement — le
premier flash via `idf.py flash` l'initialise automatiquement.

---

## Séquence complète Core → device

```text
1. CI produit build/embewi_agent.bin   (workload compilé, signé si prod)
2. CI calcule size + digest
3. Core : POST /ota/prepare  {deployment_id, chip, size, digest, ...}
          ← {accepted: true, target_slot: "ota_1"}
4. Core : PUT  /ota/write    <binaire brut>  X-Embewi-Digest: sha256:...
          ← {written: N, digest: "sha256:...", status: "written"}
5. Core : POST /ota/activate {deployment_id}
          ← {status: "rebooting", target_slot: "ota_1"}
6. Agent reboot → pending_verify → self-check 15 s
7. OK  → mark_valid, heartbeat state=running
   KO  → rollback, heartbeat state=rollback
```
