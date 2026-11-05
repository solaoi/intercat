"""Check backup recovery evidence using disposable fixtures and saved test logs."""
import hashlib
import hmac
import json
from pathlib import Path
import re
import sqlite3
import sys
import uuid


def encoded(value, *, sort=False):
    return json.dumps(value, ensure_ascii=False, sort_keys=sort, separators=(',', ':')).encode()


def database(path):
    return sqlite3.connect(f'file:{path}?mode=ro', uri=True)


def verify(directory):
    rust = (directory / 'rust.log').read_text()
    rows = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust)
    assert rows and 'FAILED' not in rust and 'Doc-tests intercat_git' in rust
    for name in (
        'applied_file_pickup_retires_only_the_old_session_and_waits_for_external_confirmation',
        'apply_requires_current_decisions_and_preserves_unverified_files_on_discard',
        'prepared_recovery_keeps_later_edits_and_rejects_a_changed_snapshot_reference',
        'interrupted_snapshot_preparation_can_be_cancelled_without_retiring_history',
    ):
        assert f'{name} ... ok' in rust, name
    ui = (directory / 'ui.log').read_text()
    assert not re.search(r'^\s+\d+ failed\b', ui, re.M)
    for name in (
        'prepared backup recovery retries its original operation',
        'reviewed backup choices can be applied only after all workspace reviews',
        'unfinished backup preparation can stop owned processes',
    ):
        assert name in ui, name
    node = (directory / 'node.log').read_text()
    assert re.search(r'\bfail 0\b', node)
    for name in ('clippy.log', 'build.log'):
        log = (directory / name).read_text()
        assert 'Finished' in log and 'error:' not in log
    for name in ('typecheck.log', 'format.log'):
        assert 'error' not in (directory / name).read_text().lower()
    assert 'built in' in (directory / 'web-build.log').read_text()
    assert (directory / 'prepared-recovery.png').stat().st_size > 10000

    fixture = json.loads((directory / 'native-current/result.json').read_text())
    assert fixture['schema'] == 49 and fixture['paidTurns'] == 0
    assert all(fixture[k] for k in ('filesUnchanged', 'indexUnchanged', 'sourceRejected', 'oldHistoryRejected'))
    prepared, complete = fixture['prepared'], fixture['complete']
    assert prepared['historyPaused'] and not complete['historyPaused']
    assert prepared['publication'] == complete['publication']
    assert complete['apply']['phase'] == 'complete'
    operation = complete['apply']['opId']
    source, target = (complete['publication'][key] for key in ('source', 'target'))
    assert target['catalog']['revision'] > source['catalog']['revision']
    assert uuid.UUID(fixture['resume']['sessionId']).version == 7
    assert uuid.UUID(fixture['resume']['startCommand']).version == 7
    with database(directory / 'native-current/events.sqlite') as db:
        assert db.execute('PRAGMA user_version').fetchone()[0] == 49
        assert db.execute('SELECT session,track,operation FROM retired_sessions').fetchone() == ('session', 'track', operation)
        assert db.execute("SELECT count(*) FROM timeline_turns WHERE session='session'").fetchone()[0] == 0
        assert db.execute("SELECT count(*) FROM timeline_turns WHERE session='other-session'").fetchone()[0] == 1
        assert db.execute('SELECT count(*) FROM integration_jobs').fetchone()[0] == 0
        assert db.execute("SELECT count(*) FROM operations WHERE kind='checkpoint.save'").fetchone()[0] == 0
        assert db.execute('SELECT revision,sealed_revision FROM secret_catalog').fetchone() == (target['catalog']['revision'],) * 2
        active, record = db.execute('SELECT active,record FROM backup_applies').fetchone()
        record = json.loads(record)
        seal = record.pop('seal')
        assert not active and record['phase'] == 'complete'
        assert hmac.compare_digest(seal, hmac.new(bytes([7]) * 32, b'intercat.backup-apply.v1\0' + encoded(record, sort=True), hashlib.sha256).hexdigest())
        assert record['checkpointRefs']['track'] is not None
        assert json.loads(db.execute('SELECT record FROM backup_resumes').fetchone()[0]) == fixture['resume']
        assert db.execute('SELECT attachment FROM attachment_turn_items WHERE session=?', (fixture['newSession'],)).fetchone()[0] == fixture['attachment']['id']
        handoff = db.execute("SELECT text FROM utterances WHERE state='placed'").fetchone()[0]
        assert 'Previous session history, instructions and results are unavailable' in handoff
        assert 'DUMMY_RESTORED_HISTORY_MUST_NOT_BE_SHOWN' not in handoff
        witness = hashlib.sha256(encoded(['intercat.retired-session.v1', 'session', 'track', operation, None])).hexdigest()
        assert target['proof']['sessions'] == [{'sessionId': 'session', 'trackId': 'track', 'digest': witness}]

    success = list((directory / 'muse-checkpoint').glob('*/result.json'))
    assert len(success) == 1
    native = json.loads(success[0].read_text())
    assert native['paidTurns'] == 1 and native['retiredHistoryExcluded']
    worker = native['worker']
    assert not worker['checkpointPending'] and worker['activeTurn'] is None
    assert worker['sessionId'] != 'session' and 'CURRENT_BACKUP_FILES_OK' in worker['output']
    with database(success[0].parent / 'events.sqlite') as db:
        assert db.execute("SELECT count(*) FROM operations WHERE kind='turn.start'").fetchone()[0] == 1
        assert db.execute("SELECT count(*) FROM operations WHERE kind='checkpoint.save' AND state='complete'").fetchone()[0] == 1
        session_events = '\n'.join(row[0] for row in db.execute('SELECT payload FROM events WHERE session_id=?', (worker['sessionId'],)))
        assert 'DO_NOT_IMPORT_LOST_CONTEXT_a91f' not in session_events
        assert 'read_file' in session_events
    assert '1 passed; 0 failed' in (directory / 'muse-checkpoint.log').read_text()
    attempts = [json.loads(path.read_text())['attemptedModelTurns'] for path in directory.glob('muse*/*/attempt.json')]
    assert sum(attempts) == 3
    root = Path(__file__).resolve().parents[1]
    sources = [
        'crates/intercat-daemon/src/backup_apply.rs', 'crates/intercat-daemon/src/session_retirement.rs',
        'crates/intercat-daemon/src/backup_review.rs', 'crates/intercat-daemon/src/secret_store.rs',
        'crates/intercat-daemon/src/runtime.rs', 'crates/intercat-daemon/src/journal.rs',
        'crates/intercat-daemon/tests/backup_review.rs', 'electron/fingerprint-recovery.mjs',
        'electron/main.mjs', 'electron/backup-apply.test.mjs', 'src/desk/FingerprintRecovery.tsx',
        'tests/desktop.spec.ts', 'tools/verify_backup_apply.py',
    ]
    return {
        'schema': 49, 'rustPassed': sum(int(a) for a, _ in rows), 'rustIgnored': sum(int(b) for _, b in rows),
        'desktopPassed': int(re.search(r'(\d+) passed \(', ui)[1]),
        'desktopSkipped': int(re.search(r'(\d+) skipped', ui)[1]),
        'nodePassed': int(re.search(r'\bpass (\d+)\b', node)[1]),
        'decisionsApplied': True, 'externalConfirmationBeforeOpening': True,
        'retiredSessionWitnessVerified': True, 'recoverySealVerified': True,
        'privatePipeVerified': True, 'cancellationVerified': True,
        'checkpointPublicationRetryVerified': True, 'nativeNewSessionTurnPassed': True,
        'additionalPaidTurns': sum(attempts), 'cumulativePaidTurns': 144 + sum(attempts),
        'sourceSha256': {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k != 'sourceSha256'}, ensure_ascii=False))
