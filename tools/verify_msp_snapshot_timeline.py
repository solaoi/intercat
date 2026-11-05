"""Check snapshot projection through the native journal and Electron view."""
import json
from pathlib import Path
import re
import sys

from verify_msp_gaps import sha


def verify(directory):
    directory = directory.resolve()
    root = Path(__file__).resolve().parents[1]
    previous = directory.parent / 'msp-snapshot-20261105'
    base = json.loads((previous / 'verification.json').read_text())
    baseline = json.loads((previous / 'regression-inputs.json').read_text())
    stable = ['crates/intercat-daemon/src/session_view.rs',
              'crates/intercat-daemon/src/session_snapshot.rs',
              'crates/intercat-daemon/src/reconcile.rs',
              'crates/intercat-daemon/src/reconcile_store.rs',
              'crates/intercat-daemon/src/msp.rs', 'crates/intercat-daemon/src/history.rs']
    for name in stable:
        assert sha(root / name) == baseline[name], name
    for name in ['fixture.json', 'ts.json']:
        assert sha(directory / name) == sha(previous / name)
    rust = json.loads((directory / 'rust.json').read_text())
    ts = json.loads((directory / 'ts.json').read_text())
    assert rust['schemaVersion'] == 52 and rust['validatedFrames'] == 13
    assert len(rust['cases']) == len(ts['cases']) == 4
    for actual, expected in zip(rust['cases'], ts['cases']):
        assert actual['id'] == expected['id']
        assert actual['items'] == expected['items'] and actual['sessionState'] == expected['sessionState']
        assert actual['restartVerified'] and actual['rawOnlyRestartVerified']
        assert len(actual['timeline']['clips']) == 1
        clip = actual['timeline']['clips'][0]
        assert clip['category'] == 'snapshot' and clip['turnId'] is None
        assert clip['label'] == 'Recovered history' and clip['approximate'] and not clip['running']
        assert actual['details'][0]['items'] == [dict(item, outputs={}) for item in actual['items']]
    projected = json.loads((directory / 'native-projection.json').read_text())
    expected = next(case for case in rust['cases'] if case['id'] == 'late-snapshot')
    assert projected['items'] == expected['items'] and projected['details'][0]['items'] == expected['details'][0]['items']
    for key in ['id', 'category', 'label', 'turnId', 'approximate', 'running']:
        assert projected['timeline']['clips'][0][key] == expected['timeline']['clips'][0][key]
    assert (directory / 'snapshot-history.png').read_bytes().startswith(b'\x89PNG\r\n\x1a\n')
    desktop = (directory / 'desktop-targeted.log').read_text()
    assert '2 passed' in desktop and 'snapshot-only history' in desktop and 'live steps open' in desktop
    related = (directory / 'related-tests.log').read_text()
    assert sorted(int(n) for n in re.findall(r'test result: ok\. (\d+) passed', related)) == [5, 10, 22, 24]
    names = ['snapshot_only_items_are_visible_without_inventing_a_turn_or_duration',
             'snapshot_updates_keep_existing_clip_addresses_and_accept_the_next_revision',
             'projecting_a_snapshot_user_message_does_not_mark_input_delivered_or_repeat_effects',
             'invalid_snapshot_projection_rolls_back_both_the_seed_and_its_clips']
    log = (directory / 'rust-tests.log').read_text()
    assert all(name + ' ... ok' in related and name + ' ... ok' in log for name in names)
    totals = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', log)
    assert 'FAILED' not in log and 'Doc-tests intercat_git' in log
    assert sum(int(n) for n, _ in totals) == 442 and sum(int(n) for _, n in totals) == 25
    assert 'Finished' in (directory / 'clippy.log').read_text()
    assert not (directory / 'format.log').read_text().strip()
    assert 'tsc --noEmit' in (directory / 'typecheck.log').read_text()
    assert 'built in' in (directory / 'web-build.log').read_text()
    for name, digest in json.loads((directory / 'regression-inputs.json').read_text()).items():
        assert sha(root / name) == digest, name
    files = ['crates/intercat-daemon/src/timeline.rs', 'crates/intercat-daemon/src/snapshot_store.rs',
             'crates/intercat-daemon/src/journal.rs', 'crates/intercat-daemon/tests/snapshots.rs',
             'crates/intercat-daemon/examples/msp_snapshot.rs', 'src/bridge.d.ts',
             'src/timeline/StepInspector.tsx', 'tests/snapshot-history.spec.ts', 'tests/connection.spec.ts',
             'tools/verify_msp_snapshot_timeline.py']
    artifacts = ['fixture.json', 'ts.json', 'rust.json', 'native-projection.json', 'snapshot-history.png',
                 'related-tests.log', 'rust-tests.log', 'clippy.log', 'format.log', 'typecheck.log',
                 'web-build.log', 'desktop-targeted.log', 'regression-inputs.json', 'run-settings.json']
    return {'requirement': 'M1.14', 'requirementComplete': False, 'schema': 52,
            'scope': 'snapshot-derived items through timeline window/detail and Electron rendering',
            'snapshotCases': 4, 'typedFrames': 13, 'snapshotTests': names,
            'rustPassed': 442, 'rustIgnored': 25, 'desktopTargetedPassed': 2,
            'nativeKeychainSchemaAssertionUpdatedNotRun': True,
            'previousStateReplayEvidence': {'path': str(previous.relative_to(root) / 'verification.json'),
                'sha256': sha(previous / 'verification.json'), 'cases': base['stateReplayCases'],
                'frames': base['stateReplayFrames'], 'differentCases': base['stateReplayDifferences'],
                'unchangedStateAndRecoverySources': {name: sha(root / name) for name in stable}},
            'remaining': ['snapshot cursors absent from recovered history', 'transient delta display',
                          'host opening and handshake', 'actual worker disconnect/reconnect and resend decisions'],
            'paidTurnsThisTurn': 0, 'cumulativePaidTurns': 154,
            'sourceSha256': {name: sha(root / name) for name in files},
            'artifactSha256': {name: sha(directory / name) for name in artifacts}}


if __name__ == '__main__':
    target = Path(sys.argv[1])
    result = verify(target)
    (target / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in ['schema', 'snapshotCases', 'rustPassed',
                                                'desktopTargetedPassed', 'requirementComplete']}))
