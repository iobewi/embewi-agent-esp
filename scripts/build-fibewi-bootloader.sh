#!/usr/bin/env bash
# TEMPORARY (feat/esp32s3-target branch): builds the ESP32-S3 bootloader
# instead of ESP32-C3's. Revert CHIP/TARGET/the cargo invocation below (drop
# `+esp`/`-Z build-std`, which stable riscv32 doesn't need) if/when this
# branch goes back to targeting ESP32-C3.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FIBEWI_REV="56a8a66ed748276f5f392e9426605005e0548570"
CHECKOUT="$ROOT/target/fibewi-bootloader-src"
CHIP=esp32s3
TARGET=xtensa-esp32s3-none-elf

if [[ ! -d "$CHECKOUT/.git" ]]; then
  rm -rf "$CHECKOUT"
  git clone --filter=blob:none --no-checkout https://github.com/iobewi/fibewi "$CHECKOUT"
fi
git -C "$CHECKOUT" fetch --depth 1 origin "$FIBEWI_REV"
git -C "$CHECKOUT" checkout --detach --force "$FIBEWI_REV" >/dev/null

cd "$CHECKOUT/bootloader/esp32c3"
cargo +esp build --release --features "$CHIP" -Z build-std=core,alloc --target "$TARGET"
printf "%s\n" "$PWD/target/$TARGET/release/fibewi-esp-bootloader"
