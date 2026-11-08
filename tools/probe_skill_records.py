"""Inspect skill metadata on the pinned, offline echo host using only own files."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time
from unittest.mock import patch

from verify_host_hooks import Host, command_id


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    if hashlib.sha256(binary.read_bytes()).hexdigest() != "1c96114408acbeade9a8fb79e3b4425e5b64d50425938568aef01bdc04ce2e03":
        raise RuntimeError("The verified Muse 1.4.14 binary is required.")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    root = output / "workspace"
    root.mkdir()
    subprocess.run(["/usr/bin/git", "init", "-q", str(root)], check=True,
                   env={"PATH": "/usr/bin:/bin", "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"})
    isolated_home = root / "home"
    isolated_home.mkdir()
    config = root / "config"
    muse = config / "muse"
    muse.mkdir(parents=True)
    (muse / "settings.json").write_text(json.dumps({
        "schema_version": 1, "provider": "echo",
        "agents": {"execution_capacity": 1}, "run": {"toolset": []},
    }))
    names = ["group-demo", "plain-demo", "default-demo"]
    for name, group in zip(names, [True, False, None]):
        skill = muse / "skills" / name
        skill.mkdir(parents=True)
        declaration = "" if group is None else f"intercat.group: {str(group).lower()}\n"
        (skill / "SKILL.md").write_text(
            f"---\nname: {name}\ndescription: Agent-created metadata probe.\n{declaration}"
            f"---\nDummy instructions for {name}.\n"
        )
    with patch.dict(os.environ, {"HOME": str(isolated_home)}):
        host = Host(binary, root, config, output, False)
    result = {"provider": "echo", "paidTurns": 0, "turns": []}
    try:
        host.request("initialize", {"clientInfo": {"name": "intercat_skill_probe", "version": "0.1.0"}, "capabilities": {}})
        host.send({"jsonrpc": "2.0", "method": "initialized"})
        session = host.request("session/start", {"commandId": command_id(), "workspaceRoot": str(root), "providerId": "echo", "approvalMode": "promptUnmatched"})["session"]
        if session["providerId"] != "echo":
            raise RuntimeError("Never run this probe on a paid provider.")
        result["session"] = session
        sid = session["sessionId"]
        catalog = host.request("skill/list", {"sessionId": sid})
        result["catalog"] = [row for row in catalog["skills"] if row["selector"] in names]
        for name in names:
            host.events.clear()
            ack = host.request("turn/start", {"commandId": command_id(), "sessionId": sid, "input": [{"type": "skill", "source": "user", "selector": name, "arguments": "Dummy input."}]})
            pending = list(host.events)
            host.events.clear()
            events = []
            deadline = time.monotonic() + 15
            while True:
                frame = pending.pop(0) if pending else host.receive(deadline)
                events.append(frame)
                if frame.get("method") == "turn/completed" and frame["params"]["turnId"] == ack["turnId"]:
                    break
            result["turns"].append({
                "name": name, "ack": ack,
                "userMessages": [frame["params"]["item"] for frame in events
                    if frame.get("method") == "item/completed" and frame["params"]["item"].get("kind") == "userMessage"],
            })
        source = Path(session["path"]).resolve()
        if not source.is_relative_to(root):
            raise RuntimeError("Unexpected raw log path.")
        rows = [json.loads(line) for line in source.read_text().splitlines()]
        result["skillRecords"] = [row for row in rows if "skill" in json.dumps(row).lower()]
    finally:
        host.close()
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"provider": result["provider"], "paidTurns": 0, "catalog": result["catalog"], "turns": result["turns"]}, indent=2))


if __name__ == "__main__":
    main()
