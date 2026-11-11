"""V54: compare isolated clean/LFS snapshots using generated repositories only."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
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
           "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1",
           "GIT_TERMINAL_PROMPT": "0", "LC_ALL": "C", "GIT_OPTIONAL_LOCKS": "0"}
    git_binary = subprocess.check_output(["/usr/bin/xcrun", "--find", "git"], text=True).strip()
    calls = []

    def git(root, *args, environment=None, sandbox=None, checked=True, data=None):
        command = [git_binary, *map(str, args)]
        if sandbox:
            command = ["/usr/bin/sandbox-exec", "-p", sandbox, *command]
        result = subprocess.run(command, cwd=root, env=environment or env, input=data,
                                capture_output=True, timeout=45)
        calls.append({"args": list(map(str, args)), "sandbox": bool(sandbox),
                      "code": result.returncode, "stderr": result.stderr.decode(errors="replace")})
        if checked and result.returncode:
            raise RuntimeError(calls[-1])
        return result

    def text(root, *args, **kw):
        return git(root, *args, **kw).stdout.decode().strip()

    def quote(path):
        return json.dumps(str(path))

    result = {"probe": "V54", "paidTurns": 0, "verified": False}
    try:
        source = output / "source"
        source.mkdir()
        git(source, "init", "-q", "-b", "main")
        git(source, "config", "user.name", "Dummy")
        git(source, "config", "user.email", "dummy@example.invalid")
        git(source, "lfs", "install", "--local", "--skip-repo")
        git(source, "config", "filter.upper.clean", "/usr/bin/tr a-z A-Z")
        git(source, "config", "filter.upper.smudge", "/bin/cat")
        git(source, "config", "filter.upper.required", "true")
        (source / ".gitattributes").write_text("*.upper filter=upper\n*.lfs filter=lfs diff=lfs merge=lfs -text\n")
        for name, content in {"unchanged": "DUMMY_SAME\n", "deleted": "DUMMY_DELETE\n",
                              "value.upper": "before\n", "old.lfs": "DUMMY_OLD_LFS\n"}.items():
            (source / name).write_text(content)
        git(source, "add", "-A")
        git(source, "commit", "-qm", "Dummy history")
        old_lfs = hashlib.sha256((source / "old.lfs").read_bytes()).hexdigest()
        (source / "old.lfs").unlink()
        (source / "current.lfs").write_text("DUMMY_CURRENT_LFS_BEFORE\n")
        git(source, "add", "-A")
        git(source, "commit", "-qm", "Dummy baseline")
        (source / "value.upper").write_text("changed content\n")
        (source / "deleted").unlink()
        (source / "new").write_text("DUMMY_NEW\n")
        payload = b"DUMMY_CURRENT_LFS_CHANGED\n"
        (source / "current.lfs").write_bytes(payload)
        oid = hashlib.sha256(payload).hexdigest()
        expected = output / "ordinary"
        shutil.copytree(source, expected)
        git(expected, "add", "-A")
        expected_tree = text(expected, "write-tree")
        index_before = (source / ".git/index").read_bytes()
        head_before = text(source, "rev-parse", "HEAD")
        isolated = output / "isolated"
        isolated.mkdir()
        git(isolated, "init", "--bare", "-q")
        shutil.copyfile(source / ".git/index", isolated / "index")
        isolated_env = {**env, "GIT_INDEX_FILE": str(isolated / "index"),
                        "GIT_OBJECT_DIRECTORY": str(isolated / "objects"),
                        "GIT_ALTERNATE_OBJECT_DIRECTORIES": "", "TMPDIR": str(isolated)}
        profile = ("(version 1)(deny default)(allow process-fork)(allow process-exec)"
                   "(allow process-info*)(allow sysctl-read)(allow mach-lookup)"
                   "(allow signal (target self))(allow file-read-metadata)"
                   "(allow file-read* (literal \"/\") (subpath \"/System\")"
                   " (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\")"
                   " (subpath \"/dev\") (subpath \"/private/var/db/dyld\")"
                   " (subpath \"/private/preboot\") (subpath \"/Library/Developer\")"
                   f" (subpath {quote(Path(git_binary).parents[2])})"
                   f" (subpath {quote(source)}) (subpath {quote(isolated)})"
                   f" (subpath {quote(lfs.parent)}))"
                   f"(deny file-read* (subpath {quote(source / '.git/objects')}))"
                   f"(deny file-read* (subpath {quote(source / '.git/lfs')}))"
                   f"(allow file-write* (subpath {quote(isolated)}) (literal \"/dev/null\"))")
        settings = ["-c", "core.splitIndex=false", "-c", "core.fsmonitor=false",
                    "-c", "core.hooksPath=/dev/null", "-c", f"lfs.storage={isolated / 'lfs'}"]
        git(source, *settings, "add", "--sparse", "-A", environment=isolated_env, sandbox=profile)
        tree = text(source, *settings, "write-tree", "--missing-ok", environment=isolated_env, sandbox=profile)
        assert tree == expected_tree, (tree, expected_tree)
        object_path = isolated / "lfs/objects" / oid[:2] / oid[2:4] / oid
        assert object_path.read_bytes() == payload
        assert not (isolated / "lfs/objects" / old_lfs[:2] / old_lfs[2:4] / old_lfs).exists()
        denied = git(source, "cat-file", "-p", f"{head_before}:unchanged",
                     environment={**isolated_env, "GIT_OBJECT_DIRECTORY": str(source / ".git/objects")},
                     sandbox=profile, checked=False)
        assert denied.returncode != 0
        # Daemon-side import: copy only fresh objects, then validate the whole tree.
        for entry in (isolated / "objects").glob("??/*"):
            destination = source / ".git/objects" / entry.parent.name / entry.name
            destination.parent.mkdir(exist_ok=True)
            if not destination.exists():
                shutil.copyfile(entry, destination)
        git(source, "fsck", "--connectivity-only", "--no-reflogs", tree)
        assert text(source, "show", f"{tree}:value.upper") == "CHANGED CONTENT"
        assert text(source, "show", f"{tree}:unchanged") == "DUMMY_SAME"
        assert "deleted" not in text(source, "ls-tree", "--name-only", tree).splitlines()
        assert (source / ".git/index").read_bytes() == index_before
        assert text(source, "rev-parse", "HEAD") == head_before
        # Populate only the tree's LFS payload in an independent integration clone.
        clone = output / "integration"
        clone.mkdir()
        git(clone, "init", "-q")
        git(clone, "config", "filter.upper.clean", "/usr/bin/tr a-z A-Z")
        git(clone, "config", "filter.upper.smudge", "/bin/cat")
        git(clone, "config", "filter.upper.required", "true")
        git(clone, "lfs", "install", "--local", "--skip-repo")
        git(clone, "config", "lfs.storage", str(clone / ".git/lfs"))
        pending = [("tree", tree)]
        copied = set()
        while pending:
            kind, sha = pending.pop()
            if sha in copied:
                continue
            copied.add(sha)
            content = git(source, "cat-file", kind, sha).stdout
            assert text(clone, "hash-object", "-w", "-t", kind, "--stdin", data=content) == sha
            if kind == "tree":
                for row in git(source, "ls-tree", "-z", sha).stdout.split(b"\0"):
                    if row:
                        _, child_kind, child_sha = row.split(b"\t", 1)[0].split()
                        pending.append((child_kind.decode(), child_sha.decode()))
        destination = clone / ".git/lfs/objects" / oid[:2] / oid[2:4] / oid
        destination.parent.mkdir(parents=True)
        shutil.copyfile(object_path, destination)
        clone_profile = profile.replace(quote(source) + ")", quote(clone) + ")")
        clone_profile = clone_profile.replace(quote(isolated) + ")", quote(clone) + ")")
        git(clone, "read-tree", "--reset", "-u", tree, sandbox=clone_profile)
        assert (clone / "current.lfs").read_bytes() == payload
        git(clone, "add", "-A", sandbox=clone_profile)
        assert text(clone, "write-tree") == tree
        (clone / "current.lfs").write_bytes(b"DUMMY_AFTER_CHECK\n")
        git(clone, "add", "-A", sandbox=clone_profile)
        after_check = text(clone, "write-tree")
        pointer = text(clone, "show", f"{after_check}:current.lfs")
        after_oid = hashlib.sha256(b"DUMMY_AFTER_CHECK\n").hexdigest()
        assert after_oid in pointer
        assert (clone / ".git/lfs/objects" / after_oid[:2] / after_oid[2:4] / after_oid).exists()
        assert not (clone / ".git/lfs/objects" / old_lfs[:2] / old_lfs[2:4] / old_lfs).exists()
        result.update(verified=True, expectedTree=expected_tree, isolatedTree=tree,
                      unchangedAndDeletedMatch=True, originalIndexAndHeadUnchanged=True,
                      originalObjectsDenied=True, isolatedLfsClean=True, integrationCleanBeforeAndAfter=True,
                      lfsVersion=subprocess.check_output([lfs, "version"], text=True).strip())
    except Exception as error:
        result["error"] = str(error)
    finally:
        (output / "calls.json").write_text(json.dumps(calls, indent=2) + "\n")
        (output / "verification.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result), flush=True)
    raise SystemExit(0 if result["verified"] else 1)


if __name__ == "__main__":
    main()
