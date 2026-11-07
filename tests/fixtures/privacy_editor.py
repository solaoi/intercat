"""Deterministic local-only privacy transport fixture; all values are dummy."""
import json
import os
import threading
import time
from pathlib import Path
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

root = Path.cwd()


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b'{"accepting":true,"modelLoaded":true}')

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        with (root / 'requests.jsonl').open('a') as log:
            log.write(json.dumps({'path': self.path, 'body': body, 'priority': self.headers.get('X-Intercat-Priority')}, ensure_ascii=False) + '\n')
        if self.path == '/cancel-reading':
            result = {'cancelled': True}
        elif self.path == '/tokens':
            result = {'tokens': 9000 if (root / 'oversized').exists() else 100}
        elif self.path == '/privacy':
            (root / 'privacy-received').touch()
            while (root / 'hold').exists():
                time.sleep(.005)
            findings = []
            for source in json.loads(body['tail']):
                text = source['text']
                for value, kind in [('Avery Dummy', 'name'), ('蒼井テスト', 'name'), ('example pass phrase 42', 'password')]:
                    if value in text:
                        findings.append({'source': source['source'], 'kind': kind, 'value': value, 'quote': value})
                for address in source['addresses']:
                    findings.append({'source': source['source'], 'kind': 'address', 'value': address, 'quote': address})
            if (root / 'invent').exists():
                findings.append({'source': 0, 'kind': 'name', 'value': 'Invented Person', 'quote': 'Invented Person'})
            counter = root / 'preempt-count'
            remaining = int(counter.read_text()) if counter.exists() else 0
            interrupted = (root / 'preempt').exists() or remaining > 0
            if remaining > 0:
                counter.write_text(str(remaining - 1))
            result = {'text': '{"findings":[' if (root / 'partial').exists() else json.dumps({'findings': findings}, ensure_ascii=False), 'preempted': interrupted}
            if interrupted:
                result['preemptReason'] = 'timeout' if (root / 'preempt').exists() else 'priority'
        else:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header('X-Intercat-Inference', 'started')
        self.end_headers()
        try:
            self.wfile.write(json.dumps(result).encode())
        except (BrokenPipeError, ConnectionResetError):
            pass


server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print(json.dumps({'host': '127.0.0.1', 'port': server.server_port}), flush=True)
server.serve_forever()
