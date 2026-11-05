"""Verify durable session views against the pinned TS facade; M1.14 stays open."""
import hashlib
import json
from pathlib import Path
import re
import sys
import uuid

from build_msp_view_fixture import fixture as build_fixture
from compare_msp_replay import compare


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def tree(root, paths):
    digest = hashlib.sha256()
    for path in sorted(paths):
        digest.update(str(path.relative_to(root)).encode() + b'\0' + bytes.fromhex(sha(path)))
    return digest.hexdigest()


def verify_protocol_regressions(directory):
    wire = directory / 'wire'
    rust = json.loads((wire / 'rust.json').read_text())
    oracle = json.loads((wire / 'ts.json').read_text())
    assert rust['validatedTypes'] == 162
    assert rust['schemaFingerprint'] == oracle['schemaFingerprint']
    assert rust['results'] == oracle['results']
    assert rust['sent'] == oracle['sent'] and len(rust['sent']) == 25
    assert rust['requests'] == oracle['requests']
    assert len(rust['notifications']) == len(oracle['notifications']) == 47
    for actual, expected in zip(rust['notifications'], oracle['notifications']):
        if expected['method'] == 'item/delta':
            expected = json.loads(json.dumps(expected))
            delta = expected['params'].pop('delta')
            expected['params']['deltaBytes'] = len(delta.encode())
        assert actual == expected
    for label, folder, output in [('normal', 'approval-normal', 'rust.json'),
                                  ('repair', 'approval-repair', 'repair.json')]:
        base = directory / folder
        summary = json.loads((base / 'comparison.json').read_text())
        assert len(summary) == 8 and sum(c['decisions'] for c in summary) == 13
        assert all(c['matched'] and c['complete'] for c in summary)
        receipts = 0
        for entry in summary:
            case = base / entry['case']
            actual = json.loads((case / output).read_text())
            expected = json.loads((case / 'typescript.json').read_text())
            assert actual['complete'] and not actual['unexpected'] and not expected['errors']
            assert actual['receipts'] == expected['receipts']
            receipts += len(actual['receipts'])
            assert len(actual['operations']) == len(actual['sent']) == len(expected['sent'])
            commands, ids = set(), set()
            for frame, reference, op in zip(actual['sent'], expected['sent'], actual['operations']):
                params = frame['params']
                uuid.UUID(params['commandId'])
                assert params['commandId'] not in commands and frame['id'] not in ids
                commands.add(params['commandId']); ids.add(frame['id'])
                assert {k:v for k,v in params.items() if k != 'commandId'} == {k:v for k,v in reference['params'].items() if k != 'commandId'}
                assert op['state'] == 'complete'
                assert op['kind'] == ('approval/decide' if label == 'repair' else 'approval.decide')
                saved = op['payload']['request'] if label == 'repair' else op['payload']
                assert saved == params
            if label == 'repair':
                assert actual['sourceUnchanged'] and len(actual['starts']) == len(actual['startOperations']) == 1
                start, op = actual['starts'][0], actual['startOperations'][0]
                assert op['kind'] == 'turn.start' and op['state'] == 'dispatched'
                assert op['payload'] == start['params']
                assert start['params']['commandId'] == actual['run']['startCommand']
                assert 'test result: ok. 1 passed; 0 failed' in (case / 'repair.log').read_text()
            else:
                assert not actual['starts']
        assert receipts == 1
    return {'sentFrames': 25, 'receivedNotifications': 47, 'typedValuesValidated': 162, 'approvalCasesPerWorker': 8,
            'approvalDecisionsPerWorker': 13, 'approvalReceiptsPerWorker': 1}


def safe_numbers(value):
    if type(value) is int:
        assert abs(value) <= 2**53, 'An integer cannot be compared exactly as binary64'
    elif isinstance(value, dict):
        for child in value.values():
            safe_numbers(child)
    elif isinstance(value, list):
        for child in value:
            safe_numbers(child)


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    fixture = json.loads((directory / 'fixture.json').read_text())
    safe_numbers([case['frames'] for case in fixture['cases']])
    assert build_fixture(root) == fixture, 'The input must include the current recorded corpus'
    assert fixture['recordedCases'] == 520 and fixture['syntheticCases'] == 11
    assert len(fixture['validation']) == 75
    assert not fixture['inventory']['unparseableLines']
    comparison = compare(directory)
    protocol = verify_protocol_regressions(directory)
    assert comparison['differentCases'] == 0
    results = json.loads((directory / 'rust.json').read_text())['cases']
    assert all(case['restartVerified'] for case in results)
    for case in results:
        assert all(re.fullmatch('[a-f0-9]{64}', digest) for digest in case['steps'])
        assert set(case['final']) == {'items', 'turns', 'activeTurnId', 'sessionState',
            'pendingApprovals', 'resolvedApprovals', 'pendingUserInputs', 'settledUserInputs'}
    state_case = next(c for c in results if c['id'] == 'synthetic-session-families')['final']
    assert len(state_case['sessionState']) == 9
    assert state_case['sessionState']['session/goalChanged']['goal'] is None
    assert state_case['sessionState']['session/branchChanged']['branch'] is None
    assert state_case['sessionState']['session/nameChanged']['extension'] == {'preserved': True}
    assert 'A previous turn still has an unknown outcome' in (directory / 'retract-before.log').read_text()
    assert 'test retracted_turn_is_not_active_after_restart ... FAILED' in (directory / 'retract-before.log').read_text()
    assert '21 passed; 0 failed' in (directory / 'reconcile-after.log').read_text()
    assert 'usage_decimal_preserves_the_wire_value_through_storage ... FAILED' in (directory / 'numeric-before.log').read_text()
    assert 'usage_decimal_preserves_the_wire_value_through_storage ... ok' in (directory / 'reconcile-after.log').read_text()
    assert 'Finished' in (directory / 'clippy.log').read_text()
    assert not (directory / 'format.log').read_text().strip()
    log = (directory / 'rust-tests.log').read_text()
    totals = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', log)
    inputs = json.loads((directory / 'regression-inputs.json').read_text())
    assert all(sha(root / name) == digest for name, digest in inputs.items())
    assert totals and 'FAILED' not in log and 'Doc-tests intercat_git' in log
    assert 'retracted_turn_is_not_active_after_restart ... ok' in log
    sdk = root / 'research/verify/support/node_modules/@muse-code/sdk'
    assert json.loads((sdk / 'package.json').read_text())['version'] == '1.4.14'
    sdk_tree = tree(sdk, (sdk / 'dist').rglob('*.js'))
    previous = json.loads((directory.parent / 'msp-repair-oracle-20261104/verification.json').read_text())
    assert sdk_tree == previous['oracleTreeSha256']
    unchanged = ['crates/intercat-daemon/src/msp.rs', 'crates/intercat-daemon/src/execution.rs',
                 'crates/intercat-daemon/src/approval.rs', 'crates/intercat-daemon/src/repair_execution.rs']
    for name in unchanged:
        assert sha(root / name) == previous['sourceSha256'][name], name
    frontend = tree(root, [p for folder in ['src', 'electron'] for p in (root / folder).rglob('*') if p.is_file()])
    assert frontend == previous['frontendTreeSha256']
    paths = ['crates/intercat-daemon/src/session_view.rs', 'crates/intercat-daemon/src/reconcile_store.rs',
             'crates/intercat-daemon/src/reconcile.rs', 'crates/intercat-daemon/tests/reconcile.rs',
             'crates/intercat-daemon/tests/history.rs', 'crates/intercat-daemon/tests/reconcile_native.rs',
             'Cargo.toml',
             'crates/intercat-daemon/examples/msp_replay.rs', 'tools/build_msp_view_fixture.py',
             'tools/build_msp_replay_fixture.py', 'tools/audit_msp_corpus.py',
             'tools/msp_replay_oracle.mjs', 'tools/compare_msp_replay.py', 'tools/verify_msp_replay.py']
    artifacts = ['fixture.json', 'ts.json', 'rust.json', 'retract-before.log', 'reconcile-after.log',
                 'rust-tests.log', 'clippy.log', 'format.log', 'build.log', 'rust.log', 'ts.log', 'numeric-before.log', 'regression-inputs.json']
    artifacts += [str(p.relative_to(directory)) for folder in ['wire', 'approval-normal', 'approval-repair']
                  for p in (directory / folder).rglob('*') if p.is_file()]
    return {**comparison, 'protocolRegression': protocol, 'requirement': 'M1.14', 'requirementComplete': False,
            'recordedCases': fixture['recordedCases'], 'syntheticCases': fixture['syntheticCases'],
            'syntheticFramesValidated': len(fixture['validation']),
            'recordedSources': len(fixture['inventory']['wireLogs']),
            'restartVerifiedCases': len(results), 'retractionMismatchVerifiedBeforeFix': True,
            'numericRoundingMismatchVerifiedBeforeFix': True,
            'stepHashNumbers': 'IEEE-754 binary64 bits, without decimal-format differences',
            'oracleVersion': '1.4.14', 'oracleTreeSha256': sdk_tree,
            'rustPassed': sum(int(p) for p, _ in totals), 'rustIgnored': sum(int(i) for _, i in totals),
            'rustTreeSha256': tree(root, list((root / 'crates').rglob('*.rs'))
                + list((root / 'crates').rglob('Cargo.toml')) + [root / 'Cargo.toml', root / 'Cargo.lock']),
            'frontendTreeSha256': frontend, 'desktopPassedFromUnchangedFrontend': 76,
            'unchangedTransportAndApprovalSources': unchanged,
            'paidTurnsThisTurn': 0, 'cumulativePaidTurns': 154,
            'sourceSha256': {name: sha(root / name) for name in paths},
            'artifactSha256': {name: sha(directory / name) for name in artifacts}}


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in ['cases', 'comparedFrames', 'differentCases',
        'restartVerifiedCases', 'rustPassed', 'rustIgnored', 'requirementComplete']}))
