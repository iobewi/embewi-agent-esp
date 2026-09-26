#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ESPBEWI_REV="e5fa7fcd30ccf480d9c680328d3889abbd440dcc"
CHECKOUT="$ROOT/target/espbewi-bootloader-src"
CHIP=esp32s3
TARGET=xtensa-esp32s3-none-elf

if [[ ! -d "$CHECKOUT/.git" ]]; then
  rm -rf "$CHECKOUT"
  git clone --filter=blob:none --no-checkout https://github.com/iobewi/espbewi "$CHECKOUT"
fi
git -C "$CHECKOUT" fetch --depth 1 origin "$ESPBEWI_REV"
git -C "$CHECKOUT" checkout --detach --force "$ESPBEWI_REV" >/dev/null

cd "$CHECKOUT/bootloader/esp"
cargo +esp build --release --locked --features "$CHIP" -Z build-std=core,alloc --target "$TARGET"
printf "%s\n" "$PWD/target/$TARGET/release/espbewi-bootloader"
