"""Verify the exercised approval paths; do not claim all of M1.14."""
import hashlib
import json
from pathlib import Path
import re
import sys
import uuid


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def tree(root, paths):
    value = hashlib.sha256()
    for path in sorted(paths):
        value.update(str(path.relative_to(root)).encode() + b'\0' + bytes.fromhex(sha(path)))
    return value.hexdigest()


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    read = lambda name: json.loads((directory / name).read_text())
    cases = read('comparison.json')
    assert len(cases) == 8 and all(c['matched'] and c['complete'] for c in cases)
    assert {c['case'] for c in cases} == {'update_after_ack', 'update_before_open_ack',
        'update_before_terminal_ack', 'duplicate_requests_after_terminal', 'abort_initial',
        'abort_updated_stage', 'abort_update_before_ack', 'server_request_receipt'}
    before = read('rust-before.json')
    assert len(before['sent']) == 1 and not before['complete']
    assert before['workerError'] == 'host closed stdout'
    decisions = receipts = 0
    artifacts = ['fixture.json', 'comparison.json', 'rust-before.json', 'rust-before.log']
    for case in cases:
        name = case['case']
        fixture = read(f'{name}/fixture.json')
        actual = read(f'{name}/rust.json')
        expected = read(f'{name}/typescript.json')
        assert not expected['errors'] and actual['complete'] and expected['complete']
        assert actual['workerError'] == 'host closed stdout'
        assert len(actual['sent']) == len(expected['sent']) == fixture['terminalAt']
        assert len(actual['operations']) == len(actual['sent'])
        command_ids = set()
        request_ids = set()
        for frame, other, operation in zip(actual['sent'], expected['sent'], actual['operations']):
            assert frame['method'] == other['method'] == 'approval/decide'
            params = frame['params']
            uuid.UUID(params['commandId'])
            assert params['commandId'] not in command_ids
            assert frame['id'] not in request_ids
            assert isinstance(frame['id'], int) and not isinstance(frame['id'], bool) and frame['id'] > 0
            command_ids.add(params['commandId'])
            request_ids.add(frame['id'])
            assert operation['payload']['commandId'] == params['commandId']
            assert operation['kind'] == 'approval.decide' and operation['state'] == 'complete'
            assert operation['payload'] == params
            assert {k: v for k, v in params.items() if k != 'commandId'} == {
                k: v for k, v in other['params'].items() if k != 'commandId'}
            stage = fixture['request'] if params['requirementId']['sourceIndex'] == 0 else fixture['update']
            assert params['requirementId'] == stage['currentRequirementId']
            assert params['choiceId'] in {c['choiceId'] for c in stage['availableChoices']}
        assert actual['receipts'] == expected['receipts']
        if fixture['serverRequest']:
            assert actual['receipts'] == [{'jsonrpc': '2.0', 'id': 1001, 'result': {}}]
        else:
            assert not actual['receipts']
        decisions += len(actual['sent']); receipts += len(actual['receipts'])
        artifacts.extend(f'{name}/{filename}' for filename in ['fixture.json', 'rust.json', 'typescript.json', 'rust.log', 'typescript.log'])
    # Receipt automation must preserve the previously exercised 22-method wire contract.
    wire = read('wire-rust.json')
    old_dir = directory.parent / 'msp-oracle-20261104'
    oracle = json.loads((old_dir / 'ts-transport.json').read_text())
    old_rust = json.loads((old_dir / 'rust-transport.json').read_text())
    wire_proof = json.loads((old_dir / 'verification.json').read_text())
    assert sha(old_dir / 'rust-transport.json') == wire_proof['artifactSha256']['rust-transport.json']
    assert wire == old_rust
    assert wire['sent'] == oracle['sent'] and len(wire['sent']) == 25
    # The same 520 recorded-session cases must still match after transport changes.
    state = read('recorded-state.json')['cases']
    state_dir = directory.parent / 'msp-state-replay-20261104'
    old_state = json.loads((state_dir / 'rust.json').read_text())['cases']
    assert state == old_state and len(state) == 520
    total_frames = sum(len(case['steps']) for case in state)
    rust_log = (directory / 'rust-tests.log').read_text()
    totals = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust_log)
    assert totals and 'FAILED' not in rust_log and 'Doc-tests intercat_git' in rust_log
    for name in ['clearing_finished_turns_preserves_terminal_decisions',
                 'duplicate_of_the_inflight_stage_does_not_erase_a_waiting_update',
                 'repair_approval_requires_an_explicit_terminal_ack',
                 'approval_receipt_precedes_consumption_and_never_decides_the_request']:
        assert name + ' ... ok' in rust_log
    clippy = (directory / 'clippy.log').read_text()
    assert 'Finished' in clippy and 'error:' not in clippy
    assert not (directory / 'format.log').read_text().strip()
    sdk = root / 'research/verify/support/node_modules/@muse-code/sdk'
    assert json.loads((sdk / 'package.json').read_text())['version'] == '1.4.14'
    sdk_hash = tree(sdk, (sdk / 'dist').rglob('*.js'))
    previous = json.loads((state_dir / 'verification.json').read_text())
    assert sha(state_dir / 'rust.json') == previous['artifactSha256']['rust.json']
    assert sha(state_dir / 'fixture.json') == previous['artifactSha256']['fixture.json']
    assert sdk_hash == previous['oracleTreeSha256']
    frontend_hash = tree(root, [p for folder in ['src', 'electron'] for p in (root / folder).rglob('*') if p.is_file()])
    assert frontend_hash == previous['frontendTreeSha256']
    artifacts += ['wire-rust.json', 'recorded-state.json', 'rust-tests.log', 'clippy.log', 'format.log']
    paths = ['crates/intercat-daemon/src/approval.rs', 'crates/intercat-daemon/src/execution.rs',
             'crates/intercat-daemon/src/repair_execution.rs', 'crates/intercat-daemon/src/msp.rs',
             'crates/intercat-daemon/examples/msp_approval.rs', 'crates/intercat-daemon/examples/msp_transport.rs',
             'tools/build_approval_fixture.py', 'tools/msp_approval_oracle.mjs',
             'tools/run_msp_approval_oracle.py', 'tools/verify_msp_approvals.py']
    return {'requirement': 'M1.14', 'requirementComplete': False,
            'scope': 'normal worker approval decisions, shared stage tracker and presentation receipt',
            'casesCompared': len(cases), 'decisionsCompared': decisions, 'receiptsCompared': receipts,
            'typedFixtureTypes': ['ApprovalRequestParams', 'ApprovalUpdatedParams'],
            'durableCommandsMatched': decisions,
            'wireClientMethodsPreserved': 22, 'recordedStateCasesPreserved': len(state),
            'recordedStateFramesPreserved': total_frames,
            'rustPassed': sum(int(p) for p, _ in totals), 'rustIgnored': sum(int(i) for _, i in totals),
            'oracleTreeSha256': sdk_hash, 'frontendTreeSha256': frontend_hash,
            'desktopPassedFromUnchangedFrontend': previous['desktopPassedFromUnchangedFrontend'],
            'rustTreeSha256': tree(root, list((root / 'crates').rglob('*.rs'))
                + list((root / 'crates').rglob('Cargo.toml')) + [root / 'Cargo.toml', root / 'Cargo.lock']),
            'remaining': ['repair-worker end-to-end oracle comparison', 'full session view',
                          'snapshot and gap splicing', 'handshake/method orchestration',
                          'reconnect and resend decisions'],
            'paidTurnsThisTurn': 0, 'cumulativePaidTurns': 154,
            'sourceSha256': {name: sha(root / name) for name in paths},
            'artifactSha256': {name: sha(directory / name) for name in artifacts}}


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({k: result[k] for k in ['casesCompared', 'decisionsCompared', 'receiptsCompared',
         'recordedStateCasesPreserved', 'rustPassed', 'rustIgnored', 'requirementComplete']}))
