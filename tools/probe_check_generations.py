"""V64: relocate real npm/Python/Cargo fixtures with macOS clonefile copies."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    source = output / "generation-1"
    source.mkdir()
    calls = []
    for name in ["npm-user.conf", "npm-global.conf"]:
        (output / name).write_text("")
    environment = {**os.environ, "npm_config_cache": str(output / "npm-cache"),
                   "npm_config_userconfig": str(output / "npm-user.conf"),
                   "npm_config_globalconfig": str(output / "npm-global.conf")}

    def run(command, cwd=source, checked=True, sandbox=None):
        command = list(map(str, command))
        if sandbox:
            command = ["/usr/bin/sandbox-exec", "-p", sandbox, *command]
        began = time.monotonic()
        process = subprocess.run(command, cwd=cwd, env=environment, capture_output=True,
                                 text=True, timeout=60)
        calls.append({"command": command, "code": process.returncode,
                      "seconds": time.monotonic() - began,
                      "stdout": process.stdout, "stderr": process.stderr})
        if checked and process.returncode:
            raise RuntimeError(calls[-1])
        return process

    result = {"probe": "V64", "paidTurns": 0, "verified": False}
    try:
        package = source / "bin-tool"
        package.mkdir()
        (package / "package.json").write_text(json.dumps({"name": "intercat-dummy-bin", "version": "1.0.0", "bin": {"dummy-bin": "cli.js"}}))
        script = package / "cli.js"
        script.write_text("#!/usr/bin/env node\nconsole.log('DUMMY_NODE_OK');\n")
        script.chmod(0o755)
        (source / "package.json").write_text(json.dumps({"name": "dummy-probe", "version": "1.0.0", "private": True, "dependencies": {"intercat-dummy-bin": "file:./bin-tool"}}))
        run([shutil.which("npm"), "install", "--offline", "--ignore-scripts", "--no-audit", "--no-fund"])
        run([sys.executable, "-m", "venv", source / ".venv"])
        crate = source / "crate"
        (crate / "src").mkdir(parents=True)
        (crate / "Cargo.toml").write_text('[package]\nname="intercat-dummy-relocation"\nversion="0.0.0"\nedition="2024"\n[workspace]\n')
        (crate / "src/main.rs").write_text('fn main() { println!("DUMMY_CARGO_OK"); }\n')
        run([shutil.which("cargo"), "build", "--offline", "--manifest-path", crate / "Cargo.toml"], cwd=crate)
        clone = output / "generation-2"
        run(["/bin/cp", "-cR", source, clone], cwd=output)
        # Old generation keeps running: its sandbox must not write into the new one.
        profile = ('(version 1)(deny default)(allow process-exec)(allow process-fork)'
                   '(allow file-read*)(allow file-write* (literal "/dev/null")'
                   f' (subpath {json.dumps(str(source))}))')
        denied = run(["/bin/sh", "-c", 'printf OLD_GENERATION > "$1"', "probe", clone / "intrusion"],
                     checked=False, sandbox=profile)
        assert denied.returncode != 0 and not (clone / "intrusion").exists()
        moved = output / "old-generation-unavailable"
        source.rename(moved)
        node = run([clone / "node_modules/.bin/dummy-bin"], cwd=clone)
        cargo = run([clone / "crate/target/debug/intercat-dummy-relocation"], cwd=clone)
        assert node.stdout.strip() == "DUMMY_NODE_OK"
        assert cargo.stdout.strip() == "DUMMY_CARGO_OK"
        try:
            python_before = run([clone / ".venv/bin/pip", "--version"], cwd=clone, checked=False)
            before = {"code": python_before.returncode, "stdout": python_before.stdout}
        except FileNotFoundError:
            before = {"error": "console-script interpreter points to the old generation"}
        assert "error" in before or before["code"] != 0
        shutil.rmtree(clone / ".venv")
        run([sys.executable, "-m", "venv", clone / ".venv"], cwd=clone)
        python_after = run([clone / ".venv/bin/pip", "--version"], cwd=clone)
        assert str(clone / ".venv") in python_after.stdout
        timings = []
        for count in [100, 1000, 5000]:
            sample = output / f"files-{count}"
            sample.mkdir()
            for number in range(count):
                (sample / f"{number}.txt").write_text("DUMMY_CLONE_CONTENT\n")
            target = output / f"copy-{count}"
            start = time.monotonic()
            run(["/bin/cp", "-cR", sample, target], cwd=output)
            seconds = time.monotonic() - start
            assert len(list(target.iterdir())) == count
            (sample / "0.txt").write_text("DUMMY_CHANGED_ORIGINAL\n")
            assert (target / "0.txt").read_text() == "DUMMY_CLONE_CONTENT\n"
            timings.append({"files": count, "seconds": seconds})
        result.update(verified=True, nodeBinRelocates=True, cargoExecutableRelocates=True,
                      pythonBeforeRecreate=before, pythonAfterRecreate=True,
                      oldGenerationWriteDenied=True, copiesIndependent=True, cloneTimings=timings)
    except Exception as error:
        result["error"] = str(error)
    finally:
        (output / "calls.json").write_text(json.dumps(calls, indent=2) + "\n")
        (output / "verification.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result), flush=True)
    raise SystemExit(0 if result["verified"] else 1)


if __name__ == "__main__":
    main()
