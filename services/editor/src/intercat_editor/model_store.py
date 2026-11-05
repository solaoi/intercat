"""Install the pinned Editor artifact using resumable Hub downloads."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
from urllib.request import Request, urlopen
from .lifetime import watch_parent


MANIFEST = json.loads(Path(__file__).with_name("model_manifest.json").read_text())


def matches(path, entry):
    try:
        info = path.lstat()
    except FileNotFoundError:
        return False
    if not stat.S_ISREG(info.st_mode):
        raise ValueError(f"Model file is not a regular file: {entry['path']}")
    if info.st_size != entry["size"]:
        return False
    digest = hashlib.sha256() if entry["algorithm"] == "sha256" else hashlib.sha1()
    if entry["algorithm"] == "git-sha1":
        digest.update(f"blob {info.st_size}\0".encode())
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(4 * 1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest() == entry["digest"]


def verify_model(model, manifest=MANIFEST):
    model = Path(model)
    for entry in manifest["files"]:
        if not matches(model / entry["path"], entry):
            raise ValueError(f"Editor model file is missing or damaged: {entry['path']}")


def remote_file(manifest, entry):
    from huggingface_hub import get_hf_file_metadata, hf_hub_url
    url = hf_hub_url(manifest['repository'], entry['path'], revision=manifest['revision'])
    metadata = get_hf_file_metadata(url, token=False)
    if (metadata.commit_hash != manifest['revision'] or metadata.size != entry['size']
            or metadata.etag != entry['digest']):
        raise ValueError('The Editor artifact metadata differs from the pinned revision.')
    return metadata.location


def download_file(manifest, entry, path, partials, progress, resolve=remote_file):
    # hf_hub_download 2.4 uses process-unique temporary files and cannot resume
    # across calls. Keep our pinned partial and use standard HTTP Range instead.
    partial = partials / entry['digest']
    try:
        info = partial.lstat()
    except FileNotFoundError:
        info = None
    if info and (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1):
        raise ValueError('The partial Editor download must be a private regular file.')
    offset = info.st_size if info else 0
    if offset > entry['size']:
        partial.unlink(); offset = 0
    if offset != entry['size']:
        url = resolve(manifest, entry)
        request = Request(url, headers={'Range': f'bytes={offset}-'} if offset else {})
        with urlopen(request, timeout=30) as response:
            if response.status == 206:
                expected = f'bytes {offset}-{entry["size"] - 1}/{entry["size"]}'
                if response.headers.get('Content-Range') != expected:
                    raise ValueError('Invalid Editor download range.')
            elif response.status == 200:
                offset = 0
            else:
                raise ValueError('The Editor download did not return file data.')
            descriptor = os.open(partial, os.O_WRONLY | os.O_CREAT | os.O_NOFOLLOW | (os.O_APPEND if offset else os.O_TRUNC), 0o600)
            with os.fdopen(descriptor, 'ab' if offset else 'wb') as stream:
                if os.fstat(stream.fileno()).st_nlink != 1:
                    raise ValueError('The partial Editor download is shared with another file.')
                try:
                    while chunk := response.read(4 * 1024 * 1024):
                        if offset + len(chunk) > entry['size']:
                            raise ValueError('The Editor download exceeds its expected size.')
                        stream.write(chunk); stream.flush()
                        offset += len(chunk)
                        progress(offset)
                finally:
                    stream.flush(); os.fsync(stream.fileno())
    if not matches(partial, entry):
        partial.unlink(missing_ok=True)
        raise ValueError(f'The downloaded Editor file failed verification: {entry["path"]}')
    os.replace(partial, path)


def install(root, emit, manifest=MANIFEST, download=None):
    # Neither the user's selected model nor a shared Hub cache is modified.
    import fcntl

    root = Path(root)
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    if not stat.S_ISDIR(root.lstat().st_mode):
        raise ValueError("The Editor download directory must not be a link.")
    with (root / "install.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        model = root / "model"
        model.mkdir(exist_ok=True, mode=0o700)
        if not stat.S_ISDIR(model.lstat().st_mode):
            raise ValueError("The Editor model directory must not be a link.")
        partials = root / 'partial'
        partials.mkdir(exist_ok=True, mode=0o700)
        if not stat.S_ISDIR(partials.lstat().st_mode):
            raise ValueError('The partial Editor directory must not be a link.')
        marker = root / "complete.json"
        marker.unlink(missing_ok=True)
        total = sum(entry["size"] for entry in manifest["files"])
        completed = 0
        fetch = download or download_file
        for entry in manifest["files"]:
            path = model / entry["path"]
            emit({"phase": "checking", "received": completed, "total": total, "file": entry["path"]})
            valid = matches(path, entry)
            if not valid:
                def progress(received):
                    emit({"phase": "downloading", "received": completed + received,
                          "total": total, "file": entry['path']})
                fetch(manifest, entry, path, partials, progress)
                emit({"phase": "verifying", "received": completed, "total": total, "file": entry["path"]})
                if not matches(path, entry):
                    raise ValueError(f"The downloaded Editor file failed verification: {entry['path']}")
            completed += entry["size"]
            with path.open("rb") as stream:
                os.fsync(stream.fileno())
        # Only a fully verified and synced artifact can be selected at startup.
        with tempfile.NamedTemporaryFile(mode='w', dir=root, prefix='.complete-', delete=False) as stream:
            temporary = Path(stream.name)
            json.dump({"revision": manifest["revision"]}, stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, marker)
        for folder in (model, root):
            descriptor = os.open(folder, os.O_RDONLY)
            try:
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
        emit({"phase": "complete", "received": total, "total": total, "model": str(model)})
        return model


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True, type=Path)
    args = parser.parse_args()
    # Closing the daemon's private lifetime pipe also ends a download after a
    # daemon crash. Partial Hub files remain available for an explicit resume.
    watch_parent()
    def emit(value):
        print(json.dumps(value), flush=True)
    try:
        import mlx.core as mx
        from mlx_lm import load as _load
        if sys.platform != "darwin" or not mx.metal.is_available():
            raise RuntimeError("The local Editor requires an Apple silicon Mac with Metal.")
        memory = int(subprocess.check_output(["/usr/sbin/sysctl", "-n", "hw.memsize"], text=True))
        if memory < 32 * 1024**3:
            raise RuntimeError("This Mac can’t run the Editor (needs 32 GB).")
        install(args.root, emit)
    except Exception as error:
        emit({"phase": "failed", "detail": f"{type(error).__name__}: {error}"[:400]})
        raise SystemExit(1)


if __name__ == "__main__":
    main()
