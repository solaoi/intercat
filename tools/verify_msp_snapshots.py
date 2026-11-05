"""Verify stored snapshot splices and explicitly bound the oracle's scope."""
import json
from pathlib import Path
import re
import sys

from build_msp_snapshot_fixture import build
from compare_msp_replay import compare
from verify_msp_gaps import calls, sha
from verify_msp_replay import tree


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    fixture = json.loads((directory / 'fixture.json').read_text())
    assert fixture == build()
    rust = json.loads((directory / 'rust.json').read_text())
    ts = json.loads((directory / 'ts.json').read_text())
    assert len(rust['cases']) == len(ts['cases']) == 4
    assert rust['validatedFrames'] == 13
    for actual, expected, case in zip(rust['cases'], ts['cases'], fixture['cases']):
        assert actual['id'] == expected['id'] == case['id']
        assert actual['items'] == expected['items']
        assert actual['sessionState'] == expected['sessionState']
        assert actual['restartVerified'] and actual['rawOnlyRestartVerified']
        state = actual['fullState']
        original = case['read']['history']['snapshot']['state']
        assert state['items'] == expected['items']
        assert state['name'] == expected['sessionState']['session/nameChanged']['name']
        assert state['goal'] == expected['sessionState']['session/goalChanged']['goal']
        assert state['futureState'] == original['futureState']
        assert state['tokenUsage'] == original['tokenUsage']
        assert state['turnCount'] == original['turnCount']
        assert ('reasoningEffort' in state) == ('reasoningEffort' in original)
    names = ['a_late_snapshot_is_spliced_at_its_cursor_before_the_newer_suffix',
             'an_invalid_snapshot_receipt_cannot_replace_a_committed_seed',
             'replaying_an_anchor_does_not_revive_the_snapshot_turn',
             'snapshot_replaces_absent_families_and_preserves_nulls_and_unknown_item_fields',
             'snapshot_runtime_references_follow_terminal_and_request_events',
             'unsupported_snapshots_are_ignored_and_unanchored_or_contradictory_ones_are_rejected',
             'reconcile_persists_the_served_snapshot_without_dispatching']
    related = (directory / 'related-tests.log').read_text()
    assert sorted(int(n) for n in re.findall(r'test result: ok\. (\d+) passed', related)) == [6, 22, 24]
    log = (directory / 'rust-tests.log').read_text()
    assert 'FAILED' not in log and 'Doc-tests intercat_git' in log
    for name in names:
        assert name + ' ... ok' in related and name + ' ... ok' in log
    totals = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', log)
    assert sum(int(n) for n, _ in totals) == 438
    assert sum(int(n) for _, n in totals) == 25
    assert 'Finished' in (directory / 'clippy.log').read_text()
    assert not (directory / 'format.log').read_text().strip()
    for name, digest in json.loads((directory / 'regression-inputs.json').read_text()).items():
        assert sha(root / name) == digest, name
    initial = directory.parent / 'msp-gap-restart-20261105'
    for name in ['fixture.json', 'ts.json']:
        assert sha(directory / 'state-replay' / name) == sha(initial / 'state-replay' / name)
        assert sha(directory / 'gaps' / name) == sha(initial / name)
    replay = compare(directory / 'state-replay')
    assert replay['cases'] == 531 and replay['comparedFrames'] == 19219 and replay['differentCases'] == 0
    assert all(c['restartVerified'] for c in json.loads((directory / 'state-replay/rust.json').read_text())['cases'])
    gaps = json.loads((directory / 'gaps/rust.json').read_text())
    gaps_ts = json.loads((directory / 'gaps/ts.json').read_text())
    assert len(gaps['cases']) == len(gaps_ts['cases']) == 8
    for actual, expected in zip(gaps['cases'], gaps_ts['cases']):
        assert actual['id'] == expected['id'] and actual['final'] == expected['final']
        assert actual['current'] == expected['current']
        assert actual['restartVerified'] and actual['restartBeforeFill']
        assert calls(actual, True) == calls(expected, False)
    previous = json.loads((initial / 'verification.json').read_text())
    sdk = root / 'research/verify/support/node_modules/@muse-code/sdk'
    assert tree(sdk, (sdk / 'dist').rglob('*.js')) == previous['oracleTreeSha256']
    frontend = tree(root, [p for d in ['src', 'electron'] for p in (root / d).rglob('*') if p.is_file()])
    assert frontend == previous['frontendTreeSha256']
    sources = ['crates/intercat-daemon/src/session_snapshot.rs', 'crates/intercat-daemon/src/session_view.rs',
               'crates/intercat-daemon/src/snapshot_store.rs', 'crates/intercat-daemon/src/reconcile_store.rs',
               'crates/intercat-daemon/src/reconcile.rs', 'crates/intercat-daemon/src/journal.rs',
               'crates/intercat-daemon/tests/snapshots.rs', 'crates/intercat-daemon/tests/reconcile.rs',
               'crates/intercat-daemon/examples/msp_snapshot.rs', 'tools/build_msp_snapshot_fixture.py',
               'tools/msp_snapshot_oracle.mjs', 'tools/verify_msp_snapshots.py']
    artifacts = ['fixture.json', 'rust.json', 'ts.json', 'related-tests.log', 'rust-tests.log',
                 'clippy.log', 'format.log', 'regression-inputs.json', 'snapshot-run.log',
                 'state-replay/fixture.json', 'state-replay/ts.json', 'state-replay/rust.json',
                 'gaps/fixture.json', 'gaps/ts.json', 'gaps/rust.json']
    return {'requirement': 'M1.14', 'requirementComplete': False,
            'scope': 'snapshot seed at an observed history cursor, durable suffix, raw-only and committed restart',
            'snapshotCases': 4, 'validatedSnapshotFrames': 13, 'snapshotContractTests': names,
            'oracleScope': 'official ItemStore.seed and SessionStateStore.seed; not full Session snapshot orchestration',
            'stateReplayCases': 531, 'stateReplayFrames': 19219, 'stateReplayDifferences': 0,
            'gapCases': 8, 'rustPassed': 438, 'rustIgnored': 25, 'schema': 51,
            'remaining': ['snapshot cursors absent from recovered history', 'snapshot-only item timeline projection',
                          'transient delta display', 'host opening and handshake',
                          'actual worker disconnect/reconnect and resend decisions'],
            'frontendTreeSha256': frontend, 'desktopPassedFromUnchangedFrontend': 76,
            'oracleTreeSha256': previous['oracleTreeSha256'],
            'paidTurnsThisTurn': 0, 'cumulativePaidTurns': 154,
            'sourceSha256': {name: sha(root / name) for name in sources},
            'artifactSha256': {name: sha(directory / name) for name in artifacts}}


if __name__ == '__main__':
    target = Path(sys.argv[1])
    result = verify(target)
    (target / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in ['snapshotCases', 'stateReplayCases', 'gapCases',
                                                'rustPassed', 'rustIgnored', 'requirementComplete']}))
