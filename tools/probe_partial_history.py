"""V61: measure whether tags expose historical blobs in a partial clone.

Only agent-created files and a local bare remote are used. References:
https://git-scm.com/docs/git-clone and https://git-scm.com/docs/git
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    source = output / "dummy-source"
    remote = output / "dummy-origin.git"
    job = output / "job"
    temporary = job / "tmp"
    control = job / "control"
    clone = job / "repo"
    source.mkdir()
    for path in [temporary / "home", control / "empty-hooks", control / "empty-template"]:
        path.mkdir(parents=True)
    git = Path(subprocess.check_output(["/usr/bin/xcrun", "--find", "git"], text=True).strip()).resolve()
    environment = {"PATH": f"{git.parent}:/usr/bin:/bin:/usr/sbin:/sbin",
                   "HOME": str(temporary / "home"), "TMPDIR": str(temporary),
                   "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1",
                   "GIT_TERMINAL_PROMPT": "0", "GIT_NO_REPLACE_OBJECTS": "1"}
    exec_directory = Path(subprocess.check_output([str(git), "--exec-path"], env=environment, text=True).strip()).resolve()
    calls = []

    def run(arguments, cwd=source, checked=True, profile=None):
        command = [str(git), "-c", "core.fsmonitor=false", "-c", "commit.gpgsign=false",
                   "-c", f"core.hooksPath={control / 'empty-hooks'}", *map(str, arguments)]
        if profile:
            command = ["/usr/bin/sandbox-exec", "-p", profile, *command]
        began = time.monotonic()
        process = subprocess.run(command, cwd=cwd, env=environment, capture_output=True,
                                 text=True, timeout=60)
        calls.append({"command": command, "code": process.returncode,
                      "seconds": time.monotonic() - began,
                      "stdout": process.stdout, "stderr": process.stderr})
        if checked and process.returncode:
            raise RuntimeError(calls[-1])
        return process

    result = {"probe": "V61", "verified": False, "paidTurns": 0}
    try:
        run(["init", "-q", "-b", "main"])
        run(["config", "user.name", "Dummy"])
        run(["config", "user.email", "dummy@example.invalid"])
        (source / ".env").write_text("DUMMY_OLD_VALUE=agent-created-history-only\n")
        for step in range(6):
            if step == 1:
                (source / ".env").unlink()
            (source / "current.txt").write_text(f"agent-created current revision {step}\n")
            run(["add", "-A"])
            run(["commit", "-qm", f"dummy revision {step}"])
            if step == 3:
                run(["tag", "-am", "dummy tag", "dummy-v1"])
        head = run(["rev-parse", "HEAD"]).stdout.strip()
        old = run(["rev-parse", "HEAD~5:.env"]).stdout.strip()
        # Git permits a tag to point directly to a blob. Check this as well as
        # ordinary commit tags: a tag must not silently import past file bytes.
        run(["tag", "dummy-history-blob", old])
        index = (source / ".git/index").read_bytes()
        run(["clone", "--bare", "--no-hardlinks", "--", source, remote], cwd=output)
        run(["config", "uploadpack.allowFilter", "true"], cwd=remote)
        refs = run(["show-ref"], cwd=remote).stdout
        run(["clone", "--filter=blob:none", "--no-checkout",
             f"--template={control / 'empty-template'}", "--", remote.as_uri(), clone], cwd=job)
        # Transfer prefetches only the reviewed tree, before project code runs.
        run(["checkout", "--quiet", "--detach", head], cwd=clone)
        present = set(run(["cat-file", "--batch-all-objects", "--batch-check=%(objectname)"], cwd=clone).stdout.splitlines())
        transferred = old in present
        roots = ["/System", "/usr", "/bin", "/sbin", "/dev", "/Library/Developer",
                 "/private/preboot", "/private/var/db/dyld", str(job), str(git.parent),
                 str(exec_directory), str(exec_directory.parent.parent)]
        profile = ("(version 1)(deny default)(allow process-exec)(allow process-fork)"
                   "(allow process-info*)(allow mach-lookup)(allow sysctl-read)"
                   "(allow signal (target self))(allow file-read-metadata)"
                   '(allow file-read* (literal "/") ' + " ".join(f"(subpath {json.dumps(path)})" for path in roots)
                   + " ".join(f" (literal {json.dumps(str(path))})" for path in job.parents) + ")"
                   f'(allow file-write* (subpath {json.dumps(str(job))}) (literal "/dev/null"))'
                   f"(deny file-read* file-write* (subpath {json.dumps(str(source))}) (subpath {json.dumps(str(remote))}))")
        described = run(["describe", "--tags"], cwd=clone, profile=profile).stdout.strip()
        assert described.startswith("dummy-v1-")
        history = run(["rev-list", "--count", "HEAD"], cwd=clone, profile=profile).stdout.strip()
        assert history == "6"
        # Keep the promisor URL configured: exercise a genuine lazy-fetch
        # attempt, rather than disabling lazy fetching for this assertion.
        attempt = run(["show", "HEAD~5:.env"], cwd=clone, checked=False, profile=profile)
        readable = attempt.returncode == 0 and "agent-created-history-only" in attempt.stdout
        assert readable == transferred
        after = set(run(["cat-file", "--batch-all-objects", "--batch-check=%(objectname)"], cwd=clone).stdout.splitlines())
        assert (old in after) == transferred
        assert not (clone / ".git/objects/info/alternates").exists()
        assert run(["show-ref"], cwd=remote).stdout == refs
        assert run(["rev-parse", "HEAD"]).stdout.strip() == head
        assert (source / ".git/index").read_bytes() == index
        result.update(verified=True, tagsDescribe=described, historyCommits=6,
                      oldBlobTransferred=transferred, oldBlobReadableByCode=readable,
                      lazyFetchAttempted=not transferred, lazyFetchDenied=not readable,
                      originalUnchanged=True,
                      decision=("Partial history can expose historical blobs through blob tags. Use full history only with explicit disclosure."
                                if readable else "Partial history blocks historical blobs, including the blob-tag fixture."))
    except Exception as error:
        result["error"] = str(error)
    finally:
        (output / "calls.json").write_text(json.dumps(calls, indent=2) + "\n")
        (output / "verification.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result), flush=True)
    raise SystemExit(0 if result["verified"] else 1)


if __name__ == "__main__":
    main()
