"""Check the process-recovery regression evidence and native zero-turn run."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys


def verify(directory: Path) -> dict:
    rust = (directory / "rust.log").read_text()
    summaries = re.findall(r"test result: ok\. (\d+) passed; 0 failed; (\d+) ignored", rust)
    assert summaries and "FAILED" not in rust
    for name in (
        "persisted_group_finds_a_new_unmarked_child_after_both_parents_exit",
        "recovery_rejects_missing_incomplete_public_or_wrong_workspace_evidence",
        "a_reused_pid_cannot_inherit_a_saved_process_or_group",
        "a_restarted_daemon_stops_recorded_hosts_and_keeps_unfinished_work_held",
        "retiring_a_connection_preserves_children_and_next_host_inherits_their_ownership",
    ):
        assert f"{name} ... ok" in rust, name
    final_host = (directory / "host-final.log").read_text()
    assert "failed_setup_and_spawn_leave_proof_that_no_host_was_started ... ok" in final_host
    assert "test result: ok. 2 passed; 0 failed" in final_host
    desktop = (directory / "desktop.log").read_text()
    passed = re.search(r"(\d+) passed \(", desktop)
    assert passed and not re.search(r"^\s+\d+ failed\b", desktop, re.MULTILINE)
    native = list((directory / "native").glob("*/result.json"))
    assert len(native) == 1, "Use a fresh evidence directory for each native run."
    result = json.loads(native[0].read_text())
    assert result["sameSession"] and result["newHost"] and result["modelTurns"] == 0
    assert result["first"]["sessionId"] == result["recovered"]["sessionId"]
    assert result["first"]["identity"] != result["recovered"]["identity"]
    database = sqlite3.connect(f"file:{native[0].with_name('events.sqlite')}?mode=ro", uri=True)
    operations = dict(database.execute("SELECT kind,COUNT(*) FROM operations GROUP BY kind"))
    assert operations.get("turn.start", 0) == 0
    for method in ("session/start", "session/resume", "session/leave"):
        assert operations.get(method) == 1, (method, operations)
    assert database.execute("SELECT COUNT(*) FROM events WHERE kind='worker.reconnectFailed'").fetchone()[0] == 1
    database.close()
    root = Path(__file__).resolve().parents[1]
    sources = [
        "crates/intercat-daemon/src/process.rs",
        "crates/intercat-daemon/src/process_ledger.rs",
        "crates/intercat-daemon/src/host.rs",
        "crates/intercat-daemon/src/runtime.rs",
        "crates/intercat-daemon/src/rpc.rs",
        "crates/intercat-daemon/src/runtime_tests.rs",
        "crates/intercat-daemon/tests/host_recovery.rs",
        "crates/intercat-daemon/tests/process_recovery.rs",
        "src/main.tsx",
        "tests/desktop.spec.ts",
        "tools/verify_process_recovery.py",
    ]
    return {
        "rustPassed": sum(int(item[0]) for item in summaries),
        "rustIgnored": sum(int(item[1]) for item in summaries),
        "finalHostPassed": 2,
        "desktopPassed": int(passed[1]),
        "nativeSameSession": True,
        "nativeNewHost": True,
        "nativeRecoveryMs": result["recoveryMs"],
        "nativeOperations": operations,
        "newPaidTurns": 0,
        "sourceSha256": {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources},
    }


if __name__ == "__main__":
    output = Path(sys.argv[1])
    evidence = verify(output)
    (output / "verification.json").write_text(json.dumps(evidence, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({key: value for key, value in evidence.items() if key != "sourceSha256"}, ensure_ascii=False))
