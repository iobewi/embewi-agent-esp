# Configuration

## McuConfigMap — config runtime (§4a/§7a)

`McuConfigMap` est une ressource Kubernetes portée par le Core. Elle découple
le câblage matériel (GPIOs, fréquences…) du binaire OTA : on peut reconfigurer
un device sans reflasher.

### Ressource Kubernetes

```yaml
apiVersion: embewi.io/v1alpha1
kind: McuConfigMap
metadata:
  name: wheel-left-gpio
data:
  gpio_button: "9"
  gpio_ws2812: "10"
  ntp_server:  "ntp.local"
  # clés arbitraires — opaques pour l'agent, interprétées par l'app workload
```

Référence depuis un `McuDeployment` (champ optionnel) :

```yaml
apiVersion: embewi.io/v1alpha1
kind: McuDeployment
spec:
  nodeName: embewi-a1b2c3
  firmware: registry.local/embewi/wheel-controller:v1.1.0
  configMapRef: wheel-left-gpio   # absent = défauts build actifs
```

### Règles de binding (§7a)

| Situation | Comportement |
|---|---|
| `configMapRef` absent | Aucun push config ; les défauts build (`CONFIG_EMBEWI_*`) restent actifs |
| `configMapRef` présent | Le Core réconcilie `McuConfigMap.data` contre `GET /config` et pousse les écarts via `POST /config` |
| `McuConfigMap` référencé mais inexistant | Erreur `ConfigMapNotFound` — le déploiement est bloqué |

Un même `McuConfigMap` peut être partagé entre plusieurs `McuDeployment`
ciblant des devices **identiquement câblés**. Toute modification du
`McuConfigMap` déclenche une réconciliation sur tous les devices qui y
référencent — ce qui peut provoquer un reboot coordonné de la flotte.

### Clés standardisées (convention inter-apps)

Ces clés sont lues par le firmware agent ou les workloads fournis. Elles ne
sont pas imposées par l'agent (qui est opaque aux clés) mais constituent la
convention entre Core et workloads.

| Clé | Type | Défaut build | Lu par | Description |
|---|---|---|---|---|
| `gpio_button` | int | 9 (C3/C6/H2) · 0 (ESP32/S2/S3) | `apps/button` | GPIO du bouton BOOT |
| `gpio_ws2812` | int | 10 | `apps/rainbow` | GPIO de la LED WS2812B (RMT TX) |
| `ntp_server` | str | `pool.ntp.org` | `embewi_time.c` | Serveur NTP (DHCP aussi utilisé) |

Les clés **spécifiques à un workload custom** sont opaques pour l'agent — les
documenter dans le `McuConfigMap` du déploiement, pas ici.

Les clés préfixées `_` sont réservées à l'agent (`_gen` = compteur de
génération). Elles sont ignorées silencieusement par `POST /config`.

### Modèle de config en couches (§4a)

```text
Priorité (haute → basse) :
  1. NVS runtime  → poussé par le Core via POST /config (McuConfigMap.data)
  2. Défaut build → CONFIG_EMBEWI_* baked dans le binaire (Kconfig)
```

L'agent lit la config NVS **une seule fois au boot** (`embewi_app_init`).
Toute modification via `POST /config` nécessite un reboot pour prendre effet
(`POST /reboot` ou prochain `POST /ota/activate`).

`GET /config` expose les deux couches simultanément :
- `active` : snapshot figé au boot (ce que les apps voient)
- `nvs` : NVS courant (peut diverger si `POST /config` sans reboot)

`generation > active_generation` → config poussée, reboot en attente.

### Validation et limites

**L'agent ne valide pas la sémantique des valeurs** — il stocke et expose.
La validation (plage GPIO valide, fréquence cohérente…) est responsabilité
du Core, typiquement via un `ValidatingAdmissionWebhook` sur le `McuConfigMap`.

Limites pratiques imposées par les buffers agent (NVS + JSON) :

| Limite | Valeur |
|---|---|
| Longueur max d'une clé | 15 caractères (contrainte NVS ESP-IDF) |
| Longueur max d'une valeur | 63 caractères |
| Nombre de clés | Libre, limité par la partition NVS (~8 KB par namespace) |

### Comportement merge-on-key

`POST /config` avec `{"data":{"gpio_ws2812":"48"}}` ne modifie que `gpio_ws2812`.
Les autres clés sont inchangées. Valeur `""` = effacer la clé (retour au défaut
build au prochain boot).

---

## NVS — namespaces

| Namespace | Contenu |
|---|---|
| `embewi_prov` | WiFi (ssid/pass), `ctrl_url`, `node_id`, `token`, IP statique |
| `embewi_tls` | Certificat TLS + clé privée (format PEM) |
| `embewi` | `fw_size`, `app_port`, `staged` (idempotence OTA) |
| `embewi_cfg` | McuConfigMap (clés runtime + `_gen`) |
| `embewi_chk` | Canary du self-check (clé temporaire, effacée après lecture) |

Toute opération NVS suit le pattern : `nvs_open` / get-set / `nvs_commit` /
`nvs_close`. Une valeur vide provoque l'effacement de la clé.

---

## Provisioning — premier boot

Au premier boot (NVS vide), l'agent ouvre un AP Wi-Fi `embewi-XXYY` (2 derniers
octets de la MAC) et sert un portail de configuration HTTPS sur `192.168.4.1`.

Champs du formulaire :

| Champ | Obligatoire | Description |
|---|---|---|
| `node_id` | oui | Identifiant unique du device (libre, ex. `embewi-moteur-gauche`) |
| `ssid` | oui | SSID du réseau Wi-Fi |
| `pass` | non | Mot de passe Wi-Fi |
| `ctrl_url` | oui | URL du contrôleur Kubernetes (ex. `http://192.168.1.10:8080`) |
| `token` | non | Token Bearer (auto-généré 128 bits si vide) |
| IP statique | non | `ip`, `mask`, `gw`, `dns` (DHCP si absent) |

Le token est affiché **une seule fois** sur la page de confirmation —
le conserver immédiatement (il est le secret d'authentification du device).
La fenêtre AP se ferme après **10 minutes** sans soumission (reboot).

---

## Certificat TLS du serveur HTTPS

L'agent sert son API en HTTPS. Priorité du certificat :

1. **NVS** — cert poussé par le Core via `POST /tls/cert` (CA-signé, rotation
   sans OTA).
2. **Fallback auto-signé** — certificat EC P-256 généré au **build** par
   `openssl`, embarqué dans le binaire. Fournit le chiffrement transport dès le
   premier boot. Le Core doit faire du certificate pinning en mode fallback.

> Le cert fallback est **généré localement au build**, jamais commité dans le
> dépôt. `openssl` doit être dans le PATH au moment du build.

---

## `sdkconfig.defaults` — options critiques

| Option | Valeur | Rôle |
|---|---|---|
| `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE` | `y` | **Obligatoire** — rollback automatique si `mark_valid` non appelé |
| `CONFIG_ESP_HTTPS_SERVER_ENABLE` | `y` | Serveur HTTPS :443 |
| `CONFIG_FREERTOS_USE_TRACE_FACILITY` | `y` | `task_hwm_min` sur **toutes** les tâches dans le heartbeat (sinon, tâche heartbeat uniquement) |
| `CONFIG_ESP_TASK_WDT_TIMEOUT_S` | `30` | Watchdog tâche (backstop du self-check) |
| `CONFIG_FREERTOS_HZ` | `1000` | Tick 1 ms |
| `CONFIG_COMPILER_OPTIMIZATION_SIZE` | `y` | Optimisation taille binaire |

> **Nouveau `sdkconfig`** : effacer le `sdkconfig` généré et relancer `idf.py
> build` pour que les nouvelles options de `sdkconfig.defaults` soient adoptées.

---

## Profil prod (`sdkconfig.defaults.prod`)

Voir la page [Sécurité production](embewi-prod-security) pour la procédure
complète. Résumé des options supplémentaires activées :

| Option | Effet |
|---|---|
| `CONFIG_SECURE_BOOT_V2_ENABLED` | Bootloader refuse les images non signées |
| `CONFIG_SECURE_FLASH_ENC_ENABLED` | Flash chiffrée (mode RELEASE) |
| `CONFIG_NVS_ENCRYPTION` | NVS chiffré (schéma flash-enc, partition `nvs_keys`) |
| `CONFIG_EMBEWI_VERIFY_CORE_CERT` | CA du Core embarquée pour authentifier les flux sortants |
| `CONFIG_PARTITION_TABLE_OFFSET=0x10000` | Table de partition décalée (bootloader signé plus grand) |

Ces options impliquent des opérations eFuse **irréversibles** au premier flash.

---

## Kconfig — options build

Accessibles via `idf.py menuconfig` → **Embewi GPIO** / **Embewi Sécurité** :

| Symbole | Défaut | Description |
|---|---|---|
| `CONFIG_EMBEWI_BUTTON_GPIO` | 9 (C3/C6/H2) / 0 (autres) | GPIO bouton BOOT |
| `CONFIG_EMBEWI_WS2812_GPIO` | 10 | GPIO LED WS2812B (RMT TX) |
| `CONFIG_EMBEWI_VERIFY_CORE_CERT` | `n` | Vérification CA Core sur flux sortants |


La GPIO des workloads est surchargeable au runtime via McuConfigMap
(`gpio_button` / `gpio_ws2812`). Voir la page [Workloads](workload) pour la
sélection au build et la construction de l'artefact OTA.
