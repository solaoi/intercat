"""Check M1.53 evidence using the recorded database and Git bundle."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import tempfile


def verify(directory):
    rust = (directory / 'rust.log').read_text()
    counts = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust)
    assert counts and 'Doc-tests intercat_git' in rust and 'FAILED' not in rust
    for name in (
        'empty_unborn_project_has_a_stable_parentless_base_without_moving_head_or_index',
        'interrupted_project_selection_recovers_before_preparing_an_empty_worker',
    ):
        assert f'{name} ... ok' in rust, name
    for name in ('clippy.log', 'build.log'):
        log = (directory / name).read_text()
        assert 'Finished' in log and 'error:' not in log
    assert not (directory / 'format.log').read_text().strip()
    assert 'tsc --noEmit' in (directory / 'typecheck.log').read_text()
    assert 'built in' in (directory / 'web-build.log').read_text()
    ui = (directory / 'ui.log').read_text()
    assert '66 passed' in ui and not re.search(r'^\s+\d+ failed', ui, re.M)
    assert '1 passed' in (directory / 'ui-unborn.log').read_text()
    node = (directory / 'node.log').read_text()
    assert re.search(r'(?:#|ℹ) fail 0\b', node)
    assert re.search(r'(?:#|ℹ) pass 27\b', node)

    evidence = json.loads((directory / 'integration.json').read_text())
    assert evidence['paidTurns'] == 0
    assert evidence['mainEmpty'] and evidence['sourceUnchanged'] and evidence['indexAbsent']
    assert evidence['openPRDisabled']
    assert evidence['head'] == 'ref: refs/heads/main\n'
    reviewed, completed = evidence['reviewed'], evidence['completed']
    assert reviewed['project']['baseBranch'] is None
    assert reviewed['status'] == 'checked' and completed['status'] == 'done'
    candidate = completed['candidate']
    assert candidate['check']['code'] == 0 and len(candidate['commits']) == 2
    assert candidate['head'] == reviewed['candidate']['head']
    base = reviewed['project']['base']
    with tempfile.TemporaryDirectory(prefix='intercat-unborn-evidence-') as temporary:
        repository = Path(temporary) / 'source.git'
        def git(*args):
            return subprocess.check_output(
                ['/usr/bin/git', '-c', 'core.hooksPath=/dev/null', *args],
                text=True, stderr=subprocess.PIPE,
                env={'PATH': '/usr/bin:/bin', 'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_NOSYSTEM': '1'},
            ).strip()
        git('clone', '--mirror', '--quiet', str(directory.resolve() / 'source.bundle'), str(repository))
        def read(*args):
            return git('--git-dir', str(repository), *args)
        assert read('show', '-s', '--format=%P', base) == ''
        assert read('ls-tree', base) == ''
        assert read('rev-parse', f'refs/intercat/starting-points/{base}') == base
        assert read('for-each-ref', '--format=%(refname)', 'refs/heads/main') == ''
        assert read('rev-parse', evidence['branch']) == candidate['head']
        assert read('rev-list', '--count', evidence['branch']) == '3'
        parent = base
        for commit in candidate['commits']:
            assert read('show', '-s', '--format=%P', commit['sha']) == parent
            parent = commit['sha']
        assert read('show', f"{evidence['branch']}:index.js") == 'exports.value = 2;'
        assert 'now 2' in read('show', f"{evidence['branch']}:README.md")

    with sqlite3.connect(f'file:{directory / "events.sqlite"}?mode=ro', uri=True) as database:
        assert database.execute('PRAGMA user_version').fetchone()[0] == 50
        operation = database.execute("SELECT state FROM operations WHERE kind='project.configure'").fetchall()
        assert operation == [('complete',)]
        intent = database.execute("SELECT seq FROM events WHERE kind='intent.project.configure'").fetchone()[0]
        confirmed = database.execute("SELECT seq FROM events WHERE kind='project.configured'").fetchone()[0]
        assert intent < confirmed
        assert database.execute("SELECT count(*) FROM operations WHERE kind='turn.start'").fetchone()[0] == 0
        assert database.execute("SELECT count(*) FROM operations WHERE kind='checkpoint.save' AND state='complete'").fetchone()[0] == 2
        saved = json.loads(database.execute('SELECT record FROM integration_jobs').fetchone()[0])
        assert saved['status'] == 'done' and saved['candidate']['head'] == candidate['head']

    root = Path(__file__).resolve().parents[1]
    sources = [
        'crates/intercat-git/src/lib.rs',
        'crates/intercat-git/tests/administration.rs',
        'crates/intercat-daemon/src/project.rs',
        'crates/intercat-daemon/src/journal.rs',
        'crates/intercat-daemon/src/preparation.rs',
        'crates/intercat-daemon/src/checkout.rs',
        'crates/intercat-daemon/src/rpc.rs',
        'crates/intercat-daemon/tests/project.rs',
        'crates/intercat-daemon/examples/commit_fixture.rs',
        'src/bridge.d.ts', 'src/commit/CommitSheet.tsx', 'tests/commit.spec.ts',
        'tools/verify_unborn_project.py',
    ]
    return {
        'schema': 50, 'milestoneRequirement': 'M1.53',
        'rustPassed': sum(int(a) for a, _ in counts),
        'rustIgnored': sum(int(b) for _, b in counts),
        'desktopPassed': 66, 'nodePassed': 27,
        'emptyParentlessStartingPoint': base,
        'firstCommitParentIsStartingPoint': True,
        'mainRemainsUnborn': True, 'openPRDisabledAt200Percent': True,
        'projectIntentSurvivesRestartAndPruning': True,
        'additionalPaidTurns': 0, 'cumulativePaidTurns': 148,
        'sourceSha256': {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: value for key, value in result.items() if key != 'sourceSha256'}, ensure_ascii=False))
