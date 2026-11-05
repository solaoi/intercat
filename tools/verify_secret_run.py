#!/usr/bin/env python3
"""Check recorded native secret tests without another model call or secret value."""
import json
import sqlite3
import sys
from pathlib import Path


def verify(directory: Path) -> dict:
    result = json.loads((directory / "result.json").read_text())
    database = sqlite3.connect(f"file:{directory / 'events.sqlite'}?mode=ro", uri=True)
    completed = {}
    questions = {}
    for (payload,) in database.execute("SELECT payload FROM events WHERE kind='msp.applied'"):
        frame = json.loads(payload)
        if frame.get("method") == "userInput/settled":
            params = frame["params"]
            questions[params["userInputId"]] = params
        item = frame.get("params", {}).get("item", {})
        if item.get("kind") == "toolCall" and item.get("status") == "completed":
            if item.get("itemId") not in completed or item.get("revision", 0) > completed[item["itemId"]].get("revision", 0):
                completed[item["itemId"]] = item
    expected = ["python3 verify_token.py"]
    if result.get("removedValueAbsent"):
        expected = ["/usr/bin/python3 verify_token.py", "/usr/bin/python3 verify_absent.py"]
    if result.get("savedToProject"):
        expected.extend(["python3 verify_token.py", "python3 verify_absent.py"])
    if result.get("notReinjected"):
        expected.append("python3 verify_absence.py")
    commands = []
    for item in completed.values():
        if result.get("mspQuestion") and item.get("tool") == "request_user_input":
            continue
        assert item.get("tool") == "bash", "An unexpected tool ran in the fixture"
        command = json.loads(item["args"])["command"]
        assert command in expected, "An unexpected command ran in the fixture"
        output = json.loads(item["visibleOutput"])
        assert output["exit_code"] == 0 and output["terminal_status"] == "completed"
        masked = "<secret:DEMO_TOKEN>" in output["output"]
        if command.endswith(" verify_token.py"):
            assert masked, "The deliberate token output was not masked"
        commands.append({"command": command, "itemId": item["itemId"], "turnId": item["turnId"], "exitCode": 0, "masked": masked})
    assert sorted(item["command"] for item in commands) == sorted(expected), "A fixture command is missing or repeated"
    proof = {"commands": commands, "eachExpectedCommandRanOnce": True, "rawOutputCopied": False}
    if result.get("mspQuestion"):
        assert len(questions) == 1, "Expected exactly one settled repair question"
        question = next(iter(questions.values()))
        assert question["outcome"] == "interrupted" and not question["answers"]
        assert question.get("clarification") is None
        proof["questionInterruptedWithoutAnswer"] = True
        target = json.loads(database.execute("SELECT record FROM secret_inputs").fetchone()[0])
        assert target["question"]["id"] == question["userInputId"]
        positions = {}
        for seq, kind, payload in database.execute("SELECT seq,kind,payload FROM events ORDER BY seq"):
            frame = json.loads(payload)
            params = frame.get("params", {})
            if kind == "msp.applied" and frame.get("method") == "userInput/settled" and params.get("userInputId") == question["userInputId"]:
                positions.setdefault("questionSettled", seq)
            if kind == "msp.applied" and frame.get("method") == "turn/completed" and params.get("turnId") == target["turnId"]:
                positions.setdefault("turnEnded", seq)
            if kind == "secret.settled" and frame.get("opId") == target["opId"]:
                positions["hostClosed"] = seq
            if kind == "repair.secret-queued" and frame.get("secretInput") == target["opId"]:
                positions["checkpointSaved"] = seq
            if kind == "secret.ready" and frame.get("opId") == target["opId"]:
                positions["newHostReady"] = seq
        assert max(positions["questionSettled"], positions["turnEnded"]) < positions["hostClosed"] < positions["checkpointSaved"] < positions["newHostReady"]
        proof["confirmedBeforeRestart"] = positions
    (directory / "execution-verification.json").write_text(json.dumps(proof, indent=2) + "\n")
    return proof


if __name__ == "__main__":
    for name in sys.argv[1:]:
        path = Path(name).resolve()
        verify(path)
        print(f"Verified {path}")
