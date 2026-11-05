"""Observe pinned Muse history budgets using only the offline echo provider."""
import argparse
import hashlib
import json
from pathlib import Path
import tempfile
import time

from verify_host_hooks import Host, command_id


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--turns', type=int, default=96)
    parser.add_argument('--display-bytes', type=int, default=0)
    parser.add_argument('--restart', action='store_true')
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == '1c96114408acbeade9a8fb79e3b4425e5b64d50425938568aef01bdc04ce2e03'
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    (output / 'probe.py').write_text(Path(__file__).read_text())
    if not 0 <= args.turns <= 96:
        raise ValueError('turns must be between 0 and 96')
    results = {'paidTurns': 0, 'provider': 'echo', 'turns': [], 'reads': []}
    with tempfile.TemporaryDirectory(prefix='intercat-history-limits-') as temporary:
        root = Path(temporary)
        config = root / 'config'
        (config / 'muse').mkdir(parents=True)
        (config / 'muse/settings.json').write_text(json.dumps({
            'schema_version': 1, 'provider': 'echo', 'run': {'toolset': []},
            'permissions': {'schema_version': 1, 'default_profile': 'intercat-worker',
                            'profiles': {'intercat-worker': {'extends': ':ask-me'}}},
        }))
        host = Host(binary, root, config, output, False)
        try:
            initialized = host.request('initialize', {'clientInfo': {'name': 'intercat_history_probe', 'version': '0.1.0'}, 'capabilities': {'experimentalApi': True}})
            host.send({'jsonrpc': '2.0', 'method': 'initialized'})
            results['schema'] = initialized['schema']
            started = host.request('session/start', {'commandId': command_id(), 'workspaceRoot': str(root), 'providerId': 'echo', 'approvalMode': 'promptUnmatched'})
            session = started['session']['sessionId']
            results['sessionId'] = session
            results['startCursor'] = started['viewCursor']
            size = 65536
            first_delta = None
            for index in range(1, args.turns + 1):
                command = command_id()
                text = ('OFFLINE_HISTORY_FIXTURE_' * (size // 24 + 1))[:size]
                params = {'commandId': command, 'sessionId': session, 'input': [{'type': 'text', 'text': text}]}
                if args.display_bytes:
                    params['displayText'] = 'd' * args.display_bytes
                ack = host.request('turn/start', params)
                frames = list(host.events)
                host.events.clear()
                deadline = time.monotonic() + 60
                while not any(f.get('method') == 'turn/completed' and f['params']['turnId'] == ack['turnId'] for f in frames):
                    frames.append(host.receive(deadline))
                completed = next(f for f in frames if f.get('method') == 'turn/completed' and f['params']['turnId'] == ack['turnId'])
                if first_delta is None:
                    first_delta = next((f['params']['viewCursor'] for f in frames if f.get('method') == 'item/delta'), None)
                results['turns'].append({'index': index, 'inputBytes': size, 'turnId': ack['turnId'], 'terminal': completed['params']['terminal'], 'notifications': len(frames)})
                if index not in [1, 4, 8, 16, 32, 64, args.turns]:
                    continue
                for preference in ['snapshot', 'auto']:
                    read = host.request('session/resume', {'commandId': command_id(), 'sessionId': session, 'excludeItems': False, 'history': preference})
                    history = read['history']
                    row = {'index': index, 'inputBytes': size, 'requested': preference, 'mode': history['mode'], 'noneReason': history.get('noneReason'), 'bytes': len(json.dumps(read).encode()), 'items': len(history.get('items') or (history.get('snapshot') or {}).get('state', {}).get('items', []))}
                    results['reads'].append(row)
                    (output / f'resume-{index}-{preference}.json').write_text(json.dumps(read) + '\n')
                    print(json.dumps(row), flush=True)
                (output / 'result.json').write_text(json.dumps(results, indent=2) + '\n')
            results['oldCursorRequests'] = []
            for label, cursor in [('start', started['viewCursor']), ('delta', first_delta)]:
                if cursor is None:
                    continue
                for method in ['view/page', 'session/resume']:
                    params = {'sessionId': session, 'cursor': cursor}
                    params.update({'limit': 200} if method == 'view/page' else {'commandId': command_id(), 'excludeItems': False})
                    try:
                        value = host.request(method, params)
                        (output / f'old-{label}-{method.replace("/", "-")}.json').write_text(json.dumps(value) + '\n')
                        row = {'label': label, 'method': method, 'cursor': cursor, 'historyMode': value.get('history', {}).get('mode'), 'events': len(value.get('events', []))}
                    except RuntimeError as error:
                        row = {'label': label, 'method': method, 'cursor': cursor, 'error': json.loads(str(error))}
                    results['oldCursorRequests'].append(row)
                    print(json.dumps(row), flush=True)
            results['errorRequests'] = []
            for label, params in [
                ('missing', {'cursor': 'unknown-test-cursor'}),
                ('latestCompaction', {'anchor': 'latestCompaction'}),
            ]:
                params.update(sessionId=session, limit=200)
                try:
                    value = host.request('view/page', params)
                    row = {'label': label, 'result': value}
                except RuntimeError as error:
                    row = {'label': label, 'error': json.loads(str(error))}
                results['errorRequests'].append(row)
                print(json.dumps(row), flush=True)
            if args.restart:
                host.close()
                cold_output = output / 'cold'
                cold_output.mkdir()
                host = Host(binary, root, config, cold_output, False)
                host.request('initialize', {'clientInfo': {'name': 'intercat_history_probe', 'version': '0.1.0'}, 'capabilities': {'experimentalApi': True}})
                host.send({'jsonrpc': '2.0', 'method': 'initialized'})
                results['coldCursorRequests'] = []
                for label, cursor in [('start', started['viewCursor']), ('delta', first_delta)]:
                    if cursor is None:
                        continue
                    params = {'commandId': command_id(), 'sessionId': session, 'cursor': cursor, 'excludeItems': False}
                    try:
                        value = host.request('session/resume', params)
                        row = {'label': label, 'mode': value['history']['mode'], 'head': value['viewCursor']}
                        (output / f'cold-{label}.json').write_text(json.dumps(value) + '\n')
                    except RuntimeError as error:
                        row = {'label': label, 'error': json.loads(str(error))}
                    results['coldCursorRequests'].append(row)
                    print(json.dumps(row), flush=True)
        except Exception as error:
            results['error'] = str(error)
            raise
        finally:
            host.close()
            (output / 'result.json').write_text(json.dumps(results, indent=2) + '\n')


if __name__ == '__main__':
    main()
