"""One paid dummy turn against the actual daemon; never print its RPC token."""
import argparse
import ctypes
import json
import os
from pathlib import Path
import secrets
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
from verify_host_hooks import command_id


def exposure(pid, needle):
    """Inspect only this known process; retain counts and booleans, not bytes."""
    library = ctypes.CDLL(None, use_errno=True)
    mib = (ctypes.c_int * 3)(1, 49, pid)  # CTL_KERN, KERN_PROCARGS2
    size = ctypes.c_size_t()
    if library.sysctl(mib, 3, None, ctypes.byref(size), None, 0) != 0:
        raise OSError(ctypes.get_errno(), 'Cannot inspect owned process')
    if not 4 < size.value <= 2 * 1024 * 1024:
        raise ValueError('Invalid owned process metadata size')
    memory = ctypes.create_string_buffer(size.value)
    try:
        if library.sysctl(mib, 3, memory, ctypes.byref(size), None, 0) != 0:
            raise OSError(ctypes.get_errno(), 'Cannot inspect owned process')
        payload = memory.raw[:size.value]
        argc = int.from_bytes(payload[:4], 'little', signed=True)
        if not 0 < argc < 65536:
            raise ValueError('Invalid owned process argument count')
        cursor = payload.index(b'\0', 4) + 1
        while payload[cursor] == 0:
            cursor += 1
        argv = []
        for _ in range(argc):
            end = payload.index(b'\0', cursor)
            argv.append(payload[cursor:end])
            cursor = end + 1
        environ = [part for part in payload[cursor:].split(b'\0') if part]
        return {'argvCount': argc, 'environmentCount': len(environ),
                'tokenInArguments': any(needle in part for part in argv),
                'tokenInEnvironment': any(needle in part for part in environ)}
    finally:
        ctypes.memset(memory, 0, len(memory))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--muse', required=True, type=Path)
    parser.add_argument('--auth', type=Path, default=Path.home() / '.config/muse/auth.json')
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    temporary = tempfile.TemporaryDirectory(prefix='ic-rpc-', dir='/private/tmp')
    seed = subprocess.run(['cargo', 'run', '--offline', '-q', '-p', 'intercat-daemon', '--example', 'rpc_boundary_fixture', '--', str(Path(temporary.name) / 'fixture'), str(args.muse.resolve()), str(args.auth.resolve())], check=True, capture_output=True, text=True)
    fixture = json.loads(seed.stdout)
    data = Path(fixture['data'])
    editor = Path(temporary.name) / 'editor'
    package = editor / 'intercat_editor'
    package.mkdir(parents=True)
    (package / '__init__.py').write_text('')
    (package / 'server.py').write_text('''import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_GET(self):
  body=b'{"accepting":true,"modelLoaded":true}'
  self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_POST(self):
  self.rfile.read(int(self.headers['Content-Length']))
  body=b'{"stages":{},"inferenceMs":0,"cachePrefix":false}'
  self.send_response(200);self.send_header('X-Intercat-Inference','started');self.end_headers();self.wfile.write(body)
server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True)
server.serve_forever()
''')
    model = Path(temporary.name) / 'model'
    model.mkdir()
    subprocess.run(['cargo', 'build', '--offline', '-q', '--bin', 'intercatd'], check=True)
    token = secrets.token_hex(32)
    process = subprocess.Popen([str(Path('target/debug/intercatd').resolve()), 'serve', str(data), str(editor)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=(output / 'daemon.log').open('w'), text=True)
    connection = None
    counter = 0
    result = {'attemptedPaidTurns': 0, 'previousPaidTurns': 149}
    (output / 'attempt.json').write_text(json.dumps(result))
    try:
        process.stdin.write(json.dumps({'token': token, 'secretKey': bytes([7] * 32).hex(), 'receipt': None}) + '\n')
        process.stdin.flush()
        hello = json.loads(process.stdout.readline())
        process.stdin.write(json.dumps(hello['fingerprints']['catalog']) + '\n')
        process.stdin.close()
        endpoint = data / 'run/rpc.sock'
        deadline = time.monotonic() + 20
        while not endpoint.exists():
            if process.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError('Daemon did not open its socket')
            time.sleep(.05)
        def open_socket():
            stream = socket.socket(socket.AF_UNIX)
            stream.settimeout(10)
            stream.connect(str(endpoint))
            return stream, stream.makefile('rwb')
        connection, wire = open_socket()
        def call(method, params=None, channel=None):
            nonlocal counter
            counter += 1
            channel = channel or wire
            channel.write((json.dumps({'jsonrpc': '2.0', 'id': counter, 'method': method, 'params': params or {}}) + '\n').encode())
            channel.flush()
            reply = json.loads(channel.readline())
            if 'error' in reply:
                raise RuntimeError(f"{method} failed with code {reply['error'].get('code')}")
            return reply['result']
        outsider, outsider_wire = open_socket()
        try:
            try: call('system.status', channel=outsider_wire)
            except RuntimeError as error:
                result['unauthenticatedRejected'] = str(error).endswith('-32001')
            else: result['unauthenticatedRejected'] = False
        finally: outsider_wire.close(); outsider.close()
        call('rpc.authenticate', {'token': token})
        call('editor.configure', {'opId': command_id(), 'config': {'python': sys.executable, 'model': str(model)}})
        status = call('system.status')
        project = status['projectSecrets']
        import hmac, hashlib
        key_check = hmac.new(bytes([7] * 32), b'intercat.secret-fingerprints.v1', hashlib.sha256).hexdigest()
        catalog = call('secret.project.prepare', {'opId': command_id(), 'projectId': project['projectId'], 'revision': 0, 'fingerprints': [], 'keyCheck': key_check})
        call('secret.confirm', catalog['current'])
        call('secret.project.activate', {'projectId': project['projectId'], 'revision': 0, 'entries': []})
        deadline = time.monotonic() + 30
        while True:
            status = call('system.status')
            worker = next((w for w in status['workers'] if w['trackId'] == fixture['track']), None)
            if worker and worker.get('identity') and worker['phase'] == 'ready' and status['editor']['modelLoaded']: break
            if worker and worker['phase'] in ['failed', 'needs-recovery']: raise RuntimeError('Fixture worker did not become ready')
            if time.monotonic() > deadline: raise RuntimeError('Fixture worker start timed out')
            time.sleep(.1)
        marker = b'INTERCAT_EXPOSURE_POSITIVE_CONTROL'
        positive = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(5)', marker.decode()], env={**os.environ, 'EXPOSURE_CONTROL': marker.decode()})
        try:
            observed = exposure(positive.pid, marker)
            assert observed['tokenInArguments'] and observed['tokenInEnvironment']
            result['metadataPositiveControl'] = True
        finally: positive.terminate(); positive.wait()
        result['daemon'] = exposure(process.pid, token.encode())
        result['worker'] = exposure(worker['identity']['pid'], token.encode())
        assert all(not row['tokenInArguments'] and not row['tokenInEnvironment'] for row in [result['daemon'], result['worker']])
        workspace = Path(next(w['path'] for w in status['workspaces'] if w['trackId'] == fixture['track']))
        result['attemptedPaidTurns'] = 1
        (output / 'attempt.json').write_text(json.dumps({'attemptedPaidTurns': 1, 'previousPaidTurns': 149}))
        call('worker.start', {'opId': command_id(), 'trackId': fixture['track']})
        answered = set()
        deadline = time.monotonic() + 120
        while True:
            status = call('system.status')
            worker = next(w for w in status['workers'] if w['trackId'] == fixture['track'])
            for approval in worker.get('approvals', []):
                identity = (approval['id'], json.dumps(approval['requirement'], sort_keys=True))
                if identity in answered: continue
                allowed = approval['subject'] == fixture['command'] and approval['canAllow']
                call('worker.answer', {'opId': command_id(), 'trackId': fixture['track'], 'approvalId': approval['id'], 'requirement': approval['requirement'], 'response': {'action': 'allow' if allowed else 'deny'}})
                answered.add(identity)
            file = workspace / 'rpc-result.json'
            if file.exists() and worker.get('checkpoint') and not worker.get('checkpointPending'):
                result['shell'] = json.loads(file.read_text())
                break
            if time.monotonic() > deadline: raise RuntimeError('Fixture turn did not complete')
            time.sleep(.1)
        assert result['shell'] == {'connected': False, 'errno': 1}
        assert result['unauthenticatedRejected']
        result['workerSession'] = worker['sessionId']
        result['passed'] = True
    finally:
        if connection: connection.close()
        process.terminate()
        try: process.wait(timeout=20)
        except subprocess.TimeoutExpired: process.kill(); process.wait(); result['forcedDaemonStop'] = True
        with sqlite3.connect(data / 'events.sqlite') as db, sqlite3.connect(output / 'events.sqlite') as destination:
            db.backup(destination)
            result['savedTurnStarts'] = db.execute("SELECT count(*) FROM operations WHERE kind='turn.start'").fetchone()[0]
            result['tokenNotInDatabase'] = token.encode() not in (output / 'events.sqlite').read_bytes()
        result['paidTurns'] = result['savedTurnStarts']
        result['cumulativePaidTurns'] = 149 + result['paidTurns']
        (output / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
        print(json.dumps(result))
        temporary.cleanup()


if __name__ == '__main__':
    main()
