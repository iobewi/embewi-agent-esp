# embewi-boot

Second-stage bootloader Embewi pour ESP32-C3, en Rust `no_std` (sans ESP-IDF).

```text
ROM Espressif -> embewi-boot -> ota_0 / ota_1 -> embewi-agent
```

## État : spike (étape 1)

Ce crate ne prouve encore qu'une chose : qu'un second-stage Rust peut prendre
la main après la ROM, charger l'image **actuelle** de l'agent et la démarrer.

Ce qu'il fait : coupe d'abord la protection « flashboot » des watchdogs TIMG0
et RTC (voir ci-dessous), puis lit la table de partitions (`0x8000`), sélectionne `ota_0`,
valide l'en-tête (magic, `chip_id`, segments dans des plages autorisées, aucun
recouvrement avec sa propre zone RAM), copie les segments RAM, configure le
MMU/cache pour les segments DROM/IROM (pages de 64 Ko) et saute à l'entry
point. Toute anomalie s'affiche et l'arrête, sans jamais sauter.

Ce qu'il ne fait **pas** encore : rollback, machine d'états `otadata`
(le slot est toujours `ota_0`), vérification du checksum/SHA de l'image,
armement d'un watchdog (`esp_hal::init` les désactive tous).

## Construire

```sh
scripts/build-boot.sh     # -> web/firmware/esp32c3/firmware.bin
```

`web/firmware/esp32c3/firmware.bin` est l'image mergée (bootloader + table de
partitions + agent) à flasher à l'offset `0x0` avec ESP Web Tools
(`http-server web -p 8080`), comme avant.

## Contraintes de conception

- **Tout en RAM.** La ROM ne charge que les segments RAM d'un second stage.
  `boot.x` place le code en IRAM `0x403cb000..0x403d4000` et les données/pile
  en DRAM `0x3fcd4000..0x3fcdc000`, dans la fenêtre du bootloader ESP-IDF
  (la ROM garde ses propres données au-dessus de `0x3fcdc710`).
- **Moins de 32 Ko** : la table de partitions commence à `0x8000`. Actuellement
  ~16 Ko, notamment parce que le journal n'utilise pas `core::fmt`
  (`log!` : texte et hexadécimal seulement, ~10 Ko économisés).
- **Crate à part** (son propre `[workspace]`, exclu du workspace de l'agent) :
  autre linker script, autre mémoire, autres features.
- Les fonctions ROM (flash, cache, MMU) viennent de `esp-rom-sys`, via
  `esp-hal`.

## Origine

Séquence de boot conforme au second-stage documenté d'ESP-IDF (Apache-2.0,
`components/bootloader_support`). Code écrit pour Embewi ; aucun code repris
d'un dépôt sans licence.

## Tester sans matériel

L'émulateur Espressif (`qemu-system-riscv32 -machine esp32c3`) embarque la
vraie ROM du C3 et exécute l'image mergée. Ce n'est pas du silicium (cache,
timings flash) : il ne remplace pas le test sur device.

## Résultat sur matériel (ESP32-C3-Zero) -- point de référence

Validé sur silicium : ROM -> `embewi-boot` -> `ota_0` -> agent réel -> Wi-Fi +
SNTP -> NVS et écritures flash (campagne `scripts/test-api.sh safe`, 42/42) ->
reboot (`staged` conservé, retour en ~3 s). C'est le dernier état où le slot
est fixé à `ota_0` et où il n'y a ni A/B, ni rollback, ni vérification
checksum/SHA : à utiliser comme point de bisect.

Deux choses que l'émulateur QEMU n'avait **pas** révélées, et qui doivent
rester connues :

1. **`WDT_FLASHBOOT_MOD_EN`** (ci-dessous) : sans lui, reset en boucle.
2. **`otadata` vierge** : `esp-bootloader-esp-idf` le lit comme « slot
   courant = Factory » (`ota.rs`, branche `UNINITIALIZED_SEQUENCE`), donc
   `next_partition()` -- utilisé par `/ota/write` -- désigne `ota_0`, le slot
   **en cours d'exécution**. Constaté : la campagne `safe` a écrasé l'en-tête
   de `ota_0` (`bad image magic 0x65`, le `e` de `embewi-test-api-sh-payload`).
   Le bootloader ESP-IDF masquait ce cas en écrivant `otadata` (`seq=1`,
   VALID) au premier boot. Ce spike ne le fait pas : `scripts/build-boot.sh`
   le seed dans l'image mergée, et `web/recover.html` le réécrit avec `ota_0`.
   **Mesure temporaire** : le bootloader final doit traiter un `otadata`
   vierge comme un cas normal (`ota_0` valide -> initialisation atomique ->
   `ota_0` actif/valide), sans état runtime fabriqué par l'outillage.

## Watchdogs « flashboot » (leçon du premier essai matériel)

La ROM démarre depuis la flash avec le watchdog principal (TIMG0) et le
watchdog RTC en mode *flashboot* : le matériel en garde un armé tant que
`WDT_FLASHBOOT_MOD_EN` n'est pas effacé. `Wdt::disable()` d'`esp-hal` n'efface
que `WDT_EN` (il ne touche au bit flashboot qu'à l'activation) : sans cette
étape, l'agent démarre puis est reseté par `rst:0x7 (TG0WDT_SYS_RST)`, en
boucle. Le second-stage ESP-IDF fait la même chose (`bootloader_config_wdt`).
QEMU ne modélise pas ce watchdog : ce défaut n'y apparaît pas.
