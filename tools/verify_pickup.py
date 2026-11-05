"""Verify file-recovery evidence without opening a worker or contacting Muse."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys


def open_db(path):
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


def verify(directory):
    rust = (directory / "rust.log").read_text()
    summaries = re.findall(r"test result: ok\. (\d+) passed; 0 failed; (\d+) ignored", rust)
    assert summaries and "test result: FAILED" not in rust
    final_domain = (directory / "domain-final.log").read_text()
    for name in (
        "recovery_waits_for_stop_and_receipt_then_starts_only_one_new_handoff",
        "a_pending_recovery_survives_restart_without_changing_its_session_or_checkpoint",
        "activation_rejects_a_changed_workspace_without_creating_a_handoff",
        "oversized_context_is_not_truncated_and_leaves_the_workspace_available",
        "handoff_includes_tools_saved_while_the_old_worker_was_stopping",
        "a_checkpoint_keeps_its_original_session_after_file_recovery",
        "old_report_questions_are_carried_forward_without_blocking_the_new_session",
    ):
        assert f"{name} ... ok" in final_domain, name
    desktop = (directory / "desktop-final.log").read_text()
    passed = re.search(r"(\d+) passed \(", desktop)
    assert passed and not re.search(r"^\s+\d+ failed\b", desktop, re.MULTILINE)
    results = list((directory / "native-final").glob("*/result.json"))
    assert len(results) == 1
    result = json.loads(results[0].read_text())
    assert result["newSession"] != result["previousSession"]
    assert result["modelTurns"] == 1 and result["unfinishedEditPreserved"]
    assert result["indexUnchanged"] and result["sourceUnchanged"]
    db = open_db(results[0].with_name("events.sqlite"))
    recoveries = db.execute("SELECT payload,state FROM operations WHERE kind='worker.pickup'").fetchall()
    assert len(recoveries) == 1 and recoveries[0][1] == "complete"
    recovery = json.loads(recoveries[0][0])
    assert recovery["phase"] == "ready" and recovery["sessionId"] == result["newSession"]
    assert recovery["snapshot"]["sha"] == result["snapshot"]["sha"]
    assert recovery["snapshot"]["reference"] != recovery["previous"]["checkpoint"]["reference"]
    assert recovery["snapshot"]["reference"].endswith("-recovery/" + recovery["request"]["opId"])
    operations = dict(db.execute("SELECT kind,COUNT(*) FROM operations GROUP BY kind"))
    assert operations["turn.start"] == operations["session/start"] == 1
    inputs = db.execute("SELECT text,state FROM utterances").fetchall()
    assert len(inputs) == 2
    assert sum(state == "held" for _, state in inputs) == 1
    handoff = [text for text, state in inputs if state == "landed"]
    assert len(handoff) == 1 and "files may be ahead of the last checkpoint" in handoff[0]
    assert "Do not change files" in handoff[0] and recovery["snapshot"]["sha"] in handoff[0]
    checkpoints = [json.loads(row[0])["checkpoint"] for row in db.execute("SELECT payload FROM operations WHERE kind='checkpoint.save'")]
    assert len(checkpoints) == 2
    assert recovery["previous"]["checkpoint"] in checkpoints
    assert all(checkpoint["sha"] != recovery["snapshot"]["sha"] for checkpoint in checkpoints)
    db.close()
    paid = 0
    for parent in ("native", "native-final"):
        for path in (directory / parent).glob("*/events.sqlite"):
            db = open_db(path)
            paid += db.execute("SELECT COUNT(*) FROM operations WHERE kind='turn.start'").fetchone()[0]
            db.close()
    root = Path(__file__).resolve().parents[1]
    sources = [
        "crates/intercat-daemon/src/pickup.rs", "crates/intercat-daemon/src/pickup_run.rs",
        "crates/intercat-daemon/src/runtime.rs", "crates/intercat-daemon/src/journal.rs",
        "crates/intercat-daemon/src/rpc.rs", "crates/intercat-daemon/src/branch.rs",
        "crates/intercat-daemon/src/report_store.rs",
        "crates/intercat-daemon/src/runtime_tests.rs", "crates/intercat-daemon/tests/pickup.rs",
        "crates/intercat-daemon/tests/branch.rs", "src/desk/PickupButton.tsx",
        "src/main.tsx", "src/bridge.d.ts", "electron/main.mjs", "electron/preload.cjs",
        "electron/pickup.mjs", "electron/pickup.test.mjs", "tests/desktop.spec.ts",
        "tools/verify_pickup.py",
    ]
    return {
        "rustPassed": sum(int(p) for p, _ in summaries),
        "rustIgnored": sum(int(i) for _, i in summaries),
        "desktopPassed": int(passed[1]),
        "nativeModelTurns": result["modelTurns"], "additionalPaidTurns": paid,
        "cumulativePaidTurns": 122 + paid,
        "oneHandoff": True, "oldInputHeld": True, "separateSnapshot": True,
        "newSession": True, "sourceAndIndexPreserved": True,
        "sourceSha256": {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in sources},
    }


if __name__ == "__main__":
    output = Path(sys.argv[1])
    report = verify(output)
    (output / "verification.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k != "sourceSha256"}, ensure_ascii=False))
