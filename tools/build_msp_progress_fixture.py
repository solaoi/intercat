"""Build streaming cases, preserving a native stream and explicit edge cases."""
import json
from pathlib import Path
import sys

S = '00000000-0000-7000-8000-000000000001'
T = '00000000-0000-7000-8000-000000000002'
U = '00000000-0000-7000-8000-000000000003'


def frame(method, **params):
    return {'frame': {'jsonrpc': '2.0', 'method': method, 'params': {'sessionId': S, **params}}}


def send(turn=T):
    return {'send': {'commandId': turn, 'sessionId': S, 'input': [{'type': 'text', 'text': 'offline'}]}}


def item(method='item/started', revision=1, text='', identity='i', turn=T, kind='agentMessage'):
    return frame(method, item={'itemId': identity, 'turnId': turn, 'kind': kind,
                              'revision': revision, 'text': text,
                              'status': 'completed' if method == 'item/completed' else 'inProgress'})


def delta(text, identity='i', field=None):
    return frame('item/delta', itemId=identity, delta=text, **({} if field is None else {'field': field}))


def build(root):
    cases = []
    def add(name, actions, initial=None):
        cases.append({'id': name, 'sessionId': S, 'initialTurn': initial, 'actions': actions})
    add('unicode-stream', [send(), frame('turn/started', turnId=T, commandId=T), item(),
                          delta('こんにちは'), delta('🌱\n', field='text'),
                          item('item/completed', 2, 'こんにちは🌱\n'), frame('turn/completed', turnId=T, terminal='completed')])
    add('same-and-older-revisions', [send(), item(), delta('live\n'), item(text='duplicate'),
                                    item('item/updated', 1, 'same'), item('item/updated', 0, 'older'),
                                    delta('tail\n'), item('item/completed', 2, 'authoritative\n'),
                                    item('item/updated', 2, 'same terminal'), delta('late')])
    add('authoritative-replacement', [send(), item(), delta('old\n'), item('item/updated', 2, 'new\n'),
                                      delta('tail\n'), item('item/completed', 3, 'final\n'),
                                      item('item/updated', 4, 'corrected\n')])
    add('delta-before-item', [send(), delta('early 日本語\n'), item(), delta('tail\n'),
                             item('item/completed', 2, 'early 日本語\ntail\n')])
    add('early-authoritative-body', [send(), delta('old early'), item(text='new authoritative\n'), delta('tail\n')])
    add('duplicate-turn-start', [send(), frame('turn/started', turnId=T, commandId=T), item(), delta('live\n'),
                                 frame('turn/started', turnId=T, commandId=T), delta('retained\n')])
    add('duplicate-command', [send(), item(), delta('live\n'), send(), item(), delta('retained\n')])
    add('retired-and-unrelated-turn', [send(), item(), delta('done\n'), frame('turn/completed', turnId=T, terminal='completed'),
                                      frame('turn/started', turnId=T, commandId=T), item('item/updated', 2, 'late'),
                                      frame('turn/started', turnId=U, commandId=U), item(text='unsolicited', identity='j', turn=U),
                                      send(U), frame('turn/started', turnId=T, commandId=T), item('item/updated', 3, 'older turn'),
                                      item(text='new\n', identity='j', turn=U, revision=2)])
    foreign = delta('foreign')
    foreign['frame']['params']['sessionId'] = 'other'
    add('scope-and-field', [send(), item(), delta('visible\n'), foreign, delta('hidden', field='summary.0'),
                            item(text='tool', identity='tool', kind='toolCall'), delta('tool suffix', identity='tool'),
                            item(text='another turn', identity='foreign', turn=U), delta('foreign suffix', identity='foreign'),
                            delta('more\n')])
    add('gap-discards-prefix', [send(), item(), delta('before\n'), frame('view/gap', next='cursor'), delta('missing suffix'),
                                send(U), item(text='fresh\n', identity='j', turn=U)])
    add('resumed-active-turn', [item(text='snapshot\n'), delta('tail\n'), item('item/completed', 2, 'finished\n')], T)
    add('unicode-display-bound', [send(), item(), delta('🌱' * 17000), delta('discard'), item('item/completed', 2, 'complete\n')])

    native = root / 'research/verify/msp-retention-20261105/discovery-display/received.jsonl'
    actions = []
    turn = session = None
    for number, line in enumerate(native.read_text().splitlines(), 1):
        value = json.loads(line)
        if value.get('method') == 'turn/started' and turn is None:
            turn = value['params']['turnId']
            session = value['params']['sessionId']
            actions.append({'send': {'commandId': turn, 'sessionId': session, 'input': [{'type': 'text', 'text': 'offline native replay'}]}})
        if turn is not None and 'method' in value and value.get('params', {}).get('sessionId') == session:
            actions.append({'frame': value, 'line': number})
        if value.get('method') == 'turn/completed' and value['params']['turnId'] == turn:
            break
    assert actions and actions[-1]['frame']['method'] == 'turn/completed'
    cases.append({'id': 'native-echo-stream', 'sessionId': session, 'initialTurn': None,
                  'source': str(native.relative_to(root)), 'actions': actions})
    return {'cases': cases}


if __name__ == '__main__':
    root = Path(__file__).resolve().parents[1]
    output = Path(sys.argv[1])
    output.parent.mkdir(parents=True, exist_ok=True)
    data = build(root)
    output.write_text(json.dumps(data, ensure_ascii=False) + '\n')
    print(json.dumps({'cases': len(data['cases']), 'actions': sum(len(c['actions']) for c in data['cases'])}))
