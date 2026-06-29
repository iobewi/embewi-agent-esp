# Configuration

Design de la configuration de l'agent : ressources Kubernetes (`McuConfigMap`),
NVS, provisioning, TLS et options de build. Pour les règles de codage côté
workload (comment lire la config depuis le firmware), voir
[Développer un workload](workload).

---

## McuConfigMap (§7a)

`McuConfigMap` est une ressource Kubernetes portée par le Core. Elle découple
le câblage matériel (GPIOs, adresses, serveurs…) du binaire OTA : un même
firmware peut être déployé sur des devices câblés différemment.

### Ressource Kubernetes

```yaml
apiVersion: embewi.io/v1alpha1
kind: McuConfigMap
metadata:
  name: wheel-left-gpio
data:
  gpio_button: "9"       # toutes les valeurs sont des strings
  gpio_ws2812: "10"
  ntp_server:  "ntp.local"
  # clés arbitraires — opaques pour l'agent, interprétées par le workload
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

### Règles de binding

| Situation | Comportement |
|---|---|
| `configMapRef` absent | Aucun push config — les défauts build (`CONFIG_EMBEWI_*`) restent actifs |
| `configMapRef` présent | Le Core réconcilie `McuConfigMap.data` contre `GET /config` et pousse les écarts via `POST /config` |
| `McuConfigMap` référencé mais inexistant | Erreur `ConfigMapNotFound` — le déploiement est bloqué |

Un même `McuConfigMap` peut être partagé entre plusieurs `McuDeployment`
ciblant des devices **identiquement câblés**. Toute modification déclenche une
réconciliation sur tous les devices référençants — potentiellement un reboot
coordonné de la flotte. Anticiper cet effet avant de modifier un ConfigMap partagé.

### Clés standardisées

Les clés suivantes sont définies par convention entre Core et workloads fournis.
L'agent lui-même est opaque aux clés — il stocke et expose sans valider.

| Clé | Type | Défaut build | Effet |
|---|---|---|---|
| `gpio_button` | int | 9 (C3/C6/H2) · 0 (ESP32/S2/S3) | GPIO du bouton BOOT (`apps/button`) |
| `gpio_ws2812` | int | 10 | GPIO de la LED WS2812B (`apps/rainbow`) |
| `ntp_server` | str | `pool.ntp.org` | Serveur NTP principal |

Les clés spécifiques à un workload custom sont à documenter dans le `McuConfigMap`
du déploiement. Les clés préfixées `_` sont réservées à l'agent (`_gen`) et
ignorées silencieusement par `POST /config`.

### Limites

| Limite | Valeur |
|---|---|
| Longueur max d'une clé | 15 caractères (contrainte NVS ESP-IDF) |
| Longueur max d'une valeur | 63 caractères |
| Nombre de clés | Limité par la partition NVS (~8 KB par namespace) |

La validation sémantique des valeurs est responsabilité du Core, typiquement
via un `ValidatingAdmissionWebhook` sur la ressource `McuConfigMap`.

### Comportement runtime

```text
Modèle de priorité (haute → basse) :
  1. NVS runtime  → poussé par POST /config  (McuConfigMap.data)
  2. Défaut build → CONFIG_EMBEWI_* baked dans le binaire (Kconfig)
```

La config est appliquée **au prochain reboot** — `POST /config` seul ne suffit
pas. Le Core doit enchaîner avec `POST /reboot` (ou le reboot arrive avec le
prochain `POST /ota/activate`).

`GET /config` expose les deux couches pour comparer :
- `active` — snapshot figé au boot (ce que les apps voient réellement)
- `nvs` — NVS courant (peut diverger si `POST /config` sans reboot)

`generation > active_generation` → config poussée, reboot en attente.

---

## NVS — namespaces

| Namespace | Contenu |
|---|---|
| `embewi_prov` | WiFi (ssid/pass), `ctrl_url`, `node_id`, `token`, IP statique |
| `embewi_tls` | Certificat TLS + clé privée (format PEM) |
| `embewi` | `fw_size`, `app_port`, `staged` (idempotence OTA) |
| `embewi_cfg` | McuConfigMap (clés runtime + `_gen`) |
| `embewi_chk` | Canary du self-check (clé temporaire, effacée après lecture) |

---

## Provisioning — premier boot

Au premier boot (NVS vide), l'agent ouvre un AP Wi-Fi `embewi-XXYY` (2 derniers
octets de la MAC de base) et sert un portail de configuration HTTPS sur
`192.168.4.1`. La fenêtre se ferme après **10 minutes** sans soumission (reboot).

| Champ | Obligatoire | Description |
|---|---|---|
| `node_id` | oui | Identifiant unique du device (ex. `embewi-moteur-gauche`) |
| `ssid` | oui | SSID du réseau Wi-Fi |
| `pass` | non | Mot de passe Wi-Fi |
| `ctrl_url` | oui | URL du contrôleur (ex. `https://core.local:8443`) |
| `token` | non | Token Bearer (auto-généré 128 bits si vide) |
| IP statique | non | `ip`, `mask`, `gw`, `dns` (DHCP si absent) |

Le token est affiché **une seule fois** sur la page de confirmation — le noter
immédiatement. Rotation possible sans re-provisioning via `POST /token`.

---

## Certificat TLS du serveur HTTPS

L'agent sert son API en HTTPS. Priorité du certificat :

1. **NVS** — cert poussé par le Core via `POST /tls/cert` (CA-signé, rotation sans OTA).
2. **Fallback auto-signé** — EC P-256 généré au build par `openssl`, embarqué dans
   le binaire. Le Core doit faire du certificate pinning en mode fallback.

> Le cert fallback est généré au build, jamais commité. `openssl` doit être dans
> le PATH au moment du build.

---

## `sdkconfig.defaults` — options critiques

| Option | Valeur | Rôle |
|---|---|---|
| `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE` | `y` | **Obligatoire** — rollback automatique si `mark_valid` non appelé |
| `CONFIG_ESP_HTTPS_SERVER_ENABLE` | `y` | Serveur HTTPS :443 |
| `CONFIG_FREERTOS_USE_TRACE_FACILITY` | `y` | `task_hwm_min` sur toutes les tâches dans le heartbeat |
| `CONFIG_ESP_TASK_WDT_TIMEOUT_S` | `30` | Watchdog tâche (backstop du self-check) |
| `CONFIG_FREERTOS_HZ` | `1000` | Tick 1 ms |
| `CONFIG_COMPILER_OPTIMIZATION_SIZE` | `y` | Optimisation taille binaire |

> Après modification de `sdkconfig.defaults`, effacer le `sdkconfig` généré et
> relancer `idf.py build` pour adopter les nouvelles options.

---

## Profil prod (`sdkconfig.defaults.prod`)

Voir [Sécurité production](embewi-prod-security) pour la procédure complète.

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

Accessibles via `idf.py menuconfig` :

| Symbole | Défaut | Description |
|---|---|---|
| `CONFIG_EMBEWI_BUTTON_GPIO` | 9 (C3/C6/H2) / 0 (autres) | GPIO bouton BOOT (défaut build, surchargeable via McuConfigMap) |
| `CONFIG_EMBEWI_WS2812_GPIO` | 10 | GPIO LED WS2812B (défaut build, surchargeable via McuConfigMap) |
| `CONFIG_EMBEWI_VERIFY_CORE_CERT` | `n` | Vérification CA Core sur flux sortants (prod) |
