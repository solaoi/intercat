"""V38: real runtime prefix, concurrent HTTP decisions and MLX preemption.

Uses only local weights and dummy requests. This measures the service boundary;
the daemon's future background-generation callers need separate acceptance.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import re
import statistics
from threading import Event, Thread
import time
import urllib.request

from verify_editor import Engine, Scheduler, create_server


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--model', required=True)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    source = Path('crates/intercat-daemon/src/decision.rs').read_text()
    prefix = json.loads(re.search(r'const PREFIX: &str = ("(?:[^"\\]|\\.)*");', source)[1])
    labels = [' ' + letter for letter in 'ABCDEFGH']
    stages = [
        {'name': 'decision', 'prompt': '\nDecision:', 'candidates': labels},
        {'name': 'lane', 'prompt': '\nLane: ', 'candidates': ['1', '2', '3', '9'],
         'when': {'decision': [' A', ' C', ' D']}},
        {'name': 'boundary', 'prompt': '\nBoundary:', 'candidates': [' A', ' B', ' C', ' D'],
         'when': {'decision': [' B', ' C']}},
        {'name': 'command', 'prompt': '\nCommand: ', 'candidates': [str(n) for n in range(1, 11)],
         'when': {'decision': [' H']}},
    ]
    fixtures = [{
        'prefix': prefix, 'cachePrefix': False, 'temperature': 1.0, 'stages': stages,
        'tail': json.dumps({'tracks': [
            {'lane': 1, 'title': 'Arithmetic tests', 'state': 'working'},
            {'lane': 2, 'title': 'Documentation', 'state': 'waiting'},
            {'lane': 3, 'title': 'Dummy server', 'state': 'working'}],
            'selected': 1, 'target': {'mode': 'none'}, 'request': request}, ensure_ascii=False),
    } for request in ['Add another arithmetic test.', 'Open the commit sheet.',
                      'サーバーの作業を今止めて', 'ドキュメントの案は後で']]
    (args.output / 'requests.json').write_text(json.dumps(fixtures, ensure_ascii=False, indent=2))
    (args.output / 'probe-source.py').write_text(Path(__file__).read_text())
    decoded = Event()

    class ObservedEngine(Engine):
        def generate(self, *arguments):
            # Observe the real generator at its public yield boundary. The
            # production cancellation and disposal path remains unchanged.
            import mlx_lm
            original = mlx_lm.stream_generate

            def observe(*args, **kwargs):
                for response in original(*args, **kwargs):
                    decoded.set()
                    yield response

            mlx_lm.stream_generate = observe
            try:
                return super().generate(*arguments)
            finally:
                mlx_lm.stream_generate = original

    scheduler = Scheduler(lambda: ObservedEngine(args.model))
    loaded = scheduler.ready.result(timeout=120)
    server = create_server(scheduler)
    Thread(target=server.serve_forever, daemon=True).start()
    rows, backgrounds = [], []

    def send(index, mode):
        started = time.perf_counter()
        request = urllib.request.Request(f'http://127.0.0.1:{server.server_port}/decide',
                                         data=json.dumps(fixtures[index]).encode(),
                                         headers={'Content-Type': 'application/json'})
        with urllib.request.urlopen(request, timeout=120) as response:
            assert response.getheader('X-Intercat-Inference') == 'started'
            admitted = time.perf_counter()
            value = json.load(response)
        return {'index': index, 'mode': mode, 'admissionMs': (admitted-started)*1000,
                'wallMs': (time.perf_counter()-started)*1000, **value}

    def record(row):
        rows.append(row)
        with (args.output / 'timings.jsonl').open('a') as file:
            file.write(json.dumps(row, ensure_ascii=False) + '\n')
        print(json.dumps({key: row[key] for key in ['mode', 'index', 'inferenceMs', 'queueMs', 'wallMs']}), flush=True)

    try:
        send(0, 'warmup')
        for _ in range(3):
            for index in range(4):
                record(send(index, 'serial'))
        for cycle in range(4):
            decoded.clear()
            background = scheduler.submit('generate', {
                'prompt': prefix + '\nDescribe the dummy arithmetic tests in detail. Write many examples.',
                'maxTokens': 512,
            })
            assert background.started.wait(timeout=10)
            if cycle < 2:
                time.sleep(.15)
            else:
                assert decoded.wait(timeout=30)
                time.sleep(.05)
            with ThreadPoolExecutor(max_workers=3) as pool:
                for row in pool.map(lambda index: send(index, 'background-burst'), range(3)):
                    record(row)
            result = background.result(timeout=120)
            assert result['preempted'] and result['text'] == ''
            backgrounds.append(result)
        assert any(row['tokens'] > 0 for row in backgrounds)
        assert any(row['tokens'] == 0 for row in backgrounds)
        baseline = {row['index']: row['stages'] for row in rows if row['mode'] == 'serial'}
        assert all(row['stages'] == baseline[row['index']] for row in rows)
        summary = {'passed': True, 'paidTurns': 0, 'loaded': loaded, 'backgrounds': backgrounds,
                   'modes': {mode: {key: {'median': statistics.median(values), 'max': max(values)}
                                    for key in ['inferenceMs', 'queueMs', 'admissionMs', 'wallMs']
                                    for values in [[r[key] for r in rows if r['mode'] == mode]]}
                             for mode in ['serial', 'background-burst']},
                   'limitations': ['Local service only; daemon background generation is not connected yet.',
                                   'Dummy three-track context; maximum token budget is a separate probe.']}
        (args.output / 'verification.json').write_text(json.dumps(summary, ensure_ascii=False, indent=2) + '\n')
        print(json.dumps(summary), flush=True)
    finally:
        server.shutdown()
        server.server_close()
        scheduler.close()


if __name__ == '__main__':
    main()
