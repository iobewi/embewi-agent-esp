# Configuration

Design interne de la configuration de l'agent : modèle NVS, provisioning,
TLS et options de build.

- Ressource Kubernetes `McuConfigMap` et règles de binding → [Déployer un workload](deploy)
- Lecture de la config depuis un workload → [Développer un workload](workload)

---

## Configuration runtime — modèle interne (§4a)

Le Core pousse des paires clé/valeur via `POST /config`. L'agent les stocke
dans le namespace NVS `embewi_cfg` **sans interpréter leur sémantique** — il
est opaque au contenu. La signification des clés appartient au workload (et au
Core qui valide).

```text
Modèle de priorité (haute → basse) :
  1. NVS runtime  → poussé par POST /config  (McuConfigMap.data)
  2. Défaut build → CONFIG_EMBEWI_* baked dans le binaire (Kconfig)
```

La config est appliquée **au prochain reboot** — `POST /config` seul ne suffit
pas. Le Core enchaîne avec `POST /reboot` (ou le reboot vient de `POST /ota/activate`).

`GET /config` expose les deux couches :
- `active` — snapshot figé au boot (ce que les apps voient réellement)
- `nvs` — NVS courant (peut diverger si `POST /config` sans reboot)

`generation > active_generation` → config poussée, reboot en attente.

### Clé lue par l'agent lui-même

| Clé | Défaut build | Lu par |
|---|---|---|
| `ntp_server` | `pool.ntp.org` | `embewi_time.c` — serveur NTP principal |

Toutes les autres clés sont opaques pour l'agent et destinées au workload.
Les clés préfixées `_` sont réservées à l'agent (`_gen` = compteur de
génération) et ignorées silencieusement par `POST /config`.

### Limites des buffers agent

| Limite | Valeur |
|---|---|
| Longueur max d'une clé | 15 caractères (contrainte NVS ESP-IDF) |
| Longueur max d'une valeur | 63 caractères |
| Taille totale | Partition NVS `embewi_cfg` (~8 KB) |

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
