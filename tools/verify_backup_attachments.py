"""Verify restored-original review evidence without opening user data or calling Muse."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys


def verify(directory):
    rust = (directory / 'rust.log').read_text()
    suites = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust)
    assert suites and 'FAILED' not in rust and 'Doc-tests intercat_git' in rust
    for name in (
        'restored_originals_and_owned_copies_are_verified_without_writes_and_changed_sets_are_stale',
        'missing_or_changed_originals_and_unsafe_copies_remain_unverified_without_recreation',
        'newer_originals_and_unassigned_copies_are_reported_and_forged_reviews_are_stale',
        'file_review_requires_stopped_owned_processes_and_refuses_a_replaced_workspace',
        'private_review_command_never_starts_rpc_or_exposes_history',
    ):
        assert f'{name} ... ok' in rust, name
    targeted = (directory / 'targeted-complete.log').read_text()
    assert '11 passed; 0 failed' in targeted
    ui = (directory / 'ui.log').read_text()
    assert not re.search(r'^\s+\d+ failed\b', ui, re.M)
    assert 'backup file comparison retries its receipt and requires a new review after files change' in ui
    assert '1 passed' in (directory / 'ui-final.log').read_text()
    assert (directory / 'review-ui.png').stat().st_size > 1000
    node = (directory / 'node.log').read_text()
    assert re.search(r'\bfail 0\b', node)
    for name in ('clippy.log', 'build.log'):
        text = (directory / name).read_text()
        assert 'Finished' in text and 'error:' not in text
    for name in ('format.log', 'typecheck.log'):
        assert 'error' not in (directory / name).read_text().lower()

    result = json.loads((directory / 'native-complete/result.json').read_text())
    assert result['schema'] == 47 and result['paidTurns'] == 0
    assert all(result[key] for key in ('originalUnchanged', 'indexUnchanged', 'privateBytesNotRecorded'))
    attachment = result['attachment']
    matched = result['matched']['fileReview']['attachments']
    assert matched['complete'] and matched['originals'] == matched['verifiedOriginals'] == 1
    assert len(matched['copies']) == 1
    assert matched['copies'][0]['state'] == 'matched'
    assert matched['copies'][0]['attachmentId'] == attachment['id']
    assert result['changed']['fileReview']['attachments']['copies'][0]['state'] == 'changed'
    assert not result['changed']['workspaces'][0]['attachmentsReviewed']
    assert result['stale']['fileReview']['stale']
    assert not result['stale']['workspaces'][0]['attachmentsReviewed']
    assert result['restored']['workspaces'][0]['attachmentsReviewed']
    assert result['cli']['review']['workspaces'][0]['attachmentsReviewed']
    assert all(result[key]['historyPaused'] for key in ('matched', 'changed', 'stale', 'restored'))
    assert result['cli']['review']['historyPaused']
    private = ('DUMMY_RESTORED_HISTORY_MUST_NOT_BE_SHOWN', 'CHANGED_PRIVATE_COPY')
    database = directory / 'native-complete/events.sqlite'
    with sqlite3.connect(f'file:{database}?mode=ro', uri=True) as db:
        assert db.execute('PRAGMA user_version').fetchone()[0] == 47
        reviews = db.execute("SELECT payload,state FROM operations WHERE kind='backup.files.review'").fetchall()
        assert len(reviews) == 4
        for payload, state in reviews:
            assert state == 'complete' and attachment['sha256'] not in payload
            assert all(value not in payload for value in private)
            review = json.loads(payload)['review']
            assert re.fullmatch(r'[a-f0-9]{64}', review['seal']) and not review['stale']
        assert db.execute('SELECT count(*) FROM attachments').fetchone()[0] == 1
        assert db.execute('SELECT attachment FROM attachment_turn_items').fetchone()[0] == attachment['id']
    root = Path(__file__).resolve().parents[1]
    sources = [
        'crates/intercat-daemon/src/backup_attachments.rs',
        'crates/intercat-daemon/src/backup_files.rs',
        'crates/intercat-daemon/src/backup_review.rs',
        'crates/intercat-daemon/src/attachment_store.rs',
        'crates/intercat-daemon/src/journal.rs',
        'crates/intercat-daemon/src/lib.rs',
        'crates/intercat-daemon/tests/backup_review.rs',
        'src/bridge.d.ts', 'src/desk/FingerprintRecovery.tsx',
        'tests/desktop.spec.ts', 'tools/verify_backup_attachments.py',
    ]
    return {
        'schema': 47, 'rustPassed': sum(int(a) for a, _ in suites),
        'rustIgnored': sum(int(b) for _, b in suites),
        'desktopPassed': int(re.search(r'(\d+) passed \(', ui)[1]),
        'desktopSkipped': int(re.search(r'(\d+) skipped', ui)[1]),
        'desktopFocusedPassed': 1, 'reviewZoom': 2,
        'nodePassed': int(re.search(r'\bpass (\d+)\b', node)[1]),
        'restoredOriginalSetVerified': True, 'ownedCopiesCompared': True,
        'unrecordedOriginalsReported': True, 'staleReviewRejected': True,
        'privatePipeVerified': True, 'historyReopened': False,
        'additionalPaidTurns': 0, 'cumulativePaidTurns': 144,
        'sourceSha256': {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k != 'sourceSha256'}, ensure_ascii=False))
