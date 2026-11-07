"""M1.20: one paid dummy turn through the production daemon and host policy."""
import argparse
import hashlib
import hmac
import json
from pathlib import Path
import secrets
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time

from verify_host_hooks import command_id

NAMES = ['plain.txt', '.env', '.env.', '.env.production', '.env.example', '.env.sample',
         '.env.template', '.env.dist', '.env.example.local', '.env.d', '.env.é',
         '.env.e\u0301', '.env.Ω', '.env.本番', '.env.😀', '.envrc', '.netrc', 'cert.key', 'cert.pem']


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--muse', required=True, type=Path)
    parser.add_argument('--auth', type=Path, default=Path.home() / '.config/muse/auth.json')
    parser.add_argument('--previous-paid-turns', required=True, type=int)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    result = {'attemptedPaidTurns': 0, 'previousPaidTurns': args.previous_paid_turns}
    (output / 'attempt.json').write_text(json.dumps(result))
    with tempfile.TemporaryDirectory(prefix='ic-secret-', dir='/private/tmp') as temporary:
        root = Path(temporary)
        seed = subprocess.run(['cargo', 'run', '--offline', '-q', '-p', 'intercat-daemon',
                               '--example', 'secret_boundary_fixture', '--', str(root / 'fixture'),
                               str(args.muse.resolve()), str(args.auth.resolve())],
                              capture_output=True, text=True)
        (output / 'fixture.log').write_text(seed.stderr)
        if seed.returncode:
            raise RuntimeError(f'Fixture setup failed; see {output / "fixture.log"}')
        fixture = json.loads(seed.stdout)
        (output / 'fixture.json').write_text(json.dumps(fixture, indent=2) + '\n')
        result['integration'] = fixture['integration']
        data = Path(fixture['data'])
        source = Path(fixture['source'])
        before = {str(p.relative_to(source)): hashlib.sha256(p.read_bytes()).hexdigest()
                  for p in source.rglob('*') if p.is_file() and '.git' not in p.relative_to(source).parts}
        editor = root / 'editor'
        package = editor / 'intercat_editor'
        package.mkdir(parents=True)
        (package / '__init__.py').write_text('')
        # Only allows the already-classified held input to be started manually.
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
        model = root / 'model'
        model.mkdir()
        subprocess.run(['cargo', 'build', '--offline', '-q', '--bin', 'intercatd'], check=True)
        token = secrets.token_hex(32)
        with (output / 'daemon.log').open('w') as log:
            process = subprocess.Popen([str(Path('target/debug/intercatd').resolve()), 'serve', str(data), str(editor)],
                                       stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, text=True)
            connection = None
            counter = 0
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
                connection = socket.socket(socket.AF_UNIX)
                connection.settimeout(10)
                connection.connect(str(endpoint))
                wire = connection.makefile('rwb')

                def call(method, params=None):
                    nonlocal counter
                    counter += 1
                    wire.write((json.dumps({'jsonrpc': '2.0', 'id': counter, 'method': method, 'params': params or {}}) + '\n').encode())
                    wire.flush()
                    reply = json.loads(wire.readline())
                    if 'error' in reply:
                        raise RuntimeError(f"{method} failed with code {reply['error'].get('code')}")
                    return reply['result']

                call('rpc.authenticate', {'token': token})
                call('editor.configure', {'opId': command_id(), 'config': {'python': sys.executable, 'model': str(model)}})
                project = call('system.status')['projectSecrets']
                key_check = hmac.new(bytes([7] * 32), b'intercat.secret-fingerprints.v1', hashlib.sha256).hexdigest()
                catalog = call('secret.project.prepare', {'opId': command_id(), 'projectId': project['projectId'], 'revision': 0, 'fingerprints': [], 'keyCheck': key_check})
                call('secret.confirm', catalog['current'])
                call('secret.project.activate', {'projectId': project['projectId'], 'revision': 0, 'entries': []})
                deadline = time.monotonic() + 30
                while True:
                    status = call('system.status')
                    worker = next((w for w in status['workers'] if w['trackId'] == fixture['track']), None)
                    if worker and worker.get('identity') and worker['phase'] == 'ready' and status['editor']['modelLoaded']:
                        break
                    if worker and worker['phase'] in ['failed', 'needs-recovery']:
                        raise RuntimeError('Fixture worker did not become ready')
                    if time.monotonic() > deadline:
                        raise RuntimeError('Fixture worker start timed out')
                    time.sleep(.1)
                workspace = Path(next(w['path'] for w in status['workspaces'] if w['trackId'] == fixture['track']))
                assert all(not (workspace / parent / '.env').exists() for parent in ('', 'packages/app'))
                result['workerIgnoredSecretsInitiallyAbsent'] = True
                for parent in ('', 'packages/app'):
                    for name in NAMES:
                        (workspace / parent / name).write_text('DUMMY_ONLY\n')
                result['attemptedPaidTurns'] = 1
                (output / 'attempt.json').write_text(json.dumps(result, indent=2))
                call('worker.start', {'opId': command_id(), 'trackId': fixture['track']})
                answered = set()
                deadline = time.monotonic() + 180
                while True:
                    status = call('system.status')
                    worker = next(w for w in status['workers'] if w['trackId'] == fixture['track'])
                    for approval in worker.get('approvals', []):
                        identity = (approval['id'], json.dumps(approval['requirement'], sort_keys=True))
                        if identity in answered:
                            continue
                        allowed = approval['subject'] == fixture['command'] and approval['canAllow']
                        call('worker.answer', {'opId': command_id(), 'trackId': fixture['track'], 'approvalId': approval['id'], 'requirement': approval['requirement'], 'response': {'action': 'allow' if allowed else 'deny'}})
                        answered.add(identity)
                    file = workspace / 'secret-result.json'
                    if file.exists() and worker.get('checkpoint') and not worker.get('checkpointPending'):
                        result['shellAccess'] = json.loads(file.read_text())
                        result['checkpoint'] = worker['checkpoint']
                        result['workerSession'] = worker['sessionId']
                        break
                    if time.monotonic() > deadline:
                        raise RuntimeError('Fixture turn did not complete')
                    time.sleep(.1)
                after = {str(p.relative_to(source)): hashlib.sha256(p.read_bytes()).hexdigest()
                         for p in source.rglob('*') if p.is_file() and '.git' not in p.relative_to(source).parts}
                assert before == after
                result['sourceFilesUnchanged'] = True
                wire.close()
            except BaseException as error:
                result['failure'] = str(error)
                raise
            finally:
                if connection:
                    connection.close()
                process.terminate()
                try:
                    process.wait(timeout=20)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    result['forcedDaemonStop'] = True
                with sqlite3.connect(data / 'events.sqlite') as database, sqlite3.connect(output / 'events.sqlite') as saved:
                    database.backup(saved)
                    result['savedTurnStarts'] = database.execute("SELECT count(*) FROM operations WHERE kind='turn.start'").fetchone()[0]
                result['paidTurns'] = result['savedTurnStarts']
                result['cumulativePaidTurns'] = args.previous_paid_turns + result['paidTurns']
                (output / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
                print(json.dumps({k: v for k, v in result.items() if k not in ('shellAccess', 'checkpoint', 'integration')}))


if __name__ == '__main__':
    main()
