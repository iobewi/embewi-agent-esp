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
espflash save-image --chip "$CHIP" --partition-table partitions.csv --merge --skip-padding "$SRC" "$DEST"
echo "Image écrite: $DEST"

# POC 1 (agent/workload architecture spike, src/workload.rs) -- esp32c3
# only, `crates/workload-poc` doesn't target anything else yet. Built and
# objcopy'd separately from the agent's own merged image above: it's not a
# `partitions.csv` entry, just bytes placed at a fixed flash offset (see
# src/workload.rs's WORKLOAD_PHYS_OFFSET doc comment) in the unallocated
# tail past `ota_1`. web/manifest.json's ESP32-C3 build lists it as a
# second `parts` entry at that same offset, so ESP Web Tools flashes both
# in one browser session -- if WORKLOAD_PHYS_OFFSET ever changes there,
# update the offset in web/manifest.json too, by hand (same
# hand-kept-in-sync situation as workload.ld's ORIGIN).
if [[ "$CHIP" == "esp32c3" ]]; then
    WORKLOAD_SRC_DIR="crates/workload-poc"
    WORKLOAD_ELF="${WORKLOAD_SRC_DIR}/target/riscv32imc-unknown-none-elf/release/workload-poc"
    WORKLOAD_DEST="web/firmware/esp32c3/workload.bin"

    echo "Build workload-poc (POC 1)..."
    (cd "$WORKLOAD_SRC_DIR" && cargo build --release)
    (cd "$WORKLOAD_SRC_DIR" && cargo objcopy --release -- -O binary workload.bin)
    cp "${WORKLOAD_SRC_DIR}/workload.bin" "$WORKLOAD_DEST"
    echo "Image écrite: $WORKLOAD_DEST"
fi
