"""Build ordered, session-scoped replays from every indexed incoming wire log."""
import collections
import hashlib
import json
from pathlib import Path
import sqlite3
import sys

from audit_msp_corpus import audit


def records(root, source):
    path = root / source['path']
    if source['format'] == 'sqlite-events':
        with sqlite3.connect(path.as_uri() + '?mode=ro', uri=True) as db:
            for seq, payload in db.execute("SELECT seq,payload FROM events WHERE kind='msp.received' ORDER BY seq"):
                yield {'sequence': seq}, json.loads(payload)
    else:
        for number, line in enumerate(path.read_bytes().splitlines(), 1):
            try:
                frame = json.loads(line)
            except (ValueError, UnicodeError):
                continue
            if isinstance(frame, dict) and isinstance(frame.get('frame'), dict):
                frame = frame['frame']
            yield {'line': number}, frame


def build(root):
    inventory = audit(root)
    cases = {}
    excluded = collections.Counter()
    for source in inventory['wireLogs']:
        sessions = collections.defaultdict(list)
        for position, frame in records(root, source):
            if not isinstance(frame, dict) or not isinstance(frame.get('method'), str):
                continue
            if frame.get('jsonrpc') != '2.0':
                excluded['notJsonRpc'] += 1
                continue
            if 'id' in frame:
                excluded['requests'] += 1
                continue
            params = frame.get('params')
            if not isinstance(params, dict) or not isinstance(params.get('sessionId'), str):
                excluded['noSession'] += 1
                continue
            sessions[params['sessionId']].append((position, frame))
        for session, entries in sessions.items():
            frames = [frame for _, frame in entries]
            # Capture drivers add `at`/`ms` around otherwise identical wire
            # messages. Keep the original frames, but replay that sequence only
            # once and retain every source locator. emittedAtMs is a wire field.
            wire = [{key: value for key, value in frame.items() if key not in {'at', 'ms'}}
                    for frame in frames]
            encoded = json.dumps(wire, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()
            identity = hashlib.sha256(encoded).hexdigest()
            source_ref = {'path': source['path'], 'sessionId': session,
                          'positions': [position for position, _ in entries]}
            if identity in cases:
                cases[identity]['sources'].append(source_ref)
            else:
                cases[identity] = {'id': identity, 'sessionId': session, 'frames': frames,
                                   'sources': [source_ref]}
    return {'inventory': inventory, 'excluded': dict(excluded),
            'caseIdentityIgnoresCaptureKeys': ['at', 'ms'], 'cases': list(cases.values())}


if __name__ == '__main__':
    result = build(Path(__file__).resolve().parents[1])
    output = Path(sys.argv[1])
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(result, ensure_ascii=False, separators=(',', ':')) + '\n')
    print(json.dumps({'cases': len(result['cases']),
                      'sourceSessions': sum(len(case['sources']) for case in result['cases']),
                      'replayFrames': sum(len(case['frames']) for case in result['cases']),
                      'excluded': result['excluded']}))
