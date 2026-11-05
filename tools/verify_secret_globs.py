"""Isolate native deny-glob semantics using disposable, paid fixture turns."""
import argparse
import json
import os
from pathlib import Path
import shlex
import time
from verify_host_hooks import Host, command_id


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--case", action="append", required=True,
                        choices=["negated-caret", "positive-class", "alternation"])
    args = parser.parse_args()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    summaries = []
    names = [".env", ".env.", ".env.e", ".env.examp", ".env.example", ".env.exampleX",
             ".env.sample", ".env.template", ".env.dist", ".env.production", ".env.custom",
             ".env.abc", ".env.本番", "plain.txt"]
    cases = {"negated-caret": {"**/.env.[^dest]*": "deny"},
             "positive-class": {"**/.env.[abc]*": "deny"},
             "alternation": {"**/.env.{production,custom}": "deny"}}
    for case in args.case:
        rules = cases[case]
        output = root / case
        output.mkdir()
        config = output / "config"
        muse = config / "muse"
        muse.mkdir(parents=True)
        workspace = output / "workspace"
        workspace.mkdir()
        for directory in [workspace, workspace / "nested/deep"]:
            directory.mkdir(parents=True, exist_ok=True)
            for name in names:
                (directory / name).write_text("DUMMY_ONLY\n")
        script = "import json\nfrom pathlib import Path\nr={}\n"
        script += f"for d in ['', 'nested/deep/', 'new/']:\n Path(d or '.').mkdir(parents=True,exist_ok=True)\n for name in {names!r}:\n  p=d+name;r[p]={{}}\n  for action in ['read','write']:\n   try:\n    if action=='read': Path(p).read_text()\n    else: Path(p).write_text('DUMMY_ONLY\\n')\n    r[p][action]=True\n   except OSError:r[p][action]=False\n"
        script += "Path('results.json').write_text(json.dumps(r)); print(json.dumps(r))\n"
        (workspace / "probe.py").write_text(script)
        settings = {"schema_version": 1, "provider": "meta", "model": "muse-spark-1.3-contributor", "contributor_mode_enabled": False, "agents": {"execution_capacity": 1}, "run": {"toolset": ["bash"]}, "permissions": {"schema_version": 1, "default_profile": "probe", "profiles": {"probe": {"extends": ":ask-me", "filesystem": {":workspace_roots": rules}}}}}
        (muse / "settings.json").write_text(json.dumps(settings))
        auth = muse / "auth.json"
        auth.symlink_to(Path.home() / ".config/muse/auth.json")
        host = Host(args.binary.resolve(), workspace, config, output, False)
        attempted = 0
        try:
            host.request("initialize", {"clientInfo": {"name": "intercat_glob_probe", "version": "0.1.0"}, "capabilities": {"experimentalApi": True}})
            host.send({"jsonrpc": "2.0", "method": "initialized"})
            session = host.request("session/start", {"commandId": command_id(), "workspaceRoot": str(workspace), "approvalMode": "promptUnmatched", "providerId": "meta", "modelId": "muse-spark-1.3-contributor"})["session"]["sessionId"]
            command = shlex.join([os.sys.executable, "probe.py"])
            attempted = 1
            host.request("turn/start", {"commandId": command_id(), "sessionId": session, "reasoningEffort": "minimal", "input": [{"type": "text", "text": "Run exactly this bash command once in this disposable fixture. Do not use other commands, edit, retry, or elevate. Report briefly and finish. " + command}]})
            frames = list(host.events)
            deadline = time.monotonic() + 90
            while not any(f.get("method") == "turn/completed" for f in frames):
                frame = host.receive(deadline)
                frames.append(frame)
                if frame.get("method") == "approval/request":
                    p = frame["params"]
                    raw = json.loads(p.get("rawArgs", "{}"))
                    allowed = p.get("subject", {}).get("command") == command and not p.get("protectedWrite", False) and not raw.get("sandbox_permissions")
                    host.request("approval/decide", {"commandId": command_id(), "sessionId": session, "approvalId": p["approvalId"], "requirementId": p["currentRequirementId"], "choiceId": "allow_once" if allowed else "abort"})
                    frames.extend(host.events)
            result = json.loads((workspace / "results.json").read_text()) if (workspace / "results.json").exists() else None
            summaries.append({"case": case, "attemptedTurns": attempted, "access": result})
        except Exception as error:
            summaries.append({"case": case, "attemptedTurns": attempted, "error": str(error)})
        finally:
            host.close()
            auth.unlink(missing_ok=True)
        (root / "results.json").write_text(json.dumps(summaries, indent=2) + "\n")
        print(json.dumps({k: v for k, v in summaries[-1].items() if k != "access"}), flush=True)


if __name__ == "__main__":
    main()
