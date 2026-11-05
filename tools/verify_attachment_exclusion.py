"""Check local Git exclusion evidence; does not read auth or call Muse."""
import hashlib
import json
from pathlib import Path
import re
import sys


def verify(directory):
    rust = (directory / 'rust.log').read_text()
    rows = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust)
    assert rows and 'test result: FAILED' not in rust and 'Doc-tests intercat_git' in rust
    for name in (
        'attachment_copies_stay_out_of_checkpoints_even_when_force_added_and_unignored',
        'tracked_attachment_name_collisions_are_preserved_and_copies_use_another_root',
        'unregistered_attachment_names_remain_user_files',
        'attachments_from_hooks_and_checks_never_enter_candidate_commits',
        'attachment_name_collisions_keep_project_code_through_hooks_and_commit_checks',
        'attachment_filter_does_not_hide_a_check_that_creates_a_new_secret_file',
        'attachment_filter_does_not_hide_check_changes_to_unregistered_directories',
        'attachment_ownership_survives_resume_and_rejects_incoming_project_collisions',
        'occupied_project_roots_are_kept_and_new_copy_namespaces_are_excluded',
        'directory_selection_never_reuses_existing_files_or_follows_a_parent_link',
        'child_waits_for_receipt_then_retains_parent_masks_across_restart',
    ):
        assert f'{name} ... ok' in rust, name
    final = (directory / 'targeted-final.log').read_text()
    assert 'test result: FAILED' not in final and '0 failed' in final
    assert 'attachment_ownership_survives_resume_and_rejects_incoming_project_collisions ... ok' in final
    ui = (directory / 'ui.log').read_text()
    passed = re.search(r'(\d+) passed \(', ui)
    skipped = re.search(r'(\d+) skipped', ui)
    assert passed
    failed = re.search(r'^\s+(\d+) failed\b', ui, re.M)
    rerun_count = 0
    if failed:
        assert int(failed[1]) == 1
        rerun = (directory / 'ui-rerun.log').read_text()
        assert re.search(r'\b1 passed \(', rerun) and not re.search(r'^\s+\d+ failed\b', rerun, re.M)
        name = 'desktop Keep resumes both exact commits and hides the detected values'
        assert name in ui and name in rerun
        assert 'TimeoutError: locator.click: Timeout 30000ms exceeded.' in ui
        rerun_count = 1
    node = (directory / 'node.log').read_text()
    assert re.search(r'\bfail 0\b', node)
    for name in ('clippy-final.log', 'build.log'):
        log = (directory / name).read_text()
        assert 'Finished' in log and 'error:' not in log
    for name in ('typecheck.log', 'format.log'):
        assert 'error' not in (directory / name).read_text().lower()
    root = Path(__file__).resolve().parents[1]
    sources = [
        'crates/intercat-git/src/attachments.rs', 'crates/intercat-git/src/lib.rs',
        'crates/intercat-daemon/src/attachments.rs', 'crates/intercat-daemon/src/checkout.rs',
        'crates/intercat-daemon/src/checkpoint.rs', 'crates/intercat-daemon/src/candidate.rs',
        'crates/intercat-daemon/src/preparation.rs', 'crates/intercat-daemon/src/branch.rs',
        'crates/intercat-daemon/src/branch_run.rs', 'crates/intercat-daemon/src/repair.rs',
        'crates/intercat-daemon/src/backup_files.rs', 'crates/intercat-daemon/src/journal.rs',
        'crates/intercat-daemon/src/execution.rs', 'src/bridge.d.ts',
        'crates/intercat-daemon/tests/checkpoint.rs', 'crates/intercat-daemon/tests/checkout.rs',
        'crates/intercat-daemon/tests/candidate.rs', 'crates/intercat-daemon/tests/branch.rs',
        'tools/verify_attachment_exclusion.py',
    ]
    return {
        'schema': 44,
        'rustPassed': sum(int(a) for a, _ in rows),
        'rustIgnored': sum(int(b) for _, b in rows),
        'desktopPassed': int(passed[1]) + rerun_count,
        'desktopInitialPassed': int(passed[1]),
        'desktopInitialTimeouts': rerun_count,
        'desktopRerunPassed': rerun_count,
        'desktopSkipped': int(skipped[1]) if skipped else 0,
        'nodePassed': int(re.search(r'\bpass (\d+)\b', node)[1]),
        'ownedDirectoriesOnly': True, 'forceAddExcluded': True,
        'hookAddedCopiesExcluded': True, 'userFilesPreserved': True,
        'checkMutationStillDetected': True, 'originalAttachmentStoreImplemented': False,
        'additionalPaidTurns': 0, 'cumulativePaidTurns': 135,
        'sourceSha256': {s: hashlib.sha256((root / s).read_bytes()).hexdigest() for s in sources},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k != 'sourceSha256'}, ensure_ascii=False))
