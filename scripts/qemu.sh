#!/usr/bin/env bash
# Build, image, and run the firmware inside Espressif's QEMU (ESP32-S3 machine).
#
#   scripts/qemu.sh build          # cargo build --release --features qemu, with sdkconfig.qemu layered in
#   scripts/qemu.sh image [NAME]   # 16 MB flash image for board NAME (default "a"), provisioned with its own push name
#   scripts/qemu.sh run [NAME]     # boot that image; serial on stdout, Ctrl-A X to quit
#   scripts/qemu.sh test           # pair board "a", reboot it and check it stays paired, then message board "b"
#   scripts/qemu.sh all            # build + image + test (what CI runs)
#
# Requirements beyond the normal build (see README "Test without hardware"):
#   - qemu-system-xtensa from Espressif's fork (QEMU_XTENSA env var, or on PATH)
#   - esptool (pip install esptool) for elf2image / merge_bin
#   - esp-idf-nvs-partition-gen (pip) for the provisioning NVS image; the ESP-IDF
#     python env under .embuild already has it and is used when present
#   - the mock server reachable from the host on port 8080 for `test`
#
# Guest networking is QEMU user-mode (slirp): the guest gets 10.0.2.15 by DHCP and
# reaches the host as 10.0.2.2, which is what the firmware's `qemu` feature uses as
# the default WhatsApp server URL. Each board's admin dashboard is forwarded to a
# host port (${ADMIN_PORT:-8081} for "a", one higher per further board), bound to
# loopback only (it has no authentication and can factory-reset or reboot the
# device), so `test` can drive it with curl.
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
FLASH_SIZE="${FLASH_SIZE:-16MB}"
ADMIN_PORT="${ADMIN_PORT:-8081}"
QEMU_XTENSA="${QEMU_XTENSA:-qemu-system-xtensa}"
ESPTOOL="${ESPTOOL:-esptool}"
# How long each boot may take to report a live WhatsApp connection. QEMU is
# well under real-chip speed for the key generation at first pairing.
TEST_TIMEOUT="${TEST_TIMEOUT:-600}"
# How long a message may take to show up on the other board.
MESSAGE_TIMEOUT="${MESSAGE_TIMEOUT:-90}"

log() { printf '\033[1;34m[qemu.sh]\033[0m %s\n' "$*" >&2; }

# The python that has esp-idf-nvs-partition-gen: the ESP-IDF env esp-idf-sys
# created, or whatever NVS_PYTHON names (CI points it at a venv).
nvs_python() {
    if [[ -n "${NVS_PYTHON:-}" ]]; then
        echo "$NVS_PYTHON"
        return
    fi
    local candidate
    for candidate in .embuild/espressif/python_env/*/bin/python; do
        if [[ -x "$candidate" ]]; then
            echo "$candidate"
            return
        fi
    done
    echo python3
}

flash_image() { echo "$OUT_DIR/flash_image-$1.bin"; }

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
    # relative; sdkconfig.qemu carries the deeper one.) esp-idf-sys adds the
    # chip-specific sdkconfig.defaults.esp32s3 by itself, before the overlay.
    log "building $PROFILE with sdkconfig.qemu into $TARGET_DIR"
    ESP_IDF_SDKCONFIG_DEFAULTS="$PWD/sdkconfig.defaults;$PWD/sdkconfig.qemu" \
    ESP_IDF_TOOLS_INSTALL_DIR="custom:$PWD/.embuild" \
    CARGO_TARGET_DIR="$TARGET_DIR" \
        cargo build --profile "$PROFILE" --features qemu
    test -f "$ELF"
}

# The provisioning image for the default `nvs` partition: the push name the
# firmware pairs under (src/main.rs `push_name`). Against the mock server the
# push name selects the account, so this is what makes two boards from one
# firmware image two different WhatsApp numbers.
nvs_image() {
    local name="$1"
    local out="$OUT_DIR/nvs-$name.bin"
    local csv="$OUT_DIR/nvs-$name.csv"
    cat > "$csv" <<CSV
key,type,encoding,value
wa,namespace,,
push_name,data,string,esp32-qemu-$name
CSV
    "$(nvs_python)" -m esp_idf_nvs_partition_gen generate "$csv" "$out" 0x6000 >&2
    echo "$out"
}

cmd_image() {
    local name="${1:-a}"
    # esp-idf-sys drops the second-stage bootloader and the partition table next to
    # the ELF; the app image is derived from the ELF here. Offsets are the ESP32-S3
    # defaults (bootloader 0x0, partition table 0x8000, nvs 0x9000 and factory app
    # 0x10000 as in partitions.csv). QEMU requires the image to be exactly one of
    # the sizes the emulated flash supports, hence --fill-flash-size. The
    # `wa_store` partition starts erased and is written by the firmware itself:
    # QEMU writes the emulated flash back to this file, which is what lets the
    # persistence test reboot the same image.
    local app="$OUT_DIR/whatsapp-esp32.bin" image nvs
    image="$(flash_image "$name")"
    log "elf2image -> $app"
    "$ESPTOOL" --chip esp32s3 elf2image --flash_size "$FLASH_SIZE" -o "$app" "$ELF"
    nvs="$(nvs_image "$name")"
    log "merge_bin -> $image (push name esp32-qemu-$name)"
    "$ESPTOOL" --chip esp32s3 merge_bin --fill-flash-size "$FLASH_SIZE" -o "$image" \
        0x0 "$OUT_DIR/bootloader.bin" \
        0x8000 "$OUT_DIR/partition-table.bin" \
        0x9000 "$nvs" \
        0x10000 "$app"
    ls -l "$image"
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
    local name="$1" port="$2"
    QEMU_CMD=(
        "$QEMU_XTENSA" -nographic -M esp32s3 -m 8M
        -drive "file=$(flash_image "$name"),if=mtd,format=raw"
        -nic "user,model=open_eth,hostfwd=tcp:127.0.0.1:${port}-:8081"
        -serial mon:stdio
    )
    if [[ -n "${QEMU_GDB:-}" ]]; then
        QEMU_CMD+=(-s -S)
    fi
}

# Board "a" gets $ADMIN_PORT, "b" the next one, and so on, so a board keeps the
# same admin port whether it is started by `run` or by `test`.
board_port() {
    local name="$1"
    printf '%s' "$((ADMIN_PORT + $(LC_ALL=C printf '%d' "'$name") - $(LC_ALL=C printf '%d' "'a")))"
}

cmd_run() {
    local name="${1:-a}"
    test -f "$(flash_image "$name")" || cmd_image "$name"
    qemu_cmd "$name" "$(board_port "$name")"
    exec "${QEMU_CMD[@]}"
}

# ---- test ------------------------------------------------------------------

# Every QEMU this run started, so the EXIT trap can stop them all. Not `local`
# anywhere: the trap runs after the function that set it has returned, and
# `set -u` would otherwise abort the trap and leave QEMU running.
QEMU_PIDS=()
stop_all() {
    local pid
    for pid in "${QEMU_PIDS[@]:-}"; do
        [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    done
}

# boot NAME PORT LOG: start a board in the background; sets BOOT_PID.
boot() {
    local name="$1" port="$2" serial="$3"
    test -f "$(flash_image "$name")" || cmd_image "$name"
    : > "$serial"
    log "booting board $name (admin on 127.0.0.1:$port); serial log at $serial"
    qemu_cmd "$name" "$port"
    "${QEMU_CMD[@]}" > "$serial" 2>&1 < /dev/null &
    BOOT_PID=$!
    QEMU_PIDS+=("$BOOT_PID")
}

stop() {
    local pid="$1"
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

# wait_markers PID LOG MARKER...: each marker must show up in the serial log, in
# order, before the deadline; a crash signature or a QEMU exit fails at once.
wait_markers() {
    local pid="$1" serial="$2"
    shift 2
    local deadline=$((SECONDS + TEST_TIMEOUT)) marker
    for marker in "$@"; do
        until grep -q -F "$marker" "$serial"; do
            if ! kill -0 "$pid" 2>/dev/null; then
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
}

admin_get() { curl -sS --max-time 10 "http://127.0.0.1:$1$2"; }

# json_field JSON EXPR: evaluate a python expression over the parsed document.
json_field() { python3 -c 'import json,sys; d=json.load(sys.stdin); print(eval(sys.argv[1]))' "$2" <<<"$1"; }

# The pn `/device` reports is the device's own JID (user:device@server); a
# message is addressed to the account, so the device part comes off.
account_jid() { sed -E 's/[:.][0-9]+@/@/' <<<"$1"; }

# wait_for_message PORT SUBSTRING: poll /messages on a board until an inbound
# message contains SUBSTRING.
wait_for_message() {
    local port="$1" needle="$2" deadline=$((SECONDS + MESSAGE_TIMEOUT)) messages
    until messages="$(admin_get "$port" /messages)" \
        && [[ "$(json_field "$messages" "any($needle in (m['text'] or '') for m in d['messages'])")" == True ]]; do
        if (( SECONDS >= deadline )); then
            log "no message containing $needle on port $port; /messages -> $messages"
            return 1
        fi
        sleep 1
    done
    log "ok: message containing $needle arrived on port $port"
}

# The markers a boot logs on its way to a live session, in order. Each one is a
# real stage of the stack on the emulated chip: boot, DHCP over the emulated NIC,
# the Noise handshake against the mock server, then the session.
BOOT_MARKERS=("whatsapp-esp32 starting" "Ethernet connected! IP:" "Bot built, starting run loop")

cmd_test() {
    trap stop_all EXIT
    local port_a port_b
    port_a="$(board_port a)"
    port_b="$(board_port b)"

    # QEMU writes the emulated flash back into the image file, which is what the
    # reboot stage relies on. It also means a previous run left board a paired,
    # so both images are rebuilt here: stage 1 asserts an empty store.
    log "rebuilding both flash images so the run starts from an unpaired store"
    cmd_image a
    cmd_image b

    # 1. First boot of board a: fresh store, QR pairing against the mock server.
    boot a "$port_a" "$OUT_DIR/qemu-a-boot1.log"
    local pid_a=$BOOT_PID device
    wait_markers "$pid_a" "$OUT_DIR/qemu-a-boot1.log" "${BOOT_MARKERS[@]}" \
        "WhatsApp NVS loaded: device=false" "QR CODE" "Connected to WhatsApp!"
    device="$(admin_get "$port_a" /device)"
    log "board a, boot 1: GET /device -> $device"
    [[ "$(json_field "$device" "d['connected']")" == True ]] || { log "board a is not connected"; return 1; }
    local pn_a
    pn_a="$(json_field "$device" "d['pn'] or ''")"
    if [[ -z "$pn_a" ]]; then
        log "board a connected without reporting a number; /device -> $device"
        return 1
    fi

    # 2. Reboot board a from the same flash image. The credentials and Signal
    #    state must come back from the wa_store partition: no QR this time, the
    #    same number, and a live session.
    log "stopping board a and booting it again from the same image"
    stop "$pid_a"
    boot a "$port_a" "$OUT_DIR/qemu-a-boot2.log"
    pid_a=$BOOT_PID
    wait_markers "$pid_a" "$OUT_DIR/qemu-a-boot2.log" "${BOOT_MARKERS[@]}" \
        "WhatsApp NVS loaded: device=true" "Connected to WhatsApp!"
    if grep -q -F "QR CODE" "$OUT_DIR/qemu-a-boot2.log"; then
        log "board a asked for a QR scan after the reboot: the stored credentials were not used"
        return 1
    fi
    device="$(admin_get "$port_a" /device)"
    log "board a, boot 2: GET /device -> $device"
    [[ "$(json_field "$device" "d['connected'] and d['pn'] == '$pn_a'")" == True ]] \
        || { log "board a did not come back as the same connected device"; return 1; }

    # 3. A second board with its own push name, so the mock server gives it its
    #    own number. Board a pings it; b's bot answers with a reaction, a quoted
    #    reply and an edit; both sides must see the other's message land.
    boot b "$port_b" "$OUT_DIR/qemu-b-boot1.log"
    local pid_b=$BOOT_PID
    wait_markers "$pid_b" "$OUT_DIR/qemu-b-boot1.log" "${BOOT_MARKERS[@]}" "Connected to WhatsApp!"
    device="$(admin_get "$port_b" /device)"
    log "board b: GET /device -> $device"
    local to_b sent
    to_b="$(account_jid "$(json_field "$device" "d['pn'] or ''")")"
    if [[ -z "$to_b" ]]; then
        log "board b connected without reporting a number; /device -> $device"
        return 1
    fi
    log "board a -> board b ($to_b): ping"
    sent="$(curl -sS --max-time 60 -H 'Content-Type: application/json' \
        -d "{\"to\":\"$to_b\",\"text\":\"\\ud83e\\udd80ping\"}" "http://127.0.0.1:$port_a/send")"
    log "POST /send -> $sent"
    [[ "$(json_field "$sent" "d.get('result') == 'sent'")" == True ]] || { log "send failed"; return 1; }
    wait_for_message "$port_b" "'\U0001f980ping'"
    wait_markers "$pid_b" "$OUT_DIR/qemu-b-boot1.log" "Reaction sent" "Send took"
    wait_for_message "$port_a" "'Pong!'"

    # Diagnostic only: the numbers are for the log, not a gate.
    admin_get "$port_a" /metrics | tee "$OUT_DIR/qemu-metrics.json" >&2 || true
    echo >&2
    log "PASS"
}

case "${1:-}" in
    build) cmd_build ;;
    image) cmd_image "${2:-a}" ;;
    run) cmd_run "${2:-a}" ;;
    test) cmd_test ;;
    all) cmd_build; cmd_test ;;
    *) sed -n '2,25p' "$0"; exit 2 ;;
esac
