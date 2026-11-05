"""Verify file-review evidence without reading private application data or running Muse."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys


def verify(directory):
    text = (directory / 'rust.log').read_text()
    rows = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', text)
    assert rows and 'test result: FAILED' not in text
    for name in (
        'file_review_covers_content_deletion_mode_ignored_files_and_detects_stale_receipts',
        'protected_special_hardlinked_and_attachment_files_never_receive_a_false_complete_review',
        'file_review_requires_stopped_owned_processes_and_refuses_a_replaced_workspace',
    ):
        assert f'{name} ... ok' in text
    final = (directory / 'targeted-final.log').read_text()
    assert '8 passed; 0 failed' in final
    assert 'changed_saved_file_review_metadata_is_not_accepted_as_a_current_confirmation ... ok' in final
    ui = (directory / 'ui.log').read_text()
    passed = re.search(r'(\d+) passed \(', ui)
    skipped = re.search(r'(\d+) skipped', ui)
    assert passed and not re.search(r'^\s+\d+ failed\b', ui, re.M)
    assert 'backup file comparison retries its receipt and requires a new review after files change' in ui
    node = (directory / 'node.log').read_text()
    assert re.search(r'\bfail 0\b', node)
    for name in ('clippy.log', 'build.log'):
        log = (directory / name).read_text()
        assert 'Finished' in log and 'error:' not in log
    for name in ('typecheck.log', 'format.log'):
        assert 'error' not in (directory / name).read_text().lower()
    native = json.loads((directory / 'native/result.json').read_text())
    assert all(native[k] for k in ('contentCompared', 'ignoredFilesIncluded', 'modeAndDeletionCompared', 'staleReviewDetected', 'indexUnchanged', 'rawContentNotStored'))
    assert native['paidTurns'] == 0
    review = json.loads((directory / 'native/review.json').read_text())
    assert review['historyPaused'] and review['workspaces'][0]['filesReviewed']
    assert not review['workspaces'][0]['attachmentsReviewed']
    file_review = review['fileReview']
    assert file_review['complete'] and not file_review['stale']
    assert re.fullmatch(r'[a-f0-9]{64}', file_review['seal'])
    changes = {c['path']: c['kind'] for c in file_review['changes']}
    for path, kind in [('kept.txt','modified'),('removed.txt','deleted'),('mode.sh','mode-changed'),('type.txt','type-changed'),('ignored-work.txt','added'),('pointer','added')]:
        assert changes[path] == kind
    database = native['database']
    assert Path(database).name == database and database.startswith('events-') and database.endswith('.sqlite')
    private = 'DUMMY_RESTORED_HISTORY_MUST_NOT_BE_SHOWN'
    with sqlite3.connect(f'file:{directory / "native" / database}?mode=ro', uri=True) as db:
        records = db.execute("SELECT payload,state FROM operations WHERE kind='backup.files.review'").fetchall()
        assert len(records) == 2
        for payload, state in records:
            assert state == 'complete' and private not in payload
            assert hashlib.sha256(private.encode()).hexdigest() not in payload
            stored = json.loads(payload)['review']
            assert re.fullmatch(r'[a-f0-9]{64}', stored['seal']) and not stored['stale']
    root = Path(__file__).resolve().parents[1]
    sources = [
        'crates/intercat-daemon/src/backup_files.rs', 'crates/intercat-daemon/src/backup_review.rs',
        'crates/intercat-daemon/src/journal.rs', 'crates/intercat-daemon/src/lib.rs',
        'crates/intercat-daemon/tests/backup_review.rs', 'electron/fingerprint-recovery.mjs',
        'src/bridge.d.ts', 'src/desk/FingerprintRecovery.tsx', 'tests/desktop.spec.ts',
        'tools/verify_backup_files.py',
    ]
    return {
        'rustPassed': sum(int(a) for a, _ in rows), 'rustIgnored': sum(int(b) for _, b in rows),
        'desktopPassed': int(passed[1]), 'desktopSkipped': int(skipped[1]) if skipped else 0,
        'nodePassed': int(re.search(r'\bpass (\d+)\b', node)[1]), 'fileReview': native,
        'additionalPaidTurns': 0, 'cumulativePaidTurns': 135,
        'historyReopened': False, 'attachmentOriginalsVerified': False,
        'sourceSha256': {s: hashlib.sha256((root / s).read_bytes()).hexdigest() for s in sources},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k != 'sourceSha256'}, ensure_ascii=False))
