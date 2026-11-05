"""Verify budget/large-response recovery without claiming native expiry coverage."""
import json
from pathlib import Path
import re
import sys

from compare_msp_replay import compare
from verify_msp_gaps import calls, sha
from verify_msp_replay import tree


def verify(directory):
    directory = directory.resolve()
    root = Path(__file__).resolve().parents[1]
    native = json.loads((directory / 'discovery-2/result.json').read_text())
    assert native['provider'] == 'echo' and native['paidTurns'] == 0
    assert len(native['turns']) == 96
    assert all(turn['terminal'] == 'completed' for turn in native['turns'])
    reads = {(r['index'], r['requested']): r for r in native['reads']}
    assert reads[64, 'snapshot']['mode'] == 'snapshot'
    assert reads[64, 'snapshot']['bytes'] > 8 * 1024 * 1024
    assert reads[96, 'snapshot']['mode'] == reads[96, 'auto']['mode'] == 'none'
    assert reads[96, 'snapshot']['noneReason'] == reads[96, 'auto']['noneReason'] == 'historyBudget'
    for name in ['resume-64-snapshot.json', 'resume-96-snapshot.json']:
        assert json.loads((directory / 'discovery-2' / name).read_text())['session']['providerId'] == 'echo'
    errors = json.loads((directory / 'errors/result.json').read_text())['errorRequests']
    assert errors[0]['error']['code'] == -32011
    assert errors[0]['error']['data']['reason'] == 'missingAnchor'
    assert errors[1]['error']['code'] == -32042
    assert errors[1]['error']['data']['kind'] == 'noBoundary'
    cold = json.loads((directory / 'cold-cursor/result.json').read_text())
    assert len(cold['turns']) == 1 and cold['paidTurns'] == 0
    assert all(row.get('mode') == 'none' for row in cold['coldCursorRequests'])
    for name, count in [('discovery-1', 5), ('discovery-display', 1)]:
        auxiliary = json.loads((directory / name / 'result.json').read_text())
        assert len(auxiliary['turns']) == count and auxiliary['paidTurns'] == 0
    large = json.loads((directory / 'large-restored.json').read_text())
    assert large['responseBytes'] > 8 * 1024 * 1024 and large['items'] == 256
    assert large['stateMatches'] and large['restartMatches'] and not large['outputInterrupted']
    large_fixture = json.loads((directory / 'large-fixture.json').read_text())['cases'][0]
    native_large = json.loads((directory / 'discovery-2/resume-64-snapshot.json').read_text())
    assert large_fixture['read'] == native_large
    large_oracle = json.loads((directory / 'large-ts.json').read_text())['cases'][0]
    assert large_oracle['items'] == native_large['history']['snapshot']['state']['items']
    omitted = json.loads((directory / 'none-restored.json').read_text())
    assert omitted['mode'] == 'none' and omitted['items'] == 0
    assert omitted['stateMatches'] and omitted['restartMatches'] and not omitted['outputInterrupted']
    assert 'viewTruncated: Err' in (directory / 'boundary-before.log').read_text()
    assert 'Requested history was rejected' in (directory / 'large-before.log').read_text()
    history = (directory / 'history-tests.log').read_text()
    assert '27 passed; 0 failed' in history
    log = (directory / 'rust-tests.log').read_text()
    assert 'FAILED' not in log and 'Doc-tests intercat_git' in log
    counts = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', log)
    assert sum(int(n) for n, _ in counts) == 454
    assert sum(int(n) for _, n in counts) == 25
    for name in [
        'expired_views_and_pruned_boundaries_retry_the_latest_compaction_once',
        'a_budget_omitted_resume_retains_the_gap_until_paged_history_is_committed',
        'an_unavailable_latest_boundary_preserves_the_gap_without_a_retry_loop',
        'msp::tests::requested_history_can_exceed_the_notification_budget',
        'msp::tests::oversized_unexpected_or_cancelled_replies_keep_the_notification_limit',
        'output_flow::tests::one_history_lease_is_bounded_and_does_not_consume_the_notification_budget',
    ]:
        assert name + ' ... ok' in log, name
    assert 'Finished' in (directory / 'clippy.log').read_text()
    assert not re.search(r'(warning|error):', (directory / 'clippy.log').read_text())
    assert not (directory / 'format.log').read_text().strip()
    replay = compare(directory / 'state-replay')
    assert replay['cases'] == 531 and replay['comparedFrames'] == 19219 and replay['differentCases'] == 0
    gaps = json.loads((directory / 'gaps/rust.json').read_text())
    oracle = json.loads((directory / 'gaps/ts.json').read_text())
    assert len(gaps['cases']) == len(oracle['cases']) == 8
    for observed, expected in zip(gaps['cases'], oracle['cases']):
        assert observed['id'] == expected['id'] and observed['final'] == expected['final']
        assert observed['current'] == expected['current']
        assert observed['restartVerified'] and observed['restartBeforeFill']
        assert calls(observed, True) == calls(expected, False)
    previous = directory.parent / 'msp-unobserved-20261105'
    base = json.loads((previous / 'verification.json').read_text())
    sdk = root / 'research/verify/support/node_modules/@muse-code/sdk'
    assert tree(sdk, (sdk / 'dist').rglob('*.js')) == base['oracleTreeSha256']
    for category in ['state-replay', 'gaps']:
        for name in ['fixture.json', 'ts.json']:
            assert sha(directory / category / name) == sha(previous / category / name)
    frontend = tree(root, [p for d in ['src', 'electron'] for p in (root / d).rglob('*') if p.is_file()])
    assert frontend == base['frontendTreeSha256']
    sources = ['crates/intercat-daemon/src/' + name + '.rs' for name in [
        'msp', 'output_flow', 'history', 'history_store', 'session_snapshot',
        'session_view', 'snapshot_store', 'reconcile', 'reconcile_store']]
    sources += ['crates/intercat-daemon/tests/history.rs',
                'crates/intercat-daemon/examples/msp_history_response.rs',
                'tools/probe_msp_history_limits.py', 'tools/verify_msp_retention.py']
    artifacts = ['discovery-2/result.json', 'discovery-2/probe.py',
                 'discovery-1/result.json', 'discovery-display/result.json',
                 'discovery-2/resume-64-snapshot.json', 'discovery-2/resume-96-snapshot.json',
                 'errors/result.json', 'errors/probe.py', 'cold-cursor/result.json', 'cold-cursor/probe.py',
                 'large-restored.json', 'none-restored.json',
                 'large-fixture.json', 'large-ts.json',
                 'boundary-before.log', 'large-before.log', 'history-tests.log', 'lib-tests.log',
                 'rust-tests.log', 'clippy.log', 'format.log', 'run-settings.json']
    artifacts += [f'{category}/{name}' for category in ['state-replay', 'gaps']
                  for name in ['fixture.json', 'ts.json', 'rust.json']]
    return {
        'requirement': 'M1.14', 'requirementComplete': False, 'schema': 53,
        'scope': 'native historyBudget observation, large captured history receipt, bounded history leases and contract-level expired-cursor re-anchoring',
        'nativeEchoTurns': 96, 'auxiliaryEchoTurns': 7, 'paidTurnsThisTurn': 0, 'cumulativePaidTurns': 156,
        'nativeBudgetDowngrade': True, 'largeHistoryResponseBytes': large['responseBytes'],
        'nativeSnapshotItemsRestored': 256, 'largeSnapshotSDKItemsCompared': 256, 'historyContractTests': 27,
        'rustPassed': 454, 'rustIgnored': 25, 'stateReplayCases': 531,
        'stateReplayFrames': 19219, 'stateReplayDifferences': 0, 'gapCases': 8,
        'frontendTreeSha256': frontend, 'desktopTestsRerunThisTurn': False,
        'remaining': ['native retention eviction and pruned-boundary reproduction',
                      'complete worker recovery after history-budget downgrade',
                      'transient delta display', 'complete host initialization SDK comparison',
                      'actual worker fault/reconnect/resend decisions',
                      'actual VoiceOver and all 60 acceptance gates'],
        'sourceSha256': {name: sha(root / name) for name in sources},
        'artifactSha256': {name: sha(directory / name) for name in artifacts},
    }


if __name__ == '__main__':
    target = Path(sys.argv[1])
    result = verify(target)
    (target / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({k: result[k] for k in ['nativeEchoTurns', 'rustPassed', 'stateReplayCases', 'requirementComplete']}))
