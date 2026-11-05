"""Verify both production execution loops against one pinned TS approval oracle."""
import json
from pathlib import Path
import re
import sys
import uuid

from verify_msp_approvals import sha, tree


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    read = lambda name: json.loads((directory / name).read_text())
    previous = json.loads((directory.parent / 'msp-approval-oracle-20261104/verification.json').read_text())
    cases = read('comparison.json')
    normal_cases = read('normal/comparison.json')
    assert len(cases) == len(normal_cases) == 8
    assert {case['case'] for case in cases} == {case['case'] for case in normal_cases}
    assert all(case['matched'] and case['complete'] and case['sourceUnchanged'] for case in cases)
    assert all(case['matched'] and case['complete'] for case in normal_cases)
    decisions = receipts = starts = 0
    artifacts = ['comparison.json', 'normal/comparison.json', 'build.jsonl']
    for case in cases:
        name = case['case']
        fixture = read(f'{name}/fixture.json')
        assert sha(directory / name / 'fixture.json') == previous['artifactSha256'][f'{name}/fixture.json']
        assert fixture == read(f'normal/{name}/fixture.json')
        expected = read(f'{name}/typescript.json')
        assert expected == read(f'normal/{name}/typescript.json') and not expected['errors']
        normalized = []
        for label, filename in [('repair', f'{name}/repair.json'), ('normal', f'normal/{name}/rust.json')]:
            result = read(filename)
            assert result['complete'] and result['workerError'] == 'host closed stdout'
            assert not result['unexpected']
            assert len(result['sent']) == len(expected['sent']) == fixture['terminalAt']
            assert result['receipts'] == expected['receipts']
            assert len(result['operations']) == len(result['sent'])
            command_ids = set()
            request_ids = set()
            for frame, reference, op in zip(result['sent'], expected['sent'], result['operations']):
                assert frame['method'] == reference['method'] == 'approval/decide'
                params = frame['params']; uuid.UUID(params['commandId'])
                assert params['commandId'] not in command_ids and frame['id'] not in request_ids
                command_ids.add(params['commandId']); request_ids.add(frame['id'])
                assert op['state'] == 'complete'
                assert op['kind'] == ('approval/decide' if label == 'repair' else 'approval.decide')
                saved = op['payload']['request'] if label == 'repair' else op['payload']
                assert saved == params
                expected_params = {key: value for key, value in reference['params'].items() if key != 'commandId'}
                assert {key: value for key, value in params.items() if key != 'commandId'} == expected_params
            normalized.append([{key: value for key, value in frame['params'].items() if key != 'commandId'}
                               for frame in result['sent']])
            if label == 'repair':
                assert result['sourceUnchanged']
                assert len(result['starts']) == len(result['startOperations']) == 1
                start = result['starts'][0]
                operation = result['startOperations'][0]
                assert start['method'] == 'turn/start'
                assert operation['kind'] == 'turn.start' and operation['state'] == 'dispatched'
                assert operation['payload'] == start['params']
                assert start['params']['commandId'] == result['run']['startCommand']
                assert start['params']['sessionId'] == result['run']['session'] == fixture['session']
                assert start['params']['input'][0]['selector'] == 'intercat-report'
                assert result['run']['turn'] == fixture['turn'] and result['run']['phase'] == 'running'
                assert start['params']['commandId'] not in command_ids and start['id'] not in request_ids
                assert 'test result: ok. 1 passed; 0 failed' in (directory / name / 'repair.log').read_text()
                starts += 1
            else:
                assert not result['starts']
        assert normalized[0] == normalized[1]
        decisions += len(expected['sent'])
        receipts += len(expected['receipts'])
        artifacts += [f'{name}/{f}' for f in ['fixture.json', 'typescript.json', 'repair.json', 'repair.log', 'typescript.log']]
        artifacts += [f'normal/{name}/{f}' for f in ['fixture.json', 'typescript.json', 'rust.json', 'rust.log', 'typescript.log']]
    log = (directory / 'rust-tests.log').read_text()
    totals = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', log)
    assert totals and 'FAILED' not in log and 'Doc-tests intercat_git' in log
    assert 'repair_execution::oracle_tests::approvals_match_the_pinned_sdk ... ignored' in log
    clippy = (directory / 'clippy.log').read_text()
    assert 'Finished' in clippy and 'error:' not in clippy
    assert not (directory / 'format.log').read_text().strip()
    regression_inputs = read('full-regression-inputs.json')
    # Only the explicitly rerun fake-peer observer changed after this default
    # regression build; it has no default test and is used by the two probes.
    changed = [name for name, digest in regression_inputs.items() if sha(root / name) != digest]
    assert changed == ['crates/intercat-daemon/tests/support/approval_fixture.rs']
    sdk = root / 'research/verify/support/node_modules/@muse-code/sdk'
    assert json.loads((sdk / 'package.json').read_text())['version'] == '1.4.14'
    sdk_hash = tree(sdk, (sdk / 'dist').rglob('*.js'))
    assert sdk_hash == previous['oracleTreeSha256']
    unchanged = ['crates/intercat-daemon/src/msp.rs', 'crates/intercat-daemon/src/execution.rs',
                 'crates/intercat-daemon/src/approval.rs']
    for path in unchanged:
        assert sha(root / path) == previous['sourceSha256'][path]
    frontend = tree(root, [p for folder in ['src', 'electron'] for p in (root / folder).rglob('*') if p.is_file()])
    assert frontend == previous['frontendTreeSha256']
    paths = unchanged + ['crates/intercat-daemon/src/repair_execution.rs',
        'crates/intercat-daemon/src/repair_execution_oracle.rs',
        'crates/intercat-daemon/tests/support/approval_fixture.rs',
        'crates/intercat-daemon/examples/msp_approval.rs',
        'tools/msp_approval_oracle.mjs', 'tools/run_msp_approval_oracle.py',
        'tools/run_msp_repair_oracle.py', 'tools/verify_msp_repair_oracle.py']
    artifacts += ['rust-tests.log', 'clippy.log', 'format.log', 'full-regression-inputs.json']
    return {'requirement': 'M1.14', 'requirementComplete': False,
        'scope': 'normal and repair execution-loop approval handling', 'casesPerWorker': len(cases),
        'decisionsPerWorker': decisions, 'receiptsPerWorker': receipts, 'repairStartsChecked': starts,
        'privateWorkspaceCases': len(cases), 'sourceCheckoutsUnchanged': True,
        'oracleVersion': '1.4.14', 'oracleTreeSha256': sdk_hash,
        'sameTransportAndStageTrackerAsPreviousProof': True,
        'testObserverRerunAfterRegression': changed,
        'rustPassed': sum(int(p) for p, _ in totals), 'rustIgnored': sum(int(i) for _, i in totals),
        'rustTreeSha256': tree(root, list((root / 'crates').rglob('*.rs'))
            + list((root / 'crates').rglob('Cargo.toml')) + [root / 'Cargo.toml', root / 'Cargo.lock']),
        'frontendTreeSha256': frontend,
        'desktopPassedFromUnchangedFrontend': previous['desktopPassedFromUnchangedFrontend'],
        'remaining': ['full session view', 'snapshot and gap splicing', 'host opening and handshake',
                      'reconnect and resend decisions'],
        'paidTurnsThisTurn': 0, 'cumulativePaidTurns': 154,
        'sourceSha256': {name: sha(root / name) for name in paths},
        'artifactSha256': {name: sha(directory / name) for name in artifacts}}


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in ['casesPerWorker', 'decisionsPerWorker', 'receiptsPerWorker',
          'repairStartsChecked', 'rustPassed', 'rustIgnored', 'requirementComplete']}))
