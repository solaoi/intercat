"""Corrupt only an APFS copy of Glimmer and repair it through the real daemon."""
import argparse
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import sqlite3
import subprocess
import tempfile
import time
import uuid


def operation():
    value = bytearray(secrets.token_bytes(16))
    value[:6] = int(time.time() * 1000).to_bytes(6, 'big')
    value[6] = value[6] & 15 | 0x70
    value[8] = value[8] & 63 | 0x80
    return str(uuid.UUID(bytes=bytes(value)))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--python', required=True, type=Path)
    parser.add_argument('--model', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    repository = Path(__file__).resolve().parents[1]
    data = Path(tempfile.mkdtemp(prefix='ic-editor-', dir='/private/tmp'))
    target = data / 'editor-model/model'
    target.mkdir(parents=True)
    manifest = json.loads((repository / 'services/editor/src/intercat_editor/model_manifest.json').read_text())
    for entry in manifest['files']:
        subprocess.run(['/bin/cp', '-c', str(args.model / entry['path']), str(target / entry['path'])], check=True)
    original = (args.model / 'config.json').read_bytes()
    token = secrets.token_hex(32)
    secret_key = secrets.token_hex(32)
    process = None
    stream = None
    connection = None
    history = []
    sequence = 0
    receipt = None
    (args.output / 'attempt.json').write_text(json.dumps({'dataDirectory': str(data), 'paidTurns': 0}))

    def start():
        nonlocal process, stream, connection, receipt
        log = (args.output / 'daemon.log').open('ab')
        process = subprocess.Popen([str(repository / 'target/debug/intercatd'), 'serve', str(data), str(repository / 'services/editor/src')], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log,
                                   env={**os.environ, 'INTERCAT_GITLEAKS': str(repository / '.tools/gitleaks-8.30.1/gitleaks')})
        process.stdin.write(json.dumps({'token': token, 'secretKey': secret_key, 'receipt': receipt}).encode() + b'\n'); process.stdin.flush()
        hello = json.loads(process.stdout.readline())
        if 'fingerprints' not in hello: raise RuntimeError(f'Daemon bootstrap failed: {hello}')
        receipt = hello['fingerprints']
        process.stdin.write(json.dumps(hello['fingerprints']['catalog']).encode() + b'\n'); process.stdin.flush()
        deadline = time.monotonic() + 15
        while True:
            connection = socket.socket(socket.AF_UNIX)
            connection.settimeout(20)
            try:
                connection.connect(str(data / 'run/rpc.sock'))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                connection.close()
                if time.monotonic() > deadline: raise
                time.sleep(.05)
        stream = connection.makefile('rwb')
        rpc('rpc.authenticate', {'token': token})

    def rpc(method, params=None):
        nonlocal sequence
        sequence += 1
        stream.write(json.dumps({'jsonrpc': '2.0', 'id': sequence, 'method': method, 'params': params or {}}).encode() + b'\n'); stream.flush()
        value = json.loads(stream.readline())
        if 'error' in value: raise RuntimeError(value['error'])
        return value['result']

    def wait(check, seconds=180):
        deadline = time.monotonic() + seconds
        previous = None
        while time.monotonic() < deadline:
            status = rpc('system.status')
            state = status['editor']
            if state != previous:
                history.append(state); previous = state
                print(json.dumps({'phase': state['phase'], 'restart': state['restart'], 'download': state.get('download')}), flush=True)
            if check(status): return status
            time.sleep(.25)
        raise TimeoutError('Editor state did not arrive')

    def stop():
        nonlocal connection, stream, process
        if stream: stream.close(); stream = None
        if connection: connection.close(); connection = None
        if process and process.poll() is None:
            process.terminate()
            process.wait(timeout=20)

    try:
        start()
        # Preserve the virtualenv invocation path rather than resolving its symlink.
        config = {'python': str(args.python.absolute()), 'model': str(target)}
        rpc('editor.configure', {'opId': operation(), 'config': config})
        first = wait(lambda status: status['editor']['phase'] == 'ready')
        (target / 'config.json').write_text('{"corrupted": true}')
        os.kill(first['editor']['pid'], signal.SIGKILL)
        failed = wait(lambda status: status['editor']['canRetry'])
        assert failed['editor']['restart'] == 3 and failed['editor']['phase'] == 'unavailable'
        word = rpc('utterance.create', {'opId': operation(), 'text': 'Hold this dummy request until the local Editor is ready.', 'target': {'mode': 'new'}, 'selectedTrackId': None, 'attachmentIds': []})
        held = rpc('system.status')
        assert next(x for x in held['utterances'] if x['id'] == word['id'])['state'] == 'staged'
        op = operation()
        rpc('editor.download', {'opId': op})
        # A lost acknowledgement can be retried without another download process.
        rpc('editor.download', {'opId': op})
        repaired = wait(lambda status: status['editor']['phase'] == 'ready')
        assert (target / 'config.json').read_bytes() == original
        assert (args.model / 'config.json').read_bytes() == original
        catalog = rpc('secret.catalog')
        receipt = {'catalog': catalog['current'], 'proof': catalog['proof']}
        rpc('secret.confirm', receipt['catalog'])
        stop()
        start()
        restarted = wait(lambda status: status['editor']['phase'] == 'ready')
        stop()
        with sqlite3.connect(data / 'events.sqlite') as source, sqlite3.connect(args.output / 'evidence.sqlite') as destination:
            source.backup(destination)
        (args.output / 'states.json').write_text(json.dumps(history, indent=2))
        result = {'passed': True, 'retries': failed['editor']['restart'], 'heldInput': word['id'], 'repaired': repaired['editor'], 'restart': restarted['editor'], 'originalUnchanged': True, 'paidTurns': 0, 'dataDirectory': str(data)}
        (args.output / 'result.json').write_text(json.dumps(result, indent=2))
    finally:
        (args.output / 'states.json').write_text(json.dumps(history, indent=2))
        stop()


if __name__ == '__main__':
    main()
