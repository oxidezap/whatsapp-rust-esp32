#!/usr/bin/env bash
# Run cargo for one of the supported boards.
#
#   scripts/build.sh [--board esp32s3|esp32c5] [cargo build args...]
#   BOARD=esp32c5 scripts/build.sh --release
#   CARGO_CMD=clippy scripts/build.sh --board esp32c5 -- -D warnings
#
# A board is a target triple plus the ESP-IDF version and MCU name esp-idf-sys
# builds against; everything else (sdkconfig, partitions, source) is shared, with
# the chip-specific sdkconfig picked up automatically from sdkconfig.defaults.<mcu>.
# `cargo build` with no wrapper is the esp32s3 case (see .cargo/config.toml).
set -euo pipefail

cd "$(dirname "$0")/.."

usage() {
    echo "usage: $0 [--board esp32s3|esp32c5] [cargo args...]" >&2
    exit 2
}

BOARD="${BOARD:-esp32s3}"
if [[ "${1:-}" == "--board" ]]; then
    # Guard before expanding $2: `set -u` would otherwise abort with an
    # unbound-variable error instead of this usage line.
    [[ $# -ge 2 ]] || usage
    BOARD="$2"
    shift 2
fi

case "$BOARD" in
    esp32s3)
        TARGET=xtensa-esp32s3-espidf
        IDF=v5.4
        ;;
    esp32c5)
        TARGET=riscv32imac-esp-espidf
        # The C5 is not in v5.4; v5.5.5 also carries the USB Serial/JTAG reset
        # workaround the Waveshare board needs (earlier v5.5.x lose the console
        # after a reset).
        IDF=v5.5.5
        ;;
    *)
        echo "unknown board '$BOARD' (esp32s3 or esp32c5)" >&2
        usage
        ;;
esac

# Process environment beats .cargo/config.toml's [env] block, so this is the
# whole board switch. Both ESP-IDF versions install side by side under .embuild.
export MCU="$BOARD" ESP_IDF_VERSION="$IDF"
exec cargo "${CARGO_CMD:-build}" --target "$TARGET" "$@"
