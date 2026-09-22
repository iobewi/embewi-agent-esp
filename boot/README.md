# embewi-boot

Second-stage bootloader Embewi pour ESP32-C3, en Rust `no_std` (sans ESP-IDF).

```text
ROM Espressif -> embewi-boot -> ota_0 / ota_1 -> embewi-agent
```

## État : étape 5 -- bootstrap d'`otadata` (pas encore de rollback)

Chaîne : ROM -> `embewi-boot` -> slot choisi par `otadata` -> agent.

Toutes les *décisions* (quel slot, quoi écrire dans `otadata`, l'image est-elle
amorçable) viennent de `crates/embewi-boot-core`, testé sur l'hôte contre des
coupures de courant (modèle adversarial). Ce crate-ci les exécute sur la vraie
flash :

- coupe la protection « flashboot » des watchdogs (voir ci-dessous) ;
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

Écriture d'une entrée `otadata`, chaque étape relue avant la suivante (une
erreur = arrêt) :

```text
effacer le secteur        -> relire : effacé
écrire le corps           -> relire : exactement le corps, mot de commit encore effacé
écrire le mot de commit   -> relire : exactement l'entrée committée, et elle se décode
   (commande flash séparée)
```

Le mot de commit veut donc dire « j'ai vérifié CE corps », pas seulement « une
seconde commande a tourné ».

Pas encore : vérification checksum/SHA (les contrôles s'arrêtent à
`Verify::Structure`), armement d'un watchdog pour une image qui se fige, et un
**agent qui écrit ce format** : l'agent actuel écrit encore des entrées
ESP-IDF, que ce format ignore volontairement (`activate`/`confirm`/`reject` du
crate `embewi-boot-core` à adopter -- étape suivante). Tant que ce n'est pas
fait, seul le bootloader crée des entrées.

## Construire

```sh
scripts/build-boot.sh     # -> web/firmware/esp32c3/firmware.bin (otadata vierge)
```

`web/firmware/esp32c3/firmware.bin` est l'image mergée (bootloader + table de
partitions + agent) à flasher à l'offset `0x0` avec ESP Web Tools
(`http-server web -p 8080`).

## Résultat sur matériel (ESP32-C3-Zero) -- spike, point de référence

Validé sur silicium avec le spike (slot fixé à `ota_0`, sans A/B ni
vérification) : ROM -> `embewi-boot` -> `ota_0` -> agent réel -> Wi-Fi + SNTP ->
NVS et écritures flash (`scripts/test-api.sh safe`, 42/42) -> reboot. **L'étape 5
(écritures flash `otadata`, sélection A/B) n'est validée que sous QEMU** : à
refaire sur le device avant de s'y fier.

Deux choses que QEMU n'avait **pas** révélées lors du spike :

1. **`WDT_FLASHBOOT_MOD_EN`** (ci-dessous) : sans lui, reset en boucle.
2. **`otadata` vierge** : `esp-bootloader-esp-idf` le lit comme « slot courant =
   Factory », donc `next_partition()` -- utilisé par `/ota/write` -- désigne
   `ota_0`, le slot **en cours d'exécution** (constaté : en-tête de `ota_0`
   écrasé). Le bootloader ESP-IDF masquait ce cas ; `embewi-boot` l'initialise
   désormais lui-même.

Et une que QEMU a révélée à l'étape 5 : la taille de puce du driver ROM (ci-dessus).

## Contraintes de conception

- **Tout en RAM.** La ROM ne charge que les segments RAM d'un second stage.
  `boot.x` place le code en IRAM `0x403cb000..0x403d4000` et les données/pile
  en DRAM `0x3fcd4000..0x3fcdc000`, dans la fenêtre du bootloader ESP-IDF
  (la ROM garde ses propres données au-dessus de `0x3fcdc710`).
- **Moins de 32 Ko** : la table de partitions commence à `0x8000`. Actuellement
  ~21 Ko ; le journal n'utilise pas `core::fmt` (`log!` : texte et hexadécimal
  seulement, ~10 Ko économisés).
- **Crate à part** (son propre `[workspace]`, exclu de celui de l'agent) :
  autre linker script, autre mémoire, autres features. Dépend par chemin de
  `crates/embewi-boot-core`.
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
