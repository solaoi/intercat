"""Audit the full initialize/open sequence and its explicit admission policy."""
import copy
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys
import uuid

from build_msp_handshake_fixture import build
from verify_msp_replay import tree


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def normalized(frames):
    frames = copy.deepcopy(frames)
    for frame in frames:
        if frame.get('params') == {}:
            del frame['params']
        if 'commandId' in frame.get('params', {}):
            assert uuid.UUID(frame['params']['commandId']).version == 7
            frame['params']['commandId'] = 'start-command'
    return frames


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    fixture = json.loads((directory / 'fixture.json').read_text())
    assert fixture == build(root, directory)
    rust = json.loads((directory / 'rust.json').read_text())['cases']
    sdk = json.loads((directory / 'ts.json').read_text())['cases']
    assert len(fixture['cases']) == len(rust) == len(sdk) == 17
    accepted = []
    for case, actual, expected in zip(fixture['cases'], rust, sdk):
        assert case['id'] == actual['id'] == expected['id']
        assert actual['outcome']['ok'] == expected['outcome']['ok']
        expected_frames = normalized(expected['sent'])
        if case['id'] == 'unverified-fingerprint':
            # Intercat pins one tested host. The SDK supports additive upgrades;
            # that broader policy is intentionally not adopted by this product.
            expected_frames = [f for f in expected_frames if f['method'] != 'initialized']
        assert normalized(actual['sent']) == expected_frames, case['id']
        assert not any(f['method'].startswith('turn/') for f in actual['sent'])
        if actual['outcome']['ok']:
            accepted.append(case['id'])
        if case['id'] == 'initialize-once':
            assert actual['outcome']['repeated']['ok'] is False
            assert expected['outcome']['repeated']['ok'] is False
        if case['behavior'] == 'exit':
            assert actual['exit']['code'] == expected['exit']['code'] == case['exitCode']
    assert accepted == ['durable-opening', 'initialize-once']
    for name in ['ephemeral-refused', 'absent-durability-refused', 'unknown-durability-refused']:
        row = next(c for c in rust if c['id'] == name)
        assert [f['method'] for f in row['sent']] == ['initialize', 'initialized']
    assert '2 failed' in (directory / 'before.log').read_text()
    assert '1 passed; 0 failed' in (directory / 'oracle-tests.log').read_text()
    assert '1 passed; 0 failed' in (directory / 'native-tests.log').read_text()
    native_files = list((directory / 'native').glob('*/result.json'))
    assert len(native_files) == 1
    native = json.loads(native_files[0].read_text())
    assert native['sameSession'] and native['newHost'] and native['modelTurns'] == 0
    assert 0 < native['recoveryMs'] <= 20000
    with sqlite3.connect(native_files[0].with_name('events.sqlite').as_uri() + '?mode=ro', uri=True) as db:
        assert db.execute("SELECT count(*) FROM operations WHERE kind='turn.start'").fetchone()[0] == 0
        assert db.execute("SELECT count(*) FROM operations WHERE kind='session/resume' AND state='complete'").fetchone()[0] >= 1
    log = (directory / 'rust-tests.log').read_text()
    assert 'FAILED' not in log and 'Doc-tests intercat_git' in log
    counts = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', log)
    assert sum(int(n) for n, _ in counts) == 464
    assert sum(int(n) for _, n in counts) == 28
    for name in ['initialize_cannot_be_repeated_on_one_connection',
                 'initialize_waits_until_its_notification_is_written',
                 'committed_approval_and_question_responses_use_the_priority_writer']:
        assert name + ' ... ok' in log
    clippy = (directory / 'clippy.log').read_text()
    assert 'Finished' in clippy and not re.search(r'(warning|error):', clippy)
    assert not (directory / 'format.log').read_text().strip()
    prior = json.loads((directory.parent / 'msp-progress-20261105/verification.json').read_text())
    sdk_path = root / 'research/verify/support/node_modules/@muse-code/sdk'
    assert tree(sdk_path, (sdk_path / 'dist').rglob('*.js')) == prior['oracleTreeSha256']
    frontend = tree(root, [p for d in ['src', 'electron'] for p in (root / d).rglob('*') if p.is_file()])
    assert frontend == prior['frontendTreeSha256']
    sources = ['crates/intercat-daemon/src/' + n + '.rs' for n in ['msp', 'runtime', 'runtime_tests', 'repair_host']]
    sources += ['tools/build_msp_handshake_fixture.py', 'tools/fake_msp_handshake.mjs',
                'tools/msp_handshake_oracle.mjs', 'tools/verify_msp_handshake.py']
    source_hashes = json.loads((directory / 'run-sources.json').read_text())
    assert source_hashes == {name: sha(root / name) for name in sources}
    artifacts = ['fixture.json', 'rust.json', 'ts.json', 'before.log', 'oracle-tests.log', 'native-tests.log',
                 'rust-tests.log', 'clippy.log', 'format.log', 'run-sources.json']
    artifacts += [str(p.relative_to(directory)) for p in native_files]
    artifacts += [str(p.with_name('events.sqlite').relative_to(directory)) for p in native_files]
    artifacts += [str(p.relative_to(directory)) for p in sorted(directory.glob('*/*.jsonl'))]
    return {
        'requirement': 'M1.14', 'requirementComplete': False, 'schema': 53,
        'scope': 'SDK handshake plus common ordinary/repair session-opening orchestration, exact sent frames and refusal boundaries',
        'cases': 17, 'acceptedCases': accepted,
        'policyDifferences': ['Intercat refuses an unverified fingerprint before initialized; the general SDK warns',
                              'Intercat requires explicitly durable sessions; the SDK supports other profiles'],
        'nativeIdleCrashRecoveryMs': native['recoveryMs'], 'nativeModelTurns': 0,
        'rustPassed': 464, 'rustIgnored': 28, 'frontendTreeSha256': frontend, 'desktopTestsRerun': False,
        'oracleTreeSha256': prior['oracleTreeSha256'], 'paidTurnsThisStep': 0, 'cumulativePaidTurns': 156,
        'remaining': ['active-worker fault/reconnect/resend SDK comparison',
                      'native retained-history eviction and pruned-boundary reproduction',
                      'actual VoiceOver and all 60 acceptance gates'],
        'sourceSha256': source_hashes, 'artifactSha256': {name: sha(directory / name) for name in artifacts},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1]).resolve()
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in ['cases', 'nativeIdleCrashRecoveryMs', 'rustPassed', 'requirementComplete']}))
