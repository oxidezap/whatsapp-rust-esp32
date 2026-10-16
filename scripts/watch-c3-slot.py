#!/usr/bin/env python3
"""Lean C3 slot watch for the PR #15 QEMU triage.

The fault-proof heap walk reports a deterministic victim header
(currently 0x3fc9168c, raw 0xa5a5a5a5) corrupted between the last clean
telemetry and the first ServerAck. This script arms ONE hardware write
watchpoint on that header while QEMU is halted (-S), then lets the target
run free: no breakpoints, no single-stepping, negligible timing impact
(unlike the earlier task-metadata probe).

On a hit it prints the writer's registers and nearby stack words and
continues; the offline decoder (scripts/decode-panic.py) resolves the PC.
If the writer is a task-stack creation memset overlapping live heap, the
backtrace names the task creator.

Usage: watch-c3-slot.py ELF OUTFILE [ADDR]
Never fails loudly: any error is recorded in OUTFILE and the target is
resumed, so a diagnostic failure cannot wedge the test job.
"""
import socket
import sys
import time
from pathlib import Path

SLOT_DEFAULT = 0x3FC9168C
# Serial marker after which the fault window opens (70 sync keys stored,
# ~5 history messages, app-state sync, then the first ServerAck). Arming
# earlier catches boot-time pool init (thousands of legitimate hits and a
# wedged target); arming here leaves only in-window writers.
MARKER = 'Processed app state key share'


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

    def interrupt(self):
        self.connection.sendall(b'\x03')

    def registers(self):
        raw = bytes.fromhex(self.command('g'))
        return [int.from_bytes(raw[i:i + 4], 'little') for i in range(0, 132, 4)]


def main():
    elf, outfile, serial = sys.argv[1], sys.argv[2], sys.argv[3]
    slot = int(sys.argv[4], 0) if len(sys.argv) > 4 else SLOT_DEFAULT
    log = open(outfile, 'w')
    def emit(line):
        print(line, flush=True)
        log.write(line + '\n')
        log.flush()
    try:
        deadline = time.monotonic() + 60
        while True:
            try:
                connection = socket.create_connection(('127.0.0.1', 1234), timeout=1)
                break
            except OSError:
                if time.monotonic() >= deadline:
                    raise
                time.sleep(0.1)
        connection.settimeout(600)
        remote = Remote(connection)
        # Let the target boot undisturbed; the slot is legitimately written
        # during pool init, so arming now would spam and wedge the run.
        remote.send('c')
        emit('target resumed, waiting for marker')
        armed = False
        start = time.monotonic()
        while not armed:
            if time.monotonic() - start > 540:
                emit('marker never appeared, leaving target running')
                connection.close()
                return 0
            try:
                text = Path(serial).read_text(errors='replace')
            except OSError:
                text = ''
            if MARKER in text:
                remote.interrupt()
                stop = remote.packet()
                emit(f'interrupted at marker: {stop}')
                before = remote.memory(slot, 4)
                emit(f'SLOT {slot:#x} before={before.hex()}')
                try:
                    response = remote.command(f'Z2,{slot:x},4')
                    emit(f'WATCH armed on {slot:#x}: {response}')
                except RuntimeError as e:
                    emit(f'WATCH unsupported: {e}')
                    remote.send('c')
                    connection.close()
                    return 0
                armed = True
            else:
                time.sleep(0.2)
        remote.send('c')
        while True:
            stop = remote.packet()
            regs = remote.registers()
            after = remote.memory(slot, 4)
            emit(f'HIT stop={stop} pc={regs[32]:08x} ra={regs[1]:08x} sp={regs[2]:08x} '
                 f's0={regs[8]:08x} slot={after.hex()}')
            emit('registers ' + ' '.join(f'x{i}={value:08x}' for i, value in enumerate(regs[:32])))
            try:
                sp = regs[2]
                words = remote.memory(sp, 128)
                parts = []
                for i in range(0, 128, 4):
                    parts.append('%08x' % int.from_bytes(words[i:i+4], 'little'))
                emit('stack ' + ' '.join(parts))
            except RuntimeError as e:
                emit(f'stack unreadable: {e}')
            # Keep watching: resume the target so the run continues.
            remote.send('c')
    except Exception as e:  # noqa: BLE001 - diagnostics must not wedge CI
        emit(f'watch-c3-slot failed: {e!r}')
        return 0


if __name__ == '__main__':
    main()
