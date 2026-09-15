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
