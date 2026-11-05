"""Verify the unobserved snapshot cut and bound the native/SDK evidence."""
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
    native = json.loads((directory / 'native-7/result.json').read_text())
    assert native['modelTurns'] == 1
    assert sum(c['method'] == 'turn/start' for c in native['calls']) == 1
    assert native['before'] and native['localItems']
    assert native['whileUnsubscribed'] == []
    assert len(native['suffix']) == 1
    assert native['suffix'][0]['method'] == 'session/nameChanged'
    assert native['localCursor'] == native['subscribeHead'] == native['afterHead']
    cold = native['cold']
    assert cold['journalReopenVerified'] and cold['activeTurn'] is None
    assert cold['resumeTurn'] == native['submitted'][0]
    assert cold['command'] == native['submitted'][1]
    assert len(cold['replay']) == 2
    assert cold['replay'][0][0]['method'] == 'item/completed'
    assert 'viewCursor' not in cold['replay'][0][0]['params']
    assert cold['fullState'] == cold['resume']['history']['snapshot']['state']
    assert cold['items'] == native['localItems']
    assert 'completed (model turns: 1)' in (directory / 'native-7.log').read_text()
    prior = json.loads((directory / 'native-6/result.json').read_text())
    assert prior['cold']['fullState']['turnCount'] == 2
    assert prior['cold']['resume']['history']['snapshot']['state']['turnCount'] == 1
    attempted = sum(json.loads(p.read_text())['attempted']
                    for p in directory.glob('native-*/model-turn.json'))
    assert attempted == 2

    fixture = json.loads((directory / 'fixture.json').read_text())
    actual = json.loads((directory / 'rust.json').read_text())
    expected = json.loads((directory / 'ts.json').read_text())
    assert actual['schemaVersion'] == 53 and actual['validatedFrames'] == 14
    assert len(actual['cases']) == len(expected['cases']) == len(fixture['cases']) == 5
    for observed, oracle, case in zip(actual['cases'], expected['cases'], fixture['cases']):
        assert observed['id'] == oracle['id'] == case['id']
        assert observed['items'] == oracle['items']
        assert observed['sessionState'] == oracle['sessionState']
        assert observed['restartVerified'] and observed['rawOnlyRestartVerified']
    opening = fixture['cases'][-1]
    assert opening['opening'] and not opening['before'] and not opening['after']
    resume = next(c['result'] for c in native['calls'] if c['method'] == 'session/resume')
    assert opening['read'] == resume and opening['suffix'] == native['suffix']
    old = directory.parent / 'msp-snapshot-20261105'
    for category in ['state-replay', 'gaps']:
        for name in ['fixture.json', 'ts.json']:
            assert sha(directory / category / name) == sha(old / category / name)
    replay = compare(directory / 'state-replay')
    assert replay['cases'] == 531 and replay['comparedFrames'] == 19219
    assert replay['differentCases'] == 0
    assert all(c['restartVerified'] for c in json.loads(
        (directory / 'state-replay/rust.json').read_text())['cases'])
    gaps = json.loads((directory / 'gaps/rust.json').read_text())
    gaps_ts = json.loads((directory / 'gaps/ts.json').read_text())
    assert len(gaps['cases']) == len(gaps_ts['cases']) == 8
    for observed, oracle in zip(gaps['cases'], gaps_ts['cases']):
        assert observed['id'] == oracle['id'] and observed['final'] == oracle['final']
        assert observed['current'] == oracle['current']
        assert observed['restartVerified'] and observed['restartBeforeFill']
        assert calls(observed, True) == calls(oracle, False)
    log = (directory / 'rust-tests.log').read_text()
    totals = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', log)
    assert 'FAILED' not in log and 'Doc-tests intercat_git' in log
    assert sum(int(n) for n, _ in totals) == 448
    assert sum(int(n) for _, n in totals) == 25
    assert 'last_turn_restatement_does_not_count_a_snapshot_turn_twice ... ok' in log
    assert 'Finished' in (directory / 'clippy.log').read_text()
    assert not re.search(r'(warning|error):', (directory / 'clippy.log').read_text())
    assert not (directory / 'format.log').read_text().strip()
    assert 'tsc --noEmit' in (directory / 'typecheck.log').read_text()
    sdk = root / 'research/verify/support/node_modules/@muse-code/sdk'
    base = json.loads((old / 'verification.json').read_text())
    assert tree(sdk, (sdk / 'dist').rglob('*.js')) == base['oracleTreeSha256']
    frontend = tree(root, [p for d in ['src', 'electron']
                           for p in (root / d).rglob('*') if p.is_file()])
    ui_evidence = directory.parent / 'msp-snapshot-timeline-20261105/verification.json'
    ui = json.loads(ui_evidence.read_text())
    presentation = ['src/bridge.d.ts', 'src/timeline/StepInspector.tsx']
    for name in presentation:
        assert sha(root / name) == ui['sourceSha256'][name], name
    sources = ['crates/intercat-daemon/src/' + name + '.rs' for name in [
        'history', 'history_store', 'journal', 'session_snapshot', 'session_view',
        'snapshot_store', 'reconcile', 'reconcile_store', 'runtime']]
    sources += ['crates/intercat-daemon/tests/snapshots.rs',
                'crates/intercat-daemon/examples/msp_snapshot.rs',
                'crates/intercat-daemon/examples/msp_snapshot_probe.rs',
                'tools/msp_snapshot_oracle.mjs', 'tools/verify_msp_opening.py',
                'tests/connection.spec.ts']
    artifacts = ['fixture.json', 'rust.json', 'ts.json', 'rust-tests.log', 'rust-initial-tests.log',
                 'clippy.log', 'format.log', 'typecheck.log', 'run-settings.json',
                 'native-6/result.json', 'native-6.log', 'native-6/model-turn.json',
                 'native-7/result.json', 'native-7.log', 'native-7/model-turn.json',
                 'state-replay/fixture.json', 'state-replay/ts.json', 'state-replay/rust.json',
                 'gaps/fixture.json', 'gaps/ts.json', 'gaps/rust.json']
    return {
        'requirement': 'M1.14', 'requirementComplete': False, 'schema': 53,
        'scope': 'full resume snapshot at an unobserved cursor, subscription suffix, journal reopen and graceful native host restart',
        'snapshotCases': 5, 'validatedFrames': 14,
        'oracleScope': 'official ItemStore.seed and SessionStateStore.seed, not complete SDK Session orchestration',
        'nativeColdResume': True, 'nativeSnapshotItems': len(cold['items']),
        'nativeNoSecondSubmit': True, 'nativeFullStateEquality': True,
        'regressionFound': 'lastTurn restatement counted an already completed snapshot turn twice',
        'stateReplayCases': 531, 'stateReplayFrames': 19219, 'stateReplayDifferences': 0,
        'gapCases': 8, 'rustPassed': 448, 'rustIgnored': 25,
        'frontendTreeSha256': frontend, 'oracleTreeSha256': base['oracleTreeSha256'],
        'previousSnapshotUIEvidence': {
            'path': str(ui_evidence.relative_to(root)), 'sha256': sha(ui_evidence),
            'unchangedPresentationSources': {name: sha(root / name) for name in presentation},
            'desktopTestsRerunThisTurn': False,
        },
        'nativeKeychainSchemaAssertionUpdatedNotRun': True,
        'paidTurnsThisTurn': attempted, 'cumulativePaidTurns': 154 + attempted,
        'remaining': ['retention-pruned and budget-downgraded native snapshot recovery',
                      'transient delta display', 'complete host initialization SDK comparison',
                      'actual worker disconnect/reconnect and resend decisions under failure',
                      '300-row keyboard and actual VoiceOver acceptance', 'all 60 acceptance gates'],
        'sourceSha256': {name: sha(root / name) for name in sources},
        'artifactSha256': {name: sha(directory / name) for name in artifacts},
    }


if __name__ == '__main__':
    target = Path(sys.argv[1])
    result = verify(target)
    (target / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in [
        'snapshotCases', 'stateReplayCases', 'rustPassed', 'cumulativePaidTurns', 'requirementComplete']}))
