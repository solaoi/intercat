"""V55: one paid dummy turn tests a private Unix socket, never a real daemon."""
import argparse
import json
from pathlib import Path
import shlex
import socket
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--muse", type=Path, required=True)
    parser.add_argument("--daemon", type=Path, required=True)
    args = parser.parse_args()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    root.chmod(0o700)
    private = root / "run"
    private.mkdir(mode=0o700)
    repository = root / "source-repo"
    repository.mkdir()
    script = '''import json, socket, sys
from pathlib import Path
s = socket.socket(socket.AF_UNIX)
try:
    s.connect(sys.argv[1])
    result = {"connected": True}
except OSError as error:
    result = {"connected": False, "errno": error.errno}
finally:
    s.close()
Path("socket-result.json").write_text(json.dumps(result))
print(json.dumps(result))
'''
    (repository / "probe.py").write_text(script)
    for cmd in [["git", "init", "--quiet", "-b", "main"], ["git", "add", "."], ["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "-m", "fixture"]]:
        subprocess.run(cmd, cwd=repository, check=True)
    server = socket.socket(socket.AF_UNIX)
    endpoint = private / "rpc.sock"
    server.bind(str(endpoint))
    endpoint.chmod(0o600)
    server.listen()
    server.setblocking(False)
    command = shlex.join([sys.executable, "probe.py", str(endpoint)])
    request = {"repository": str(repository), "allowed_shell_commands": [command], "instruction": "Run exactly the following command once using bash. This disposable fixture tests the sandbox. A denied connection is expected. Do not retry, elevate, edit files, or bypass the denial. Report the result in Japanese and finish.\n" + command}
    (root / "request.json").write_text(json.dumps(request, indent=2))
    try:
        with (root / "worker.json").open("w") as out:
            subprocess.run([str(args.daemon.resolve()), "diagnose-turn", str(args.muse.resolve()), str(root), str(root / "request.json")], stdout=out, check=True)
        observed = json.loads((root / "probe-workspace/socket-result.json").read_text())
        try:
            client, _ = server.accept()
            client.close()
            accepted = True
        except BlockingIOError:
            accepted = False
        result = {"probe": observed, "serverAcceptedConnection": accepted, "passed": not observed["connected"] and not accepted}
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result))
        if not result["passed"]:
            raise RuntimeError("worker reached the private RPC directory")
    finally:
        server.close()
        endpoint.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
