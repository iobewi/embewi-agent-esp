# embewi-boot

Second-stage bootloader Embewi pour ESP32-C3, en Rust `no_std` (sans ESP-IDF).

```text
ROM Espressif -> embewi-boot -> ota_0 / ota_1 -> embewi-agent
```

## État : chaîne A/B + rollback + anti-freeze, validée sur silicium

Chaîne : ROM -> `embewi-boot` -> slot choisi par `otadata` -> agent.

Toutes les *décisions* (quel slot, quoi écrire dans `otadata`, l'image est-elle
amorçable) viennent de `fibewi-esp::boot`, partagé par le bootloader
**et** par l'agent (`src/ota.rs`) -- une seule définition de ce qu'est une
entrée `otadata` valide et de quel slot est actif, des deux côtés du saut.
Testé sur l'hôte contre des coupures de courant sous un modèle *adversarial*
(champs programmés dans n'importe quel ordre, pas seulement en ordre
d'adresses) dans le dépôt FiBeWI.
`embewi-boot` exécute ces décisions sur la vraie flash :

- coupe la protection « flashboot » des watchdogs (voir plus bas) ;
- règle la taille de la puce dans le driver flash de la ROM (sans quoi la
  lecture d'`ota_1`, au-delà de 2 Mo, échoue -- trouvé sous QEMU) ;
- lit la table de partitions (`otadata`, `ota_0`, `ota_1`) puis les deux
  entrées `otadata` ;
- `plan_boot` : `otadata` **vierge** = premier boot normal : `ota_0` est validée,
  puis `Valid(seq=1)` est écrit **par le bootloader** (l'image livrée ne contient
  aucun état runtime) ; un `New` passe `Pending` *avant* le saut ; un `Pending`
  jamais confirmé devient `Aborted` (rollback) ; une image non amorçable est
  marquée `Invalid` ; rien d'utilisable => `HALT` explicite, jamais de devinette ;
- charge l'image (segments RAM copiés, DROM/IROM mappés par le MMU) et saute.

Écriture d'une entrée `otadata` (`New`/`Pending`/`Valid`/`Invalid`/`Aborted`,
format **EWBT** : 32 octets, deux secteurs, compatible en taille/position avec
`esp_ota_select_entry_t` mais avec un magic, un `ext_crc` et un **mot de commit
programmé séparément** dans `seq_label` -- qu'ESP-IDF ignore -- pas de mode
legacy), chaque étape relue avant la suivante, une erreur = arrêt :

```text
effacer le secteur        -> relire : effacé
écrire le corps           -> relire : exactement le corps, mot de commit encore effacé
écrire le mot de commit   -> relire : exactement l'entrée committée, et elle se décode
   (commande flash séparée)
```

Le mot de commit veut donc dire « j'ai vérifié CE corps », pas seulement « une
seconde commande a tourné ». Cette écriture est identique côté agent
(`ota.rs::execute_otadata_write`) : `activate`/`confirm`/`reject` du crate
partagé, plus aucune entrée écrite au format ESP-IDF depuis
la migration vers `fibewi-esp::boot`.

Un watchdog matériel (TIMG0, indépendant du `LPWR`/RTC déjà utilisé par
`/reboot`) protège toute la fenêtre `pending_verify`, armé juste après
`esp_rtos::start` (pas plus tôt : `TimerGroup::new(TIMG0)` réinitialise le
bloc à sa première utilisation et effacerait un watchdog armé avant) et
désactivé seulement **après** que `confirm` ait été relu et décodé -- un gel
n'importe où dans cette fenêtre, y compris avant que le self-check logiciel
lui-même ne soit jamais exécuté, se termine en `Pending` non confirmé, donc
en rollback au boot suivant. Détail dans `src/ota.rs`, section
« anti-freeze watchdog ».

**Ce qui n'est pas encore couvert** : `embewi-boot` s'arrête à
`Verify::Structure` avant de sauter (`boot/src/main.rs`) -- il ne revérifie
pas le checksum XOR ni le SHA-256 appendu de l'image à chaque boot, seulement
sa structure (segments dans les plages autorisées, pas de recouvrement du
bootloader). Il fait confiance au SHA-256 vérifié une fois par l'OTA au moment
de l'écriture ; un bit-flip en flash *après* une écriture réussie ne serait
détecté qu'au niveau structure, pas au niveau contenu.

### Trois défauts trouvés par les gates matériels eux-mêmes, avant tout dégât

| Défaut | Où | Comment il a été trouvé |
|---|---|---|
| Ciblage du mauvais slot avec deux entrées `Valid` (celle en cours d'exécution, pas la plus récente) | `src/ota.rs`, sélection du slot cible pour `/ota/write` | Un `prepare` de contrôle après un `confirm` réussi, avant toute écriture réelle |
| Taille de puce non réglée dans le driver flash ROM : `ota_1` (> 2 Mo) illisible | `boot/src/main.rs` | Sous QEMU, en écrivant réellement `otadata` (pas seulement en le lisant) |
| `TimerGroup::new(TIMG0)` réinitialise tout le bloc, effaçant un watchdog armé trop tôt | `src/bin/main.rs` | Relecture du code avant tout flash, en concevant le WDT anti-freeze |

### Gate de conformité anti-brick, validé sur ESP32-C3 réel

Trois scénarios, chacun rejoué avec une vraie image OTA (pas un binaire
factice) :

1. **Self-check normal** : `activate` -> reboot -> `Pending` -> self-check ->
   `confirm` -> `Valid`. Confirmé en ~4 s, aucun reset parasite.
2. **Reset explicite avant `confirm`** (feature `fault-injection`) : l'entrée
   reste `Pending` au moment de la coupure -> boot suivant -> `Aborted` ->
   retour sur le dernier slot `Valid`.
3. **Gel pur, sans aucun appel logiciel à `software_reset()`** (feature
   `fault-injection-freeze`, une boucle infinie dès l'entrée dans la tâche de
   self-check, avant même que son propre timeout logiciel ne soit jamais
   interrogé) : silence total (Wi-Fi et HTTP compris) pendant ~24 s, puis
   reset matériel -> `Aborted` -> retour sur le dernier slot `Valid`. C'est la
   preuve que la protection ne dépend d'aucune coopération de ce firmware,
   seulement du compte à rebours matériel.

Dans les trois cas : digest et `deployment_id` corrects après coup, `staged`
nettoyé, `/health` ok, et une nouvelle tentative recible correctement le slot
abandonné (l'entrée `Aborted` n'est plus jamais candidate).

## Construire

```sh
scripts/build-boot.sh     # -> web/firmware/esp32c3/firmware.bin (otadata vierge)
```

`web/firmware/esp32c3/firmware.bin` est l'image mergée (bootloader + table de
partitions + agent) à flasher à l'offset `0x0` avec ESP Web Tools
(`http-server web -p 8080`).

## Résultat sur matériel (ESP32-C3-Zero)

Historique, du spike au gate anti-brick complet (voir git log pour le détail
commit par commit) :

1. **Spike** (slot fixé à `ota_0`, sans A/B ni rollback) : ROM -> `embewi-boot`
   -> `ota_0` -> agent réel -> Wi-Fi + SNTP -> NVS et écritures flash
   (`scripts/test-api.sh safe`, 42/42) -> reboot.
2. **Bootstrap `otadata` et sélection A/B réelle**, d'abord validés sous QEMU
   (écritures flash comprises, pas seulement lues), puis sur device.
3. **Migration de l'agent** vers le même format `fibewi-esp::boot` --
   c'est ce cycle qui a révélé le défaut de ciblage de slot (tableau
   ci-dessus).
4. **Gate rollback réel** (`Pending` -> reset -> `Aborted` -> retour), puis
   **watchdog anti-freeze**, puis **gate gel pur** (tableau ci-dessus) : les
   trois cas de la matrice de conformité anti-brick.

QEMU (la vraie ROM du C3, écritures flash comprises) a servi de première
passe à chaque étape avant le device réel, et a lui-même révélé un défaut
(taille de puce non réglée, tableau ci-dessus) qu'un test purement en lecture
n'aurait pas montré.

## Contraintes de conception

- **Tout en RAM.** La ROM ne charge que les segments RAM d'un second stage.
  `boot.x` place le code en IRAM `0x403cb000..0x403d4000` et les données/pile
  en DRAM `0x3fcd4000..0x3fcdc000`, dans la fenêtre du bootloader ESP-IDF
  (la ROM garde ses propres données au-dessus de `0x3fcdc710`).
- **Moins de 32 Ko** : la table de partitions commence à `0x8000`. Actuellement
  ~21 Ko ; le journal n'utilise pas `core::fmt` (`log!` : texte et hexadécimal
  seulement, ~10 Ko économisés).
- **Crate à part** (son propre `[workspace]`, exclu de celui de l'agent) :
  autre linker script, autre mémoire, autres features. Il dépend du backend
  `fibewi-esp` sans activer de feature matérielle, et ne consomme que le
  module pur `fibewi_esp::boot`.
- Les fonctions ROM (flash, cache, MMU) viennent de `esp-rom-sys`, via `esp-hal`.

## Origine

Séquence de boot conforme au second-stage documenté d'ESP-IDF (Apache-2.0,
`components/bootloader_support`). Code écrit pour Embewi ; aucun code repris
d'un dépôt sans licence.

## Tester sans matériel

L'émulateur Espressif (`qemu-system-riscv32 -machine esp32c3`) embarque la
vraie ROM du C3 et exécute l'image mergée, **écritures flash comprises** : la
flash émulée (fichier de 4 Mo passé en `-drive file=...,if=mtd,format=raw`) est
modifiée en place, donc `scripts/ewbt-otadata.py decode <flash> 0xf000` montre ce
que le bootloader a écrit. Scénarios éprouvés : `otadata` vierge (seed),
redémarrage sans écriture, `Pending` jamais confirmé (-> `Aborted`, retour sur
`ota_0`), candidat `New` sur `ota_1` (`Pending` écrit avant le saut, boot de
`ota_1`), candidat non amorçable (-> `Invalid`), corps non committé ignoré,
entrées ESP-IDF héritées (re-seed), `ota_0` corrompu (`HALT`).

Ce n'est pas du silicium (cache, timings flash, watchdogs, écritures réelles) :
il ne remplace pas le test sur device.

## Watchdogs « flashboot » (leçon du premier essai matériel)

La ROM démarre depuis la flash avec le watchdog principal (TIMG0) et le
watchdog RTC en mode *flashboot* : le matériel en garde un armé tant que
`WDT_FLASHBOOT_MOD_EN` n'est pas effacé. `Wdt::disable()` d'`esp-hal` n'efface
que `WDT_EN` (il ne touche au bit flashboot qu'à l'activation) : sans cette
étape, l'agent démarre puis est reseté par `rst:0x7 (TG0WDT_SYS_RST)`, en
boucle. Le second-stage ESP-IDF fait la même chose (`bootloader_config_wdt`).
QEMU ne modélise pas ce watchdog : ce défaut n'y apparaît pas.
