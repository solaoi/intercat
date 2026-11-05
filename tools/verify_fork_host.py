"""Offline probe: fork a completed parent turn through an independent Muse host."""
import argparse
import json
from pathlib import Path
import time

from verify_host_hooks import Host, command_id


def connect(binary, workspace, config, output):
    host = Host(binary, workspace, config, output, False)
    host.request("initialize", {"clientInfo": {"name": "intercat_fork_probe", "version": "0.1.0"}, "capabilities": {"experimentalApi": True}})
    host.send({"jsonrpc": "2.0", "method": "initialized"})
    return host


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    workspace = root / "workspace"
    workspace.mkdir()
    config = root / "config"
    (config / "muse").mkdir(parents=True)
    (config / "muse/settings.json").write_text(json.dumps({"schema_version": 1, "provider": "echo", "agents": {"execution_capacity": 1}, "run": {"toolset": ["read_file"]}, "permissions": {"schema_version": 1, "default_profile": "intercat-worker", "profiles": {"intercat-worker": {"extends": ":ask-me"}}}}))
    hosts = []
    result = {"paidTurns": 0}
    try:
        for name in ["parent", "child"]:
            output = root / name
            output.mkdir()
            hosts.append(connect(args.binary.resolve(), workspace, config, output))
        parent, child = hosts
        started = parent.request("session/start", {"commandId": command_id(), "workspaceRoot": str(workspace), "providerId": "echo", "approvalMode": "promptUnmatched"})
        session = started["session"]["sessionId"]
        turn = parent.request("turn/start", {"commandId": command_id(), "sessionId": session, "input": [{"type": "text", "text": "DUMMY_OFFLINE_FORK_MARKER"}]})["turnId"]
        frames = list(parent.events)
        while not any(frame.get("method") == "turn/completed" and frame["params"]["turnId"] == turn for frame in frames):
            frames.append(parent.receive(time.monotonic() + 20))
        result.update(parent=session, turn=turn)
        fork_request = {"commandId": command_id(), "sessionId": session, "cutPoint": {"lastTurnId": turn}, "excludeItems": True}
        fork = child.request("session/fork", fork_request)
        result["fork"] = fork
        state = parent.request("session/read", {"sessionId": session})
        result["parentReadableWhileChildOwnsFork"] = state["session"]["sessionId"] == session
        result["differentSession"] = fork["session"]["sessionId"] != session
        child.close()
        hosts.remove(child)
        output = root / "replay"
        output.mkdir()
        replay = connect(args.binary.resolve(), workspace, config, output)
        hosts.append(replay)
        retried = replay.request("session/fork", fork_request)
        result["replaySameSession"] = retried["session"]["sessionId"] == fork["session"]["sessionId"]
        # A replay must own the child, not only return the old result envelope.
        result["replayCanRelease"] = replay.request("session/leave", {"commandId": command_id(), "sessionId": fork["session"]["sessionId"], "stop": True})["status"] == "accepted"
        result["verified"] = all(result[key] for key in ["parentReadableWhileChildOwnsFork", "differentSession", "replaySameSession", "replayCanRelease"])
    except Exception as error:
        result["error"] = str(error)
        result["verified"] = False
    finally:
        for host in reversed(hosts):
            host.close()
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: value for key, value in result.items() if key != "fork"}))


if __name__ == "__main__":
    main()
