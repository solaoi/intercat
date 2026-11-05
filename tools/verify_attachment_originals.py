"""Verify private-original and delivery evidence without calling a model."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys


def verify(directory):
    rust = (directory / 'rust.log').read_text()
    rows = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust)
    assert rows and 'FAILED' not in rust and 'Doc-tests intercat_git' in rust
    for name in (
        'originals_survive_source_changes_restart_and_copy_replacement_without_entering_git',
        'corrupt_originals_or_metadata_never_produce_a_reference_copy',
        'unsafe_sources_limits_and_reused_operations_are_rejected',
        'combined_reference_limit_rejects_the_entire_input_transaction',
    ):
        assert f'{name} ... ok' in rust, name
    ui = (directory / 'ui.log').read_text()
    assert not re.search(r'^\s+\d+ failed\b', ui, re.M)
    assert 'reference file chips survive a lost save acknowledgement and use the same original IDs' in ui
    node = (directory / 'node.log').read_text()
    assert re.search(r'\bfail 0\b', node)
    for name in ('clippy.log', 'build.log'):
        log = (directory / name).read_text()
        assert 'Finished' in log and 'error:' not in log
    for name in ('typecheck.log', 'format.log'):
        assert 'error' not in (directory / name).read_text().lower()
    log = (directory / 'native.log').read_text()
    assert 'real_attachment_original_is_copied_and_read_without_entering_the_checkpoint ... ok' in log
    results = list((directory / 'native').glob('*/result.json'))
    assert len(results) == 1
    result = json.loads(results[0].read_text())
    assert result['paidTurns'] == 1 and result['schema'] == 45
    assert all(result[key] for key in ('readVerifiedOriginal', 'checkpointExcluded', 'sourceUnchanged'))
    expected = 'REFERENCE_ORIGINAL_OK_73ce'
    descriptor = result['attachment']
    assert descriptor['sha256'] == hashlib.sha256((expected + '\n').encode()).hexdigest()
    with sqlite3.connect(f'file:{results[0].parent / "events.sqlite"}?mode=ro', uri=True) as db:
        assert db.execute('PRAGMA user_version').fetchone()[0] == 45
        starts = db.execute("SELECT payload FROM operations WHERE kind='turn.start'").fetchall()
        assert len(starts) == 1
        prompt = json.loads(starts[0][0])['input'][0]['arguments']
        assert expected not in prompt and descriptor['id'] in prompt
        calls = {}
        for (payload,) in db.execute("SELECT payload FROM events WHERE kind='msp.received'"):
            value = json.loads(payload)
            item = value.get('frame', value).get('params', {}).get('item', {})
            if item.get('kind') == 'toolCall':
                calls[item['itemId']] = item
        assert len(calls) == 1
        call = next(iter(calls.values()))
        assert call['tool'] == 'read_file' and call['status'] == 'completed'
        path = json.loads(call['args'])['path']
        assert '/.intercat/attachments/' in path and descriptor['id'] in path
        assert expected in call['visibleOutput']
        imported = db.execute("SELECT payload FROM operations WHERE kind='attachment.import'").fetchone()[0]
        assert expected not in imported
        assert db.execute('SELECT attachment FROM input_attachments WHERE input=?', (result['inputId'],)).fetchone()[0] == descriptor['id']
    root = Path(__file__).resolve().parents[1]
    sources = [
        'crates/intercat-daemon/src/attachment_store.rs', 'crates/intercat-daemon/src/journal.rs',
        'crates/intercat-daemon/src/rpc.rs', 'crates/intercat-daemon/src/execution.rs',
        'crates/intercat-daemon/src/runtime_tests.rs', 'crates/intercat-daemon/tests/attachments.rs',
        'electron/attachments.mjs', 'electron/attachments.test.mjs', 'electron/main.mjs',
        'electron/preload.cjs', 'src/main.tsx', 'src/bridge.d.ts', 'src/desk/Requests.tsx', 'src/style.css',
        'tests/desktop.spec.ts', 'tools/verify_attachment_originals.py',
    ]
    return {
        'schema': 45,
        'rustPassed': sum(int(a) for a, _ in rows), 'rustIgnored': sum(int(b) for _, b in rows),
        'desktopPassed': int(re.search(r'(\d+) passed \(', ui)[1]),
        'desktopSkipped': int(re.search(r'(\d+) skipped', ui)[1]),
        'nodePassed': int(re.search(r'\bpass (\d+)\b', node)[1]),
        'nativeReadFileCalls': 1, 'additionalPaidTurns': 1, 'cumulativePaidTurns': 136,
        'referenceFiles': True, 'pastedReferences': False, 'backupOriginalReview': False,
        'sourceSha256': {s: hashlib.sha256((root / s).read_bytes()).hexdigest() for s in sources},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k != 'sourceSha256'}, ensure_ascii=False))
