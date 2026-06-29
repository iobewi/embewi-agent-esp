# Développer un workload

Un **workload** est le code métier embarqué dans l'agent. Il est compilé
statiquement avec le firmware — le binaire résultant est l'artefact OTA que
le Core déploie sur le device.

L'agent fournit au workload :
- un **cycle de vie garanti** (4 fonctions appelées dans un ordre précis)
- un accès à la **config runtime** poussée par le Core (`McuConfigMap`)
- un **port TCP** alloué par le Core pour exposer un service applicatif

---

## Interface SDK (`main/embewi_app.h`)

Toute app workload implémente exactement ces quatre fonctions :

```c
void embewi_app_init(void);
bool embewi_app_selfcheck(void);
void embewi_app_service_start(uint16_t port);
void embewi_app_service_stop(void);
```

### Cycle de vie

```text
boot
 │
 ├─ embewi_cfg_boot_init()     ← snapshot config figé (AVANT app_init)
 │
 ├─ embewi_app_init()          ← lire config, init hardware
 │
 ├─ [OTA post-activate ?]
 │    └─ embewi_app_selfcheck()  × N  ← hardware OK ? (fenêtre 15 s)
 │
 ├─ embewi_app_service_start(port)  ← démarrer le service TCP
 │
 │   ... device en service ...
 │
 └─ embewi_app_service_stop()       ← sur changement de port (POST /app/port)
      embewi_app_service_start(nouveau_port)
```

| Fonction | Appelée | Contrainte |
|---|---|---|
| `embewi_app_init` | Une fois au boot, avant le self-check | Ne pas écrire dans `embewi_cfg` (voir ci-dessous) |
| `embewi_app_selfcheck` | Pendant `pending_verify`, appels répétés sur 15 s | Doit répondre vite — pas de TLS, pas de requêtes réseau |
| `embewi_app_service_start(port)` | Au boot après NTP, et après chaque `POST /app/port` | Le `port+1` est réservé au ctrl socket httpd — ne pas l'utiliser |
| `embewi_app_service_stop` | Avant chaque `service_start` sur nouveau port | Idempotente si le service n'était pas démarré |

---

## McuConfigMap — lecture depuis le workload

La config runtime est poussée par le Core via Kubernetes (`McuConfigMap`) et
stockée en NVS par l'agent. Le workload y accède via deux fonctions :

```c
// Lit une valeur string. Retourne false si la clé est absente ou vide.
bool embewi_cfg_get(const char *key, char *out, size_t len);

// Lit un entier. Retourne default_val si la clé est absente ou non parseable.
int  embewi_cfg_get_int(const char *key, int default_val);
```

### Règle fondamentale : lire dans `embewi_app_init`, jamais écrire

```c
void embewi_app_init(void) {
    // ✅ Lire la config au boot
    int gpio = embewi_cfg_get_int("gpio_button", CONFIG_EMBEWI_BUTTON_GPIO);

    char server[64] = "pool.ntp.org";
    embewi_cfg_get("mon_serveur", server, sizeof(server));

    // ❌ INTERDIT — écrire dans embewi_cfg ici
    // embewi_cfg_write("ma_cle", "valeur");  // ne pas faire
}
```

**Pourquoi l'écriture est interdite dans `embewi_app_init` :** le snapshot
`active` de `GET /config` est figé *avant* cet appel. Toute écriture NVS ici
serait invisible dans `active` mais visible dans `nvs`, créant une divergence
que le Core interpréterait comme une config en attente de reboot — alors que
l'app a déjà lu les valeurs incorrectes.

### Clés standardisées disponibles

| Clé | Fonction | Défaut build | Description |
|---|---|---|---|
| `gpio_button` | `embewi_cfg_get_int` | `CONFIG_EMBEWI_BUTTON_GPIO` | GPIO du bouton BOOT (9 sur C3/C6/H2, 0 sinon) |
| `gpio_ws2812` | `embewi_cfg_get_int` | `CONFIG_EMBEWI_WS2812_GPIO` | GPIO de la LED WS2812B (RMT TX) |
| `ntp_server` | `embewi_cfg_get` | `"pool.ntp.org"` | Serveur NTP (lu par l'agent, pas par le workload) |

### Ajouter des clés custom

Déclarer les clés dans le `McuConfigMap` Kubernetes du déploiement :

```yaml
apiVersion: embewi.io/v1alpha1
kind: McuConfigMap
metadata:
  name: mon-app-config
data:
  gpio_button: "9"
  vitesse_max:  "120"       # clé custom pour ce workload
  endpoint_api: "http://192.168.1.10:8080"
```

Lire dans le workload avec la valeur par défaut appropriée :

```c
void embewi_app_init(void) {
    int vitesse = embewi_cfg_get_int("vitesse_max", 100);

    char api[128] = "http://localhost:8080";
    embewi_cfg_get("endpoint_api", api, sizeof(api));
}
```

Contraintes clé/valeur imposées par les buffers NVS :

| Limite | Valeur |
|---|---|
| Longueur max d'une clé | 15 caractères |
| Longueur max d'une valeur | 63 caractères |
| Préfixe `_` | Réservé à l'agent — ne pas utiliser |

---

## Selfcheck

`embewi_app_selfcheck()` est appelé de façon répétée pendant les **15 secondes**
qui suivent un boot post-OTA (`pending_verify`). Si elle retourne `false` — ou
si la fenêtre expire sans retour `true` — le bootloader rollback sur l'image
précédente.

```c
bool embewi_app_selfcheck(void) {
    // ✅ Tester : driver initialisé, peripheral présent
    return s_chan != NULL && gpio_get_level(s_gpio) >= 0;

    // ❌ Ne pas faire : logique applicative, réseau, TLS, NVS
}
```

**Ce que selfcheck doit tester :** uniquement ce qui peut faire planter le
service — driver initialisé, capteur répondant, canal hardware actif.

**Ce que selfcheck ne doit pas faire :** logique métier, appels réseau,
lectures NVS, opérations bloquantes. La fenêtre est courte et partagée avec
l'émission du heartbeat.

---

## Service applicatif

Le Core alloue un port TCP au device (`POST /app/port`, persisté en NVS).
L'agent passe ce port à `embewi_app_service_start(port)`. Le workload choisit
librement le protocole : HTTP, HTTPS, WebSocket, TCP brut…

```c
void embewi_app_service_start(uint16_t port) {
    httpd_config_t cfg = HTTPD_DEFAULT_CONFIG();
    cfg.server_port = port;
    cfg.ctrl_port   = port + 1;  // port+1 réservé au ctrl socket httpd
    httpd_start(&s_srv, &cfg);
    // enregistrer les routes...
}

void embewi_app_service_stop(void) {
    if (s_srv) { httpd_stop(s_srv); s_srv = NULL; }
}
```

> `ctrl_port = port + 1` est obligatoire pour éviter le conflit avec le socket
> de contrôle interne de l'httpd de l'agent (qui utilise lui aussi `port + 1`).

---

## Workloads fournis (`apps/`)

Les workloads dans `apps/` sont des **références d'implémentation** — ils
montrent comment utiliser chaque partie de l'interface.

### `apps/button/main.c` — bouton BOOT

Illustre : lecture McuConfigMap (`gpio_button`), tâche FreeRTOS de polling,
service HTTP simple, selfcheck GPIO.

| Point | Valeur |
|---|---|
| Config lue | `gpio_button` → `embewi_cfg_get_int` |
| Tâche | `embewi_btn` — polling 50 ms, stack 2 048 B, priorité 3 |
| Service | `GET /sensors` → `{"counter": N}` |
| Selfcheck | `gpio_get_level(s_gpio) == 1` (bouton relâché) |

### `apps/rainbow/main.c` — LED WS2812B

Illustre : lecture McuConfigMap (`gpio_ws2812`), driver RMT, selfcheck hardware,
service HTTP d'état.

| Point | Valeur |
|---|---|
| Config lue | `gpio_ws2812` → `embewi_cfg_get_int` |
| Driver | RMT TX 10 MHz, protocole WS2812B GRB, MSB en premier |
| Tâche | `embewi_rainbow` — cycle HSV, stack 2 048 B, priorité 3 |
| Service | `GET /status` → `{"running": true\|false}` |
| Selfcheck | `s_chan != NULL && s_encoder != NULL` |

---

## Créer son workload

1. Créer `apps/<mon-app>/main.c` et implémenter les 4 fonctions.
2. Dans `main/CMakeLists.txt`, ajouter le cas :

```cmake
set(EMBEWI_APP "mon-app")   # ← changer ici

if(EMBEWI_APP STREQUAL "rainbow")
    set(EMBEWI_APP_SRC "../apps/rainbow/main.c")
elseif(EMBEWI_APP STREQUAL "mon-app")
    set(EMBEWI_APP_SRC "../apps/mon-app/main.c")
else()
    set(EMBEWI_APP_SRC "../apps/button/main.c")
endif()
```

Les headers `embewi_app.h` et `embewi_agent.h` sont accessibles sans chemin
supplémentaire — `INCLUDE_DIRS "."` du composant couvre `main/`.

Pour un workload multi-fichiers, lister les sources en liste CMake :

```cmake
set(EMBEWI_APP_SRC "../apps/mon-app/main.c;../apps/mon-app/drivers.c")
```

3. Créer le `McuConfigMap` Kubernetes avec les clés nécessaires au workload et
   le référencer dans le `McuDeployment`.


Pour construire l'artefact OTA et le déployer via le Core, voir
[Déployer un workload](deploy).
