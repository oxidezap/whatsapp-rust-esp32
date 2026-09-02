#!/usr/bin/env bash
# Fail when the app image no longer fits the factory partition in partitions.csv.
#
#   scripts/check-app-size.sh <app.bin> [partitions.csv]
#
# espflash only reports this at flash time, and CI never flashes, so the gate lives
# here. The margin printed is the one to watch when adding dependencies.
set -euo pipefail

app="${1:?app image}"
table="${2:-partitions.csv}"

# The factory app row: "factory, app, factory, 0x10000, 0x4C0000,". Size may be
# hex or a K/M suffix per the ESP-IDF partition-table grammar.
size_field="$(awk -F, '$2 ~ /app/ && $3 ~ /factory/ { gsub(/[[:space:]]/, "", $5); print $5; exit }' "$table")"
test -n "$size_field" || { echo "no factory app partition in $table" >&2; exit 2; }
case "$size_field" in
    0x*|0X*) part=$((size_field)) ;;
    *K|*k) part=$(( ${size_field%[Kk]} * 1024 )) ;;
    *M|*m) part=$(( ${size_field%[Mm]} * 1024 * 1024 )) ;;
    *) part=$((size_field)) ;;
esac

actual="$(stat -c %s "$app")"
margin=$((part - actual))
printf 'app image: %d bytes, factory partition: %d bytes, margin: %d bytes (%.1f%% used)\n' \
    "$actual" "$part" "$margin" "$(awk "BEGIN { print 100 * $actual / $part }")"
if (( actual > part )); then
    echo "app image exceeds the factory partition by $((actual - part)) bytes" >&2
    exit 1
fi
