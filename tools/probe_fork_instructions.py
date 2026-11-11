"""V51: bounded paid fork tests; only generated instructions reach Meta."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import time

from verify_host_hooks import Host, command_id

PINNED_SHA = "1c96114408acbeade9a8fb79e3b4425e5b64d50425938568aef01bdc04ce2e03"
PROMPT = ("This is a disposable instruction-loading test. Follow the current project instruction's "
          "exact reply token. If there is no project instruction specifying a reply token, reply exactly "
          "DUMMY_NO_RULE. Do not use tools, inspect files, or change anything. Do not copy a previous answer.")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--auth", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--trust-workspace", action="store_true")
    parser.add_argument("--start", choices=["fork", "fresh"], default="fork")
    parser.add_argument("--explicit-instructions", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == PINNED_SHA
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    config = output / "config"
    muse = config / "muse"
    muse.mkdir(parents=True)
    auth = muse / "auth.json"
    auth.symlink_to(args.auth.resolve(strict=True))
    (muse / "settings.json").write_text(json.dumps({
        "schema_version": 1, "provider": "meta", "model": "muse-spark-1.3-contributor",
        "contributor_mode_enabled": False, "agents": {"execution_capacity": 1},
        "run": {"toolset": []},
        "permissions": {"schema_version": 1, "default_profile": "intercat-worker",
                        "profiles": {"intercat-worker": {"extends": ":ask-me"}}},
    }))
    result = {"probe": "V51", "trustWorkspace": args.trust_workspace, "start": args.start,
              "explicitInstructions": args.explicit_instructions,
              "attemptedTurns": 0, "cases": [], "verified": False}
    hosts = []

    def connect(workspace, folder):
        folder.mkdir()
        host = Host(binary, workspace, config, folder, args.trust_workspace)
        hosts.append(host)
        initialized = host.request("initialize", {"clientInfo": {"name": "intercat_fork_rules_probe", "version": "1"}, "capabilities": {}})
        assert initialized["schema"]["fingerprint"] == "sha256:c94c3d1f2f51e6424074b9ec43eac3eedcf532d3dcd77351ec1383a87d3049c6"
        host.send({"jsonrpc": "2.0", "method": "initialized"})
        return host

    def turn(host, session, root):
        host.events.clear()
        result["attemptedTurns"] += 1
        (output / "verification.json").write_text(json.dumps(result, indent=2) + "\n")
        prompt = PROMPT
        if args.explicit_instructions:
            selected = next((root / name for name in ["AGENTS.md", "CLAUDE.md", ".agents/AGENTS.md", ".claude/CLAUDE.md"]
                             if (root / name).is_file()), None)
            if selected:
                prompt = ("The following snapshot contains the current project instructions. It replaces "
                          "previous project instructions, including any inherited from an older turn.\n"
                          f"<current-project-instructions>\n{selected.read_text()}</current-project-instructions>\n"
                          "Task: Follow those current instructions and reply with their exact reply token only. "
                          "Do not use tools or inspect or change files.")
            else:
                prompt = ("There are no current project instruction files. This replaces previous project "
                          "instructions, including any inherited from an older turn. For this disposable test, "
                          "reply exactly DUMMY_NO_RULE. Do not use tools or inspect or change files.")
        started = host.request("turn/start", {"commandId": command_id(), "sessionId": session,
                         "workspaceRoots": [str(root)], "reasoningEffort": "minimal",
                         "input": [{"type": "text", "text": prompt}]})
        frames = list(host.events)
        deadline = time.monotonic() + 90
        while not any(f.get("method") == "turn/completed" and f["params"]["turnId"] == started["turnId"] for f in frames):
            frame = host.receive(deadline)
            if frame.get("method") in ["approval/request", "userInput/request"]:
                raise RuntimeError("This no-tool probe unexpectedly requested user action.")
            frames.append(frame)
        terminal = next(f["params"] for f in frames if f.get("method") == "turn/completed"
                        and f["params"]["turnId"] == started["turnId"])
        if terminal["terminal"] != "completed":
            raise RuntimeError({"terminal": terminal["terminal"], "reason": terminal.get("reason")})
        items = [f["params"]["item"] for f in frames if f.get("method") == "item/completed"
                 and f["params"]["item"].get("kind") == "agentMessage"
                 and f["params"]["item"].get("turnId") == started["turnId"]]
        text = "\n".join(item.get("text", "") for item in items)
        if not text:
            raise RuntimeError("The host returned no agent message text.")
        return started["turnId"], text.strip()

    try:
        for mode in ["changed", "renamed", "removed"]:
            case = output / mode
            case.mkdir()
            shared = case / "shared-data"
            shared.mkdir()
            parent, child = case / "parent-workspace", case / "child-workspace"
            for root in [parent, child]:
                root.mkdir()
                subprocess.run(["/usr/bin/git", "init", "-q", str(root)], check=True,
                               env={"PATH": "/usr/bin:/bin", "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"})
                (root / "data").symlink_to(shared, target_is_directory=True)
            instruction = "For this disposable test, every reply must be exactly DUMMY_OLD_RULE.\n"
            (parent / "AGENTS.md").write_text(instruction)
            original = connect(parent, case / "parent-host")
            session = original.request("session/start", {"commandId": command_id(), "workspaceRoot": str(parent),
                        "providerId": "meta", "approvalMode": "promptUnmatched"})["session"]["sessionId"]
            cut, old = turn(original, session, parent)
            assert old == "DUMMY_OLD_RULE", old
            expected = "DUMMY_NO_RULE" if mode == "removed" else "DUMMY_NEW_RULE"
            if mode != "removed":
                (child / ("CLAUDE.md" if mode == "renamed" else "AGENTS.md")).write_text(
                    "For this disposable test, every reply must be exactly DUMMY_NEW_RULE.\n")
            fork_host = connect(child, case / "child-host")
            if args.start == "fork":
                fork = fork_host.request("session/fork", {"commandId": command_id(), "sessionId": session,
                                            "cutPoint": {"lastTurnId": cut}, "excludeItems": True})["session"]
            else:
                fork = fork_host.request("session/start", {"commandId": command_id(), "workspaceRoot": str(child),
                                "providerId": "meta", "approvalMode": "promptUnmatched"})["session"]
            _, current = turn(fork_host, fork["sessionId"], child)
            row = {"mode": mode, "parent": old, "child": current, "expected": expected,
                   "matches": current == expected}
            result["cases"].append(row)
            print(json.dumps(row), flush=True)
            for host in reversed(hosts):
                host.close()
            hosts.clear()
        result["verified"] = all(row["matches"] for row in result["cases"])
    except Exception as error:
        result["error"] = str(error)
    finally:
        for host in reversed(hosts):
            host.close()
        auth.unlink(missing_ok=True)
        (output / "verification.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result), flush=True)
    raise SystemExit(0 if result["verified"] else 1)


if __name__ == "__main__":
    main()
