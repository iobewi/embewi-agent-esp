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
espflash save-image --chip esp32 --merge \
    target/xtensa-esp32-none-elf/release/<bin> \
    web/firmware/esp32/firmware.bin
```

Adapter `--chip` / le triple cible / le chemin de sortie pour `esp32c3` et
`esp32s3`.

## Servir la page en local

```sh
http-server web -p 8080
```

Puis ouvrir `http://localhost:8080` dans Chrome ou Edge (Web Serial requis).
