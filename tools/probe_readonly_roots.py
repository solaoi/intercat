"""V58: exercise extra workspace roots with disposable data and bounded turns."""
import argparse
import hashlib
import json
import os
from contextlib import contextmanager
import subprocess
from pathlib import Path
import shlex
import time

from verify_host_hooks import Host, command_id

PINNED_SHA = "1c96114408acbeade9a8fb79e3b4425e5b64d50425938568aef01bdc04ce2e03"


def volume_verified(summary):
    results = summary.get("results", {})
    writes = ["direct_write", "chmod_file", "write_after_chmod", "chmod_directory",
              "new_file", "replace", "hardlink", "rename_out", "child_write",
              "unmount", "write_after_unmount"]
    return ("error" not in summary and summary.get("referenceUnchanged") is True
            and results.get("read") is True
            and all(results.get(name) is False for name in writes))


@contextmanager
def reference_files(folder, mode):
    reference = folder / "reference"
    source = folder / "source" if mode == "volume" else reference
    source.mkdir()
    for name in ["direct", "chmod", "replace", "link", "rename", "child"]:
        path = source / name
        path.write_text("SAVED_DUMMY_CONTENT\n")
        path.chmod(0o400)
    source.chmod(0o500)
    if mode == "ordinary":
        yield reference
        return
    image = folder / "reference.dmg"
    reference.mkdir()
    subprocess.run(["/usr/bin/hdiutil", "create", "-quiet", "-fs", "HFSX", "-format", "UDRO", "-srcfolder", str(source), "-volname", "Intercat reference", str(image)], check=True)
    subprocess.run(["/usr/bin/hdiutil", "attach", "-quiet", "-readonly", "-nobrowse", "-mountpoint", str(reference), str(image)], check=True)
    try:
        yield reference
    finally:
        if os.path.ismount(reference):
            subprocess.run(["/usr/bin/hdiutil", "detach", "-quiet", str(reference)], check=True)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--auth", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--case", choices=["ordinary", "volume"], required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    if hashlib.sha256(binary.read_bytes()).hexdigest() != PINNED_SHA:
        raise RuntimeError("This probe requires the pinned Muse 1.4.14 binary.")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    summaries = []
    for mode in [args.case]:
        folder = output / mode
        folder.mkdir()
        primary = folder / "workspace"
        primary.mkdir()
        with reference_files(folder, mode) as reference:
            script = primary / "probe.py"
            script.write_text(f'''from pathlib import Path
import json, os, subprocess
r=Path({str(reference)!r})
p=Path({str(primary)!r})
results={{}}
def attempt(name, action):
    try:
        action()
        results[name]=True
    except OSError:
        results[name]=False
results['read']=(r/'direct').read_text()=='SAVED_DUMMY_CONTENT\\n'
attempt('direct_write',lambda:(r/'direct').write_text('CHANGED'))
attempt('chmod_file',lambda:(r/'chmod').chmod(0o600))
attempt('write_after_chmod',lambda:(r/'chmod').write_text('CHANGED'))
attempt('chmod_directory',lambda:r.chmod(0o700))
attempt('new_file',lambda:(r/'new').write_text('CHANGED'))
(p/'replacement').write_text('CHANGED')
attempt('replace',lambda:os.replace(p/'replacement',r/'replace'))
attempt('hardlink',lambda:os.link(r/'link',p/'alias'))
if (p/'alias').exists():
    attempt('chmod_alias',lambda:(p/'alias').chmod(0o600))
    attempt('write_alias',lambda:(p/'alias').write_text('CHANGED'))
attempt('rename_out',lambda:os.rename(r/'rename',p/'renamed'))
command=['/bin/sh','-c','printf CHANGED > "$1"','probe',str(r/'child')]
results['child_write']=subprocess.run(command,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL).returncode==0
results['unmount']=False
results['write_after_unmount']=False
if {mode == 'volume'}:
    results['unmount']=subprocess.run(['/usr/bin/hdiutil','detach','-force',str(r)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,timeout=15).returncode==0
    if results['unmount']:
        attempt('write_after_unmount',lambda:(r/'direct').write_text('CHANGED_AFTER_UNMOUNT'))
(p/'results.json').write_text(json.dumps(results))
print('DUMMY_READONLY_PROBE_COMPLETE')
''')
            config = folder / "config"
            muse = config / "muse"
            muse.mkdir(parents=True)
            auth = muse / "auth.json"
            auth.symlink_to(args.auth.resolve(strict=True))
            settings = {"schema_version": 1, "provider": "meta", "model": "muse-spark-1.3-contributor",
                        "contributor_mode_enabled": False, "agents": {"execution_capacity": 1},
                        "run": {"toolset": ["bash"]},
                        "permissions": {"schema_version": 1, "default_profile": "intercat-worker",
                                        "profiles": {"intercat-worker": {"extends": ":ask-me"}}}}
            (muse / "settings.json").write_text(json.dumps(settings))
            host = Host(binary, primary, config, folder, False)
            summary = {"case": mode, "attemptedTurns": 0}
            try:
                initialized = host.request("initialize", {"clientInfo": {"name": "intercat_readonly_probe", "version": "1"}, "capabilities": {"experimentalApi": True, "userInputDialogs": True}})
                if initialized.get("schema", {}).get("fingerprint") != "sha256:c94c3d1f2f51e6424074b9ec43eac3eedcf532d3dcd77351ec1383a87d3049c6":
                    raise RuntimeError("The host's MSP fingerprint changed.")
                host.send({"jsonrpc": "2.0", "method": "initialized"})
                session = host.request("session/start", {"commandId": command_id(), "workspaceRoot": str(primary),
                    "workspaceRoots": [str(primary), str(reference)], "approvalMode": "promptUnmatched", "providerId": "meta", "modelId": "muse-spark-1.3-contributor", "recoveryMode": "on"})["session"]
                summary["sessionId"] = session["sessionId"]
                summary["profile"] = session["permissionState"]
                command = shlex.join(["/usr/bin/python3", str(script)])
                summary["attemptedTurns"] = 1
                host.request("turn/start", {"commandId": command_id(), "sessionId": session["sessionId"], "reasoningEffort": "minimal",
                    "input": [{"type": "text", "text": "This is a disposable local test. Run exactly this bash command once, wait for it, report only its completion and finish. Do not edit files, retry, run other commands or use the network. " + command}]})
                frames = list(host.events)
                end = time.monotonic() + 120
                while not any(frame.get("method") == "turn/completed" for frame in frames):
                    frame = host.receive(end)
                    frames.append(frame)
                    if frame.get("method") in ["approval/request", "approval/requested"]:
                        params = frame["params"]
                        permitted = params.get("subject", {}).get("command") == command and not params.get("protectedWrite", False)
                        choices = params.get("availableChoices", [])
                        choice = next((choice["choiceId"] for choice in choices if permitted and choice.get("scope") == "once" and choice.get("decision") == "approved"), "abort")
                        host.request("approval/decide", {"commandId": command_id(), "sessionId": session["sessionId"], "approvalId": params["approvalId"], "requirementId": params["currentRequirementId"], "choiceId": choice})
                        frames.extend(host.events)
                summary["results"] = json.loads((primary / "results.json").read_text())
                summary["referenceUnchanged"] = all((reference / name).exists() and (reference / name).read_text() == "SAVED_DUMMY_CONTENT\n" for name in ["direct", "chmod", "replace", "link", "rename", "child"])
            except Exception as error:
                summary["error"] = str(error)
            finally:
                host.close()
                auth.unlink(missing_ok=True)
            if mode == "volume":
                summary["verified"] = volume_verified(summary)
            summaries.append(summary)
            (output / "result.json").write_text(json.dumps(summaries, indent=2) + "\n")
            print(json.dumps(summary), flush=True)
            if mode == "volume" and not summary["verified"]:
                raise RuntimeError("Read-only volume verification failed.")


if __name__ == "__main__":
    main()
