#!/usr/bin/env python3
"""Attribute linked symbols, never discarded linker-map input sections.

Usage: scripts/profile-app.py ELF --nm path/to/riscv32-esp-elf-nm [--image app.bin]
The JSON is suitable for committing measurement evidence. LTO makes attribution
approximate: a symbol's name does not account for everything inlined into it.
"""
import argparse
import collections
import hashlib
import json
import re
import subprocess
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('elf', type=Path)
    parser.add_argument('--nm', required=True)
    parser.add_argument('--image', type=Path)
    parser.add_argument('--top', type=int, default=40)
    args = parser.parse_args()
    output = subprocess.check_output(
        [args.nm, '--defined-only', '--print-size', '--size-sort', '--radix=d',
         '--demangle', str(args.elf)], text=True)
    # Identical address/size pairs are linker aliases, not additional bytes.
    symbols = {}
    for line in output.splitlines():
        match = re.match(r'^\s*(\d+)\s+(\d+)\s+([tTrRdD])\s+(.+)$', line)
        if not match:
            continue
        address, size, kind, name = match.groups()
        symbols.setdefault((int(address), int(size)), (kind, name))
    buckets = collections.Counter()
    ranked = []
    for (address, size), (kind, name) in symbols.items():
        # First Rust path component, including <Type as Trait> generic symbols.
        match = re.search(r'([A-Za-z_][A-Za-z_0-9]*)::', name)
        owner = match.group(1) if match else 'C/assembly/unnamed'
        buckets[owner] += size
        ranked.append(dict(bytes=size, address=hex(address), kind=kind, name=name))
    report = dict(
        elf_sha256=hashlib.sha256(args.elf.read_bytes()).hexdigest(),
        named_initialized_symbol_bytes=sum(buckets.values()),
        attribution='First Rust path component; LTO attribution is approximate. '
                    'Includes initialized data, excludes BSS and debug sections. '
                    'Anonymous literals and alignment are not fully attributed.',
        symbols_by_owner=dict(buckets.most_common()),
        # These overlap owner buckets and each other; do not add to the total.
        protobuf_name_mentions_bytes=sum(row['bytes'] for row in ranked if 'waproto::' in row['name']),
        protobuf_drop_glue_bytes=sum(row['bytes'] for row in ranked
                                     if 'waproto::' in row['name'] and 'drop_glue' in row['name']),
        protobuf_clone_bytes=sum(row['bytes'] for row in ranked
                                 if 'waproto::' in row['name'] and 'clone' in row['name']),
        largest_symbols=sorted(ranked, key=lambda row: row['bytes'], reverse=True)[:args.top],
    )
    if args.image:
        image = args.image.read_bytes()
        report['image_bytes'] = len(image)
        report['image_sha256'] = hashlib.sha256(image).hexdigest()
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
