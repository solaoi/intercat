"""Index the pinned protocol and recorded MSP frames without printing payloads."""
import collections
import hashlib
import json
from pathlib import Path
import sqlite3
import subprocess
import sys


def audit(repository):
    schema_path = repository / 'research/verify/support/schema-stable/msp.schema.json'
    schema = json.loads(schema_path.read_text())
    design = (repository / 'docs/design.md').read_text()
    table = design.split('#### Intercat が使うメソッド', 1)[1].split('session/setApprovalMode は使わない。', 1)[0]
    required = []
    for line in table.splitlines():
        if line.startswith('| デーモン → ホスト |'):
            required.extend(line.split('|')[2].strip().replace('`', '').split('・'))
    assert len(required) == len(set(required)) == 22
    assert set(required) <= set(schema['methods']) | set(schema['notifications'])
    files = subprocess.check_output(['rg', '--files', 'research/verify', '-g', '*.jsonl', '-g', '*.sqlite'], cwd=repository, text=True).splitlines()
    counts = collections.Counter()
    samples = collections.defaultdict(list)
    invalid = []
    read_files = []
    frame_count = 0
    server_request_ids = 0
    invalid_server_request_ids = []

    def observe(frame, location):
        nonlocal frame_count, server_request_ids
        if not isinstance(frame, dict) or frame.get('jsonrpc') != '2.0' or not isinstance(frame.get('method'), str):
            return False
        method = frame['method']
        kind = 'request' if 'id' in frame else 'notification'
        key = kind + ':' + method
        if kind == 'request' and method in schema['requests']:
            server_request_ids += 1
            identity = frame['id']
            if isinstance(identity, bool) or not isinstance(identity, (int, float)) or identity <= 0 or identity % 1:
                invalid_server_request_ids.append(location)
        counts[key] += 1
        frame_count += 1
        if len(samples[key]) < 3:
            samples[key].append(location)
        return True

    for relative in sorted(files):
        path = Path(relative)
        # Owned Muse caches duplicate raw provider data; the verification
        # drivers' wire logs are the protocol evidence being indexed here.
        if any(part in {'node_modules', 'data', 'sessions', 'workspace', 'workspaces'} for part in path.parts):
            continue
        digest = hashlib.sha256()
        included = 0
        if path.suffix == '.sqlite':
            with sqlite3.connect((repository / path).as_uri() + '?mode=ro', uri=True) as database:
                if not database.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='events'").fetchone():
                    continue
                for sequence, payload in database.execute("SELECT seq,payload FROM events WHERE kind='msp.received' ORDER BY seq"):
                    digest.update(str(sequence).encode() + b'\0' + payload.encode() + b'\n')
                    included += observe(json.loads(payload), {'path': relative, 'sequence': sequence})
            if included:
                read_files.append({'path': relative, 'sha256': digest.hexdigest(), 'frames': included, 'format': 'sqlite-events'})
            continue
        with (repository / path).open('rb') as stream:
            for number, line in enumerate(stream, 1):
                digest.update(line)
                try:
                    frame = json.loads(line)
                except (ValueError, UnicodeError):
                    invalid.append({'path': relative, 'line': number})
                    continue
                if isinstance(frame, dict) and isinstance(frame.get('frame'), dict):
                    frame = frame['frame']
                included += observe(frame, {'path': relative, 'line': number})
        if included:
            read_files.append({'path': relative, 'sha256': digest.hexdigest(), 'frames': included, 'format': 'jsonl'})
    notifications = set(schema['notifications'])
    observed = {key.split(':', 1)[1] for key in counts if key.startswith('notification:')}
    return {'sdkVersion': '1.4.14', 'schemaSha256': hashlib.sha256(schema_path.read_bytes()).hexdigest(),
            'requiredClientMethods': required, 'requiredServerRequests': sorted(schema['requests']),
            'requiredNotifications': sorted(notifications), 'recordedFrames': frame_count,
            'missingRecordedNotifications': sorted(notifications - observed),
            'otherRecordedNotifications': sorted(observed - notifications),
            'counts': dict(sorted(counts.items())), 'samples': dict(sorted(samples.items())),
            'serverRequestIdsChecked': server_request_ids, 'invalidServerRequestIds': invalid_server_request_ids,
            'wireLogs': read_files, 'unparseableLines': invalid}


if __name__ == '__main__':
    output = Path(sys.argv[1])
    output.parent.mkdir(parents=True, exist_ok=True)
    result = audit(Path(__file__).resolve().parents[1])
    output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({'wireLogs': len(result['wireLogs']), 'recordedFrames': result['recordedFrames'],
                      'requiredClientMethods': len(result['requiredClientMethods']),
                      'missingRecordedNotifications': result['missingRecordedNotifications'],
                      'unparseableLines': len(result['unparseableLines'])}, ensure_ascii=False))
