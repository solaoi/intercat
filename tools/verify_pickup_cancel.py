"""Verify saved cancellation evidence without starting a worker or calling Muse."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys


def verify(directory):
    rust = (directory / "rust.log").read_text()
    summaries = re.findall(r"test result: ok\. (\d+) passed; 0 failed; (\d+) ignored", rust)
    assert summaries and "test result: FAILED" not in rust
    for name in (
        "cancel_can_arrive_before_prepare_and_survives_reopening_without_reading_history",
        "cancel_every_pre_activation_phase_keeps_snapshots_fingerprints_and_held_inputs",
        "cancellation_rejects_reused_ids_stale_owners_and_already_activated_recoveries",
        "cancellation_does_not_overwrite_a_changed_workspace_or_reassign_an_operation",
        "cancellation_interrupts_a_locked_recovery_and_retries_recorded_host_cleanup",
    ):
        assert f"{name} ... ok" in rust, name
    final_runtime = (directory / "runtime-final.log").read_text()
    assert "1 passed; 0 failed" in final_runtime
    final_rpc = (directory / "rpc-final.log").read_text()
    assert "file_recovery_cancellation_is_authenticated_durable_and_idempotent ... ok" in final_rpc
    assert "1 passed; 0 failed" in final_rpc
    ui = (directory / "ui.log").read_text()
    passed = re.search(r"(\d+) passed \(", ui)
    skipped = re.search(r"(\d+) skipped", ui)
    assert passed and not re.search(r"^\s+\d+ failed\b", ui, re.MULTILINE)
    for name in (
        "file recovery can be cancelled while pending and cancellation acknowledgements can be retried",
        "a persisted cancellation can finish after the app reopens",
    ):
        assert name in ui
    node = (directory / "node.log").read_text()
    assert re.search(r"\bfail 0\b", node)
    node_passed = re.search(r"\bpass (\d+)\b", node)
    for name in ("clippy.log", "build.log"):
        log = (directory / name).read_text()
        assert "Finished" in log and "error:" not in log
    for name in ("typecheck.log", "format.log"):
        assert "error" not in (directory / name).read_text().lower()
    artifacts = list((directory / "native").glob("*/result.json"))
    assert len(artifacts) == 1
    native = json.loads(artifacts[0].read_text())
    assert native == {
        "cancelled": True, "persistedBeforeUnlock": True,
        "damagedLedgerBlockedCompletion": True, "summaryHostStopped": True,
        "newSessionHostStopped": True, "unrelatedProcessPreserved": True,
        "sameCancellationRetried": True, "handoffDispatched": False, "paidTurns": 0,
    }
    with sqlite3.connect(f"file:{artifacts[0].with_name('events.sqlite')}?mode=ro", uri=True) as db:
        recoveries = db.execute("SELECT payload,state FROM operations WHERE kind='worker.pickup'").fetchall()
        assert len(recoveries) == 1 and recoveries[0][1] == "complete"
        recovery = json.loads(recoveries[0][0])
        assert recovery["phase"] == "cancelled"
        assert recovery["snapshot"]["reference"].endswith("-recovery/" + recovery["request"]["opId"])
        cancellations = db.execute("SELECT payload,state FROM operations WHERE kind='worker.pickup.cancel'").fetchall()
        assert len(cancellations) == 1 and cancellations[0][1] == "complete"
        cancel = json.loads(cancellations[0][0])
        assert cancel["recoveryId"] == recovery["request"]["opId"]
        worker = json.loads(db.execute("SELECT record FROM workers").fetchone()[0])
        assert worker["phase"] == "after-connection-loss"
        assert worker["sessionId"] == recovery["request"]["sessionId"]
        assert db.execute("SELECT COUNT(*) FROM utterances WHERE state <> 'held'").fetchone()[0] == 0
        assert db.execute("SELECT COUNT(*) FROM operations WHERE kind IN ('turn.start','session/start')").fetchone()[0] == 0
    root = Path(__file__).resolve().parents[1]
    sources = [
        "crates/intercat-daemon/src/pickup.rs", "crates/intercat-daemon/src/pickup_run.rs",
        "crates/intercat-daemon/src/runtime.rs", "crates/intercat-daemon/src/runtime_tests.rs",
        "crates/intercat-daemon/src/journal.rs", "crates/intercat-daemon/src/handoff.rs",
        "crates/intercat-daemon/src/rpc.rs", "crates/intercat-daemon/tests/pickup.rs",
        "crates/intercat-daemon/tests/rpc.rs",
        "electron/pickup.mjs", "electron/pickup.test.mjs", "electron/main.mjs", "electron/preload.cjs",
        "src/bridge.d.ts", "src/desk/PickupButton.tsx", "src/main.tsx", "tests/desktop.spec.ts",
        "tools/verify_pickup_cancel.py",
    ]
    return {
        "rustPassed": sum(int(p) for p, _ in summaries),
        "rustIgnored": sum(int(i) for _, i in summaries),
        "finalRuntimePassed": 1, "finalRpcPassed": 1,
        "desktopPassed": int(passed[1]), "desktopSkipped": int(skipped[1]) if skipped else 0,
        "nodePassed": int(node_passed[1]), "native": native,
        "additionalPaidTurns": 0, "cumulativePaidTurns": 135,
        "sourceSha256": {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources},
    }


if __name__ == "__main__":
    directory = Path(sys.argv[1])
    report = verify(directory)
    (directory / "verification.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({key: value for key, value in report.items() if key != "sourceSha256"}, ensure_ascii=False))
