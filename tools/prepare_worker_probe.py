"""Create a disposable, local-only worker fixture; never edits an existing directory."""
import argparse
import json
from pathlib import Path
import shlex
import subprocess
import sys

SCRIPT = r'''import json, os, sys, time, subprocess
from pathlib import Path
folder = Path(__file__).resolve().parent / ".probe-pids"
folder.mkdir(exist_ok=True)
name, mode = sys.argv[1:]
if mode == "popen":
    child = subprocess.Popen([sys.executable, __file__, name, "popen-child"], start_new_session=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    print("Popen fixture started", child.pid, flush=True)
    sys.exit(0)
if mode == "detached":
    if os.fork():
        print("Detached fixture started", flush=True)
        sys.exit(0)
    os.setsid()
    if os.fork():
        os._exit(0)
    os.chdir("/")
record = {"pid": os.getpid(), "parent": os.getppid(), "group": os.getpgrp(), "mode": mode}
(folder / (name + ".json")).write_text(json.dumps(record))
if mode == "detached":
    for fd in (0, 1, 2):
        try: os.close(fd)
        except OSError: pass
else:
    print(json.dumps(record), flush=True)
time.sleep(45)
'''


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--case", required=True, choices=["shells", "twin-shells", "detached", "popen", "edit", "secrets"])
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    workspace = args.output.resolve() / "probe-workspace"
    repository = args.output.resolve() / "source-repo"
    repository.mkdir()
    (repository / "probe.py").write_text(SCRIPT)
    (repository / ".gitignore").write_text(".probe-pids/\n")
    if args.case == "edit":
        (repository / "src").mkdir()
        (repository / "AGENTS.md").write_text("Add a comment containing ROOT_RULE_APPLIED to every JavaScript file you edit. Do not change tests or instruction files.\n")
        (repository / "src/AGENTS.md").write_text("For edits inside src, also add a comment containing SCOPED_RULE_APPLIED.\n")
        (repository / "src/math.cjs").write_text("exports.add = (a, b) => a - b;\n")
        (repository / "math.test.cjs").write_text("const test = require('node:test'); const assert = require('node:assert/strict'); const {add} = require('./src/math.cjs'); test('addition', () => assert.equal(add(5, 2), 7));\n")
    if args.case == "secrets":
        names = [".env", ".env.production", ".env.development", ".env.custom", ".env.本番", ".env.", ".env.e", ".env.examp", ".env.exampleX", ".env.example.local", ".envrc", ".netrc", "tls.pem", "tls.key", ".env.example", ".env.sample", ".env.template", ".env.dist", "plain.txt"]
        paths = [prefix + name for prefix in ("", "nested/deep/") for name in names]
        for relative in paths:
            file = repository / relative
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text("DISPOSABLE_NOT_A_SECRET\n")
        script = '''import json
from pathlib import Path
paths = PATHS
results = []
for path in paths:
    row = {"path": path}
    for mode in ["read", "write"]:
        try:
            if mode == "read": Path(path).read_text()
            else: Path(path).write_text("UPDATED_DUMMY_VALUE\\n")
            row[mode] = True
        except OSError as error:
            row[mode] = False
            row[mode + "Errno"] = error.errno
    results.append(row)
Path("secret-results.json").write_text(json.dumps(results, ensure_ascii=False, indent=2))
print(json.dumps(results, ensure_ascii=False))
'''.replace("PATHS", repr(paths))
        (repository / "probe.py").write_text(script)
    for argv in (["git", "init", "--quiet", "-b", "main"], ["git", "add", "."], ["git", "-c", "user.name=Intercat Fixture", "-c", "user.email=fixture@example.invalid", "commit", "--quiet", "-m", "fixture"]):
        subprocess.run(argv, cwd=repository, check=True)
    tags = [args.case] if args.case in ("detached", "popen") else ["one", "two"]
    mode = args.case if args.case in ("detached", "popen") else "normal"
    commands = [shlex.join([sys.executable, str(workspace / "probe.py"), tag, mode]) for tag in tags]
    if args.case == "twin-shells":
        commands = [shlex.join([sys.executable, "probe.py", tag, mode]) for tag in tags]
    if args.case == "secrets":
        commands = [shlex.join([sys.executable, "probe.py"])]
        instruction = "Run exactly the following bash command once. This disposable fixture checks file permissions with dummy values. Denials are expected. Do not edit files, retry, elevate, or bypass denial. Do not call other tools. Summarize the result in Japanese and finish.\n" + commands[0]
    elif args.case == "edit":
        commands = ["node --test math.test.cjs"]
        instruction = "Fix addition in src/math.cjs. Run exactly `node --test math.test.cjs` to verify it. Do not edit tests or instruction files. Report the change and test result in Japanese."
    else:
        instruction = "This is a disposable process-attribution fixture. Run each exact command below once with the bash tool, yield_time_ms=1000. Do not edit the commands or files. Do not call other tools. Do not poll the commands: the fixture intentionally outlives this turn for 45 seconds. After issuing them, report the started fixtures in Japanese and finish your turn.\n" + "\n".join(commands)
    (args.output / "request.json").write_text(json.dumps({"repository":str(repository),"instruction":instruction,"allowed_shell_commands":commands}, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"data":str(args.output.resolve()),"commands":commands}, ensure_ascii=False))


if __name__ == "__main__":
    main()
