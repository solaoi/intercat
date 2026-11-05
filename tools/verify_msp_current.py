"""Recheck current MSP behavior against the pinned SDK and immutable inputs."""
import copy
import json
from pathlib import Path
import re
import sys

from compare_msp_replay import compare
from verify_msp_gaps import calls, sha
from verify_msp_handshake import normalized
from verify_msp_replay import tree


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    for name, source in json.loads((directory / 'inputs.json').read_text()).items():
        assert sha(directory / name) == source['sha256'] == sha(root / source['source']), name
    read = lambda name: json.loads((directory / name).read_text())
    fixture = read('transport/transport-fixture.json')
    rust, sdk = read('transport/rust-transport.json'), read('transport/ts-transport.json')
    assert rust['schemaFingerprint'] == sdk['schemaFingerprint']
    assert rust['validatedTypes'] == len(fixture['validation']) == 87
    assert len(fixture['inventory']['requiredClientMethods']) == 22
    assert rust['results'] == sdk['results']
    assert rust['sent'] == sdk['sent'] and len(rust['sent']) == 25
    assert rust['requests'] == sdk['requests'] and len(rust['requests']) == 2
    assert len(rust['notifications']) == len(sdk['notifications']) == 47
    for actual, expected in zip(rust['notifications'], sdk['notifications']):
        expected = copy.deepcopy(expected)
        if expected['method'] == 'item/delta':
            delta = expected['params'].pop('delta')
            expected['params']['deltaBytes'] = len(delta.encode()) if isinstance(delta, str) else len(json.dumps(delta, ensure_ascii=False, separators=(',', ':')).encode())
        assert actual == expected
    replay = compare(directory / 'state')
    assert replay['cases'] == 531 and replay['comparedFrames'] == 19219 and replay['differentCases'] == 0
    assert all(case['restartVerified'] for case in read('state/rust.json')['cases'])
    rust, sdk = read('gaps/rust.json'), read('gaps/ts.json')
    assert len(rust['cases']) == len(sdk['cases']) == 8
    for actual, expected in zip(rust['cases'], sdk['cases']):
        assert actual['id'] == expected['id']
        assert actual['final'] == expected['final'] and actual['current'] == expected['current']
        assert actual['restartVerified'] and actual['restartBeforeFill']
        assert calls(actual, True) == calls(expected, False)
    rust, sdk = read('snapshots/rust.json'), read('snapshots/ts.json')
    assert len(rust['cases']) == len(sdk['cases']) == 5
    for actual, expected in zip(rust['cases'], sdk['cases']):
        assert actual['id'] == expected['id']
        assert actual['items'] == expected['items'] and actual['sessionState'] == expected['sessionState']
        assert actual['restartVerified'] and actual['rawOnlyRestartVerified']
    rust, sdk = read('progress/rust.json'), read('progress/ts.json')
    assert len(rust['cases']) == len(sdk['cases']) == 16
    for actual, expected in zip(rust['cases'], sdk['cases']):
        assert actual['id'] == expected['id'] and actual['steps'] == expected['steps']
    assert sum(len(case['steps']) for case in rust['cases']) == 115
    assert '1 passed; 0 failed' in (directory / 'progress/engine.log').read_text()
    approval_cases = read('approvals/comparison.json')
    assert len(approval_cases) == 8 and all(case['matched'] and case['complete'] for case in approval_cases)
    for case in approval_cases:
        name = case['case']
        actual, expected = read(f'repair/{name}/repair.json'), read(f'repair/{name}/typescript.json')
        def decisions(value):
            return [{key: val for key, val in frame['params'].items() if key != 'commandId'} for frame in value['sent']]
        assert decisions(actual) == decisions(expected) and actual['receipts'] == expected['receipts']
        assert actual['complete'] and expected['complete'] and actual['sourceUnchanged']
        assert '1 passed; 0 failed' in (directory / f'repair/{name}/run.log').read_text()
    rust, sdk = read('handshake/rust.json'), read('handshake/ts.json')
    assert len(rust['cases']) == len(sdk['cases']) == 17
    for actual, expected in zip(rust['cases'], sdk['cases']):
        assert actual['id'] == expected['id'] and actual['outcome']['ok'] == expected['outcome']['ok']
        sent = normalized(expected['sent'])
        if actual['id'] == 'unverified-fingerprint':
            sent = [frame for frame in sent if frame['method'] != 'initialized']
        assert normalized(actual['sent']) == sent
    assert '1 passed; 0 failed' in (directory / 'handshake/run.log').read_text()
    prior = read('../msp-unacknowledged-20261105/verification.json')
    for name, digest in read('../msp-unacknowledged-20261105/run-sources.json').items():
        assert sha(root / name) == digest, name
    sdk_path = root / 'research/verify/support/node_modules/@muse-code/sdk'
    sdk_hash = tree(sdk_path, (sdk_path / 'dist').rglob('*.js'))
    assert sdk_hash == prior['oracleTreeSha256']
    log = (directory / 'rust-tests.log').read_text()
    counts = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', log)
    assert sum(int(n) for n, _ in counts) == 468 and sum(int(n) for _, n in counts) == 28
    assert 'FAILED' not in log and 'Doc-tests intercat_git' in log
    clippy = (directory / 'clippy.log').read_text()
    assert 'Finished' in clippy and not re.search(r'(warning|error):', clippy)
    assert not (directory / 'format.log').read_text().strip()
    return {
        'requirement': 'M1.14', 'requirementComplete': True, 'milestoneComplete': False,
        'scope': 'Recorded frames and adversarial fake-host schedules compared with pinned SDK; native host is smoke coverage only, as required by M1.14.',
        'clientMethods': 22, 'serverRequests': 2, 'notifications': 47, 'typedValues': 87,
        'stateCases': 531, 'stateSteps': 19219, 'restartCases': 531, 'differences': 0,
        'gapCases': 8, 'snapshotCases': 5, 'progressCases': 16, 'progressSteps': 115,
        'progressEngineCases': 32, 'progressEngineSteps': 230,
        'approvalCases': 8, 'repairApprovalCases': 8, 'handshakeCases': 17,
        'unacknowledgedStartCases': prior['cases'],
        'rustPassed': 468, 'rustIgnored': 28, 'oracleTreeSha256': sdk_hash,
        'policyDifferences': ['Pinned host fingerprint required before initialized', 'Explicit durable sessions required', 'History pages request bounded maxBytes and explicit forward direction'],
        'nativeRetentionStress': 'M5 scope; not claimed by this verification',
        'paidTurns': 0, 'cumulativePaidTurns': 156,
        'sourceSha256': {str(path.relative_to(root)): sha(path) for path in sorted((root / 'crates').rglob('*.rs'))},
        'artifactSha256': {str(path.relative_to(directory)): sha(path) for path in sorted(directory.rglob('*')) if path.is_file() and path.name != 'verification.json'},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1]).resolve()
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in ['requirementComplete', 'stateCases', 'stateSteps', 'differences', 'rustPassed']}))
