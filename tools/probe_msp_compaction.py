"""Inspect real compaction and old-cursor behavior in an isolated echo session."""
import argparse
import hashlib
import json
from pathlib import Path
import time

from verify_host_hooks import Host, command_id


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == '1c96114408acbeade9a8fb79e3b4425e5b64d50425938568aef01bdc04ce2e03'
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    root = output / 'workspace'
    config = root / 'config'
    (config / 'muse').mkdir(parents=True)
    (config / 'muse/settings.json').write_text(json.dumps({
        'schema_version': 1, 'provider': 'echo', 'run': {'toolset': []},
        'permissions': {'schema_version': 1, 'default_profile': 'intercat-worker',
                        'profiles': {'intercat-worker': {'extends': ':ask-me'}}},
    }))
    result = {'provider': 'echo', 'calls': [], 'turns': [], 'compactions': [], 'oldCursors': []}
    host = Host(binary, root, config, output, False)
    completed_compactions = set()

    def request(method, params):
        row = {'method': method, 'params': params}
        result['calls'].append(row)
        try:
            row['result'] = host.request(method, params)
            return row['result']
        except RuntimeError as error:
            row['error'] = json.loads(str(error))
            return None

    try:
        request('initialize', {'clientInfo': {'name': 'intercat_compaction_probe', 'version': '0.1.0'},
                               'capabilities': {'experimentalApi': True}})
        host.send({'jsonrpc': '2.0', 'method': 'initialized'})
        opened = request('session/start', {'commandId': command_id(), 'workspaceRoot': str(root),
                                          'providerId': 'echo', 'approvalMode': 'promptUnmatched'})
        session = opened['session']['sessionId']
        cursors = [('start', opened['viewCursor'])]
        for cycle in range(2):
            for turn in range(4):
                text = (f'OFFLINE_COMPACTION_{cycle}_{turn}_' * 4000)[:65536]
                ack = request('turn/start', {'commandId': command_id(), 'sessionId': session,
                                            'input': [{'type': 'text', 'text': text}]})
                frames = list(host.events)
                host.events.clear()
                deadline = time.monotonic() + 30
                while not any(f.get('method') == 'turn/completed' and f['params']['turnId'] == ack['turnId'] for f in frames):
                    frames.append(host.receive(deadline))
                terminal = next(f['params']['terminal'] for f in frames if f.get('method') == 'turn/completed' and f['params']['turnId'] == ack['turnId'])
                result['turns'].append({'turnId': ack['turnId'], 'terminal': terminal})
                assert terminal == 'completed'
                if len(cursors) == 1:
                    delta = next((f for f in frames if f.get('method') == 'item/delta'), None)
                    if delta:
                        cursors.append(('firstDelta', delta['params']['viewCursor']))
            ack = request('session/compact', {'commandId': command_id(), 'sessionId': session})
            record = {'cycle': cycle, 'ack': ack}
            result['compactions'].append(record)
            if ack and ack.get('status') == 'accepted':
                frames = list(host.events)
                host.events.clear()
                deadline = time.monotonic() + 45
                def completed():
                    return next((f['params']['item'] for f in frames
                                 if f.get('method') == 'item/completed' and f['params']['item']['kind'] == 'compaction'
                                 and f['params']['item']['itemId'] not in completed_compactions), None)
                while completed() is None:
                    frames.append(host.receive(deadline))
                record['item'] = completed()
                completed_compactions.add(record['item']['itemId'])
                if record['item'].get('summarizedThrough'):
                    cursors.append((f'compaction{cycle}', record['item']['summarizedThrough']))
            for label, cursor in cursors:
                params = {'sessionId': session, 'cursor': cursor, 'limit': 200, 'maxBytes': 524288}
                value = request('view/page', params)
                row = result['calls'][-1]
                result['oldCursors'].append({'cycle': cycle, 'label': label, 'error': row.get('error'),
                                             'events': len(value['events']) if value else None})
            request('view/page', {'sessionId': session, 'anchor': 'latestCompaction', 'limit': 200, 'maxBytes': 524288})
            (output / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    finally:
        host.close()
        (output / 'result.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({'compactions': result['compactions'], 'oldCursors': result['oldCursors']}))


if __name__ == '__main__':
    main()
