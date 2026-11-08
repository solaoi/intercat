"""Observe owned user hooks in untrusted workspaces using only generated files.

Catalog mode uses the offline echo provider. Tools mode uses at most one paid
dummy turn and requires the explicitly supplied pinned Muse binary and auth.
Raw hook and MSP frames remain in the requested private evidence directory.
"""
import argparse
import hashlib
import json
from pathlib import Path
import shlex
import sys
import time

from verify_host_hooks import Host, command_id

PINNED = "1c96114408acbeade9a8fb79e3b4425e5b64d50425938568aef01bdc04ce2e03"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--mode", choices=["catalog", "tools"], default="catalog")
    parser.add_argument("--placement", choices=["file", "settings"], default="file")
    parser.add_argument("--scenario", choices=["sequential", "failure", "parallel"], default="sequential")
    parser.add_argument("--auth", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    if hashlib.sha256(binary.read_bytes()).hexdigest() != PINNED:
        raise ValueError("A pinned Muse binary is required.")
    if args.mode == "tools" and args.auth is None:
        raise ValueError("The paid tools probe needs the explicit auth path.")
    allowed_commands = {
        "sequential": ["printf ONE > one.txt; : > empty.txt; printf BEFORE > shared.txt", "printf TWO > two.txt; printf AFTER > shared.txt"],
        "failure": ["printf PARTIAL > shared.txt; : > empty.txt; exit 7", "printf AFTER_FAILURE > two.txt"],
        "parallel": ["printf ONE > one.txt", "printf TWO > two.txt"],
    }[args.scenario]
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    (output / "probe-source.py").write_bytes(Path(__file__).read_bytes())
    root = output / "workspace"
    root.mkdir(mode=0o700)
    config = output / "config"
    own = config / "muse"
    own.mkdir(parents=True, mode=0o700)
    if args.auth:
        (own / "auth.json").symlink_to(args.auth.resolve(strict=True))
    hook_script = output / "owned_hook.py"
    hook_script.write_text('''import json, pathlib, sys, time
root, output = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
payload = json.load(sys.stdin)
event = sys.argv[3]
if event == "PostToolBatch":
    time.sleep(0.25)
record = {"event":event, "timeNs":time.monotonic_ns(), "wallNs":time.time_ns(), "payload":payload,
          "files":{name:(root/name).read_text() if (root/name).is_file() else None
                   for name in ["one.txt", "two.txt", "empty.txt", "shared.txt"]}}
with output.open("a") as stream:
    stream.write(json.dumps(record)+"\\n")
''')
    hooks = {event: [{"hooks": [{"type": "command", "command": shlex.join([
        sys.executable, str(hook_script), str(root), str(output / "hook-events.jsonl"), event]),
        "timeout": 10}]}] for event in ["SessionStart", "PreLLMCall", "PostLLMCall", "PreToolUse", "PostToolUse", "PostToolUseFailure", "PostToolBatch"]}
    settings = {"schema_version": 1, "provider": "echo" if args.mode == "catalog" else "meta",
                "agents": {"execution_capacity": 1}, "run": {"toolset": ["bash"]},
                "permissions": {"schema_version": 1, "default_profile": "intercat-worker",
                                "profiles": {"intercat-worker": {"extends": ":ask-me"}}}}
    if args.mode == "tools":
        settings.update(model="muse-spark-1.3-contributor", contributor_mode_enabled=False)
    if args.placement == "file":
        (own / "hooks.json").write_text(json.dumps({"hooks": hooks}))
    else:
        settings["hooks"] = hooks
    (own / "settings.json").write_text(json.dumps(settings))
    (root / ".muse").mkdir()
    (root / ".muse/hooks.json").write_text(json.dumps({"hooks": {"SessionStart": [{"hooks": [{
        "type": "command", "command": shlex.join(["/usr/bin/touch", str(output / "project-hook-ran")]), "timeout": 5}]}]}}))
    host = Host(binary, root, config, output, False)
    result = {"mode": args.mode, "placement": args.placement, "scenario":args.scenario, "paidTurns": 0}
    try:
        host.request("initialize", {"clientInfo": {"name": "intercat_clip_probe", "version": "0.1.0"},
                                    "capabilities": {"userInputDialogs": True}})
        host.send({"jsonrpc": "2.0", "method": "initialized"})
        started = host.request("session/start", {"commandId": command_id(), "workspaceRoot": str(root),
                                                "providerId": settings["provider"], "approvalMode": "promptUnmatched"})
        sid = started["session"]["sessionId"]
        result["sessionId"] = sid
        result["catalog"] = host.request("hook/list", {"sessionId": sid})
        text = "Offline owned-hook observation. Reply DONE."
        if args.mode == "tools":
            text = ("This is a disposable file-boundary test. Do exactly two sequential bash calls. "
                    "First run: printf ONE > one.txt; : > empty.txt; printf BEFORE > shared.txt. "
                    "After that call has completed, run: printf TWO > two.txt; printf AFTER > shared.txt. "
                    "Do not combine the calls and do not use other tools. Then reply DONE.")
            if args.scenario == "failure":
                text = ("This is a disposable file-boundary test. Do exactly two sequential bash calls. "
                        "First run exactly: printf PARTIAL > shared.txt; : > empty.txt; exit 7. "
                        "Exit 7 is intentional and must not be retried. After that failure, run exactly: "
                        "printf AFTER_FAILURE > two.txt. Do not use other tools. Then reply DONE.")
            elif args.scenario == "parallel":
                text = ("This is a disposable file-boundary test. In ONE tool-call batch, issue exactly TWO bash calls: "
                        "first command: printf ONE > one.txt; second command: printf TWO > two.txt. "
                        "Request both calls in the same model response, not one after another. "
                        "They do not share a file. Do not use other tools. Then reply DONE.")
            result["paidTurns"] = 1
            (output / "result.json").write_text(json.dumps(result, indent=2))
        turn = host.request("turn/start", {"commandId": command_id(), "sessionId": sid,
                                           "reasoningEffort": "minimal", "input": [{"type": "text", "text": text}]})
        result["turnId"] = turn["turnId"]
        deadline, cursor, handled = time.monotonic() + 90, 0, set()
        while True:
            if cursor == len(host.events):
                host.events.append(host.receive(deadline))
            frame = host.events[cursor]
            cursor += 1
            params = frame.get("params", {})
            if frame.get("method") in ["approval/request", "approval/requested"]:
                key = (params["approvalId"], json.dumps(params["currentRequirementId"], sort_keys=True))
                if key in handled:
                    continue
                handled.add(key)
                subject = params.get("subject", {})
                allow = subject.get("kind") == "shell" and subject.get("workspaceRoot") == str(root) and subject.get("command") in allowed_commands
                choice = next(choice for choice in params["availableChoices"] if choice["scope"] == "once" and choice["decision"] == ("approved" if allow else "abort"))
                host.request("approval/decide", {"commandId": command_id(), "sessionId": sid,
                    "approvalId": params["approvalId"], "requirementId": params["currentRequirementId"], "choiceId": choice["choiceId"]})
                result.setdefault("approvals", []).append({"allowed":allow, "subject":subject})
            if frame.get("method") == "userInput/request":
                raise RuntimeError("The file-boundary probe unexpectedly asked a question.")
            if frame.get("method") == "turn/completed" and params.get("sessionId") == sid and params.get("turnId") == result["turnId"]:
                result["terminal"] = params
                break
        result["projectHookRan"] = (output / "project-hook-ran").exists()
        events = output / "hook-events.jsonl"
        result["ownedEvents"] = [json.loads(line) for line in events.read_text().splitlines()] if events.exists() else []
    finally:
        host.close()
        (own / "auth.json").unlink(missing_ok=True)
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"mode": args.mode, "placement": args.placement, "events": len(result.get("ownedEvents", [])),
                      "hooks": len(result.get("catalog", {}).get("hooks", [])),
                      "projectHookRan": result.get("projectHookRan"), "paidTurns": result["paidTurns"]}))


if __name__ == "__main__":
    main()
