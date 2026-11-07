from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import json
import math
import re


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
        request = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        with Path(__file__).with_name('assumption-requests.jsonl').open('a') as log:
            log.write(json.dumps({'path': self.path, 'request': request}) + '\n')
        if self.path == '/cancel-reading':
            self.reply({'cancelled': True})
            return
        if self.path == '/decide':
            stages = {}
            selected = {}
            for stage in request['stages']:
                if any(selected.get(key) not in values for key, values in stage.get('when', {}).items()):
                    continue
                labels = stage['candidates']
                choice = {'decision': ' A', 'boundary': ' D', 'lane': '1', 'command': '10', 'question': '0'}[stage['name']]
                if choice not in labels:
                    choice = labels[0]
                probabilities = {label: 1.0 if len(labels) == 1 else 0.98 if label == choice else 0.02 / (len(labels) - 1) for label in labels}
                stages[stage['name']] = {'selected': choice, 'logprobs': {label: math.log(value) * request['temperature'] for label, value in probabilities.items()}, 'probabilities': probabilities}
                selected[stage['name']] = choice
            self.reply({'cachePrefix': request['cachePrefix'], 'temperature': request['temperature'], 'stages': stages, 'inferenceMs': 1})
            return
        if self.path == '/tokens':
            count = max(1, len(request['text'].encode()) // 4)
            with Path(__file__).with_name('assumption-counts.jsonl').open('a') as log:
                log.write(json.dumps({'text': request['text'], 'tokens': count}) + '\n')
            self.reply({'tokens': count})
            return
        if self.path != '/generate' or not 1 <= request['maxTokens'] <= 512:
            self.send_error(400, 'Invalid generation request')
            return
        sources = json.loads(request['tail'])
        if not isinstance(sources, list):
            text = {'title': 'Local task', 'summary': 'Examining the local task.'} if 'summary' in request['prefix'] else {'title': 'Local task', 'line': 'I placed the request.', 'detail': 'The request has a recorded destination.'}
            self.reply({'preempted': False, 'text': json.dumps(text)})
            return
        with Path(__file__).with_name('assumption-generations.jsonl').open('a') as log:
            log.write(json.dumps(request) + '\n')
        preempt = Path(__file__).with_name('preempt-third')
        requests = Path(__file__).with_name('assumption-generations.jsonl').read_text().splitlines()
        if preempt.exists() and len(requests) == 3:
            preempt.unlink()
            Path(__file__).with_name('preempted').touch()
            self.reply({'preempted': True})
            return
        if Path(__file__).with_name('invalid-long').exists() and any(len(source['text'].encode()) > 8192 for source in sources):
            self.reply({'preempted': False, 'tokens': 40, 'text': json.dumps({'assumptions': [
                {'source': sources[0]['id'], 'question': 'Invented question?', 'options': ['Invented', 'Other'],
                 'assumed': 'Invented', 'evidence': 'This was never in the worker message.'}
            ]})})
            return
        assumptions = []
        for source in sources:
            for match in re.finditer(r'I assumed (SQLite|PostgreSQL)', source['text']):
                chosen = match.group(1)
                other = 'PostgreSQL' if chosen == 'SQLite' else 'SQLite'
                asked = 'Which database should this cache use, SQLite or PostgreSQL?'
                assumptions.append({'source': source['id'], 'question': asked if asked in source['text'] else 'Which database should the cache use?',
                                    'options': [chosen, other], 'assumed': chosen, 'evidence': match.group(0)})
        if len(assumptions) > 2:
            self.reply({'preempted': False, 'tokens': 512, 'text': '{"assumptions":['})
        else:
            self.reply({'preempted': False, 'tokens': 40 * len(assumptions) + 5,
                        'text': json.dumps({'assumptions': assumptions})})


server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
print(json.dumps({'host': '127.0.0.1', 'port': server.server_port}), flush=True)
server.serve_forever()
