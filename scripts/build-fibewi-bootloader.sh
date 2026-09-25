#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIBEWI_REV="b1daa6cc4a1513cf2eb68a4b16b4ed9fb0ad5160"
CHECKOUT="$ROOT/target/fibewi-bootloader-src"
TARGET=riscv32imc-unknown-none-elf

if [[ ! -d "$CHECKOUT/.git" ]]; then
  rm -rf "$CHECKOUT"
  git clone --filter=blob:none --no-checkout https://github.com/iobewi/fibewi "$CHECKOUT"
fi
git -C "$CHECKOUT" fetch --depth 1 origin "$FIBEWI_REV"
git -C "$CHECKOUT" checkout --detach --force "$FIBEWI_REV" >/dev/null

cd "$CHECKOUT/bootloader/esp32c3"
cargo build --release
printf "%s\n" "$PWD/target/$TARGET/release/fibewi-esp-bootloader"
