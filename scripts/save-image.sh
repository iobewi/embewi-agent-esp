#!/usr/bin/env bash
# Génère l'image mergée (bootloader + table de partitions + app) d'une
# puce donnée, prête pour ESP Web Tools, dans web/firmware/<chip>/firmware.bin.
#
# Usage: scripts/save-image.sh <chip>
#   BIN_NAME=<nom-du-binaire>  (requis — nom du binaire cargo à flasher)
#
# Le triple cible dépend de la famille de puce (Xtensa vs RISC-V) : on ne
# peut donc pas le déduire d'une simple variable CHIP, d'où cette table.
set -euo pipefail

CHIP="${1:?Usage: $0 <chip> (BIN_NAME=<nom-du-binaire> requis en env)}"
BIN_NAME="${BIN_NAME:?Variable BIN_NAME requise (nom du binaire cargo)}"

case "$CHIP" in
    esp32|esp32s2|esp32s3)
        TARGET="xtensa-${CHIP}-none-elf"
        ;;
    esp32c2|esp32c3)
        TARGET="riscv32imc-unknown-none-elf"
        ;;
    esp32c6|esp32h2)
        TARGET="riscv32imac-unknown-none-elf"
        ;;
    *)
        echo "Chip inconnu ou non supporté: $CHIP" >&2
        exit 1
        ;;
esac

SRC="target/${TARGET}/release/${BIN_NAME}"
DEST="web/firmware/${CHIP}/firmware.bin"

if [[ ! -f "$SRC" ]]; then
    echo "Binaire introuvable: $SRC (build release manquant pour $CHIP ?)" >&2
    exit 1
fi

mkdir -p "$(dirname "$DEST")"
espflash save-image --chip "$CHIP" --merge --skip-padding "$SRC" "$DEST"
echo "Image écrite: $DEST"
