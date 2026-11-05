"""Check attachment provenance and native recovery evidence without model calls."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys


def native(directory, name):
    results = list((directory / name).glob('*/result.json'))
    assert len(results) == 1, name
    path = results[0]
    result = json.loads(path.read_text())
    assert result['schema'] == 46
    db = sqlite3.connect(f'file:{path.parent / "events.sqlite"}?mode=ro', uri=True)
    assert db.execute('PRAGMA user_version').fetchone()[0] == 46
    starts = [json.loads(p) for p, in db.execute("SELECT payload FROM operations WHERE kind='turn.start'")]
    return result, db, starts


def calls(db, session):
    items = {}
    for payload, in db.execute("SELECT payload FROM events WHERE kind='msp.received' AND session_id=?", (session,)):
        value = json.loads(payload)
        item = value.get('frame', value).get('params', {}).get('item', {})
        if item.get('kind') == 'toolCall':
            items[item['itemId']] = item
    return list(items.values())


def references(start):
    prompt = start['input'][0]['arguments']
    return json.loads(prompt.splitlines()[-1])


def inherited_ids(db, session):
    return {a for a, in db.execute('SELECT attachment FROM attachment_turn_items WHERE session=?', (session,))}


def verify(directory):
    rust = (directory / 'rust.log').read_text()
    rows = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust)
    assert rows and 'FAILED' not in rust and 'Doc-tests intercat_git' in rust
    for name in (
        'branch_cut_is_frozen_and_nested_branches_cannot_receive_later_references',
        'pickup_keeps_unknown_dispatches_but_not_superseded_or_future_queued_files',
        'unconfirmed_or_conflicting_turn_boundaries_fail_closed',
        'branch_restores_the_frozen_original_set_after_restart_and_excludes_parent_late_files',
        'pickup_recreates_known_and_uncertain_references_but_keeps_unsent_inputs_held',
    ):
        assert f'{name} ... ok' in rust, name
    ui = (directory / 'ui.log').read_text()
    assert not re.search(r'^\s+\d+ failed\b', ui, re.M)
    node = (directory / 'node.log').read_text()
    assert re.search(r'\bfail 0\b', node)
    for name in ('clippy.log', 'build.log'):
        text = (directory / name).read_text()
        assert 'Finished' in text and 'error:' not in text
    for name in ('typecheck.log', 'format.log'):
        assert 'error' not in (directory / name).read_text().lower()
    admission = (directory / 'admission-fixtures.log').read_text()
    assert 'FAILED' not in admission
    assert 'a_cut_request_cannot_reserve_a_turn_and_a_started_request_cannot_be_cut ... ok' in admission
    assert 'received_message_lands_before_turn_start_acknowledgement ... ok' in admission
    questions = (directory / 'questions-final.log').read_text()
    assert 'FAILED' not in questions and '13 passed; 0 failed' in questions
    for name, test in (
        ('branch-native.log', 'real_branch_uses_the_early_tree_and_inherits_secret_fingerprints'),
        ('branch-final-native.log', 'real_branch_uses_the_early_tree_and_inherits_secret_fingerprints'),
        ('pickup-native.log', 'real_file_pickup_preserves_edits_and_runs_one_handoff_in_a_new_session'),
    ):
        assert f'{test} ... ok' in (directory / name).read_text()

    first, db, starts = native(directory, 'branch')
    assert first['paidTurns'] == len(starts) == 3
    db.close()
    branch, db, starts = native(directory, 'branch-final')
    assert branch['paidTurns'] == len(starts) == 3
    assert all(branch[k] for k in ('attachmentBoundaryVerified', 'cleanEarlyTree', 'inheritedMask', 'lateHistoryExcluded'))
    child = branch['branch']['sessionId']
    early = branch['earlyAttachment']['id']
    late = branch['lateAttachment']['id']
    snapshot = json.loads(db.execute('SELECT record FROM attachment_snapshots WHERE owner=?', (branch['branch']['request']['opId'],)).fetchone()[0])
    assert snapshot['session'] == branch['branch']['parentSessionId']
    assert snapshot['through'] == branch['first']['turnId']
    assert {a for t in snapshot['turns'] for a in t['attachments']} == {early}
    assert inherited_ids(db, child) == {early}
    child_starts = [s for s in starts if s['sessionId'] == child]
    assert len(child_starts) == 1
    refs = references(child_starts[0])
    assert [f['id'] for f in refs['files']] == [early]
    assert refs['directory'] == branch['branch']['workspace']['path'] + '/.intercat/attachments'
    assert refs['files'][0]['path'].startswith(refs['directory'] + '/')
    assert late not in child_starts[0]['input'][0]['arguments']
    child_calls = calls(db, child)
    assert len(child_calls) == 1
    assert child_calls[0]['tool'] == 'bash' and child_calls[0]['status'] == 'completed'
    assert 'EARLY_TREE_VERIFIED' in child_calls[0]['visibleOutput']
    payloads = '\n'.join(p for p, in db.execute('SELECT payload FROM events WHERE session_id=?', (child,)))
    assert '<secret:DEMO_TOKEN>' in payloads and 'LATE_ONLY_TURN' not in payloads
    db.close()

    pickup, db, starts = native(directory, 'pickup')
    assert pickup['modelTurns'] == pickup['summaryTurns'] == 1
    assert all(pickup[k] for k in ('attachmentRestored', 'indexUnchanged', 'sourceUnchanged', 'unfinishedEditPreserved'))
    session = pickup['newSession']
    attachment = pickup['attachment']['id']
    actual = [s for s in starts if s['sessionId'] == session]
    assert len(actual) == 1 and len(starts) == 2  # One old synthetic reservation.
    assert inherited_ids(db, session) == {attachment}
    frozen = [json.loads(r) for r, in db.execute('SELECT record FROM attachment_snapshots')]
    assert len(frozen) == 1 and frozen[0]['session'] == pickup['previousSession']
    assert frozen[0]['through'] is None
    assert {a for t in frozen[0]['turns'] for a in t['attachments']} == {attachment}
    refs = references(actual[0])
    assert [f['id'] for f in refs['files']] == [attachment]
    reads = [c for c in calls(db, session) if c['tool'] == 'read_file']
    assert len(reads) == 1 and reads[0]['status'] == 'completed'
    assert json.loads(reads[0]['args'])['path'] == refs['files'][0]['path']
    assert 'REFERENCE_PICKUP_OK' in reads[0]['visibleOutput']
    assert refs['files'][0]['path'].startswith(refs['directory'] + '/')
    db.close()

    root = Path(__file__).resolve().parents[1]
    sources = [
        'crates/intercat-daemon/src/lib.rs',
        'crates/intercat-daemon/src/attachment_history.rs',
        'crates/intercat-daemon/src/attachment_store.rs',
        'crates/intercat-daemon/src/journal.rs',
        'crates/intercat-daemon/src/history_store.rs',
        'crates/intercat-daemon/src/branch.rs',
        'crates/intercat-daemon/src/branch_run.rs',
        'crates/intercat-daemon/src/pickup.rs',
        'crates/intercat-daemon/src/pickup_run.rs',
        'crates/intercat-daemon/src/runtime.rs',
        'crates/intercat-daemon/src/execution.rs',
        'crates/intercat-daemon/src/runtime_tests.rs',
        'crates/intercat-daemon/tests/attachments.rs',
        'crates/intercat-daemon/tests/candidate.rs',
        'crates/intercat-daemon/tests/execution.rs',
        'crates/intercat-daemon/tests/reports.rs',
        'crates/intercat-daemon/tests/questions.rs',
        'crates/intercat-daemon/tests/timeline.rs',
        'tools/verify_attachment_inheritance.py',
    ]
    return {
        'schema': 46,
        'rustPassed': sum(int(a) for a, _ in rows), 'rustIgnored': sum(int(b) for _, b in rows),
        'desktopPassed': int(re.search(r'(\d+) passed \(', ui)[1]),
        'desktopSkipped': int(re.search(r'(\d+) skipped', ui)[1]),
        'nodePassed': int(re.search(r'\bpass (\d+)\b', node)[1]),
        'nativeBranchRuns': 2, 'nativePickupRuns': 1,
        'additionalPaidTurns': 8, 'cumulativePaidTurns': 144,
        'branchCutVerified': True, 'pickupOriginalRestored': True,
        'unknownPickupReferencesPreserved': True, 'unsentPickupInputsHeld': True,
        'backupOriginalReview': False,
        'sourceSha256': {s: hashlib.sha256((root / s).read_bytes()).hexdigest() for s in sources},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k != 'sourceSha256'}, ensure_ascii=False))
