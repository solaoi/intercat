"""Verify saved dummy branch evidence without starting a host or calling a model."""
import json
from pathlib import Path
import sqlite3
import sys


def verify(directory):
    db = sqlite3.connect(f"file:{directory / 'events.sqlite'}?mode=ro", uri=True)
    branches = [json.loads(row[0]) for row in db.execute("SELECT record FROM branches")]
    assert len(branches) == 1
    branch = branches[0]
    assert branch["phase"] == "ready"
    parent, child = branch["parentSessionId"], branch["sessionId"]
    assert parent != child
    assert branch["point"]["kind"] == "turn"
    first = branch["point"]["checkpoint"]["turnId"]
    starts = [(seq, json.loads(raw)) for seq, raw in db.execute("SELECT seq,payload FROM events WHERE kind='intent.turn.start' ORDER BY seq")]
    assert len(starts) == 3
    assert starts[0][1]["payload"]["commandId"] == first
    assert [value["payload"]["sessionId"] for _, value in starts] == [parent, parent, child]
    assert starts[2][1]["payload"]["workspaceRoots"] == [branch["workspace"]["path"]]
    fork = db.execute("SELECT state,payload FROM operations WHERE kind='session/fork'").fetchall()
    assert len(fork) == 1 and fork[0][0] == "complete"
    assert json.loads(fork[0][1])["cutPoint"]["lastTurnId"] == first
    phases = [(seq, json.loads(raw)["phase"]) for seq, raw in db.execute("SELECT seq,payload FROM events WHERE kind='branch.state' ORDER BY seq")]
    assert [phase for _, phase in phases] == ["preparing", "awaiting-receipt", "ready"]
    assert phases[-1][0] < starts[2][0]
    fingerprints = lambda session: db.execute("SELECT name,byte_len,tag FROM secret_fingerprints WHERE session=? ORDER BY name,byte_len,tag", [session]).fetchall()
    assert fingerprints(parent) and fingerprints(parent) == fingerprints(child)
    revision, sealed, digest, sealed_digest = db.execute("SELECT revision,sealed_revision,digest,sealed_digest FROM secret_catalog").fetchone()
    assert revision == sealed and digest == sealed_digest
    items = {}
    body = []
    for raw, in db.execute("SELECT payload FROM events WHERE kind='msp.applied' AND session_id=? ORDER BY seq", [child]):
        body.append(raw)
        frame = json.loads(raw)
        item = frame.get("params", {}).get("item", {})
        if frame.get("method") == "item/completed":
            items[item["itemId"]] = item
    text = "\n".join(body)
    assert "DUMMY_FORK_SECRET_7f29_猫" not in text
    assert "<secret:DEMO_TOKEN>" in text
    assert "LATE_ONLY_TURN" not in text
    assert any(item.get("turnId") == first for item in items.values())
    final_turn = starts[2][1]["payload"]["commandId"]
    tools = [item for item in items.values() if item.get("turnId") == final_turn and item.get("kind") == "toolCall"]
    assert len(tools) == 1 and tools[0]["status"] == "completed"
    assert json.loads(tools[0]["args"])["command"] == "python3 child.py"
    output = json.loads(tools[0]["visibleOutput"])
    assert output["exit_code"] == 0 and output["output"] == "EARLY_TREE_VERIFIED\n"
    proof = {"modelTurns": 3, "sameCutPointForFilesAndHistory": True, "childWorkspace": branch["workspace"]["path"], "maskedInheritedHistory": True, "laterHistoryExcluded": True, "readyBeforeDispatchSequence": phases[-1][0], "childDispatchSequence": starts[2][0]}
    (directory / "execution-verification.json").write_text(json.dumps(proof, indent=2) + "\n")
    return proof


if __name__ == "__main__":
    for argument in sys.argv[1:]:
        print(json.dumps(verify(Path(argument).resolve())))
