"""Generated loopback-only HTTPS Git server; no user data or credentials."""
import base64
import http.server
import json
from pathlib import Path
import ssl
import subprocess
import sys
import threading

root = Path(sys.argv[1]).resolve()
root.mkdir(parents=True, exist_ok=True)
home = root / 'home'
home.mkdir(exist_ok=True)
env = {'HOME': str(home), 'PATH': '/usr/bin:/bin:/usr/sbin:/sbin',
       'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_NOSYSTEM': '1'}
git = subprocess.check_output(['/usr/bin/xcrun', '--find', 'git'], env=env, text=True).strip()
remote = root / 'remote.git'
subprocess.run([git, 'init', '--bare', '-q', remote], env=env, check=True)
subprocess.run([git, '--git-dir', remote, 'config', 'http.receivepack', 'true'], env=env, check=True)
cert, key = root / 'dummy.crt', root / 'dummy.key'
subprocess.run(['/usr/bin/openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
                '-keyout', key, '-out', cert, '-subj', '/CN=localhost',
                '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1'], env=env, check=True,
               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
key.chmod(0o600)
backend = Path(subprocess.check_output([git, '--exec-path'], env=env, text=True).strip()) / 'git-http-backend'
expected = 'Basic ' + base64.b64encode(b'dummy:dummy-local-only').decode()

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.git()

    def do_POST(self):
        self.git()

    def git(self):
        if self.headers.get('Authorization') != expected:
            self.send_response(401)
            self.send_header('WWW-Authenticate', 'Basic realm="Intercat dummy"')
            self.send_header('Content-Length', '0')
            self.end_headers()
            return
        path, _, query = self.path.partition('?')
        if not path.startswith('/remote.git/'):
            self.send_error(404)
            return
        body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
        cgi = {**env, 'GIT_PROJECT_ROOT': str(root), 'GIT_HTTP_EXPORT_ALL': '1',
               'PATH_INFO': path, 'QUERY_STRING': query, 'REQUEST_METHOD': self.command,
               'CONTENT_TYPE': self.headers.get('Content-Type', ''), 'CONTENT_LENGTH': str(len(body)),
               'REMOTE_USER': 'dummy', 'SERVER_PROTOCOL': 'HTTP/1.1'}
        output = subprocess.run([backend], env=cgi, input=body, capture_output=True, timeout=20)
        headers, _, content = output.stdout.partition(b'\r\n\r\n')
        pairs = [line.decode().split(':', 1) for line in headers.split(b'\r\n') if b':' in line]
        status = next((int(value.strip().split()[0]) for name, value in pairs if name.lower() == 'status'), 200)
        self.send_response(status)
        for name, value in pairs:
            if name.lower() not in ('status', 'content-length'):
                self.send_header(name, value.strip())
        self.send_header('Content-Length', str(len(content)))
        self.end_headers()
        self.wfile.write(content)

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(cert, key)
server.socket = context.wrap_socket(server.socket, server_side=True)
thread = threading.Thread(target=server.serve_forever, daemon=True)
thread.start()
print(json.dumps({'url': f'https://127.0.0.1:{server.server_port}/remote.git',
                  'ca': str(cert), 'remote': str(remote)}), flush=True)
try:
    sys.stdin.readline()
finally:
    server.shutdown()
    server.server_close()
