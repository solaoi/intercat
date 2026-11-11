#!/usr/bin/env python3
"""Run Rust tests beside an empty directory, outside Cargo's large deps directory.

Foundation inspects the main executable's directory while creating native data
detectors. Old object files in deps can turn that inspection into a long wait.
Each executable is cloned without changing its bytes, arguments, or deadlines.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time


ROOT = Path(__file__).resolve().parent.parent
SUMMARY = re.compile(
    r"test result: .*? (\d+) passed; (\d+) failed; (\d+) ignored; "
    r"\d+ measured; (\d+) filtered out; finished in ([\d.]+)s"
)


def digest(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def source_manifest():
    paths = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "-z"],
        cwd=ROOT,
    ).split(b"\0")
    result = {}
    for raw in sorted(set(paths) - {b""}):
        relative = os.fsdecode(raw)
        path = ROOT / relative
        if path.is_symlink():
            result[relative] = {"symlink": os.readlink(path)}
        elif path.is_file():
            result[relative] = digest(path)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path, help="An ignored directory for local proof")
    output = parser.parse_args().output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    before = source_manifest()
    env = dict(os.environ, INTERCAT_GITLEAKS=str(ROOT / ".tools/gitleaks-8.30.1/gitleaks"))
    build = [
        "cargo", "test", "--offline", "--workspace", "--all-targets", "--all-features",
        "--no-run", "--message-format=json",
    ]
    artifacts = {}
    started = time.monotonic()
    with (output / "build.log").open("w") as log:
        process = subprocess.Popen(
            build, cwd=ROOT, env=env, stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT, text=True,
        )
        for line in process.stdout:
            log.write(line)
            log.flush()
            try:
                item = json.loads(line)
            except json.JSONDecodeError:
                continue
            if (
                item.get("reason") == "compiler-artifact"
                and item["profile"]["test"] and item.get("executable")
            ):
                artifacts[item["executable"]] = item
        code = process.wait()
    if code:
        print(f"Workspace build failed ({code}); see {output / 'build.log'}", flush=True)
        return code
    if not artifacts:
        raise RuntimeError("Cargo returned no test executables")
    if source_manifest() != before:
        raise RuntimeError("Workspace sources changed during the build")

    ordered = sorted(
        artifacts.values(),
        key=lambda item: (item["manifest_path"], item["target"]["kind"], item["target"]["name"]),
    )
    results = []
    failed = False
    with tempfile.TemporaryDirectory(prefix="intercat-test-bin-", dir="/private/tmp") as stage:
        executable = Path(stage) / "test"
        for index, item in enumerate(ordered, 1):
            original = Path(item["executable"])
            sha = digest(original)
            subprocess.run(["/bin/cp", "-c", str(original), str(executable)], check=True)
            if digest(executable) != sha:
                raise RuntimeError("The staged executable differs from Cargo's artifact")
            name = f"{index:03}-{original.name}.log"
            command = [str(executable), "--test-threads=4"]
            target_started = time.monotonic()
            test_env = dict(env, CARGO_MANIFEST_DIR=str(Path(item["manifest_path"]).parent))
            with (output / name).open("w") as log:
                test = subprocess.run(
                    command, cwd=Path(item["manifest_path"]).parent,
                    env=test_env, stdout=log, stderr=subprocess.STDOUT,
                )
            matches = list(SUMMARY.finditer((output / name).read_text()))
            record = {
                "name": item["target"]["name"], "kind": item["target"]["kind"],
                "manifest": item["manifest_path"], "original": str(original),
                "sha256": sha, "log": name, "exitCode": test.returncode,
                "wallSeconds": round(time.monotonic() - target_started, 3),
            }
            if matches:
                # Child-process helper summaries precede the outer harness summary.
                match = matches[-1]
                record.update(zip(("passed", "failed", "ignored", "filtered"), map(int, match.groups()[:4])))
                record["testSeconds"] = float(match[5])
            valid = bool(matches) and record["filtered"] == 0 and record["failed"] == 0
            failed |= test.returncode != 0 or not valid
            results.append(record)
            executable.unlink()
            (output / "targets.json").write_text(json.dumps(results, indent=2) + "\n")
            print(json.dumps(record), flush=True)

    after = source_manifest()
    stable = after == before
    result = {
        "verified": not failed and stable,
        "buildCommand": build, "testArguments": ["--test-threads=4"],
        "sourceStable": stable, "sourceSha256": before,
        "seconds": round(time.monotonic() - started, 3), "targets": results,
        "totals": {
            key: sum(record.get(key, 0) for record in results)
            for key in ("passed", "failed", "ignored")
        },
        "nestedSummariesCounted": False,
        "staging": "One APFS clone at a time, with identical SHA-256; removed after each test",
    }
    (output / "verification.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: result[key] for key in ("verified", "sourceStable", "totals", "seconds")}), flush=True)
    return 0 if result["verified"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
