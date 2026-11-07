"""Dummy HTTP model with a manually released second routing pass."""
import json
import math
from pathlib import Path
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def respond(self, value):
        body = json.dumps(value).encode()
        self.send_response(200)
        self.send_header('X-Intercat-Inference', 'started')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.respond({'accepting': True, 'modelLoaded': True})

    def do_POST(self):
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        if self.path == '/tokens':
            self.respond({'tokens': 10})
            return
        tail = json.loads(request['tail'])
        text = tail['request']
        self.send_response(200)
        self.send_header('X-Intercat-Inference', 'started')
        self.end_headers()
        self.wfile.flush()
        slow = text == 'slow dummy' and tail['target']['mode'] == 'none' and tail['tracks']
        if slow:
            Path('second-pass-started').touch()
            until = time.monotonic() + 15
            while not Path('release-second-pass').exists() and time.monotonic() < until:
                time.sleep(.01)
        selected, stages = {}, {}
        for stage in request['stages']:
            if any(selected.get(key) not in values for key, values in stage.get('when', {}).items()):
                continue
            choice = ' A' if stage['name'] == 'decision' and text == 'slow dummy' else ' B' if stage['name'] == 'decision' else '1' if stage['name'] == 'lane' and tail['tracks'] else '9' if stage['name'] == 'lane' else ' A'
            selected[stage['name']] = choice
            probabilities = {label: 1 if label == choice else 0 for label in stage['candidates']}
            stages[stage['name']] = {'selected': choice, 'probabilities': probabilities,
                'logprobs': {label: math.log(max(p, 1e-300)) * request['temperature'] for label, p in probabilities.items()}}
        try:
            self.wfile.write(json.dumps({'stages': stages, 'temperature': request['temperature'], 'inferenceMs': 50, 'cachePrefix': False}).encode())
        except (BrokenPipeError, ConnectionResetError):
            pass


from intercat_editor.lifetime import watch_parent

watch_parent()
server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print(json.dumps({'host': '127.0.0.1', 'port': server.server_port}), flush=True)
server.serve_forever()
