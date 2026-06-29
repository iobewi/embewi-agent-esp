# Configuration

## McuConfigMap — config runtime (§4a/§7a)

Le Core pousse des clés/valeurs via `POST /config`. L'agent les stocke en NVS
et les apps les lisent **une seule fois au boot** (`embewi_app_init`). Toute
modification nécessite un reboot pour prendre effet.

L'agent ne valide pas la sémantique des valeurs — il stocke et expose. La
validation (plage GPIO valide, fréquence cohérente…) est responsabilité du Core.

### Clés standardisées

| Clé | Type | Défaut build | Description |
|---|---|---|---|
| `gpio_button` | int | 9 (C3/C6/H2), 0 (ESP32/S2/S3) | GPIO du bouton de test (BOOT) |
| `gpio_ws2812` | int | 10 | GPIO de la LED WS2812B (app rainbow, RMT TX) |
| `ntp_server` | str | `pool.ntp.org` | Serveur NTP (le NTP DHCP est aussi utilisé) |

Les clés préfixées `_` sont réservées à l'agent (ex. `_gen` pour la génération).
Elles sont ignorées silencieusement par `POST /config`.

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
