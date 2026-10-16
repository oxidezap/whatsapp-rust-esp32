#!/usr/bin/env python3
import hashlib
import json
import os
from pathlib import Path
import signal
import ssl
import subprocess
import time
import urllib.request

OUT = Path('target/qemu-esp32c3/riscv32imc-esp-espidf/release')
RESULTS = Path('validation-results')
RESULTS.mkdir(exist_ok=True)
PROCESSES = []


def reset_mock():
    container = os.environ.get('MOCK_CONTAINER_ID')
    if not container:
        raise RuntimeError('MOCK_CONTAINER_ID is required to isolate server state between repetitions')
    subprocess.run(['docker', 'restart', container], check=True, capture_output=True)
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen('https://127.0.0.1:8080/', timeout=3,
                                        context=ssl._create_unverified_context()):
                return
        except urllib.error.HTTPError:
            return
        except OSError:
            time.sleep(1)
    raise TimeoutError('mock server did not recover after reset')


def get(port, path):
    with urllib.request.urlopen(f'http://127.0.0.1:{port}{path}', timeout=3) as response:
        return json.load(response)


def snapshot(port, directory, stage):
    for endpoint in ('metrics', 'device', 'messages', ''):
        try:
            value = get(port, '/' + endpoint)
        except Exception as error:
            value = {'error': str(error)}
        (directory / f'{stage}-{endpoint or 'store'}.json').write_text(json.dumps(value, indent=2))


def wait_connected(process, serial, port, directory, stage, restored=False):
    deadline = time.monotonic() + 180
    while time.monotonic() < deadline:
        text = serial.read_text(errors='replace')
        if any(marker in text for marker in ('memory allocation of', 'abort() was called',
               'stack overflow', 'Guru Meditation', 'no mem for receive buffer')):
            raise RuntimeError(f'firmware failed at {stage}; see {serial}')
        if process.poll() is not None:
            raise RuntimeError(f'QEMU exited at {stage}: {process.returncode}')
        if 'Connected to WhatsApp!' in text:
            device = get(port, '/device')
            if device.get('connected'):
                if restored and ('QR CODE' in text or 'WhatsApp NVS loaded: device=true' not in text):
                    raise RuntimeError('reboot did not restore the paired device')
                snapshot(port, directory, stage)
                return device
        time.sleep(1)
    raise TimeoutError(f'connection timeout at {stage}')


def boot(name, directory, stage):
    serial = directory / f'{stage}-serial.log'
    stream = serial.open('w')
    process = subprocess.Popen(['scripts/qemu.sh', 'run', name], stdout=stream,
                               stderr=subprocess.STDOUT, start_new_session=True)
    stream.close()
    PROCESSES.append(process)
    return process, serial


def stop(process):
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    try:
        process.wait(timeout=10)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()


def messages(a, b, directory, stage):
    target = b['pn'].split('@')
    target[0] = target[0].split(':')[0].split('.')[0]
    payload = json.dumps({'to': '@'.join(target), 'text': '\U0001f980ping'}).encode()
    request = urllib.request.Request('http://127.0.0.1:8081/send', data=payload,
                                     headers={'Content-Type': 'application/json'})
    with urllib.request.urlopen(request, timeout=60) as response:
        sent = json.load(response)
    (directory / f'{stage}-send.json').write_text(json.dumps(sent))
    if sent.get('result') != 'sent':
        raise RuntimeError(f'send rejected: {sent}')
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        incoming_a = get(8081, '/messages').get('messages', [])
        incoming_b = get(8082, '/messages').get('messages', [])
        if (any('Pong!' in (m.get('text') or '') for m in incoming_a)
                and any('\U0001f980ping' in (m.get('text') or '') for m in incoming_b)):
            snapshot(8081, directory, stage + '-a')
            snapshot(8082, directory, stage + '-b')
            return
        time.sleep(1)
    raise TimeoutError('encrypted round trip did not complete')


def preserve_flash(directory, stage):
    for name in ('a', 'b'):
        source = OUT / f'flash_image-{name}.bin'
        if source.exists():
            destination = directory / f'{stage}-wa_store-{name}.bin'
            with source.open('rb') as stream:
                stream.seek(0x4E0000)
                destination.write_bytes(stream.read(0x100000))


for repetition in range(1, 4):
    directory = RESULTS / f'repeat-{repetition}'
    directory.mkdir(exist_ok=True)
    outcome = {'repeat': repetition, 'stages': [], 'result': 'failed'}
    try:
        reset_mock()
        for name in ('a', 'b'):
            subprocess.run(['scripts/qemu.sh', 'image', name], check=True,
                           stdout=subprocess.DEVNULL)
        initial = {}
        for name in ('a', 'b'):
            image = OUT / f'flash_image-{name}.bin'
            with image.open('rb') as stream:
                stream.seek(0x4E0000)
                data = stream.read(0x100000)
            if data != b'\xff' * len(data):
                raise RuntimeError('initial wa_store was not erased')
            initial[name] = hashlib.sha256(data).hexdigest()
        (directory / 'initial-nvs.json').write_text(json.dumps(initial))
        pa, sa = boot('a', directory, 'initial-a')
        a = wait_connected(pa, sa, 8081, directory, 'initial-a')
        outcome['stages'].append('initial-a-connected')
        pb, sb = boot('b', directory, 'initial-b')
        b = wait_connected(pb, sb, 8082, directory, 'initial-b')
        outcome['stages'].append('initial-b-connected')
        messages(a, b, directory, 'initial-messages')
        outcome['stages'].append('initial-encrypted-roundtrip')
        stop(pa)
        stop(pb)
        preserve_flash(directory, 'before-reboot')
        pa, sa = boot('a', directory, 'restored-a')
        a2 = wait_connected(pa, sa, 8081, directory, 'restored-a', restored=True)
        pb, sb = boot('b', directory, 'restored-b')
        b2 = wait_connected(pb, sb, 8082, directory, 'restored-b', restored=True)
        if a2['pn'] != a['pn'] or b2['pn'] != b['pn']:
            raise RuntimeError('device identity changed across reboot')
        outcome['stages'].append('both-restored')
        messages(a2, b2, directory, 'restored-messages')
        outcome['stages'].append('restored-encrypted-roundtrip')
        outcome['result'] = 'passed'
        outcome['appstate_sync_failures'] = sum(
            path.read_text(errors='replace').count('event: AppStateSyncFailed')
            for path in directory.glob('*-serial.log')
        )
        outcome['appstate_complete'] = outcome['appstate_sync_failures'] == 0
    except Exception as error:
        outcome['error'] = f'{type(error).__name__}: {error}'
    finally:
        for process in PROCESSES:
            stop(process)
        PROCESSES.clear()
        preserve_flash(directory, 'final')
        (directory / 'result.json').write_text(json.dumps(outcome, indent=2))
        print(json.dumps(outcome), flush=True)
        for serial in directory.glob('*-serial.log'):
            subprocess.run(['python3', 'scripts/decode-panic.py', str(OUT / 'whatsapp-esp32'),
                            str(directory), str(serial)], check=False,
                           stdout=subprocess.DEVNULL)

results = [json.loads(path.read_text()) for path in RESULTS.glob('repeat-*/result.json')]
raise SystemExit(0 if len(results) == 3 and all(r['result'] == 'passed' for r in results) else 1)
