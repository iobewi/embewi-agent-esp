# web/ — Installation via ESP Web Tools

Page statique servie par `http-server` (installé dans le devcontainer) qui
permet de flasher le firmware depuis le navigateur (Web Serial), sans
toolchain côté client.

## Structure

- `index.html` — page d'installation (`esp-web-install-button`).
- `manifest.json` — décrit les builds disponibles par famille de puce.
- `firmware/<chip>/firmware.bin` — image mergée (bootloader + table de
  partitions + application) à l'offset `0x0`, une par cible supportée.

Les `.bin` ne sont **pas** commités (voir `.gitignore`) — à régénérer après
chaque build.

## Générer une image flashable

```sh
BIN_NAME=<nom-du-binaire> scripts/save-image.sh esp32
```

Le script (`scripts/save-image.sh`, à la racine) déduit le triple cible
(Xtensa vs RISC-V) à partir du chip demandé — une simple variable `CHIP` ne
suffit pas, chaque famille de puce ayant un triple différent — et écrit le
résultat dans `web/firmware/<chip>/firmware.bin`. Chips supportés :
`esp32`, `esp32s2`, `esp32s3`, `esp32c2`, `esp32c3`, `esp32c6`, `esp32h2`.

## Servir la page en local

```sh
http-server web -p 8080
```

Puis ouvrir `http://localhost:8080` dans Chrome ou Edge (Web Serial requis).

## Configuration matérielle (après le Wi-Fi)

La broche GPIO de la LED de statut n'est pas figée dans le firmware : elle
se règle depuis une page servie par l'appareil lui-même une fois sur le
Wi-Fi (voir `src/http/`), pas depuis cette page statique. Après une
connexion Wi-Fi réussie via Improv, un bouton « Visit Device » apparaît
directement dans la fenêtre d'ESP Web Tools et y mène.

## Image avec le bootloader Rust (`embewi-boot`)

```sh
scripts/build-boot.sh     # -> web/firmware/esp32c3/{firmware,app,otadata}.bin
```

`firmware.bin` est l'image mergée (embewi-boot + table de partitions + agent,
`otadata` seedé) servie par `index.html` / `manifest.json`. `app.bin` et
`otadata.bin` alimentent `recover.html`.

## Récupération : `recover.html`

`http://localhost:8080/recover.html` réécrit **uniquement** `ota_0` (`0x20000`)
et `otadata` (`0xf000`), sans effacer la flash : bootloader, table de
partitions et NVS (Wi-Fi, token) sont conservés. À utiliser quand une image
applicative est corrompue mais que le bootloader démarre encore (`boot: FAILED
bad image magic …`). Pas un flash complet : si le bootloader lui-même est en
cause, utiliser `index.html` avec « Erase ».

`otadata` est seedé (`seq=1`, VALID) parce que le bootloader Rust ne le gère
pas encore : vierge, l'agent croirait que le slot courant est « Factory » et
écrirait ses OTA dans le slot en cours d'exécution (voir `boot/README.md`).
