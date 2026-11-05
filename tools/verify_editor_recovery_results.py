"""Reconcile model repair, interrupted transfers, UI controls and regressions."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys


def verify(directory):
    repository = Path(__file__).resolve().parents[1]
    native = directory / 'native-final'
    result = json.loads((native / 'result.json').read_text())
    states = json.loads((native / 'states.json').read_text())
    assert result['passed'] and result['originalUnchanged'] and result['paidTurns'] == 0
    assert result['retries'] == 3
    assert any(state['canRetry'] and state['restart'] == 3 and state['phase'] == 'unavailable' for state in states)
    assert {state['generation'] for state in states if state['phase'] == 'downloading'} == {2}
    assert len({state['pid'] for state in states if state['phase'] == 'ready'}) == 3
    assert result['repaired']['modelLoaded'] and result['restart']['modelLoaded']
    with sqlite3.connect(f'file:{native / "evidence.sqlite"}?mode=ro', uri=True) as database:
        assert database.execute('PRAGMA integrity_check').fetchone()[0] == 'ok'
        assert database.execute('PRAGMA user_version').fetchone()[0] == 50
        assert database.execute("SELECT count(*) FROM operations WHERE kind='turn.start'").fetchone()[0] == 0
        assert database.execute("SELECT count(*) FROM operations WHERE kind='editor.configure' AND state='complete'").fetchone()[0] == 2
        row = database.execute('SELECT state,revision FROM utterances WHERE id=?', (result['heldInput'],)).fetchone()
        assert row == ('reading', 2)
    manifest = json.loads((repository / 'services/editor/src/intercat_editor/model_manifest.json').read_text())
    assert manifest['repository'] == 'mlx-community/Muse-Glimmer-30B-4bit'
    assert manifest['revision'] == '3e7677d7a40d348a3daba263a2b1c0aa41910710'
    assert len(manifest['files']) == 13 and sum(item['size'] for item in manifest['files']) == 19443248945
    python = (directory / 'python.log').read_text()
    assert 'Ran 12 tests' in python and '\nOK' in python
    rust = (directory / 'rust.log').read_text()
    counts = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust)
    assert counts and 'FAILED' not in rust and 'Doc-tests intercat_git' in rust
    for name in ('three_failed_restarts_stop_and_explicit_retry_loads_the_repaired_model',
                 'download_pause_survives_restart_and_completed_download_starts_the_editor'):
        assert f'{name} ... ok' in rust
    assert '7 passed; 0 failed' in (directory / 'rust-editor-final.log').read_text()
    ui = (directory / 'ui.log').read_text()
    assert '75 passed' in ui and '18 skipped' in ui and not re.search(r'\d+ failed', ui)
    assert '4 passed' in (directory / 'ui-targeted.log').read_text()
    assert 'ℹ pass 30' in (directory / 'node.log').read_text() and 'ℹ fail 0' in (directory / 'node.log').read_text()
    assert 'Finished' in (directory / 'clippy.log').read_text()
    assert 'error:' not in (directory / 'clippy.log').read_text()
    assert not (directory / 'format.log').read_text().strip()
    assert 'built in' in (directory / 'web-build.log').read_text()
    assert 'error TS' not in (directory / 'typecheck.log').read_text()
    assert (directory / 'details-200.png').read_bytes().startswith(b'\x89PNG\r\n\x1a\n')
    files = [
        'crates/intercat-daemon/src/editor.rs', 'crates/intercat-daemon/src/rpc.rs',
        'crates/intercat-daemon/tests/editor.rs', 'services/editor/pyproject.toml',
        'services/editor/src/intercat_editor/model_store.py',
        'services/editor/src/intercat_editor/model_manifest.json',
        'services/editor/src/intercat_editor/engine.py', 'services/editor/tests/test_model_store.py',
        'electron/main.mjs', 'electron/preload.cjs', 'src/main.tsx', 'src/bridge.d.ts',
        'src/desk/EditorStatus.tsx', 'src/desk/editor-status.css',
        'tests/editor-recovery.spec.ts', 'tests/support/editor.ts',
        'tools/verify_editor_repair.py', 'tools/verify_editor_recovery_results.py',
    ]
    return {'milestoneRequirement': 'M1.41', 'schema': 50,
            'rustPassed': sum(int(a) for a, _ in counts), 'rustIgnored': sum(int(b) for _, b in counts),
            'uiPassed': 75, 'uiSkipped': 18, 'pythonPassed': 12, 'nodePassed': 30,
            'realModelRepairedAndReloadedAfterDaemonRestart': True,
            'heldInputReturnedToClassification': True,
            'realWorkerDispatchAfterRecoveryVerified': False,
            'additionalPaidTurns': 0, 'cumulativePaidTurns': 151,
            'sourceSha256': {file: hashlib.sha256((repository / file).read_bytes()).hexdigest() for file in files}}


if __name__ == '__main__':
    directory = Path(sys.argv[1]).resolve()
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({key: value for key, value in result.items() if key != 'sourceSha256'}, ensure_ascii=False))
