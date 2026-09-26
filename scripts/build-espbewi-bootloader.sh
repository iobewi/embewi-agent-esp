#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ESPBEWI_REV="6983199cb2185c78ea378808953e8d334b9874ca"
CHECKOUT="$ROOT/target/espbewi-bootloader-src"
TARGET=riscv32imc-unknown-none-elf

if [[ ! -d "$CHECKOUT/.git" ]]; then
  rm -rf "$CHECKOUT"
  git clone --filter=blob:none --no-checkout https://github.com/iobewi/espbewi "$CHECKOUT"
fi
git -C "$CHECKOUT" fetch --depth 1 origin "$ESPBEWI_REV"
git -C "$CHECKOUT" checkout --detach --force "$ESPBEWI_REV" >/dev/null

cd "$CHECKOUT/bootloader/esp"
cargo build --release --locked --features esp32c3 --target "$TARGET"
printf "%s\n" "$PWD/target/$TARGET/release/espbewi-bootloader"
