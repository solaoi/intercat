"""Deterministic Editor for native-worker proposal and presentation acceptance."""
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import json
import math


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, value):
        body = json.dumps(value).encode()
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('X-Intercat-Inference', 'started')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        self.reply({'accepting': True, 'modelLoaded': True})

    def do_POST(self):
        value = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        with Path(__file__).with_name('editor-requests.jsonl').open('a') as stream:
            stream.write(json.dumps({'path': self.path, 'request': value}) + '\n')
        if self.path == '/tokens':
            self.reply({'tokens': max(1, len(value['text']) // 4)})
        elif self.path == '/cancel-reading':
            self.reply({'cancelled': True})
        elif self.path == '/decide':
            stages, selected = {}, {}
            for stage in value['stages']:
                if any(selected.get(key) not in labels for key, labels in stage.get('when', {}).items()):
                    continue
                labels = stage['candidates']
                choice = ' B' if stage['name'] == 'decision' else ' A' if stage['name'] == 'boundary' else '1' if stage['name'] == 'lane' else '0'
                if choice not in labels:
                    choice = labels[0]
                probabilities = {label: 1 if len(labels) == 1 else .99 if label == choice else .01 / (len(labels) - 1) for label in labels}
                stages[stage['name']] = {'selected': choice, 'probabilities': probabilities,
                    'logprobs': {label: math.log(probability) * value['temperature'] for label, probability in probabilities.items()}}
                selected[stage['name']] = choice
            self.reply({'stages': stages, 'cachePrefix': value['cachePrefix'], 'temperature': value['temperature'], 'inferenceMs': 1})
        elif self.path == '/generate':
            context = json.loads(value['tail'])
            if isinstance(context, list):
                result = {'assumptions': []}
            elif 'items' in context:
                result = {'title': 'Dummy file work', 'summary': 'The worker completed a step in the disposable project.'}
            else:
                worker = context.get('destination', {}).get('worker', 'the worker')
                result = {'title': 'Disposable file task', 'line': f'New track for {worker}.', 'detail': 'A new track was created for the explicit dummy request.'}
            self.reply({'text': json.dumps(result), 'tokens': 32, 'preempted': False})
        else:
            self.send_error(404)


server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print(json.dumps({'host': '127.0.0.1', 'port': server.server_port}), flush=True)
server.serve_forever()
