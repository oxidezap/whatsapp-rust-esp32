# The board table: the one place that says what a supported board is.
#
# Sourced by scripts/build.sh and scripts/qemu.sh, which otherwise had the same
# case statement twice and would drift the moment a board is added. Adding a
# board is adding one row here plus its `sdkconfig.defaults.<name>` overlay.
#
# Each row is: the Rust target triple, whether the chip has PSRAM (which decides
# whether `sdkconfig.psram` is layered in, and through `CONFIG_SPIRAM` the
# stack sizes and allocator the firmware compiles with), and how Espressif's
# QEMU emulates it, if it does at all.

# The chips Espressif's QEMU has a machine for are esp32, esp32c3 and esp32s3;
# BOARD_QEMU_BIN is empty for anything else, which is what scripts/qemu.sh
# refuses on. See docs/board-support-map.md.
board_select() {
    BOARD_NAME="$1"
    BOARD_QEMU_BIN=""
    BOARD_QEMU_MACHINE_ARGS=()
    case "$BOARD_NAME" in
        esp32s3)
            BOARD_TARGET=xtensa-esp32s3-espidf
            BOARD_HAS_PSRAM=1
            BOARD_QEMU_BIN="${QEMU_XTENSA:-qemu-system-xtensa}"
            # The board has 8 MB of PSRAM; the esp32s3 machine takes its size here.
            BOARD_QEMU_MACHINE_ARGS=(-M esp32s3 -m 8M)
            ;;
        esp32c5)
            BOARD_TARGET=riscv32imac-esp-espidf
            BOARD_HAS_PSRAM=1
            ;;
        esp32c3)
            # rv32imc, not the C5's rv32imac: the C3 has no hardware atomics.
            BOARD_TARGET=riscv32imc-esp-espidf
            BOARD_HAS_PSRAM=0
            # No -m: the C3 has no PSRAM, and its machine has no such knob. The
            # ~400 KB of on-chip SRAM is the whole memory system.
            BOARD_QEMU_BIN="${QEMU_RISCV32:-qemu-system-riscv32}"
            BOARD_QEMU_MACHINE_ARGS=(-M esp32c3)
            ;;
        *)
            echo "unknown board '$BOARD_NAME' (${BOARD_NAMES[*]})" >&2
            return 2
            ;;
    esac
}

# Every board name, for usage messages and CI matrices.
BOARD_NAMES=(esp32s3 esp32c5 esp32c3)

# The ESP-IDF version every board is built against.
BOARD_ESP_IDF_VERSION="v5.5.5"

# The SDKCONFIG_DEFAULTS list for the selected board, plus any extra overlays
# passed as arguments (scripts/qemu.sh adds sdkconfig.qemu).
#
# `sdkconfig.psram` carries everything that only means something on a chip with
# external RAM, so the ESP32-C3 simply does not get that file rather than having
# to switch a dozen symbols back off. ESP-IDF loads `<file>.<target>` right after
# each file that brings it in, so the per-chip overlays (sdkconfig.defaults.esp32c3,
# sdkconfig.qemu.esp32s3, ...) are picked up here without being named.
board_sdkconfig_defaults() {
    local files=("$PWD/sdkconfig.defaults") extra
    if [[ "$BOARD_HAS_PSRAM" == 1 ]]; then
        files+=("$PWD/sdkconfig.psram")
    fi
    for extra in "$@"; do
        files+=("$PWD/$extra")
    done
    local IFS=';'
    printf '%s' "${files[*]}"
}

# Where cargo puts this board's artifacts, for the callers that have to name the
# ELF: $1 is the cargo target dir (default `target`), $2 the profile directory
# (default `release`). Keeping the triple in this file means CI never spells one.
board_out_dir() {
    printf '%s/%s/%s' "${1:-target}" "$BOARD_TARGET" "${2:-release}"
}

# The cargo target dir scripts/qemu.sh builds this board into. Its DEPTH is load
# bearing (see the partitions.csv note in sdkconfig.qemu), so it is one function
# rather than a string assembled at each call site.
board_qemu_target_dir() {
    printf 'target/qemu-%s' "$BOARD_NAME"
}
