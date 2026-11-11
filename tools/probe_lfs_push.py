"""V62: push generated LFS bytes to local bare repositories, then fetch anew."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--lfs", type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    lfs = args.lfs.resolve(strict=True)
    home = output / "home"
    home.mkdir()
    env = {"HOME": str(home), "PATH": f"{lfs.parent}:/usr/bin:/bin:/usr/sbin:/sbin",
           "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0"}
    calls = []

    def git(root, *args, extra=None):
        command = ["/usr/bin/git", *map(str, args)]
        process = subprocess.run(command, cwd=root, env={**env, **(extra or {})},
                                 capture_output=True, text=True, timeout=45)
        calls.append({"args": list(map(str, args)), "code": process.returncode,
                      "stdout": process.stdout, "stderr": process.stderr})
        if process.returncode:
            raise RuntimeError(calls[-1])
        return process.stdout.strip()

    result = {"probe": "V62", "paidTurns": 0, "verified": False}
    try:
        remote = output / "git-remote.git"
        lfs_remote = output / "lfs-remote.git"
        git(output, "init", "--bare", "-q", "--initial-branch=main", remote)
        git(output, "init", "--bare", "-q", "--initial-branch=main", lfs_remote)
        source = output / "source"
        source.mkdir()
        git(source, "init", "-q", "-b", "main")
        git(source, "config", "user.name", "Dummy")
        git(source, "config", "user.email", "dummy@example.invalid")
        git(source, "lfs", "install", "--local")
        git(source, "remote", "add", "origin", remote.as_uri())
        (source / ".gitattributes").write_text("*.dat filter=lfs diff=lfs merge=lfs -text\n")
        (source / ".lfsconfig").write_text(f'[lfs]\n    url = "{lfs_remote.as_uri()}"\n')
        payload = b"DUMMY_LFS_PUSH_CONTENT\0\1\2\n"
        (source / "dummy.dat").write_bytes(payload)
        git(source, "add", "-A")
        git(source, "commit", "-qm", "Generated LFS fixture")
        sha = git(source, "rev-parse", "HEAD")
        oid = hashlib.sha256(payload).hexdigest()
        endpoint = next(line for line in git(source, "lfs", "env").splitlines() if line.startswith("Endpoint="))
        assert lfs_remote.as_uri() in endpoint, endpoint
        # Explicit LFS transfer precedes the exact refspec; no project hook runs here.
        git(source, "lfs", "push", "origin", sha)
        git(source, "push", "--no-verify", "--no-follow-tags", "origin", f"{sha}:refs/heads/main")
        assert git(output, "--git-dir", remote, "rev-parse", "refs/heads/main") == sha
        clone = output / "fresh-clone"
        git(output, "clone", "-q", remote.as_uri(), clone, extra={"GIT_LFS_SKIP_SMUDGE": "1"})
        assert b"oid sha256:" in (clone / "dummy.dat").read_bytes()
        git(clone, "lfs", "install", "--local")
        git(clone, "lfs", "pull")
        assert (clone / "dummy.dat").read_bytes() == payload
        assert hashlib.sha256((clone / "dummy.dat").read_bytes()).hexdigest() == oid
        result.update(verified=True, lfsBeforeGit=True, exactRefspec=True, newCloneHasPayload=True,
                      separateLfsConfigEndpoint=True, sha=sha, payloadSha256=oid)
    except Exception as error:
        result["error"] = str(error)
    finally:
        (output / "calls.json").write_text(json.dumps(calls, indent=2) + "\n")
        (output / "verification.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result), flush=True)
    raise SystemExit(0 if result["verified"] else 1)


if __name__ == "__main__":
    main()
