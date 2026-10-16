#!/usr/bin/env python3
"""Temporary ESP-IDF 5.5.5/RV32 task-metadata write probe for the C3 CI crash."""
import socket
import struct
import sys
import time
from pathlib import Path


def symbols(path):
    data = Path(path).read_bytes()
    header = struct.unpack_from('<16sHHIIIIIHHHHHH', data)
    assert header[0][:6] == b'\x7fELF\x01\x01' and header[2] == 243
    sections = [struct.unpack_from('<IIIIIIIIII', data, header[6] + i * header[11])
                for i in range(header[12])]
    result = {}
    for section in sections:
        if section[1] != 2:
            continue
        strings = sections[section[6]]
        names = data[strings[4]:strings[4] + strings[5]]
        for offset in range(section[4], section[4] + section[5], section[9]):
            name, value, size, *_ = struct.unpack_from('<IIIBBH', data, offset)
            end = names.find(b'\0', name)
            result[names[name:end].decode(errors='replace')] = (value, size)
    return result


def decode(payload):
    out = bytearray()
    i = 0
    while i < len(payload):
        c = payload[i]
        i += 1
        if c == ord('}'):
            out.append(payload[i] ^ 0x20)
            i += 1
        elif c == ord('*'):
            out.extend(out[-1:] * (payload[i] - 29))
            i += 1
        else:
            out.append(c)
    return out.decode()


class Remote:
    def __init__(self, connection):
        self.connection = connection

    def byte(self):
        value = self.connection.recv(1)
        if not value:
            raise EOFError('QEMU disconnected')
        return value

    def send(self, command):
        payload = command.encode()
        self.connection.sendall(b'$' + payload + b'#' + f'{sum(payload) % 256:02x}'.encode())

    def packet(self):
        while self.byte() != b'$':
            pass
        payload = bytearray()
        while True:
            value = self.byte()
            if value == b'#':
                break
            payload.extend(value)
        checksum = int(self.byte() + self.byte(), 16)
        assert sum(payload) % 256 == checksum
        self.connection.sendall(b'+')
        return decode(payload)

    def command(self, command):
        self.send(command)
        result = self.packet()
        if result.startswith('E'):
            raise RuntimeError(f'{command}: {result}')
        return result

    def memory(self, address, length):
        return bytes.fromhex(self.command(f'm{address:x},{length:x}'))

    def u32(self, address):
        return int.from_bytes(self.memory(address, 4), 'little')

    def registers(self):
        raw = bytes.fromhex(self.command('g'))
        return [int.from_bytes(raw[i:i + 4], 'little') for i in range(0, 132, 4)]

    def point(self, add, kind, address, size=4):
        response = self.command(f'{"Z" if add else "z"}{kind},{address:x},{size:x}')
        if response != 'OK':
            raise RuntimeError(f'unsupported watchpoint: {response!r}')


def ram(address):
    return 0x3FC80000 <= address < 0x3FCE0000


def trace(remote, names, serial):
    entry = next(value[0] for name, value in names.items()
                 if name.endswith('18log_memory_profile') and 'metrics' in name)
    remote.command('qSupported:swbreak+;hwbreak+')
    remote.command('Hg0')
    remote.point(True, 0, entry, 2)
    stop = remote.command('c')
    while True:
        regs = remote.registers()
        if regs[32] != entry:
            raise RuntimeError(f'unexpected initial stop: {stop}, pc={regs[32]:08x}')
        remote.point(False, 0, entry, 2)
        if 'Processed app state key share: 70 stored' in Path(serial).read_text(errors='replace'):
            break
        remote.command('s')
        remote.point(True, 0, entry, 2)
        stop = remote.command('c')

    # These offsets come from this pinned IDF's TCB_t/List_t DWARF. The C3 is
    # single-core, and the stopped CPU makes this a consistent list snapshot.
    roots = []
    ready, size = names['pxReadyTasksLists']
    roots.extend(ready + offset for offset in range(0, size, 20))
    for name in ('xDelayedTaskList1', 'xDelayedTaskList2', 'xSuspendedTaskList',
                 'xPendingReadyList', 'xTasksWaitingTermination'):
        if name in names:
            roots.append(names[name][0])
    ends = {root + 8 for root in roots}
    tasks = {}
    watch = {}

    def add_task(owner):
        if owner in tasks:
            return
        if not ram(owner) or remote.u32(owner + 16) != owner:
            raise RuntimeError(f'invalid TCB before watch setup: {owner:08x}')
        name = remote.memory(owner + 52, 16).split(b'\0')[0].decode(errors='replace')
        start, end = remote.u32(owner + 48), remote.u32(owner + 68)
        tasks[owner] = (name, start, end)
        print(f'TASK {owner:08x} {name!r} stack={start:08x}..{end:08x}', flush=True)
        for offset, label in ((16, 'state-owner'), (36, 'event-owner'),
                              (48, 'stack-base'), (52, 'name'), (68, 'stack-end')):
            address = owner + offset
            watch[address] = (f'{name}:{label}', remote.u32(address))
        for offset, label in ((8, 'state-next'), (12, 'state-prev')):
            watch[owner + offset] = (f'{name}:{label}', None)

    for root in roots:
        count = remote.u32(root)
        if count > 64:
            raise RuntimeError(f'bad list count before setup at {root:08x}: {count}')
        node = remote.u32(root + 12)
        seen = set()
        while node != root + 8:
            if node in seen or len(seen) > 64 or not ram(node):
                raise RuntimeError(f'invalid task list before setup: {root:08x} node={node:08x}')
            seen.add(node)
            add_task(remote.u32(node + 12))
            node = remote.u32(node + 4)

    suspended = names['xSuspendedTaskList'][0]
    watch[suspended] = ('suspended:count', 'count')
    watch[suspended + 8] = ('suspended:end-value', remote.u32(suspended + 8))
    for offset, label in ((4, 'cursor'), (12, 'next'), (16, 'prev')):
        watch[suspended + offset] = (f'suspended:{label}', None)
    print(f'WATCH armed at pc={regs[32]:08x}, sp={regs[2]:08x}, tasks={len(tasks)}, words={len(watch)}', flush=True)

    for address in watch:
        remote.point(True, 2, address)
    remote.send('c')
    while True:
        stop = remote.packet()
        before = remote.registers()
        for address in watch:
            remote.point(False, 2, address)
        remote.command('s')
        after = remote.registers()
        bad = []
        known_nodes = ends | {owner + 4 for owner in tasks} | {owner + 24 for owner in tasks}
        for address, (label, expected) in list(watch.items()):
            value = remote.u32(address)
            valid = (value < 65 if expected == 'count' else
                     value == expected if expected is not None else value in known_nodes)
            if not valid and expected is None and ram(value):
                owner = remote.u32(value + 12)
                if ram(owner) and value in (owner + 4, owner + 24) and remote.u32(owner + 16) == owner:
                    add_task(owner)
                    valid = True
            if not valid:
                bad.append(f'{label}@{address:08x}: expected={expected!r}, actual={value:08x}')
        if bad:
            current = remote.u32(names['pxCurrentTCBs'][0])
            print(f'FIRST INVALID TASK WRITE stop={stop}', flush=True)
            print(f'writer pc={before[32]:08x} after_pc={after[32]:08x} ra={before[1]:08x} sp={before[2]:08x} task={current:08x} bounds={tasks.get(current)}', flush=True)
            print('registers ' + ' '.join(f'x{i}={value:08x}' for i, value in enumerate(before[:32])), flush=True)
            print('\n'.join(bad), flush=True)
            remote.send('c')
            return
        for address in watch:
            remote.point(True, 2, address)
        remote.send('c')


def main():
    names = symbols(sys.argv[1])
    deadline = time.monotonic() + 60
    while True:
        try:
            connection = socket.create_connection(('127.0.0.1', 1234), timeout=1)
            break
        except OSError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.1)
    connection.settimeout(None)
    remote = Remote(connection)
    try:
        trace(remote, names, sys.argv[2])
    finally:
        # A diagnostic failure must not leave the normal crash detector paused.
        try:
            remote.send('c')
        except OSError:
            pass
        connection.close()


if __name__ == '__main__':
    main()
