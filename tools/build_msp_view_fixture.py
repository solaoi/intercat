"""Recorded corpus plus schema-typed lifecycle cases absent from the captures."""
import copy
import hashlib
import json
from pathlib import Path
import sys

from build_msp_replay_fixture import build


def fixture(root):
    result = build(root)
    seed_path = root / 'research/verify/msp-oracle-20261104/transport-fixture.json'
    seeds = {entry['frame']['method']: entry['frame']
             for entry in json.loads(seed_path.read_text())['notifications']}
    schema = json.loads((root / 'research/verify/support/schema-stable/msp.schema.json').read_text())
    session = '01900000-0000-7000-8000-000000000001'
    turn = '01900000-0000-7000-8000-000000000002'
    cases = []
    counter = 0

    def frame(method, **changes):
        nonlocal counter
        counter += 1
        value = copy.deepcopy(seeds[method])
        value['params'].update(sessionId=session, viewCursor=f'opaque:{counter}')
        value['params'].update(changes)
        value.pop('id', None)
        return value

    def case(name, frames):
        cases.append({'id': 'synthetic-' + name, 'sessionId': session, 'frames': frames,
                      'sources': [{'kind': 'schema-lifecycle', 'seed': str(seed_path.relative_to(root)),
                                   'seedSha256': hashlib.sha256(seed_path.read_bytes()).hexdigest()}]})

    families = ['modelChanged', 'reasoningEffortChanged', 'goalChanged', 'todoListChanged',
                'branchChanged', 'tokenUsage', 'contextUsage', 'approvalModeChanged', 'nameChanged']
    states = [frame('session/' + name) for name in families]
    states += [frame('session/goalChanged', goal=None), frame('session/branchChanged', branch=None),
               frame('session/nameChanged', name='新しい名前', extension={'preserved': True}),
               frame('session/todoListChanged', revision=1, items=[])]
    states += [copy.deepcopy(states[-1])]
    case('session-families', states)

    def event(method, **changes):
        return frame('turn/' + method, turnId=turn, **changes)

    start = lambda: event('started', commandId=turn)
    end = lambda **kw: event('completed', **kw)
    hints = lambda: [event('retryScheduled'), event('modelRetryScheduled'), event('foregroundCompleted')]
    case('retry-hints-settlement', [start(), *hints(), end(terminal='failed', error={'kind': 'modelError', 'message': 'dummy', 'retryable': False}),
                                  *hints(), start(), end(terminal='cancelled')])
    case('terminal-before-start', [end(terminal='completed'), start(), *hints()])
    case('unknown-terminal', [start(), end(terminal='future_terminal')])
    lower = '01900000-0000-7000-8000-000000000000'
    higher = '01900000-0000-7000-8000-000000000009'
    case('turn-order-and-active', [frame('turn/started', turnId=higher, commandId=higher),
                                  frame('turn/started', turnId=lower, commandId=lower),
                                  frame('turn/completed', turnId=higher, terminal='completed'),
                                  event('retryScheduled'),
                                  frame('turn/completed', turnId=lower, terminal='completed')])
    case('retract-running', [start(), event('retracted', commandId=turn, outcome='declined'),
                            event('retracted', commandId=turn, outcome='accepted'), start(),
                            end(terminal='completed'), *hints()])
    case('retract-settled', [start(), end(terminal='completed'),
                            event('retracted', commandId=turn, outcome='accepted'), end(terminal='failed')])
    case('unqueue', [event('retracted', commandId=turn, outcome='declined'),
                     event('unqueued', commandId=turn), start(), end(terminal='completed'), *hints()])

    def approval(method, **changes):
        return frame('approval/' + method, approvalId='approval-1', turnId=turn, **changes)

    case('approval-views', [approval('updated'), approval('reviewing'), approval('requested'),
                            approval('updated'), approval('requested'), approval('updated'),
                            approval('resolved'), approval('resolved', decision='denied', policyResult='deny'), approval('requested'),
                            approval('updated')])
    case('question-views', [frame('userInput/requested', userInputId='question-1', turnId=turn),
                            frame('userInput/requested', userInputId='question-1', turnId=turn),
                            frame('userInput/requested', userInputId='question-2', turnId=turn),
                            frame('userInput/settled', userInputId='question-1', turnId=turn),
                            frame('userInput/settled', userInputId='question-1', turnId=turn, reason='later losing settlement'),
                            frame('userInput/requested', userInputId='question-1', turnId=turn)])
    item = copy.deepcopy(seeds['item/started']['params']['item'])
    item.pop('turnId', None)
    item.update(itemId='session-item', revision=0)
    case('items-without-turn', [frame('item/started', item=item),
                               frame('item/completed', item={**item, 'status': 'completed'}),
                               frame('item/updated', item={**item, 'revision': 1, 'status': 'completed'}),
                               frame('item/started', item=item)])
    result['recordedCases'] = len(result['cases'])
    result['syntheticCases'] = len(cases)
    result['cases'].extend(cases)
    result['validation'] = [{'name': schema['notifications'][f['method']]['params']['$ref'].split('/')[-1],
                             'value': f['params']} for case in cases for f in case['frames']]
    return result


if __name__ == '__main__':
    value = fixture(Path(__file__).resolve().parents[1])
    Path(sys.argv[1]).write_text(json.dumps(value, ensure_ascii=False, separators=(',', ':')) + '\n')
    print(json.dumps({'recordedCases': value['recordedCases'], 'syntheticCases': value['syntheticCases'],
                      'frames': sum(len(case['frames']) for case in value['cases']),
                      'typedSyntheticFrames': len(value['validation'])}))
