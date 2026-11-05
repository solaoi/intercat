"""Compare every intermediate production projection to the pinned TS facade."""
import collections
import hashlib
import json
from pathlib import Path
import sys


def compare(directory, rust_name='rust.json'):
    fixture = json.loads((directory / 'fixture.json').read_text())
    rust = json.loads((directory / rust_name).read_text())['cases']
    oracle = json.loads((directory / 'ts.json').read_text())['cases']
    assert len(rust) == len(oracle) == len(fixture['cases'])
    differences = []
    compared = 0
    methods = collections.Counter()
    for case, actual, expected in zip(fixture['cases'], rust, oracle):
        assert actual['id'] == expected['id'] == case['id']
        assert len(actual['steps']) == len(expected['steps']) == len(case['frames'])
        bad = [i for i, (a, b) in enumerate(zip(actual['steps'], expected['steps'])) if a != b]
        final_fields = [key for key in actual['final'].keys() | expected['final'].keys()
                        if actual['final'].get(key) != expected['final'].get(key)]
        compared += len(actual['steps'])
        methods.update(frame['method'] for frame in case['frames'])
        if bad or final_fields:
            differences.append({'id': case['id'], 'firstFrame': bad[0] if bad else len(case['frames']),
                                'differentFrames': len(bad),
                                'firstMethod': case['frames'][bad[0]]['method'] if bad else 'afterReplay',
                                'source': case.get('sources', [{'kind': 'synthetic'}])[0],
                                'finalFields': sorted(final_fields)})
    return {'requirementComplete': False,
            'scope': ['all durable item snapshots', 'full turn lifecycle and active turn',
                      'nine session-state families', 'pending and resolved approval payloads',
                      'pending and settled user-input payloads'],
            'remaining': ['transient delta display', 'snapshot and gap splicing',
                          'handshake and method orchestration', 'reconnect and resend decisions'],
            'cases': len(rust), 'sourceSessions': sum(len(case['sources']) for case in fixture['cases'] if not case['id'].startswith('synthetic-')),
            'comparedFrames': compared, 'methods': dict(sorted(methods.items())),
            'differentCases': len(differences), 'differences': differences,
            'artifacts': {name: hashlib.sha256((directory / name).read_bytes()).hexdigest()
                          for name in ['fixture.json', rust_name, 'ts.json']}}


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = compare(directory, sys.argv[2] if len(sys.argv) > 2 else 'rust.json')
    (directory / 'comparison.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in ('cases', 'comparedFrames', 'differentCases')}))
    for difference in result['differences'][:20]:
        print(json.dumps({key: difference[key] for key in ('id', 'firstFrame', 'firstMethod', 'finalFields')}))
    sys.exit(bool(result['differentCases']))
