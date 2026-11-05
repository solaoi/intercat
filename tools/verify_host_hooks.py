"""V48: test project-hook admission with the real host and the offline echo provider."""

import argparse
import json
import os
from pathlib import Path
import queue
import secrets
import signal
import subprocess
import tempfile
import threading
import time
import uuid


def command_id():
    return str(uuid.UUID(int=(int(time.time() * 1000) << 80) | (7 << 76) | (secrets.randbits(12) << 64) | (2 << 62) | secrets.randbits(62)))


class Host:
    def __init__(self, binary, root, config, output, trust):
        env = {k: os.environ[k] for k in ("HOME", "PATH", "LANG") if k in os.environ}
        env.update(XDG_CONFIG_HOME=str(config), XDG_DATA_HOME=str(root / "data"), MUSE_NO_AUTO_UPDATE="1", INTERCAT_HOST="hooks-probe")
        self.stderr = (output / "stderr.txt").open("wb")
        self.log = (output / "received.jsonl").open("w")
        self.process = subprocess.Popen([str(binary), "serve", *( ["--trust-workspace"] if trust else [])],
            cwd=root, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr, start_new_session=True)
        self.frames = queue.Queue()
        self.counter = 0
        self.events = []
        def read():
            try:
                for line in self.process.stdout:
                    self.frames.put(json.loads(line))
            finally:
                self.frames.put(None)
        self.reader = threading.Thread(target=read, daemon=True)
        self.reader.start()

    def send(self, value):
        self.process.stdin.write((json.dumps(value) + "\n").encode())
        self.process.stdin.flush()

    def receive(self, deadline):
        frame = self.frames.get(timeout=max(.01, deadline - time.monotonic()))
        if frame is None:
            raise RuntimeError("host closed stdout")
        self.log.write(json.dumps(frame) + "\n")
        self.log.flush()
        return frame

    def request(self, method, params):
        self.counter += 1
        self.send({"jsonrpc": "2.0", "id": self.counter, "method": method, "params": params})
        deadline = time.monotonic() + 20
        while True:
            frame = self.receive(deadline)
            if frame.get("id") == self.counter:
                if "error" in frame:
                    raise RuntimeError(json.dumps(frame["error"]))
                return frame["result"]
            self.events.append(frame)

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait(timeout=5)
        self.reader.join(timeout=2)
        self.stderr.close()
        self.log.close()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    binary = args.binary.resolve(strict=True)
    results = []
    with tempfile.TemporaryDirectory(prefix="intercat-hooks-probe-") as temporary:
        for case, trust, extension in [("trusted", True, None), ("settings-deny", True, {"allowed_sources": [], "fallback_enabled": False}), ("policy-file", True, None), ("untrusted", False, None)]:
            output = args.output / case
            output.mkdir()
            root = Path(temporary) / case
            root.mkdir()
            config = root / "config"
            muse_config = config / "muse"
            muse_config.mkdir(parents=True)
            marker = root / "hook-ran.txt"
            (root / ".muse").mkdir()
            (root / ".muse/hooks.json").write_text(json.dumps({"hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": f"/usr/bin/touch '{marker}'", "timeout": 5}]}]}}))
            skill = muse_config / "skills" / "intercat-probe"
            skill.mkdir(parents=True)
            (skill / "SKILL.md").write_text("---\nname: intercat-probe\ndescription: Offline probe skill\n---\nReply with PROBE_SKILL.\n")
            settings = {"schema_version": 1, "provider": "echo", "agents": {"execution_capacity": 1}, "run": {"toolset": ["read_file", "search"]}, "permissions": {"schema_version": 1, "default_profile": "intercat-worker", "profiles": {"intercat-worker": {"extends": ":ask-me"}}}}
            if extension is not None:
                settings["extensions"] = {"hooks": extension}
            if case == "policy-file":
                policy = {"schema_version": 1, "extensions": {"hooks": {"allowed_sources": [], "fallback_enabled": False}}}
                (muse_config / "enterprise-policy.json").write_text(json.dumps(policy))
                validation = subprocess.run([str(binary), "config", "validate", "--plane", "policy", "--file", str(muse_config / "enterprise-policy.json")], capture_output=True, text=True)
                (output / "policy-validation.txt").write_text(validation.stdout + validation.stderr)
            (muse_config / "settings.json").write_text(json.dumps(settings))
            (output / "settings.json").write_text(json.dumps(settings, indent=2))
            host = Host(binary, root, config, output, trust)
            try:
                initialized = host.request("initialize", {"clientInfo": {"name": "intercat_hooks_probe", "version": "0.1.0"}, "capabilities": {"userInputDialogs": True}})
                host.send({"jsonrpc": "2.0", "method": "initialized"})
                session = host.request("session/start", {"commandId": command_id(), "workspaceRoot": str(root), "approvalMode": "promptUnmatched", "providerId": "echo"})
                sid = session["session"]["sessionId"]
                skills = host.request("skill/list", {"sessionId": sid})
                turn = host.request("turn/start", {"commandId": command_id(), "sessionId": sid, "input": [{"type": "text", "text": "offline hook probe"}]})
                deadline = time.monotonic() + 20
                while not any(f.get("method") == "turn/completed" for f in host.events):
                    host.events.append(host.receive(deadline))
                results.append({"case": case, "hookRan": marker.exists(), "profile": session["session"].get("permissionState"), "skills": skills, "turn": turn, "schema": initialized.get("schema")})
            except Exception as error:
                results.append({"case": case, "error": str(error), "hookRan": marker.exists()})
            finally:
                host.close()
            print(json.dumps({k: v for k, v in results[-1].items() if k in {"case", "hookRan", "error"}}), flush=True)
    (args.output / "result.json").write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
