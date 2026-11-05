"""Check saved evidence for a Keychain-backed fingerprint reconstruction."""
import argparse
import hashlib
import json
import sqlite3
from pathlib import Path


def verify(directory: Path) -> dict:
    result = json.loads((directory / "result.json").read_text())
    with sqlite3.connect(directory / "events.sqlite") as db:
        state = json.loads(db.execute(
            "SELECT value FROM settings WHERE key='fingerprint-recovery'"
        ).fetchone()[0])
        expected = {row["projectId"]: row["digest"]
                    for row in state["source"]["proof"]["projects"]}
        verified = []
        for project in state["rebuiltProjects"]:
            manifests = [[revision, manifest, list(digest)] for revision, manifest, digest in db.execute(
                "SELECT revision,manifest,digest FROM project_secret_state WHERE project=?", (project,))]
            fingerprints = [[name, length, list(tag)] for name, length, tag in db.execute(
                "SELECT name,byte_len,tag FROM project_fingerprints WHERE project=? ORDER BY name,byte_len,tag", (project,))]
            encoded = json.dumps([project, manifests, fingerprints], ensure_ascii=False, separators=(",", ":")).encode()
            assert hashlib.sha256(encoded).hexdigest() == expected[project]
            verified.append(project)
        current = db.execute("SELECT revision,hex(digest) FROM secret_catalog").fetchone()
        assert state["current"] == {"revision": current[0], "digest": current[1].lower()}
        once = db.execute("SELECT count(*) FROM secret_fingerprints").fetchone()[0]
        models = db.execute("SELECT count(*) FROM operations WHERE kind='turn.start'").fetchone()[0]
        rebuilds = db.execute("SELECT count(*) FROM operations WHERE kind='fingerprints.rebuild'").fetchone()[0]
    assert verified and once == 0 and models == 0 and rebuilds == 1
    assert state["source"]["proof"]["sessions"], "Fixture must retain an unrecoverable one-time witness"
    assert result["receiptUnchanged"] and result["sourceUnchanged"]
    assert result["held"]["fingerprintRecovery"]["historyReviewRequired"]
    assert result["held"]["connected"] is False
    return {"directory": str(directory), "verifiedProjectHashes": verified,
            "oneTimeFingerprints": once, "rebuilds": rebuilds, "modelTurns": models,
            "receiptUnchanged": True, "sourceUnchanged": True, "historyStillBlocked": True}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    print(json.dumps(verify(args.directory.resolve()), indent=2))
