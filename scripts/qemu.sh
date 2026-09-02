#!/usr/bin/env bash
# Build, image, and run the firmware inside Espressif's QEMU (ESP32-S3 machine).
#
#   scripts/qemu.sh build          # cargo build --release --features qemu, with sdkconfig.qemu layered in
#   scripts/qemu.sh image          # merge bootloader + partition table + app into a 16 MB flash image
#   scripts/qemu.sh run            # boot the image; serial on stdout, Ctrl-A X to quit
#   scripts/qemu.sh test           # boot, wait for the bot to pair and connect, verify the dashboard
#   scripts/qemu.sh all            # build + image + test (what CI runs)
#
# Requirements beyond the normal build (see README "Test without hardware"):
#   - qemu-system-xtensa from Espressif's fork (QEMU_XTENSA env var, or on PATH)
#   - esptool (pip install esptool) for elf2image / merge_bin
#   - the mock server reachable from the host on port 8080 for `test`
#
# Guest networking is QEMU user-mode (slirp): the guest gets 10.0.2.15 by DHCP and
# reaches the host as 10.0.2.2, which is what the firmware's `qemu` feature uses as
# the default WhatsApp server URL. The admin dashboard is forwarded to host port
# ${ADMIN_PORT:-8081}, bound to loopback only (it has no authentication and can
# factory-reset or reboot the device), so `test` can read it with curl.
set -euo pipefail

cd "$(dirname "$0")/.."

PROFILE="${PROFILE:-release}"
# Fixed, not overridable: sdkconfig.qemu points ESP-IDF back at partitions.csv
# with a relative path whose depth assumes exactly this location (see the
# comment there), so a different or absolute target dir would silently lose
# the custom partition table.
TARGET_DIR="target/qemu"
OUT_DIR="$TARGET_DIR/xtensa-esp32s3-espidf/$PROFILE"
ELF="$OUT_DIR/whatsapp-esp32"
FLASH_IMAGE="$OUT_DIR/flash_image.bin"
FLASH_SIZE="${FLASH_SIZE:-16MB}"
ADMIN_PORT="${ADMIN_PORT:-8081}"
QEMU_XTENSA="${QEMU_XTENSA:-qemu-system-xtensa}"
ESPTOOL="${ESPTOOL:-esptool}"
# How long `test` waits for the bot to report a live WhatsApp connection. QEMU is
# well under real-chip speed for the key generation at first pairing.
TEST_TIMEOUT="${TEST_TIMEOUT:-600}"

log() { printf '\033[1;34m[qemu.sh]\033[0m %s\n' "$*" >&2; }

cmd_build() {
    # A separate target dir keeps this build's ESP-IDF configuration (the overlay
    # changes sdkconfig, which reconfigures esp-idf-sys) from clobbering the board
    # build in target/.
    # esp-idf-sys takes the target dir's parent as the "workspace": with a custom
    # target dir every relative path it resolves (the sdkconfig defaults, the
    # ESP-IDF install dir) would land under target/, silently building a default
    # sdkconfig with no PSRAM and no Ethernet, and cloning a second multi-GB
    # ESP-IDF under target/.embuild. So everything is passed absolute, and the
    # repo's own .embuild is shared; only the qemu flavor's ESP-IDF *build* lands
    # under $TARGET_DIR. (partitions.csv is the one path that has to stay
    # relative; sdkconfig.qemu carries the deeper one.)
    log "building $PROFILE with sdkconfig.qemu into $TARGET_DIR"
    ESP_IDF_SDKCONFIG_DEFAULTS="$PWD/sdkconfig.defaults;$PWD/sdkconfig.qemu" \
    ESP_IDF_TOOLS_INSTALL_DIR="custom:$PWD/.embuild" \
    CARGO_TARGET_DIR="$TARGET_DIR" \
        cargo build --profile "$PROFILE" --features qemu
    test -f "$ELF"
}

cmd_image() {
    # esp-idf-sys drops the second-stage bootloader and the partition table next to
    # the ELF; the app image is derived from the ELF here. Offsets are the ESP32-S3
    # defaults (bootloader 0x0, partition table 0x8000, factory app 0x10000 as in
    # partitions.csv). QEMU requires the image to be exactly one of the sizes the
    # emulated flash supports, hence --fill-flash-size.
    local app="$OUT_DIR/whatsapp-esp32.bin"
    log "elf2image -> $app"
    "$ESPTOOL" --chip esp32s3 elf2image --flash_size "$FLASH_SIZE" -o "$app" "$ELF"
    log "merge_bin -> $FLASH_IMAGE"
    "$ESPTOOL" --chip esp32s3 merge_bin --fill-flash-size "$FLASH_SIZE" -o "$FLASH_IMAGE" \
        0x0 "$OUT_DIR/bootloader.bin" \
        0x8000 "$OUT_DIR/partition-table.bin" \
        0x10000 "$app"
    ls -l "$FLASH_IMAGE"
}

# Fills QEMU_CMD, the invocation as an array, so a path with whitespace in
# QEMU_XTENSA or PROFILE stays one argument.
#
# -m sets the emulated PSRAM size (the board has 8 MB). `open_eth` is the only
# NIC model the esp32s3 machine wires up. The serial console is the stdio.
# QEMU_GDB=1 adds a gdb stub on :1234 and holds the CPUs until a debugger
# continues them: `xtensa-esp32s3-elf-gdb <elf> -ex 'target remote :1234'`,
# then `info threads` / `thread apply all bt` shows every FreeRTOS task,
# which is how the hardware-AES stall in sdkconfig.qemu was found.
qemu_cmd() {
    QEMU_CMD=(
        "$QEMU_XTENSA" -nographic -M esp32s3 -m 8M
        -drive "file=$FLASH_IMAGE,if=mtd,format=raw"
        -nic "user,model=open_eth,hostfwd=tcp:127.0.0.1:${ADMIN_PORT}-:8081"
        -serial mon:stdio
    )
    if [[ -n "${QEMU_GDB:-}" ]]; then
        QEMU_CMD+=(-s -S)
    fi
}

cmd_run() {
    test -f "$FLASH_IMAGE" || cmd_image
    qemu_cmd
    exec "${QEMU_CMD[@]}"
}

cmd_test() {
    test -f "$FLASH_IMAGE" || cmd_image
    local serial="$OUT_DIR/qemu-serial.log"
    : > "$serial"
    log "booting; serial log at $serial (timeout ${TEST_TIMEOUT}s)"
    qemu_cmd
    "${QEMU_CMD[@]}" > "$serial" 2>&1 < /dev/null &
    # Not `local`: the EXIT trap runs after this function has returned, and
    # `set -u` would otherwise abort the trap and leave QEMU running.
    QEMU_PID=$!
    trap 'kill "$QEMU_PID" 2>/dev/null || true' EXIT

    # The markers, in the order the firmware logs them. Each one is a real stage
    # of the stack on the emulated chip: boot, DHCP over the emulated NIC, the
    # Noise handshake + QR pairing against the mock server, then the live session.
    local -a markers=(
        "whatsapp-esp32 starting"
        "Ethernet connected! IP:"
        "Bot built, starting run loop"
        "Connected to WhatsApp!"
    )
    local deadline=$((SECONDS + TEST_TIMEOUT))
    for marker in "${markers[@]}"; do
        until grep -q -F "$marker" "$serial"; do
            if ! kill -0 "$QEMU_PID" 2>/dev/null; then
                log "QEMU exited before '$marker'"; tail -n 60 "$serial"; return 1
            fi
            if grep -q -E "RUST PANIC|Guru Meditation|abort\(\) was called|rst:0x[0-9a-f]+ \((TG[01]WDT|RTCWDT|PANIC)" "$serial"; then
                log "firmware crashed before '$marker'"; tail -n 80 "$serial"; return 1
            fi
            if (( SECONDS >= deadline )); then
                log "timed out waiting for '$marker'"; tail -n 60 "$serial"; return 1
            fi
            sleep 1
        done
        log "ok: $marker"
    done

    # The dashboard is served by the firmware itself, so a good answer here means
    # the HTTP server, the store and the event handler all agree the bot is online.
    local device
    device="$(curl -sS --max-time 10 "http://127.0.0.1:${ADMIN_PORT}/device")"
    log "GET /device -> $device"
    grep -q '"connected":true' <<<"$device" || { log "dashboard does not report connected"; return 1; }
    curl -sS --max-time 10 "http://127.0.0.1:${ADMIN_PORT}/metrics" | tee "$OUT_DIR/qemu-metrics.json" >&2
    echo >&2
    log "PASS"
}

case "${1:-}" in
    build) cmd_build ;;
    image) cmd_image ;;
    run) cmd_run ;;
    test) cmd_test ;;
    all) cmd_build; cmd_image; cmd_test ;;
    *) sed -n '2,20p' "$0"; exit 2 ;;
esac
