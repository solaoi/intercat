"""V43: clone the verified Muse binary and check its local MSP identity."""

import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import tempfile
import time


VERSION = "Muse Code 1.4.14 (1.4.14-R7309.1)"
FINGERPRINT = "sha256:c94c3d1f2f51e6424074b9ec43eac3eedcf532d3dcd77351ec1383a87d3049c6"


def digest(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def clone(source, target):
    libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib", use_errno=True)
    libc.clonefile.argtypes = [ctypes.c_char_p, ctypes.c_char_p, ctypes.c_int]
    libc.clonefile.restype = ctypes.c_int
    if libc.clonefile(os.fsencode(source), os.fsencode(target), 0):
        raise OSError(ctypes.get_errno(), "clonefile failed")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    save = lambda name, value: (args.output / name).write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")
    source = args.source.resolve(strict=True)
    binary = args.output.resolve() / "muse"
    original_hash = digest(source)
    started = time.perf_counter()
    clone(source, binary)
    clone_ms = (time.perf_counter() - started) * 1000
    assert digest(binary) == original_hash
    version = subprocess.check_output([str(binary), "--version"], text=True, timeout=10).strip()
    assert version == VERSION, version
    with tempfile.TemporaryDirectory(prefix="intercat-copy-probe-") as root:
        # Simulate an updater removing its source, without modifying the user's binary.
        updater_source = Path(root) / "updater-source"
        surviving_copy = Path(root) / "surviving-copy"
        clone(binary, updater_source)
        clone(updater_source, surviving_copy)
        updater_source.unlink()
        assert digest(surviving_copy) == original_hash
        assert subprocess.check_output([str(surviving_copy), "--version"], text=True, timeout=10).strip() == VERSION
        # Detect damage before executing a binary. Never run the damaged copy.
        damaged = Path(root) / "damaged"
        clone(binary, damaged)
        with damaged.open("r+b") as file:
            first = file.read(1)
            file.seek(0)
            file.write(bytes([first[0] ^ 1]))
        assert digest(damaged) != original_hash
        config = Path(root) / "config"
        (config / "muse").mkdir(parents=True)
        (config / "muse/settings.json").write_text(json.dumps({
            "schema_version": 1, "provider": "meta", "model": "muse-spark-1.3-contributor",
            "contributor_mode_enabled": False, "agents": {"execution_capacity": 1},
            "run": {"toolset": ["read_file", "search"]},
        }))
        env = {key: os.environ[key] for key in ("HOME", "PATH", "LANG") if key in os.environ}
        env.update(XDG_CONFIG_HOME=str(config), MUSE_NO_AUTO_UPDATE="1", INTERCAT_HOST="copy-probe")
        with (args.output / "stderr.txt").open("wb") as stderr:
            process = subprocess.Popen([str(binary), "serve", "--trust-workspace"], cwd=root,
                                       env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                       stderr=stderr, start_new_session=True)
            selector = selectors.DefaultSelector()
            try:
                message = {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                    "clientInfo": {"name": "intercat_copy_probe", "version": "0.1.0"},
                    "capabilities": {"experimentalApi": True, "requestedCapabilities": [], "userInputDialogs": True},
                }}
                process.stdin.write((json.dumps(message) + "\n").encode())
                process.stdin.flush()
                selector.register(process.stdout, selectors.EVENT_READ)
                if not selector.select(timeout=15):
                    raise TimeoutError("initialize did not reply")
                response = json.loads(process.stdout.readline())
                save("initialize.json", response)
                assert response["result"]["schema"]["fingerprint"] == FINGERPRINT
                assert response["result"]["serverInfo"]["version"] == "1.4.14"
                process.stdin.write(b'{"jsonrpc":"2.0","method":"initialized"}\n')
                process.stdin.close()
                process.wait(timeout=10)
            finally:
                selector.close()
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=10)
    result = {"sourceSha256": original_hash, "copySha256": digest(binary), "bytes": binary.stat().st_size,
              "version": version, "fingerprint": FINGERPRINT, "cloneMs": clone_ms,
              "sourceRemovalSurvived": True, "corruptionDetectedWithoutExecution": True,
              "authenticated": False, "modelRequests": 0,
              "limitations": ["Version-specific download availability not tested"]}
    save("result.json", result)
    print(json.dumps(result), flush=True)


if __name__ == "__main__":
    main()
