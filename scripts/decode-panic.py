#!/usr/bin/env python3
"""Resolve ESP panic addresses to function names using only the ELF symtab.

Runs on the crash path of `scripts/qemu.sh test`, where no ESP toolchain is
available -- just python3, the firmware ELF and the serial log. Reads the
SYMTAB/DYNSYM function symbols (32-bit LE or BE, Xtensa or RISC-V) and maps
every code address in the crash region to the nearest preceding function.

Usage: decode-panic.py ELF OUT_DIR SERIAL...
Appends one section per serial containing a crash signature to
OUT_DIR/qemu-panic-decoded.log (which the CI artifact glob already uploads).
Never fails: diagnostics must not break the test job.
"""
import re
import struct
import sys
from pathlib import Path

CRASH_RE = re.compile(
    r'RUST PANIC|Guru Meditation|abort\(\) was called|'
    r'rst:0x[0-9a-f]+ \((TG[01]WDT|RTCWDT|PANIC)'
)
REBOOT_RE = re.compile(r'^entry 0x[0-9a-f]+')
ADDR_RE = re.compile(r'0x[0-9a-fA-F]{7,8}')
REG_RE = re.compile(
    r'MEPC|MCAUSE|MTVAL|RA\s*:|PC\s*:|SP\s*:|EXCVADDR|Backtrace|'
    r'abort\(\)|Stack overflow|RUST PANIC|panic'
)
CODE_LO, CODE_HI = 0x40000000, 0x44000000


def symtab(path):
    """(sorted [(value, size, name)]) of STT_FUNC symbols, or []."""
    try:
        data = Path(path).read_bytes()
    except OSError as e:
        print(f'decode-panic: cannot read ELF: {e}', flush=True)
        return []
    if len(data) < 52 or data[:4] != b'\x7fELF' or data[4] != 1:
        print('decode-panic: not a 32-bit ELF', flush=True)
        return []
    bo = '<' if data[5] == 1 else '>'
    try:
        (shoff,) = struct.unpack_from(bo + 'I', data, 32)
        shentsize, shnum, _shstrndx = struct.unpack_from(bo + 'HHH', data, 46)
        sects = [struct.unpack_from(bo + 'IIIIIIIIII', data, shoff + i * shentsize)
                 for i in range(shnum)]
    except struct.error:
        print('decode-panic: bad section headers', flush=True)
        return []
    syms = []
    for sect in sects:
        if sect[1] not in (2, 11):  # SYMTAB or DYNSYM
            continue
        strs = sects[sect[6]]
        strings = data[strs[4]:strs[4] + strs[5]]
        for off in range(sect[4], sect[4] + sect[5], sect[9]):
            try:
                name, value, size, info, _other, shndx = \
                    struct.unpack_from(bo + 'IIIBBH', data, off)
            except struct.error:
                break
            # STT_FUNC only: skips $x/$d mapping symbols and NOTYPE labels.
            if value == 0 or shndx == 0 or (info & 0x0F) != 2:
                continue
            end = strings.find(b'\0', name)
            label = strings[name:end].decode(errors='replace')
            if label:
                syms.append((value, size, label))
    syms.sort()
    return syms


def resolve(syms, addr):
    if not (CODE_LO <= addr < CODE_HI):
        return 'data'
    best = None
    for value, size, name in syms:
        if value <= addr and (best is None or value > best[0]):
            best = (value, size, name)
    if best is None:
        return '???'
    value, size, name = best
    within = size and addr < value + size
    return f'{name}+0x{addr - value:x}' + ('' if within else ' (outside size)')


def decode(syms, serial):
    try:
        lines = Path(serial).read_text(errors='replace').splitlines()
    except OSError as e:
        return f'--- {serial}: cannot read serial log: {e}\n'
    start = next((i for i, l in enumerate(lines) if CRASH_RE.search(l)), None)
    if start is None:
        return None
    end = next((i for i in range(start + 1, len(lines)) if REBOOT_RE.match(lines[i])),
               len(lines))
    region = lines[start:end]
    out = [f'--- crash in {serial} (line {start + 1}) ---',
           f'signature: {lines[start].strip()}']
    shown_regs = 0
    for line in region:
        if REG_RE.search(line) and shown_regs < 25:
            def sub(m):
                return f'{m.group(0)}[{resolve(syms, int(m.group(0), 16))}]'
            out.append(ADDR_RE.sub(sub, line.rstrip()))
            shown_regs += 1
    addrs = sorted({int(m, 16) for line in region for m in ADDR_RE.findall(line)
                    if CODE_LO <= int(m, 16) < CODE_HI})
    out.append(f'unique code addresses in crash region ({len(addrs)}):')
    for addr in addrs[:60]:
        out.append(f'  {addr:#010x} {resolve(syms, addr)}')
    if len(addrs) > 60:
        out.append(f'  ... and {len(addrs) - 60} more')
    out.append('--- end ---')
    return '\n'.join(out) + '\n'


def main():
    if len(sys.argv) < 4:
        print('usage: decode-panic.py ELF OUT_DIR SERIAL...', flush=True)
        return 0
    elf, out_dir, serials = sys.argv[1], sys.argv[2], sys.argv[3:]
    syms = symtab(elf)
    if not syms:
        print('decode-panic: no function symbols, skipping', flush=True)
        return 0
    for serial in serials:
        report = decode(syms, serial)
        if report is None:
            print(f'decode-panic: no crash signature in {serial}', flush=True)
            continue
        try:
            with open(Path(out_dir) / 'qemu-panic-decoded.log', 'a') as f:
                f.write(report)
        except OSError as e:
            print(f'decode-panic: cannot write report: {e}', flush=True)
            return 0
        print(report, flush=True)
    return 0


if __name__ == '__main__':
    sys.exit(main())
