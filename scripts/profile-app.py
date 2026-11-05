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
import struct
from pathlib import Path


def elf_sections(data):
    # All supported ESP-IDF targets emit little-endian ELF32.
    if data[:6] != b"\x7fELF\x01\x01":
        raise ValueError('expected a little-endian ELF32 image')
    offset = struct.unpack_from('<I', data, 32)[0]
    stride, count, names_index = struct.unpack_from('<HHH', data, 46)
    headers = [struct.unpack_from('<10I', data, offset + i * stride) for i in range(count)]
    names = headers[names_index]
    strings = data[names[4]:names[4] + names[5]]
    return [dict(name=strings[h[0]:].split(b'\0', 1)[0].decode(),
                 address=h[3], bytes=h[5], file_backed=h[1] != 8)
            for h in headers if h[2] & 2]


def verify_map(sections, contents):
    text = next(section for section in sections if section['name'] == '.flash.text')
    match = re.search(r'^\.flash\.text\s+0x([0-9a-fA-F]+)\s+0x([0-9a-fA-F]+)', contents, re.M)
    if not match or (int(match[1], 16), int(match[2], 16)) != (text['address'], text['bytes']):
        raise ValueError('linker map .flash.text does not match the ELF; refusing stale attribution')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('elf', type=Path)
    parser.add_argument('--nm', required=True)
    parser.add_argument('--image', type=Path)
    parser.add_argument('--map', type=Path, help='verify linker-map text address/size against the ELF')
    parser.add_argument('--top', type=int, default=40)
    args = parser.parse_args()
    elf = args.elf.read_bytes()
    sections = elf_sections(elf)
    if args.map:
        try:
            verify_map(sections, args.map.read_text())
        except ValueError as error:
            parser.exit(1, f'{error}\n')
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
        elf_sha256=hashlib.sha256(elf).hexdigest(),
        allocated_elf_sections=sections,
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
    if args.map:
        report["map_sha256"] = hashlib.sha256(args.map.read_bytes()).hexdigest()
    if args.image:
        image = args.image.read_bytes()
        report['image_bytes'] = len(image)
        report['image_sha256'] = hashlib.sha256(image).hexdigest()
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
