#!/usr/bin/env bash
# Run cargo for one of the supported boards.
#
#   scripts/build.sh [--board esp32s3|esp32c5|esp32c3] [cargo build args...]
#   BOARD=esp32c5 scripts/build.sh --release
#   CARGO_CMD=clippy scripts/build.sh --board esp32c3 -- -D warnings
#
# A board is a target triple, whether the chip has PSRAM, and the ESP-IDF version
# and MCU name esp-idf-sys builds against -- all of it in scripts/boards.sh, which
# scripts/qemu.sh shares. Everything else (partitions, source) is common, with the
# chip's own sdkconfig.defaults.<mcu> picked up by ESP-IDF itself.
# `cargo build` with no wrapper is the esp32s3 case (see .cargo/config.toml).
set -euo pipefail

cd "$(dirname "$0")/.."
source scripts/boards.sh

usage() {
    echo "usage: $0 [--board ${BOARD_NAMES[*]}] [cargo args...]" >&2
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

board_select "$BOARD" || usage

# Process environment beats .cargo/config.toml's [env] block, so this is the
# whole board switch.
export MCU="$BOARD" ESP_IDF_VERSION="$BOARD_ESP_IDF_VERSION"
export ESP_IDF_SDKCONFIG_DEFAULTS="$(board_sdkconfig_defaults)"
exec cargo "${CARGO_CMD:-build}" --target "$BOARD_TARGET" "$@"
