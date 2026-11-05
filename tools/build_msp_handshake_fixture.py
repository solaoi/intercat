"""Handshake/opening cases based on a captured pinned-host initialize reply."""
import copy
import json
from pathlib import Path
import sqlite3
import sys


def build(root, directory):
    source = root / 'research/verify/msp-budget-runtime-20261105/native-8/events.sqlite'
    with sqlite3.connect(source.as_uri() + '?mode=ro', uri=True) as db:
        frame = json.loads(db.execute("SELECT payload FROM events WHERE kind='msp.received' AND json_extract(payload,'$.result.schema.fingerprint') IS NOT NULL LIMIT 1").fetchone()[0])
    base = frame['result']
    opened = {'session': {'sessionId': '00000000-0000-7000-8000-000000000010',
                         'permissionState': {'profileId': 'intercat-worker'}},
              'viewCursor': 'v4:00000000-0000-7000-8000-000000000010:0'}
    cases = []
    def add(name, kind='opening', **changes):
        case = {'id': name, 'kind': kind, 'behavior': 'reply', 'initialize': copy.deepcopy(base),
                'opened': copy.deepcopy(opened), 'skills': {'skills': [{'selector': 'intercat-report'}]}}
        case.update(changes)
        cases.append(case)
        return case
    add('durable-opening')
    add('initialize-once', 'handshake', repeat=True)
    add('absent-fingerprint')['initialize']['schema'] = {}
    add('non-string-fingerprint')['initialize']['schema']['fingerprint'] = 3
    add('unverified-fingerprint')['initialize']['schema']['fingerprint'] = 'sha256:unverified'
    add('ephemeral-refused')['initialize']['sessionDurability'] = 'ephemeral'
    del add('absent-durability-refused')['initialize']['sessionDurability']
    add('unknown-durability-refused')['initialize']['sessionDurability'] = 'future-mode'
    add('foreign-profile-refused')['opened']['session']['permissionState']['profileId'] = ':unrestricted'
    del add('absent-session-refused')['opened']['session']['sessionId']
    add('missing-report-skill')['skills']['skills'] = []
    add('initialize-error', behavior='initialize-error')
    add('session-error', behavior='session-error')
    add('initialized-write-failed', behavior='close-stdin')
    add('host-eof', behavior='exit', exitCode=0)
    add('host-config-exit', behavior='exit', exitCode=78)
    add('malformed-reply', behavior='malformed')
    return {'source': str(source.relative_to(root)), 'workspace': str(directory / 'workspace'),
            'initializeParams': {'clientInfo': {'name': 'intercat', 'version': '0.1.0'},
                                 'capabilities': {'experimentalApi': True, 'userInputDialogs': True}},
            'cases': cases}


if __name__ == '__main__':
    root = Path(__file__).resolve().parents[1]
    directory = Path(sys.argv[1]).resolve()
    directory.mkdir(parents=True, exist_ok=True)
    (directory / 'workspace').mkdir(exist_ok=True)
    data = build(root, directory)
    (directory / 'fixture.json').write_text(json.dumps(data, indent=2) + '\n')
    print(json.dumps({'cases': len(data['cases'])}))
