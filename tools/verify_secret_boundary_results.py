"""Reconcile the M1.20 native matrix, real worker, and regression evidence."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import subprocess
import sys

EXAMPLES = {'.env.example', '.env.sample', '.env.template', '.env.dist', 'plain.txt'}


def check_access(rows, new_directory):
    for row in rows:
        allowed = Path(row['path']).name in EXAMPLES
        for operation in ['read', 'write', 'readAfterWrite']:
            expected = allowed and (operation != 'read' or not row['path'].startswith(new_directory + '/'))
            assert row[operation] == expected, (row['path'], operation)


def verify(directory):
    repository = Path(__file__).resolve().parents[1]
    native = directory.parent / 'native-production-secrets-20261104'
    result = json.loads((directory / 'result.json').read_text())
    assert result['paidTurns'] == result['attemptedPaidTurns'] == result['savedTurnStarts'] == 1
    assert result['cumulativePaidTurns'] == 154
    assert result['sourceFilesUnchanged'] and result['workerIgnoredSecretsInitiallyAbsent']
    assert not result.get('forcedDaemonStop') and not result.get('failure')
    integration = result['integration']
    for key in ['examplesReadable', 'ignoredSecretsAbsent', 'sourceSecretsReadableOutsideSandbox', 'sourceCatDeniedInCheckSandbox']:
        assert integration[key]
    assert integration['exitCode'] == 0 and integration['stdout'] == 'INTEGRATION_SECRET_BOUNDARY_OK\n'
    for path in ['.env', 'packages/app/.env']:
        assert f"test ! -e '{path}'" in integration['script']
        assert f"/bin/cat '{path}.example'" in integration['script']
    assert integration['script'].count('then exit 30') == 2
    check_access(result['shellAccess'], 'created')
    assert len(result['shellAccess']) == 57
    matrix = json.loads((native / 'results.json').read_text())
    assert len(matrix) == 1 and matrix[0]['case'] == 'production' and matrix[0]['paidTurns'] == 0
    assert matrix[0]['shell']['exitCode'] == 0
    check_access(matrix[0]['access'], 'new')
    assert len(matrix[0]['access']) == 1395
    rules = json.loads(subprocess.check_output(['cargo', 'run', '--offline', '-q', '-p', 'intercat-daemon', '--example', 'secret_policy'], cwd=repository))
    settings = json.loads((native / 'production/config/muse/settings.json').read_text())
    assert rules == settings['permissions']['profiles']['probe']['filesystem'][':workspace_roots']

    with sqlite3.connect(f'file:{directory / "events.sqlite"}?mode=ro', uri=True) as database:
        assert database.execute('PRAGMA integrity_check').fetchone()[0] == 'ok'
        assert database.execute('PRAGMA user_version').fetchone()[0] == 50
        for kind in ['turn.start', 'checkpoint.save']:
            assert database.execute("SELECT count(*) FROM operations WHERE kind=? AND state='complete'", (kind,)).fetchone()[0] == 1
        frames = [json.loads(payload) for payload, in database.execute("SELECT payload FROM events WHERE kind='msp.received'")]
        tools = {}
        for frame in frames:
            item = frame.get('params', {}).get('item', {})
            if item.get('kind') == 'toolCall':
                tools[item['itemId']] = item
        assert len(tools) == 5
        reads = {json.loads(item['args'])['path']: item for item in tools.values() if item['tool'] == 'read_file'}
        assert {path: item['status'] for path, item in reads.items()} == {
            '.env': 'failed', 'packages/app/.env': 'failed',
            '.env.example': 'completed', 'packages/app/.env.example': 'completed',
        }
        for path in ['.env.example', 'packages/app/.env.example']:
            assert 'DUMMY_ONLY' in reads[path]['visibleOutput']
        shells = [item for item in tools.values() if item['tool'] == 'bash']
        assert len(shells) == 1 and shells[0]['status'] == 'completed'
        arguments = json.loads(shells[0]['args'])
        assert arguments['command'] == '/usr/bin/python3 probe.py'
        assert arguments.get('sandbox_permissions') != 'require_escalated'
        shell = json.loads(shells[0]['visibleOutput'])
        assert shell['exit_code'] == 0 and shell['terminal_status'] == 'completed'
        assert shell['output'] == 'SECRET_BOUNDARY_PROBE_COMPLETE\n'
        sessions = [json.loads(payload) for payload, in database.execute("SELECT payload FROM events WHERE kind='worker.session'")]
        assert len(sessions) == 1
        permission = sessions[0]['result']['session']['permissionState']
        assert permission['profileId'] == 'intercat-worker'
        assert permission['axes']['filesystem'] == 'sandboxed'
        assert permission['axes']['network'] == 'off'
    rust = (directory / 'rust.log').read_text()
    counts = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust)
    assert counts and 'Doc-tests intercat_git' in rust and 'FAILED' not in rust
    assert 'deny_globs_match_secret_classification_for_prefixes_suffixes_and_unicode ... ok' in rust
    assert 'writer_transaction_prevents_lock_takeover_between_read_and_write ... ok' in rust
    race = directory.parent / 'secret-boundary-20261104-run3'
    previous = json.loads((race / 'result.json').read_text())
    assert previous['paidTurns'] == 1 and previous['cumulativePaidTurns'] == 153
    assert previous['cumulativePaidTurns'] + result['paidTurns'] == result['cumulativePaidTurns']
    assert 'FAILED' in (race / 'writer-before.log').read_text()
    assert '1 passed; 0 failed' in (race / 'writer-after.log').read_text()
    repeated = json.loads((race / 'questions-fixed.json').read_text())
    assert len(repeated) == 20 and all(item['exitCode'] == 0 for item in repeated)
    assert not (directory / 'format.log').read_text().strip()
    clippy = (directory / 'clippy.log').read_text()
    assert 'Finished' in clippy and 'error:' not in clippy
    paths = ['Cargo.lock', 'crates/intercat-daemon/Cargo.toml', 'crates/intercat-daemon/src/lib.rs',
             'crates/intercat-daemon/src/secret_paths.rs', 'crates/intercat-daemon/src/host.rs',
             'crates/intercat-daemon/src/journal.rs',
             'crates/intercat-git/src/lib.rs', 'crates/intercat-daemon/src/checkout.rs',
             'crates/intercat-daemon/examples/secret_boundary_fixture.rs',
             'crates/intercat-daemon/examples/secret_policy.rs',
             'tools/verify_live_secret_boundary.py', 'tools/verify_native_secret_patterns.py',
             'tools/verify_secret_boundary_results.py']
    return {'milestoneRequirement': 'M1.20', 'milestoneComplete': False,
            'estimatedM1ProgressPercent': 98.5, 'previousM1ProgressPercent': 98,
            'profileId': 'intercat-worker',
            'nativeMatrixPaths': 1395, 'realWorkerMatrixPaths': 57,
            'realReadFileDenials': 2, 'realReadFileExampleSuccesses': 2,
            'integrationSecretsAbsent': True, 'sourceCatDeniedInCheckSandbox': True,
            'sourceFilesUnchanged': True, 'paidTurnsThisTurn': 2, 'finalRunPaidTurns': 1, 'cumulativePaidTurns': 154,
            'authorizedPaidTurns': 1000,
            'journalWriterReservationRegression': True, 'repeatedQuestionSuitesPassed': 20,
            'rustPassed': sum(int(passed) for passed, _ in counts),
            'rustIgnored': sum(int(ignored) for _, ignored in counts),
            'sourceSha256': {path: hashlib.sha256((repository / path).read_bytes()).hexdigest() for path in paths}}


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    verification = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(verification, indent=2) + '\n')
    print(json.dumps({key: value for key, value in verification.items() if key != 'sourceSha256'}))
