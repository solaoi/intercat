"""Agent-authored model fixture, using the production scheduler and transport."""
import json
import math
from pathlib import Path
import sys
import time
from types import ModuleType


def record(value):
    with Path('requests.jsonl').open('a') as stream:
        stream.write(json.dumps(value) + '\n')


class Engine:
    load_ms = 0
    def memory(self): return {}
    def count_tokens(self, text): return {'tokens': len(text) // 4}
    def decide(self, request, preempted):
        batch = 'REPORTS_JSON:\n' in request.prefix
        rows = json.loads(request.prefix.split('REPORTS_JSON:\n')[1]) if batch else [{'report': json.loads(request.tail)['report']}]
        gate = Path('pause-background' if batch else 'pause-urgent')
        while gate.exists():
            if preempted():
                record({'preempted': True, 'batch': batch})
                return {'preempted': True}
            time.sleep(.01)
        if Path('malformed').exists(): return {'stages': {}, 'cachePrefix': False}
        stages = {}
        for stage, row in zip(request.stages, rows):
            selected = '1' if '[answer]' in row['report'] else '2' if '[unclear]' in row['report'] else '0'
            probabilities = {label: .98 if label == selected else .01 for label in ['0','1','2']}
            stages[stage.name] = {'selected': selected, 'probabilities': probabilities,
                'logprobs': {key: math.log(value) for key, value in probabilities.items()}}
        return {'stages': stages, 'cachePrefix': False}


module = ModuleType('intercat_editor.engine'); module.Engine = Engine
sys.modules[module.__name__] = module
from intercat_editor.lifetime import watch_parent
from intercat_editor.scheduler import Scheduler
from intercat_editor.server import create_server


class RecordedScheduler(Scheduler):
    def submit(self, kind, payload, priority=None, timeout=None, reading=None):
        if kind == 'decide':
            record({'priority': priority, 'reading': reading, 'prefix': payload.prefix,
                'tail': payload.tail, 'stages': [stage.name for stage in payload.stages]})
        return super().submit(kind, payload, priority, timeout, reading)
    def cancel_reading(self, key, index):
        record({'cancelled': key})
        return super().cancel_reading(key, index)


watch_parent()
scheduler = RecordedScheduler(Engine)
server = create_server(scheduler)
print(json.dumps({'host': '127.0.0.1', 'port': server.server_port}), flush=True)
try: server.serve_forever()
finally: server.server_close(); scheduler.close()
