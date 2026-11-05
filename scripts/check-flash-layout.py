#!/usr/bin/env python3
"""Check the complete partition table against physical flash, plus app fit."""
import argparse
import csv
from pathlib import Path


def number(value):
    value = value.strip().removesuffix("B")
    if value[-1:].upper() in ('K', 'M'):
        return int(value[:-1], 0) * {'K': 1024, 'M': 1024**2}[value[-1].upper()]
    return int(value, 0)


def check(image, table, flash_size):
    end = 0x9000  # partition table occupies 0x8000..0x9000
    factory = None
    for row in csv.reader(table.read_text().splitlines()):
        if not row or row[0].lstrip().startswith('#'):
            continue
        name, kind, subtype, offset, size = [field.strip() for field in row[:5]]
        alignment = 0x10000 if kind == 'app' else 0x1000
        start = number(offset) if offset else (end + alignment - 1) // alignment * alignment
        length = number(size)
        if start % alignment or length <= 0 or length % 0x1000:
            raise ValueError(f'{name}: invalid offset/size alignment')
        if start < end:
            raise ValueError(f'{name}: overlaps previous partition or partition table')
        end = start + length
        if end > flash_size:
            raise ValueError(f'{name}: ends at {end:#x}, exceeds {flash_size:#x} flash')
        if kind == 'app' and subtype == 'factory':
            factory = length
    if factory is None:
        raise ValueError('no factory app partition')
    actual = image.stat().st_size
    if actual > factory:
        raise ValueError(f'app exceeds factory partition by {actual - factory} bytes')
    print(f'app: {actual} bytes; factory: {factory}; app margin: {factory - actual}; '
          f'layout end: {end:#x}; physical flash: {flash_size:#x}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('image', type=Path)
    parser.add_argument('table', type=Path)
    parser.add_argument('flash_size', type=number, help='e.g. 4M or 0x400000')
    args = parser.parse_args()
    try:
        check(args.image, args.table, args.flash_size)
    except ValueError as error:
        parser.exit(1, f'{error}\n')


if __name__ == '__main__':
    main()
