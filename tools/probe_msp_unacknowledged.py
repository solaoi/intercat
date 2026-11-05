"""Prepare an offline lost-ack case; no real host or model is involved."""
import json
import copy
from pathlib import Path
import sys


def build():
    session = '00000000-0000-7000-8000-000000000001'
    command = '00000000-0000-7000-8000-000000000002'
    state = {
        'items': [], 'activeTurn': None, 'queuedTurns': [],
        'pendingApprovals': [], 'pendingUserInputs': [],
        'approvalMode': {'mode': 'promptUnmatched', 'source': 'startup', 'lastCommandId': None},
        'effectiveModel': None, 'goal': None, 'todoList': None, 'branch': None,
        'name': None, 'tokenUsage': {'promptTokens': 0, 'outputTokens': 0, 'totalTokens': 0},
        'turnCount': 0,
    }
    read = {
        'session': {'sessionId': session, 'activeTurnId': None, 'turnCount': 0},
        'viewCursor': 'head', 'sourceRange': {}, 'pendingRequests': [],
        'history': {'mode': 'snapshot', 'snapshot': {'schemaVersion': 1, 'viewCursor': 'head', 'state': state}},
    }
    pending = {'approvals': [], 'userInputs': []}
    operation = {'commandId': command, 'sessionId': session,
                 'input': [{'type': 'text', 'text': 'DISPOSABLE_UNACKNOWLEDGED_INPUT'}]}
    base = {
        'id': 'materialized-after-lost-ack', 'sessionId': session,
        'operations': [operation],
        'before': [{'jsonrpc': '2.0', 'method': 'session/updated',
                    'params': {'sessionId': session, 'viewCursor': 'head', 'sourceRange': {}}}],
        'steps': [{'method': method, 'result': result} for method, result in
                  [('session/read', read), ('approval/listPending', pending),
                   ('session/read', read), ('approval/listPending', pending)]],
        'snapshot': state,
        'replayAck': {'disposition': 'started', 'turnId': command},
        'allowReplay': True,
    }
    def stable(value):
        return [{'method': method, 'result': result} for method, result in
                [('session/read', value), ('approval/listPending', pending),
                 ('session/read', value), ('approval/listPending', pending)]]
    cases = []
    active = copy.deepcopy(read)
    active['session'].update(activeTurnId=command, turnCount=1)
    active['history']['snapshot']['state']['activeTurn'] = {'turnId': command, 'commandId': command}
    for name, error, disconnect, materialized, allowed in [
        ('materialized-after-lost-ack', None, False, True, True),
        ('abandoned', (-32030, 'commandRejected', 'abandoned'), False, False, True),
        ('unknown-rejection-reason', (-32030, 'commandRejected', 'future-reason'), False, False, True),
        ('code-only-durable-rejection', (-32030, 'future-kind', 'future-reason'), False, False, True),
        ('kind-only-durable-rejection', (-32000, 'commandRejected', 'future-reason'), False, False, True),
        ('transient-replay-failure', (-32011, 'notFound', 'missingAnchor'), False, False, True),
        ('second-disconnect', None, True, False, True),
        ('ack-without-materialization', None, False, False, True),
        ('stopped-before-replay', None, False, False, False),
    ]:
        case = copy.deepcopy(base)
        case.update(id=name, allowReplay=allowed, afterSnapshot=(active if materialized else read)['history']['snapshot']['state'])
        if allowed:
            step = {'method': 'turn/start', 'result': case['replayAck']}
            if error:
                code, kind, reason = error
                case['replayError'] = {'code': code, 'message': 'Disposable replay response', 'data': {'kind': kind, 'reason': reason}}
                step = {'method': 'turn/start', 'error': case['replayError']}
            if disconnect:
                case['replayDisconnect'] = True
                step = {'method': 'turn/start', 'disconnect': True}
            case['steps'].append(step)
            if not disconnect and (error is None or error[0] == -32030 or error[1] == 'commandRejected'):
                case['steps'].extend(stable(active if materialized else read))
        cases.append(case)
    return {'cases': cases}


if __name__ == '__main__':
    output = Path(sys.argv[1])
    output.mkdir(parents=True, exist_ok=True)
    (output / 'fixture.json').write_text(json.dumps(build(), indent=2) + '\n')
