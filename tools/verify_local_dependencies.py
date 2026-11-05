"""Verify local npm dependency evidence without calling external services."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys


def verify(directory):
    rust = (directory / 'rust.log').read_text()
    counts = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust)
    assert counts and 'Doc-tests intercat_git' in rust and 'FAILED' not in rust
    for name in (
        'local_directory_dependencies_use_saved_sources_and_invalidate_on_source_only_edits',
        'local_links_require_matching_locked_manifests_and_repository_targets',
        'local_dependency_checkpoint_passes_check_and_commit_but_stale_locks_hold_new_work',
    ):
        assert f'{name} ... ok' in rust, name
    for name in ('clippy.log', 'build.log'):
        log = (directory / name).read_text()
        assert 'Finished' in log and 'error:' not in log
    assert not (directory / 'format.log').read_text().strip()
    ui = (directory / 'ui.log').read_text()
    assert '2 passed' in ui and not re.search(r'^\s+\d+ failed', ui, re.M)
    for name in ('desktop commits real saved work', 'desktop Keep resumes both exact commits'):
        assert name in ui
    assert '11 passed; 0 failed; 1 ignored' in (directory / 'dependencies-final.log').read_text()
    assert '1 passed; 0 failed' in (directory / 'checkout-isolated.log').read_text()
    fixture = json.loads((directory / 'integration-verified/result.json').read_text())
    assert fixture['sourceUnchanged'] and fixture['modelTurns'] == 0
    checked, done = fixture['built'], fixture['done']
    assert checked['status'] == 'checked' and done['status'] == 'done'
    candidate = checked['candidate']
    assert candidate['check']['code'] == 0 and len(candidate['commits']) == 1
    assert candidate['head'] == fixture['publishedHead']
    assert len(candidate['dependencyRuns']) == 2
    assert candidate['dependencyRuns'][1]['forced']
    assert all(run['downloads'] == 0 for run in candidate['dependencyRuns'])
    for name in ('manifestFailure', 'lockFailure'):
        failed = fixture[name]['candidate']
        assert failed['state'] == 'env-failed' and failed['check'] is None
        assert failed['dependencyError']['kind'] == 'configuration'
    native = list((directory / 'native').glob('*/result.json'))
    assert len(native) == 1
    live = json.loads(native[0].read_text())
    assert live['sourceUnchanged'] and live['paidTurns'] == 1
    assert live['built']['status'] == 'checked' and live['done']['status'] == 'done'
    checked = live['built']['candidate']
    assert checked['check']['code'] == 0 and checked['check']['command'] is not None
    assert len(checked['dependencyRuns']) == 2 and checked['dependencyRuns'][1]['forced']
    assert all(run['downloads'] == 0 for run in checked['dependencyRuns'])
    paths = [change['path'] for change in checked['commits'][0]['changes']]
    assert 'package.json' in paths and 'package-lock.json' in paths
    assert 'login.cjs' in paths and all(not path.startswith('node_modules/') for path in paths)
    db_path = native[0].parent / 'events.sqlite'
    with sqlite3.connect(f'file:{db_path}?mode=ro', uri=True) as db:
        assert db.execute("SELECT count(*) FROM operations WHERE kind='turn.start'").fetchone()[0] == 1
        assert db.execute("SELECT count(*) FROM operations WHERE kind='checkpoint.save' AND state='complete'").fetchone()[0] == 1
        record = json.loads(db.execute('SELECT record FROM integration_jobs').fetchone()[0])
        assert record['status'] == 'done'
        assert record['candidate']['head'] == checked['head']
    attempts = sum(json.loads(path.read_text())['attemptedModelTurns'] for path in (directory / 'native').glob('*/attempt.json'))
    assert attempts == 1
    assert '1 passed; 0 failed' in (directory / 'native.log').read_text()
    root = Path(__file__).resolve().parents[1]
    sources = [
        'crates/intercat-daemon/src/dependencies.rs',
        'crates/intercat-daemon/src/runtime_tests.rs',
        'crates/intercat-daemon/tests/dependencies.rs',
        'crates/intercat-daemon/tests/integration.rs',
        'tools/verify_local_dependencies.py',
    ]
    return {
        'schema': 49, 'milestoneRequirement': 'M1.24',
        'rustPassed': sum(int(a) for a, _ in counts),
        'rustIgnored': sum(int(b) for _, b in counts),
        'desktopRegressionPassed': 2, 'nativeCommitPassed': True,
        'sourceOnlyChangesInvalidateInstall': True, 'outsideLinksRejected': True,
        'manifestAndLockMismatchStopBeforeCheck': True,
        'additionalPaidTurns': attempts, 'cumulativePaidTurns': 147 + attempts,
        'sourceSha256': {path: hashlib.sha256((root / path).read_bytes()).hexdigest() for path in sources},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k != 'sourceSha256'}, ensure_ascii=False))
