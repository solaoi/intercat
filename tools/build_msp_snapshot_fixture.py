"""Deterministic snapshot seeds and their durable suffixes."""
import copy
import json
from pathlib import Path
import sys


def build():
    session = '00000000-0000-7000-8000-000000000001'
    serial = 0

    def event(method, cursor, **params):
        nonlocal serial
        serial += 1
        point = {'id': f'00000000-0000-7000-8000-{serial:012d}', 'sequence': serial}
        return {'jsonrpc': '2.0', 'method': method, 'params': {
            'sessionId': session, 'viewCursor': cursor,
            'sourceRange': {'stream': {'id': session, 'kind': 'session'}, 'first': point, 'last': point}, **params}}

    def item(revision, text):
        return {'itemId': 'item', 'kind': 'agentMessage', 'revision': revision,
                'status': 'completed', 'recordedAt': '2026-11-05T00:00:00Z', 'text': text,
                'futureItem': {'retained': True}}

    base = {'items': [item(2, 'seed')], 'activeTurn': None, 'queuedTurns': [],
            'pendingApprovals': [], 'pendingUserInputs': [],
            'approvalMode': {'mode': 'promptUnmatched', 'source': 'startup', 'lastCommandId': None},
            'effectiveModel': None, 'goal': None, 'todoList': None, 'branch': None, 'name': 'snapshot',
            'tokenUsage': {'promptTokens': 2, 'outputTokens': 3, 'totalTokens': 5,
                           'cost': {'usd': 0.0026103860000000006, 'partial': False}},
            'turnCount': 2, 'futureState': {'kept': ['opaque', 9]}}

    def case(name, state=None, after=None, suffix=None, anchored=False):
        state = copy.deepcopy(base if state is None else state)
        snap = {'schemaVersion': 1, 'viewCursor': 'cut', 'state': state, 'futureEnvelope': True}
        if anchored:
            snap['anchor'] = {'boundaryCursor': 'boundary', 'summarizedThrough': 'opaque'}
        return {'id': name, 'sessionId': session,
                'before': [event('item/completed', 'before', item=item(99, 'stale high revision')),
                           event('session/nameChanged', 'cut', name='old')],
                'after': after or [], 'suffix': suffix or [],
                'read': {'session': {'sessionId': session}, 'viewCursor': 'head',
                         'history': {'mode': 'anchoredSnapshot' if anchored else 'snapshot', 'snapshot': snap}}}

    late = case('late-snapshot', after=[event('session/nameChanged', 'later', name='newer')],
                suffix=[event('item/completed', 'head', item=item(3, 'suffix'))])
    nulls = case('null-and-unknown')
    all_state = copy.deepcopy(base)
    all_state.update({'effectiveModel': {'modelId': 'seed-model', 'providerId': None, 'source': 'user'},
                      'reasoningEffort': {'reasoningEffort': 'medium', 'source': 'user'},
                      'goal': {'objective': 'seed goal', 'status': 'active', 'percentComplete': 140},
                      'todoList': {'revision': 3, 'sourceTool': 'fixture', 'items': []},
                      'branch': {'workspaceRoot': '/tmp/snapshot-fixture', 'branch': None, 'vcs': 'git'},
                      'contextUsage': {'windowTokens': 1000, 'usedTokens': 17, 'pressure': 'normal'}})
    families = case('all-state-families', state=all_state, anchored=True,
                    suffix=[event('session/goalChanged', 'cleared', goal=None),
                            event('session/nameChanged', 'head', name='after seed')])
    duplicate = copy.deepcopy(base)
    duplicate['items'].append(item(1, 'last duplicate in seed'))
    duplicates = case('seed-item-order', state=duplicate,
                      suffix=[event('item/completed', 'head', item=item(2, 'new revision'))])
    return {'cases': [late, nulls, families, duplicates]}


if __name__ == '__main__':
    Path(sys.argv[1]).write_text(json.dumps(build(), ensure_ascii=False, indent=2) + '\n')
