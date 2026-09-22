#!/usr/bin/env bash
# Construit l'image système Embewi pour ESP32-C3 avec le bootloader Rust
# `embewi-boot` (boot/) au lieu du bootloader ESP-IDF que espflash injecte
# par défaut : bootloader + table de partitions + agent, en une image mergée
# prête pour ESP Web Tools (web/firmware/esp32c3/firmware.bin).
#
# Usage: scripts/build-boot.sh
#
# Les réglages flash (dio / 4 Mo / 40 MHz) sont ceux de l'image mergée
# actuelle ; ils sont écrits dans l'en-tête du bootloader.
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=riscv32imc-unknown-none-elf
FLASH_ARGS=(--chip esp32c3 --flash-mode dio --flash-size 4mb --flash-freq 40mhz)
OUT=web/firmware/esp32c3/firmware.bin

echo "== embewi-boot"
(cd boot && cargo build --release)
# Un bootloader n'est pas une application : pas de descripteur ESP-IDF.
espflash save-image "${FLASH_ARGS[@]}" --ignore-app-descriptor \
    "boot/target/${TARGET}/release/embewi-boot" boot/target/embewi-boot.bin

echo "== embewi-agent"
cargo build --release

echo "== image mergée"
mkdir -p "$(dirname "$OUT")"
espflash save-image "${FLASH_ARGS[@]}" --merge --skip-padding \
    --bootloader boot/target/embewi-boot.bin \
    --partition-table partitions.csv \
    "target/${TARGET}/release/embewi-agent-esp" "$OUT"
# `otadata` reste VIERGE dans l'image : c'est embewi-boot qui l'initialise au
# premier boot (il valide ota_0, écrit Valid(seq=1), relit, puis boote). Aucun
# outil de build ne fabrique d'état runtime.
# app.bin (image applicative seule) alimente web/recover.html.
espflash save-image "${FLASH_ARGS[@]}" \
    "target/${TARGET}/release/embewi-agent-esp" web/firmware/esp32c3/app.bin >/dev/null
rm -f web/firmware/esp32c3/otadata.bin
echo "Image écrite: $OUT ($(stat -c%s "$OUT") octets)"
