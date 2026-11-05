"""One paid dummy turn: hold while real Glimmer is broken, then repair and dispatch."""
import argparse
import hashlib
import hmac
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

from verify_editor_repair import operation


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--python', required=True, type=Path)
    parser.add_argument('--model', required=True, type=Path)
    parser.add_argument('--muse', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--auth', type=Path, default=Path.home() / '.config/muse/auth.json')
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    repository = Path(__file__).resolve().parents[1]
    data = Path(tempfile.mkdtemp(prefix='ic-ed-flow-', dir='/private/tmp'))
    source = data / 'source'
    source.mkdir()
    (source / 'README.md').write_text('Disposable Editor recovery fixture. No project code.\n')
    for command in [['init', '-q', '-b', 'main'], ['add', '.'], ['-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'Fixture']]:
        subprocess.run(['/usr/bin/git', '-C', str(source), *command], check=True)
    model = data / 'editor-model/model'
    model.mkdir(parents=True)
    manifest = json.loads((repository / 'services/editor/src/intercat_editor/model_manifest.json').read_text())
    for entry in manifest['files']:
        subprocess.run(['/bin/cp', '-c', str(args.model / entry['path']), str(model / entry['path'])], check=True)
    original = (args.model / 'config.json').read_bytes()
    token, key = secrets.token_hex(32), secrets.token_hex(32)
    process = None
    connection = None
    stream = None
    sequence = 0
    observations = []
    passed = False
    info = {'previousPaidTurns': 151, 'maximumExpectedPaidTurns': 1, 'dataDirectory': str(data), 'source': str(source)}
    (output / 'attempt.json').write_text(json.dumps(info, indent=2))

    def rpc(method, params=None):
        nonlocal sequence
        sequence += 1
        stream.write(json.dumps({'jsonrpc': '2.0', 'id': sequence, 'method': method, 'params': params or {}}).encode() + b'\n'); stream.flush()
        reply = json.loads(stream.readline())
        if 'error' in reply: raise RuntimeError(f'{method}: {reply["error"]}')
        return reply['result']

    def turn_count():
        with sqlite3.connect(data / 'events.sqlite') as database:
            return database.execute("SELECT count(*) FROM operations WHERE kind='turn.start'").fetchone()[0]

    def wait(check, timeout=180):
        deadline = time.monotonic() + timeout
        previous = None
        while time.monotonic() < deadline:
            state = rpc('system.status')
            brief = {'editor': state['editor'], 'words': [{'id': word['id'], 'state': word['state'], 'decision': word['decision']} for word in state['utterances']], 'workers': [{'trackId': worker['trackId'], 'phase': worker['phase'], 'turn': worker['activeTurn'], 'checkpoint': worker['checkpoint']} for worker in state['workers']]}
            if brief != previous:
                observations.append({'at': time.time(), **brief}); previous = brief
                print(json.dumps({'editor': state['editor']['phase'], 'restart': state['editor']['restart'], 'words': [word['state'] for word in state['utterances']], 'workers': [worker['phase'] for worker in state['workers']]}), flush=True)
            if check(state): return state
            if turn_count() > 1: raise RuntimeError('More than the expected one dummy turn was started')
            time.sleep(.2)
        raise TimeoutError('The expected recovery and dispatch state did not arrive')

    try:
        process = subprocess.Popen([str(repository / 'target/debug/intercatd'), 'serve', str(data), str(repository / 'services/editor/src')], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=(output / 'daemon.log').open('ab'), env={**os.environ, 'INTERCAT_GITLEAKS': str(repository / '.tools/gitleaks-8.30.1/gitleaks')})
        process.stdin.write(json.dumps({'token': token, 'secretKey': key, 'receipt': None}).encode() + b'\n'); process.stdin.flush()
        hello = json.loads(process.stdout.readline())
        process.stdin.write(json.dumps(hello['fingerprints']['catalog']).encode() + b'\n'); process.stdin.flush()
        deadline = time.monotonic() + 15
        while True:
            connection = socket.socket(socket.AF_UNIX); connection.settimeout(20)
            try: connection.connect(str(data / 'run/rpc.sock')); break
            except (FileNotFoundError, ConnectionRefusedError):
                connection.close()
                if time.monotonic() > deadline: raise
                time.sleep(.05)
        stream = connection.makefile('rwb')
        rpc('rpc.authenticate', {'token': token})
        rpc('project.configure', {'opId': operation(), 'path': str(source)})
        project = rpc('system.status')['projectSecrets']
        check = hmac.new(bytes.fromhex(key), b'intercat.secret-fingerprints.v1', hashlib.sha256).hexdigest()
        catalog = rpc('secret.project.prepare', {'opId': operation(), 'projectId': project['projectId'], 'revision': 0, 'fingerprints': [], 'keyCheck': check})
        rpc('secret.confirm', catalog['current'])
        rpc('secret.project.activate', {'projectId': project['projectId'], 'revision': 0, 'entries': []})
        rpc('worker.configure', {'opId': operation(), 'source': str(args.muse.resolve()), 'credentials': str(args.auth.resolve())})
        rpc('editor.configure', {'opId': operation(), 'config': {'python': str(args.python.absolute()), 'model': str(model)}})
        ready = wait(lambda state: state['editor']['phase'] == 'ready')
        (model / 'config.json').write_text('{"damaged": true}')
        os.kill(ready['editor']['pid'], signal.SIGKILL)
        wait(lambda state: state['editor']['canRetry'])
        instruction = 'Start now. Use write_file to create after-recovery.txt containing exactly EDITOR_RECOVERY_DELIVERY_OK followed by a newline. Do not use other tools, change another file, or ask a question. Report completion and finish.'
        word = rpc('utterance.create', {'opId': operation(), 'text': instruction, 'target': {'mode': 'new'}, 'selectedTrackId': None, 'attachmentIds': []})
        time.sleep(1)
        before = rpc('system.status')
        assert next(item for item in before['utterances'] if item['id'] == word['id'])['state'] == 'staged'
        assert turn_count() == 0
        (output / 'held.json').write_text(json.dumps({'inputId': word['id'], 'turnStarts': 0, 'editor': before['editor']}, indent=2))
        repair_op = operation()
        rpc('editor.download', {'opId': repair_op})
        def finished(state):
            for item in state['utterances']:
                if item['id'] == word['id'] and item['state'] in ['asked', 'held', 'parked']:
                    raise RuntimeError(f'Input was not automatically dispatched: {item}')
            return any(worker.get('checkpoint') and not worker['checkpointPending'] for worker in state['workers'])
        final = wait(finished, 240)
        worker = next(worker for worker in final['workers'] if worker.get('checkpoint'))
        workspace = next(space for space in final['workspaces'] if space['trackId'] == worker['trackId'])
        content = (Path(workspace['path']) / 'after-recovery.txt').read_text()
        assert content == 'EDITOR_RECOVERY_DELIVERY_OK\n'
        assert not (source / 'after-recovery.txt').exists()
        assert (args.model / 'config.json').read_bytes() == original
        assert turn_count() == 1
        result = {'passed': True, 'inputId': word['id'], 'repairOperation': repair_op, 'worker': worker, 'workspace': workspace, 'input': next(item for item in final['utterances'] if item['id'] == word['id']), 'content': content, 'sourceUnchanged': True, 'modelOriginalUnchanged': True, 'paidTurns': 1, 'cumulativePaidTurns': 152}
        (output / 'result.json').write_text(json.dumps(result, indent=2))
        passed = True
    finally:
        (output / 'states.json').write_text(json.dumps(observations, indent=2))
        if stream: stream.close()
        if connection: connection.close()
        if process and process.poll() is None:
            process.terminate()
            try: process.wait(timeout=20)
            except subprocess.TimeoutExpired: process.kill(); process.wait()
        if (data / 'events.sqlite').exists():
            with sqlite3.connect(data / 'events.sqlite') as database, sqlite3.connect(output / 'evidence.sqlite') as target:
                database.backup(target)
                count = database.execute("SELECT count(*) FROM operations WHERE kind='turn.start'").fetchone()[0]
                info.update(actualPaidTurns=count, cumulativePaidTurns=151+count, passed=passed)
                (output / 'attempt.json').write_text(json.dumps(info, indent=2))
        if (data / 'editor.log').exists():
            (output / 'editor.log').write_bytes((data / 'editor.log').read_bytes())


if __name__ == '__main__':
    main()
