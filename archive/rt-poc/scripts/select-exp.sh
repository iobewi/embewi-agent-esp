#!/usr/bin/env bash
# Sélectionne l'image servie par ESP Web Tools pour une contre-expérience POC 2b:
#   scripts/select-exp.sh baseline|nvs|print
set -euo pipefail
E="${1:?Usage: $0 baseline|nvs|print}"
SRC="web/firmware/exp/${E}"
[[ -f "$SRC/firmware.bin" ]] || { echo "Image introuvable: $SRC" >&2; exit 1; }
cp "$SRC/firmware.bin" web/firmware/esp32c3/firmware.bin
cp "$SRC/workload.bin" web/firmware/esp32c3/workload.bin
echo "web/firmware/esp32c3 <- $E"
