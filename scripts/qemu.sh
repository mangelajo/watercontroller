#!/usr/bin/env bash
# Run the firmware in Espressif's QEMU fork.
#
# Steps:
#   1. (re)build firmware in release mode (debug binary doesn't fit OTA partition)
#   2. merge bootloader + partition table + app into a 4 MB flash image
#   3. boot qemu-system-xtensa with the flash image, stdout = UART,
#      forward host port 8080 -> guest port 80 (HTTP API + SPA)
#      forward host port 1883 -> guest port 1883 (if a broker is running on host)
#
# Notes on what works under QEMU:
#   - Boot, FreeRTOS tasks, NVS, HTTPD, tee logger
#   - Open_eth NIC (firmware sees this as a network interface; some
#     esp-idf-svc WiFi calls succeed via espressif/qemu's WiFi peripheral)
#
# What does NOT work:
#   - Real WiFi scan/AP (qemu doesn't simulate WiFi radio)
#   - Hardware peripherals (ADC, PCNT, GPIO) — they read static values
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
QEMU_BIN="${WC_QEMU_BIN:-$HOME/.local/qemu-xtensa/qemu/bin/qemu-system-xtensa}"
PROFILE="${WC_PROFILE:-release}"
BUILD_FW="${WC_BUILD:-1}"

if [[ ! -x "$QEMU_BIN" ]]; then
    echo "qemu-system-xtensa not found at $QEMU_BIN" >&2
    echo "Run: install Espressif QEMU first (see PLAN.md). " >&2
    exit 1
fi

if [[ "$BUILD_FW" == "1" ]]; then
    if [[ "$PROFILE" == "release" ]]; then
        "$REPO_ROOT/scripts/firmware.sh" build --release --features qemu
    else
        "$REPO_ROOT/scripts/firmware.sh" build --features qemu
    fi
fi

ELF="$REPO_ROOT/target/firmware/xtensa-esp32-espidf/$PROFILE/watercontroller-firmware"
FLASH="$REPO_ROOT/target/firmware/flash.bin"

if [[ ! -f "$ELF" ]]; then
    echo "ELF not found: $ELF" >&2
    exit 1
fi

echo "Generating flash image $FLASH …"
podman run --rm --userns=keep-id:uid=1000,gid=1000 \
    -v "$REPO_ROOT":/project:Z \
    -w /project/crates/firmware \
    docker.io/espressif/idf-rust:esp32_latest \
    espflash save-image \
        --chip esp32 \
        --merge \
        --flash-size 4mb \
        --partition-table /project/crates/firmware/partitions.csv \
        "/project/target/firmware/xtensa-esp32-espidf/$PROFILE/watercontroller-firmware" \
        /project/target/firmware/flash.bin >/dev/null

echo "Flash image: $(du -h "$FLASH" | cut -f1) at $FLASH"
echo
echo "Booting QEMU. Press Ctrl-A x to exit. HTTP forwarded on http://127.0.0.1:8080"
echo "----"

exec "$QEMU_BIN" \
    -nographic \
    -machine esp32 \
    -drive "file=$FLASH,if=mtd,format=raw" \
    -nic "user,model=open_eth,hostfwd=tcp::18080-:80,hostfwd=tcp::18023-:23" \
    -global driver=esp32.gpio,property=strap_mode,value=0x12
