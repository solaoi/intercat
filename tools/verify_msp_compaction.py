"""Verify bounded compaction recovery without claiming snapshot ingestion."""
import json
from pathlib import Path
import re
import sys

from verify_msp_gaps import calls, sha
from verify_msp_replay import tree


def verify(directory):
    root = Path(__file__).resolve().parents[1]
    previous_dir = directory.parent / 'msp-gap-restart-20261105'
    previous = json.loads((previous_dir / 'verification.json').read_text())
    baseline = json.loads((previous_dir / 'regression-inputs.json').read_text())
    inputs = json.loads((directory / 'regression-inputs.json').read_text())
    changed = {name for name in inputs if inputs[name] != baseline[name]}
    assert changed == {'crates/intercat-daemon/src/history_store.rs',
                       'crates/intercat-daemon/tests/history.rs'}, changed
    for name, digest in inputs.items():
        assert sha(root / name) == digest, name
    names = ['a_known_compaction_boundary_still_rewinds_and_records_the_summary',
             'compaction_requires_a_page_receipt_for_the_exact_boundary',
             'interrupted_compaction_fill_reopens_without_losing_the_gap_or_duplicating_items']
    before = (directory / 'before.log').read_text()
    assert all(name + ' ... FAILED' in before for name in names[:2])
    assert 'Some("pruned")' in before and 'Some("boundary")' in before
    assert 'An unrelated receipt cannot authorize a history rewind' in before
    related = (directory / 'related-tests.log').read_text()
    assert sorted(int(n) for n in re.findall(r'test result: ok\. (\d+) passed', related)) == [21, 24]
    log = (directory / 'rust-tests.log').read_text()
    assert 'FAILED' not in log and 'Doc-tests intercat_git' in log
    for name in names:
        assert name + ' ... ok' in related and name + ' ... ok' in log
    totals = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', log)
    assert sum(int(n) for n, _ in totals) == 431
    assert sum(int(n) for _, n in totals) == 25
    assert 'Finished' in (directory / 'clippy.log').read_text()
    assert not (directory / 'format.log').read_text().strip()
    assert 'Secret scan did not complete' in (directory / 'sandbox-history-tests.log').read_text()
    assert 'page_redaction_checks_strings_beyond_the_first_scanner_batch ... ok' in log
    assert 'Finished' in (directory / 'build.log').read_text()
    for name in ['fixture.json', 'ts.json']:
        assert sha(directory / name) == sha(previous_dir / name)
    rust = json.loads((directory / 'rust.json').read_text())
    expected = json.loads((directory / 'ts.json').read_text())
    assert rust['schemaVersion'] == 51 and rust['validatedFrames'] == 51
    assert len(rust['cases']) == len(expected['cases']) == 8
    for actual, wanted in zip(rust['cases'], expected['cases']):
        assert actual['id'] == wanted['id']
        assert actual['final'] == wanted['final'] and actual['current'] == wanted['current']
        assert actual['recoverySucceeded'] == actual['current'] == (not wanted['errors'])
        assert actual['restartBeforeFill'] and actual['restartVerified']
        if wanted['pendingGap'] is None:
            assert actual['pendingGap'] is None
        else:
            assert all(actual['pendingGap'][key] == wanted['pendingGap'][key]
                       for key in ['after', 'next'])
            assert actual['pendingGap']['through'] > 0
        assert calls(actual, True) == calls(wanted, False)
    settings = json.loads((directory / 'run-settings.json').read_text())
    assert settings['rustTests'] == ['cargo', 'test', '--workspace', '--', '--test-threads=1']
    assert settings['schema'] == 51 and settings['paidTurns'] == 0
    sdk = root / 'research/verify/support/node_modules/@muse-code/sdk'
    assert tree(sdk, (sdk / 'dist').rglob('*.js')) == previous['oracleTreeSha256']
    gap_source = sdk / 'dist/src/facade/gap-fill.js'
    assert 'snapshot) is NOT built here' in gap_source.read_text()
    frontend = tree(root, [p for d in ['src', 'electron']
                           for p in (root / d).rglob('*') if p.is_file()])
    assert frontend == previous['frontendTreeSha256']
    artifacts = ['before.log', 'related-tests.log', 'rust-tests.log', 'clippy.log',
                 'format.log', 'build.log', 'sandbox-history-tests.log',
                 'test-build-error.log', 'regression-inputs.json', 'run-settings.json',
                 'fixture.json', 'ts.json', 'rust.json', 'rust.log']
    return {
        'requirement': 'M1.14', 'requirementComplete': False, 'schema': 51,
        'scope': 'compaction boundary receipt, idempotent rewind, interrupted fill with Journal reopen',
        'regressions': names,
        'compactionOracle': 'local contract tests; the pinned TS GapFiller has no compaction reanchor',
        'gapCasesReplayed': 8, 'gapFramesValidated': 51,
        'snapshotIngestionImplemented': False,
        'remaining': ['full snapshot recovery', 'transient delta display',
                      'host opening and handshake', 'worker disconnect/reconnect and resend decisions'],
        'rustPassed': 431, 'rustIgnored': 25, 'testThreads': 1,
        'desktopPassedFromUnchangedFrontend': 76, 'frontendTreeSha256': frontend,
        'oracleTreeSha256': previous['oracleTreeSha256'],
        'oracleGapSourceSha256': sha(gap_source),
        'paidTurnsThisTurn': 0, 'cumulativePaidTurns': 154,
        'sourceSha256': {name: sha(root / name) for name in
                         sorted(changed | {'tools/verify_msp_compaction.py'})},
        'artifactSha256': {name: sha(directory / name) for name in artifacts},
    }


if __name__ == '__main__':
    target = Path(sys.argv[1])
    result = verify(target)
    (target / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: result[key] for key in [
        'gapCasesReplayed', 'rustPassed', 'rustIgnored', 'requirementComplete']}))
