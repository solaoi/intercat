"""Check local backup-review evidence without opening hosts or using Keychain."""
import hashlib
import json
from pathlib import Path
import re
import sys


def verify(directory):
    rust = (directory / 'rust.log').read_text()
    rows = re.findall(r'test result: ok\. (\d+) passed; 0 failed; (\d+) ignored', rust)
    assert rows and 'test result: FAILED' not in rust
    for name in (
        'review_never_opens_history_and_stops_an_owned_host_missing_from_the_restored_log',
        'unverified_or_missing_process_records_are_not_reported_as_stopped',
        'private_review_command_never_starts_rpc_or_exposes_history',
        'ordinary_verified_databases_cannot_use_the_offline_stop_interface',
    ):
        assert f'{name} ... ok' in rust
    assert '4 passed; 0 failed' in (directory / 'targeted-final.log').read_text()
    ui = (directory / 'ui.log').read_text()
    passed = re.search(r'(\d+) passed \(', ui)
    skipped = re.search(r'(\d+) skipped', ui)
    assert passed and not re.search(r'^\s+\d+ failed\b', ui, re.M)
    assert 'blocked backup review hides history and retries a lost process-stop acknowledgement' in ui
    assert '1 passed (' in (directory / 'ui-targeted-final.log').read_text()
    node = (directory / 'node.log').read_text()
    assert re.search(r'\bfail 0\b', node)
    node_passed = re.search(r'\bpass (\d+)\b', node)
    for name in ('clippy.log', 'build.log'):
        text = (directory / name).read_text()
        assert 'Finished' in text and 'error:' not in text
    for name in ('typecheck.log', 'format.log'):
        assert 'error' not in (directory / name).read_text().lower()
    native = json.loads((directory / 'native/native.json').read_text())
    assert native == {'historyHidden': True, 'unloggedHostStopped': True, 'unrelatedProcessPreserved': True, 'filesAndIndexUnchanged': True, 'receiptStillBlocked': True, 'paidTurns': 0}
    review = json.loads((directory / 'native/review.json').read_text())
    assert review['historyPaused']
    assert review['fingerprints']['sessions'][0]['reason'] == 'missing'
    assert review['workspaces'][0]['savedReference']['matches'] is False
    assert review['workspaces'][0]['filesReviewed'] is False
    assert review['workspaces'][0]['attachmentsReviewed'] is False
    assert review['hosts'][0]['knownToLog'] is False
    assert review['hosts'][0]['workspace']
    assert review['hosts'][0]['processes'] == review['hosts'][0]['groups'] == 0
    root = Path(__file__).resolve().parents[1]
    paths = [
        'crates/intercat-daemon/src/backup_review.rs', 'crates/intercat-daemon/src/process.rs',
        'crates/intercat-daemon/src/journal.rs', 'crates/intercat-daemon/src/main.rs', 'crates/intercat-daemon/src/lib.rs',
        'crates/intercat-daemon/tests/backup_review.rs', 'electron/fingerprint-recovery.mjs',
        'electron/recovery-command.mjs', 'electron/recovery-command.test.mjs', 'electron/main.mjs', 'electron/preload.cjs',
        'src/bridge.d.ts', 'src/desk/FingerprintRecovery.tsx', 'tests/desktop.spec.ts', 'tools/verify_backup_review.py',
    ]
    return {
        'rustPassed': sum(int(a) for a, _ in rows), 'rustIgnored': sum(int(b) for _, b in rows),
        'desktopPassed': int(passed[1]), 'desktopSkipped': int(skipped[1]) if skipped else 0,
        'nodePassed': int(node_passed[1]), 'native': native,
        'additionalPaidTurns': 0, 'cumulativePaidTurns': 135,
        'historyReopened': False, 'fileAndAttachmentReviewComplete': False,
        'sourceSha256': {p: hashlib.sha256((root / p).read_bytes()).hexdigest() for p in paths},
    }


if __name__ == '__main__':
    directory = Path(sys.argv[1])
    result = verify(directory)
    (directory / 'verification.json').write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k != 'sourceSha256'}, ensure_ascii=False))
