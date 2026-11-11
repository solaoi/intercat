"""Verify the saved native M1 acceptance run without exposing request contents."""
import hashlib
import json
from pathlib import Path
import sqlite3
import sys


def approval_order(database):
    requests, decisions, started = {}, {}, {}
    with sqlite3.connect(f'file:{database.resolve()}?mode=ro', uri=True) as db:
        events = db.execute('SELECT seq,ts,kind,payload FROM events ORDER BY seq').fetchall()
        for sequence, timestamp, kind, raw in events:
            frame = json.loads(raw)
            method = frame.get('method')
            params = frame.get('params', {})
            if kind == 'msp.received' and method == 'approval/request':
                requests[params['approvalId']] = params
            if kind == 'intent.approval.decide':
                params = frame['payload']
                decisions.setdefault(params['approvalId'], []).append(sequence)
            if kind == 'msp.received' and method == 'item/started':
                item = params['item']
                if item.get('kind') == 'toolCall' and item.get('tool') == 'bash':
                    started.setdefault(item['itemId'], sequence)
        for item, sequence in started.items():
            before = [value for key, request in requests.items() if request.get('itemId') == item
                      and request.get('subject', {}).get('kind') == 'shell'
                      for value in decisions.get(key, []) if value < sequence]
            assert before, ('Shell started before its recorded decision', item)
        staged = next((ts, json.loads(raw)['id']) for _, ts, kind, raw in events if kind == 'utterance.staged')
        classified = next(json.loads(raw) for _, _, kind, raw in events
                          if kind == 'utterance.classified' and json.loads(raw)['id'] == staged[1])
        submitted = next(json.loads(raw) for _, _, kind, raw in events
                         if kind == 'intent.turn.start' and json.loads(raw).get('inputId') == staged[1])
        received = next(ts for _, ts, kind, raw in events if kind == 'msp.received'
                        and json.loads(raw).get('params', {}).get('item', {}).get('kind') == 'userMessage'
                        and json.loads(raw)['params']['item'].get('commandId') == submitted['operationId'])
        scores = classified['decision']['scores']
        starts = db.execute("SELECT count(*) FROM operations WHERE kind='turn.start'").fetchone()[0]
    return {'shellStartsChecked': len(started), 'allShellDecisionsPrecedeStart': True,
            'savedInputToUserMessageMs': received - staged[0],
            'routingQueueMs': scores['queueMs'], 'routingInferenceMs': classified['decision']['inferenceMs'],
            'attemptedWorkerTurns': starts}


def verify(directory, previous):
    value = json.loads((directory / 'observations.json').read_text())
    assert value['passed'] and value['connections'] == 0 and value['chipMs'] <= 100
    first, pushed, committed, restored = (value[key] for key in ('firstDone', 'pushed', 'committed', 'restored'))
    assert len(first['tracks']) == len(pushed['tracks']) == 1
    assert first['workers'][0]['sessionId'] == pushed['workers'][0]['sessionId']
    assert first['workers'][0]['checkpoint']['turnId'] != pushed['workers'][0]['checkpoint']['turnId']
    assert any(word['state'] == 'cut' for word in first['utterances'])
    assert any(effect['kind'] == 'push' and effect['outcome'] == 'done' for effect in pushed['effects'])
    assert committed['integrations'][0]['status'] == restored['integrations'][0]['status'] == 'done'
    assert committed['integrations'][0]['id'] == restored['integrations'][0]['id']
    assert all(worker['phase'] == 'stopped' and worker['activeTurn'] is None for worker in restored['workers'])
    assert 'Intercat:' in value['commitMessage']
    order = approval_order(directory / 'events.sqlite')
    root = Path(__file__).resolve().parents[1]
    sources = ['tests/milestone-one.spec.ts', 'electron/main.mjs', 'electron/quit.mjs', 'src/main.tsx',
               'crates/intercat-daemon/src/host.rs', 'crates/intercat-daemon/src/execution.rs',
               'crates/intercat-daemon/src/runtime.rs', 'crates/intercat-daemon/src/journal.rs']
    return {'passed': True, 'requirements': [1,2,3,4,5,6,7,11], 'chipMs': value['chipMs'],
            'networkConnectionsAfterDenial': 0, **order,
            'cumulativePaidTurns': previous + order['attemptedWorkerTurns'],
            'sourceSha256': {name: hashlib.sha256((root/name).read_bytes()).hexdigest() for name in sources}}


if __name__ == '__main__':
    directory = Path(sys.argv[1]).resolve()
    result = verify(directory, int(sys.argv[2]))
    (directory/'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2)+'\n')
    print(json.dumps({key:value for key,value in result.items() if key!='sourceSha256'}, ensure_ascii=False))
