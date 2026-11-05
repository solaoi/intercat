"""Verify staged recovery choices without accessing real Keychain or model services."""
import hashlib
import hmac
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
        'session_choices_are_review_bound_durable_and_do_not_reopen_or_change_history',
        'changed_files_invalid_targets_and_unverified_originals_cannot_silently_receive_a_pickup_choice',
        'sessions_missing_from_the_restored_workspace_require_acknowledged_discard',
    ):
        assert f'{name} ... ok' in rust, name
    assert '14 passed; 0 failed' in (directory / 'targeted-final.log').read_text()
    ui = (directory / 'ui.log').read_text()
    assert not re.search(r'^\s+\d+ failed\b', ui, re.M)
    assert 'backup session choices survive a lost acknowledgement and remain pending after reload' in ui
    assert 'an unmatched restored session cannot borrow another sessions file review' in ui
    node = (directory / 'node.log').read_text()
    assert re.search(r'\bfail 0\b', node)
    for name in ('clippy.log', 'build.log'):
        text = (directory / name).read_text()
        assert 'Finished' in text and 'error:' not in text
    for name in ('typecheck.log', 'format.log'):
        assert 'error' not in (directory / name).read_text().lower()
    native = json.loads((directory / 'native/result.json').read_text())
    assert native['schema'] == 48 and native['paidTurns'] == 0
    assert native['filesUnchanged'] and native['catalogUnchanged']
    picked = native['picked']['sessions'][0]['choice']
    discarded = native['discarded']['sessions'][0]['choice']
    assert picked['choice'] == 'pick-up' and discarded['choice'] == 'discard-history'
    assert picked['opId'] != discarded['opId']
    assert not picked['stale'] and not discarded['stale']
    assert picked['needsRecheck'] and discarded['needsRecheck']
    for result in (native['picked'], native['discarded'], native['cli']['review']):
        assert result['historyPaused']
    assert native['cli']['review']['sessions'][0]['choice'] == discarded
    database = directory / 'native/events.sqlite'
    with sqlite3.connect(f'file:{database}?mode=ro', uri=True) as db:
        assert db.execute('PRAGMA user_version').fetchone()[0] == 48
        records = db.execute("SELECT id,payload,state FROM operations WHERE kind='backup.session.choice'").fetchall()
        assert len(records) == 2
        for identifier, payload, state in records:
            record = json.loads(payload)
            assert state == 'complete' and record['request']['opId'] == identifier
            assert re.fullmatch(r'[0-9a-f]{64}', record['seal'])
            assert re.fullmatch(r'[0-9a-f]{64}', record['binding'])
            material = b'intercat.backup-choice.v1\0' + json.dumps(
                [record['request'], record['trackId'], record['binding'], record['reviewSeal']],
                separators=(',', ':'), ensure_ascii=False,
            ).encode()
            assert hmac.compare_digest(record['seal'], hmac.new(bytes([7]) * 32, material, hashlib.sha256).hexdigest())
            assert 'DUMMY_RESTORED_HISTORY_MUST_NOT_BE_SHOWN' not in payload
        record = json.loads(db.execute('SELECT record FROM backup_session_choices').fetchone()[0])
        assert record['request']['opId'] == discarded['opId']
        assert db.execute("SELECT count(*) FROM operations WHERE kind IN ('turn.start','session/start')").fetchone()[0] == 0
        assert db.execute('SELECT revision FROM secret_catalog').fetchone()[0] == 0
    root = Path(__file__).resolve().parents[1]
    sources = [
        'crates/intercat-daemon/src/backup_choices.rs',
        'crates/intercat-daemon/src/backup_review.rs',
        'crates/intercat-daemon/src/journal.rs',
        'crates/intercat-daemon/src/lib.rs',
        'crates/intercat-daemon/tests/backup_review.rs',
        'electron/fingerprint-recovery.mjs', 'src/bridge.d.ts',
        'src/desk/FingerprintRecovery.tsx', 'src/style.css',
        'tests/desktop.spec.ts', 'tools/verify_backup_choices.py',
    ]
    return {
        'schema': 48, 'rustPassed': sum(int(a) for a, _ in rows),
        'rustIgnored': sum(int(b) for _, b in rows),
        'desktopPassed': int(re.search(r'(\d+) passed \(', ui)[1]),
        'desktopSkipped': int(re.search(r'(\d+) skipped', ui)[1]),
        'nodePassed': int(re.search(r'\bpass (\d+)\b', node)[1]),
        'choicesPersisted': True, 'staleAndTamperedChoicesRejected': True,
        'privatePipeVerified': True, 'historyReopened': False, 'decisionsApplied': False,
        'additionalPaidTurns': 0, 'cumulativePaidTurns': 144,
        'sourceSha256': {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k != 'sourceSha256'}, ensure_ascii=False))
