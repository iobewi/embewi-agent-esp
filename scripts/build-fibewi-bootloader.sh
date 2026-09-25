#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIBEWI_REV="aa4ad70b391e5243e2b13a4170cbc27d84dc028a"
CHECKOUT="$ROOT/target/fibewi-bootloader-src"
TARGET=riscv32imc-unknown-none-elf

if [[ ! -d "$CHECKOUT/.git" ]]; then
  rm -rf "$CHECKOUT"
  git clone --filter=blob:none --no-checkout https://github.com/iobewi/fibewi "$CHECKOUT"
fi
git -C "$CHECKOUT" fetch --depth 1 origin "$FIBEWI_REV"
git -C "$CHECKOUT" checkout --detach --force "$FIBEWI_REV" >/dev/null

cd "$CHECKOUT/bootloader/esp"
cargo build --release --locked --features esp32c3 --target "$TARGET"
printf "%s\n" "$PWD/target/$TARGET/release/fibewi-esp-bootloader"
