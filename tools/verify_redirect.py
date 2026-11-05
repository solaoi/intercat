"""Check an existing dummy redirect recording; never invokes a model."""
import json
from pathlib import Path
import sqlite3
import sys


def verify(directory):
    database = sqlite3.connect(f"file:{directory / 'events.sqlite'}?mode=ro", uri=True)
    redirects = [json.loads(row[0]) for row in database.execute("SELECT record FROM redirects")]
    assert len(redirects) == 1
    redirect = redirects[0]
    assert redirect["phase"] == "done"
    assert database.execute("SELECT state FROM utterances WHERE id=?", [redirect["inputId"]]).fetchone()[0] == "landed"
    starts = [(seq, json.loads(payload)) for seq, payload in database.execute("SELECT seq,payload FROM events WHERE kind='intent.turn.start' ORDER BY seq")]
    assert len(starts) == 2
    assert starts[1][1]["inputId"] == redirect["inputId"]
    assert starts[0][1]["payload"]["commandId"] == redirect["turnId"]
    assert all(start[1]["payload"]["sessionId"] == redirect["sessionId"] for start in starts)
    checkpoint = database.execute("SELECT min(seq) FROM events WHERE kind='worker.state' AND json_extract(payload,'$.checkpoint.turnId')=? AND json_extract(payload,'$.checkpointPending')=0", [redirect["turnId"]]).fetchone()[0]
    assert checkpoint < starts[1][0]
    assert database.execute("SELECT count(*) FROM operations WHERE kind='checkpoint.save' AND state='complete'").fetchone()[0] == 2
    tools = {}
    terminals = {}
    for raw, in database.execute("SELECT payload FROM events WHERE kind='msp.applied' ORDER BY seq"):
        frame = json.loads(raw)
        params = frame.get("params", {})
        if params.get("sessionId") != redirect["sessionId"]:
            continue
        if frame.get("method") == "turn/completed":
            terminals[params["turnId"]] = params["terminal"]
        item = params.get("item", {})
        if frame.get("method") == "item/completed" and item.get("kind") == "toolCall" and item.get("tool") == "bash":
            tools[item["itemId"]] = item
    assert terminals[redirect["turnId"]] == "cancelled"
    assert terminals[starts[1][1]["payload"]["commandId"]] == "completed"
    assert len(tools) == 2
    commands = {json.loads(item["args"])["command"]: item for item in tools.values()}
    assert set(commands) == {"python3 slow.py", "python3 finish.py"}
    assert commands["python3 slow.py"]["status"] == "cancelled"
    completed = json.loads(commands["python3 finish.py"]["visibleOutput"])
    assert completed["exit_code"] == 0
    assert completed["output"] == "correction verified\n"
    proof = {"sameSession": True, "cancelledOriginalTurn": True, "correctionExecutedOnce": True, "checkpointSequence": checkpoint, "correctionStartSequence": starts[1][0], "modelTurns": len(starts)}
    (directory / "execution-verification.json").write_text(json.dumps(proof, indent=2) + "\n")
    return proof


if __name__ == "__main__":
    for argument in sys.argv[1:]:
        print(json.dumps(verify(Path(argument).resolve())))
