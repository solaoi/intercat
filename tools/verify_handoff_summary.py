"""Audit the saved summary and recovery evidence without contacting Muse."""
import hashlib
import json
from pathlib import Path
import re
import sqlite3
import sys


def database(path):
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


def only_result(directory):
    files = list(directory.glob("*/result.json"))
    assert len(files) == 1, directory
    return files[0], json.loads(files[0].read_text())


def verify(directory):
    path, native = only_result(directory / "native-verified")
    summary = native["summary"]
    assert set(summary) == {"purpose", "constraints", "unresolved", "changedFiles", "completed", "unverified", "nextChecks", "sourceAddresses"}
    assert native["completedTurnReused"] and native["noFileChanges"] and native["knownSecretAbsentFromTasks"]
    assert native["profile"] == ":read-only"
    assert "GREEN_ONLY" in " ".join(summary["constraints"])
    assert not any("BLUE_OLD" in text for text in summary["unresolved"])
    assert not any("JSON" in text or "KiB" in text for text in summary["constraints"])
    assert summary["completed"] == [] and summary["changedFiles"] == [] and summary["unverified"]
    db = database(path.with_name("events.sqlite"))
    tasks = [json.loads(row[0]) for row in db.execute("SELECT payload FROM operations WHERE kind='handoff.summary'")]
    assert len(tasks) == native["summaryTurns"] >= 4
    assert len({task["sessionId"] for task in tasks}) == len(tasks)
    assert max(task["level"] for task in tasks) >= 1
    assert all(task["state"] == "complete" and len(task["input"].encode()) <= 64 * 1024 for task in tasks)
    assert all("DUMMY_SUMMARY_PRIVATE_VALUE" not in task["input"] for task in tasks)
    final_id = max(tasks, key=lambda task: task["level"])["id"]
    assert db.execute("SELECT COUNT(*) FROM events WHERE kind='handoff.summary' AND json_extract(payload,'$.id')=?1 AND json_extract(payload,'$.state')='complete'", [final_id]).fetchone()[0] == 2
    db.close()
    pickup_path, pickup = only_result(directory / "pickup")
    assert pickup["summaryTurns"] == pickup["modelTurns"] == 1
    assert pickup["unfinishedEditPreserved"] and pickup["indexUnchanged"] and pickup["sourceUnchanged"]
    db = database(pickup_path.with_name("events.sqlite"))
    recovery = json.loads(db.execute("SELECT payload FROM operations WHERE kind='worker.pickup'").fetchone()[0])
    assert any("unfinished.txt" in name for name in recovery["context"]["summary"]["changedFiles"])
    assert recovery["phase"] == "ready" and recovery["sessionId"] != recovery["request"]["sessionId"]
    assert db.execute("SELECT COUNT(*) FROM utterances WHERE state='held'").fetchone()[0] == 1
    db.close()
    paid = 0
    for folder in ("native", "native-final", "native-verified", "pickup"):
        for path in (directory / folder).glob("*/events.sqlite"):
            db = database(path)
            paid += db.execute("SELECT COUNT(*) FROM operations WHERE kind='turn.start'").fetchone()[0]
            paid += db.execute("SELECT COUNT(*) FROM operations WHERE kind='handoff.summary' AND json_extract(payload,'$.state') IN ('running','complete','failed')").fetchone()[0]
            db.close()
    rust = (directory / "rust.log").read_text()
    counts = re.findall(r"test result: ok\. (\d+) passed; 0 failed; (\d+) ignored", rust)
    assert counts and "test result: FAILED" not in rust
    desktop = (directory / "desktop.log").read_text()
    passed = re.search(r"(\d+) passed \(", desktop)
    assert passed and not re.search(r"^\s+\d+ failed\b", desktop, re.MULTILINE)
    root = Path(__file__).resolve().parents[1]
    source_names = [
        "crates/intercat-daemon/src/handoff.rs", "crates/intercat-daemon/src/handoff_run.rs",
        "crates/intercat-daemon/src/host.rs", "crates/intercat-daemon/src/pickup.rs",
        "crates/intercat-daemon/src/pickup_run.rs", "crates/intercat-daemon/src/journal.rs",
        "crates/intercat-daemon/src/runtime.rs", "crates/intercat-daemon/src/runtime_tests.rs",
        "crates/intercat-daemon/tests/handoff.rs", "crates/intercat-daemon/tests/pickup.rs",
        "src/bridge.d.ts", "src/desk/PickupButton.tsx", "tools/verify_handoff_summary.py",
    ]
    return {
        "rustPassed": sum(int(p) for p, _ in counts), "rustIgnored": sum(int(i) for _, i in counts),
        "desktopPassed": int(passed[1]), "nativeSummaryTurns": native["summaryTurns"],
        "latestCorrectionRetained": True, "noInventedCompletion": True, "completedTurnReused": True,
        "toolFreeReadOnlyProfile": True, "pickupSummaryTurns": 1, "pickupWorkerTurns": 1,
        "additionalPaidTurns": paid, "cumulativePaidTurns": 124 + paid,
        "sourceSha256": {name: hashlib.sha256((root / name).read_bytes()).hexdigest() for name in source_names},
    }


if __name__ == "__main__":
    directory = Path(sys.argv[1])
    report = verify(directory)
    (directory / "verification.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({k: v for k, v in report.items() if k != "sourceSha256"}, ensure_ascii=False))
