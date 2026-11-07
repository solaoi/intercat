"""Deterministic local model for rephrasing, with the production scheduler."""
import json
import math
from pathlib import Path
import sys
import time
from types import ModuleType


class Engine:
    load_ms = 0
    def memory(self): return {}
    def count_tokens(self, text): return {'tokens': max(10, len(text) // 4)}
    def generate(self, prompt, max_tokens, preempted):
        context = json.loads(prompt.rsplit('\nContext:\n', 1)[1].split('\nDescription:\n', 1)[0])
        with Path('rephrase-inputs.jsonl').open('a') as stream:
            stream.write(json.dumps(context) + '\n')
        if Path('pause-rephrase').exists():
            Path('rephrase-running').touch()
            while Path('pause-rephrase').exists():
                if preempted(): return {'text': '', 'preempted': True, 'tokens': 0}
                time.sleep(.01)
        if context.get('mode') in ['notes', 'proposal']:
            result = {'text': 'Explain the attached request without changing files.'}
        else:
            result = {'detail': 'The dummy request was assigned.', 'line': 'I assigned the dummy request.', 'title': 'Dummy request'}
        return {'text': json.dumps(result), 'preempted': False, 'tokens': 32}
    def decide(self, request, preempted=None):
        selected, stages = {}, {}
        for stage in request.stages:
            if any(selected.get(key) not in values for key, values in stage.when.items()): continue
            choice = ' B' if stage.name == 'decision' else stage.candidates[0]
            selected[stage.name] = choice
            probabilities = {label: 1 if label == choice else 0 for label in stage.candidates}
            stages[stage.name] = {'selected': choice, 'probabilities': probabilities,
                'logprobs': {label: math.log(max(p, 1e-300)) * request.temperature for label, p in probabilities.items()}}
        return {'cachePrefix': False, 'temperature': request.temperature, 'stages': stages, 'inferenceMs': 1}

module = ModuleType('intercat_editor.engine'); module.Engine = Engine
sys.modules[module.__name__] = module
from intercat_editor.lifetime import watch_parent
from intercat_editor.scheduler import Scheduler
from intercat_editor.server import create_server
watch_parent()
scheduler = Scheduler(Engine)
server = create_server(scheduler)
print(json.dumps({'host': '127.0.0.1', 'port': server.server_port}), flush=True)
try: server.serve_forever()
finally: server.server_close(); scheduler.close()
