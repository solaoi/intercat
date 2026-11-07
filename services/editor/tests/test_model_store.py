import hashlib
import json
from pathlib import Path
import tempfile
import unittest
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from intercat_editor.model_store import download_file, install, verify_model


class ModelStoreTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name) / "editor"
        self.data = {"config.json": b'{"model":"test"}', "weights.safetensors": b"dummy weights"}
        self.manifest = {"repository": "fixture/model", "revision": "a" * 40, "files": [
            {"path": name, "size": len(value), "algorithm": "sha256", "digest": hashlib.sha256(value).hexdigest()}
            for name, value in self.data.items()]}
        self.calls = []
        self.progress = []

    def tearDown(self):
        self.temporary.cleanup()

    def download(self, manifest, entry, path, partials, progress):
        self.calls.append((entry["path"], path.exists()))
        self.assertEqual(manifest["revision"], self.manifest["revision"])
        path.write_bytes(self.data[entry["path"]])

    def test_verified_files_are_reused_and_only_corruption_is_replaced(self):
        model = install(self.root, self.progress.append, self.manifest, self.download)
        self.assertEqual(len(self.calls), 2)
        verify_model(model, self.manifest)
        self.calls.clear()
        install(self.root, self.progress.append, self.manifest, self.download)
        self.assertEqual(self.calls, [])
        (model / "config.json").write_bytes(b"broken")
        install(self.root, self.progress.append, self.manifest, self.download)
        self.assertEqual(self.calls, [("config.json", True)])
        self.assertEqual(json.loads((self.root / "complete.json").read_text())["revision"], self.manifest["revision"])

    def test_interruption_keeps_completed_files_but_never_marks_a_partial_model_ready(self):
        def interrupted(manifest, entry, path, partials, progress):
            if entry["path"] == "weights.safetensors":
                raise ConnectionError("interrupted transfer")
            self.download(manifest, entry, path, partials, progress)
        with self.assertRaises(ConnectionError):
            install(self.root, self.progress.append, self.manifest, interrupted)
        self.assertFalse((self.root / "complete.json").exists())
        self.calls.clear()
        install(self.root, self.progress.append, self.manifest, self.download)
        self.assertEqual(self.calls, [("weights.safetensors", False)])

    def test_a_wrong_download_or_link_is_never_accepted_or_followed(self):
        def incorrect(manifest, entry, path, partials, progress):
            path.write_bytes(b"not the model")
        with self.assertRaisesRegex(ValueError, "failed verification"):
            install(self.root, self.progress.append, self.manifest, incorrect)
        self.assertFalse((self.root / "complete.json").exists())
        outside = Path(self.temporary.name) / "outside"
        outside.write_bytes(b"private")
        target = self.root / "model/config.json"
        target.unlink()
        target.symlink_to(outside)
        with self.assertRaisesRegex(ValueError, "regular file"):
            install(self.root, self.progress.append, self.manifest, self.download)
        self.assertEqual(outside.read_bytes(), b"private")

    def test_normal_completion_does_not_wait_for_the_lifetime_pipe(self):
        code = """from unittest.mock import patch
import sys, types
from intercat_editor import model_store
sys.argv=['model_store','--root',sys.argv[1]]
core=types.ModuleType('mlx.core')
core.metal=types.SimpleNamespace(is_available=lambda: True)
mlx=types.ModuleType('mlx'); mlx.core=core
with patch.dict(sys.modules, {'mlx':mlx,'mlx.core':core,'mlx_lm':types.SimpleNamespace(load=lambda *_: None)}), patch.object(model_store.subprocess,'check_output',return_value=str(64*1024**3)), patch.object(model_store,'install',lambda root,emit: None):
    model_store.main()
"""
        process = subprocess.Popen([sys.executable, '-c', code, str(self.root)], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            self.assertEqual(process.wait(timeout=5), 0, process.stderr.read().decode())
        finally:
            process.stdin.close(); process.stdout.close(); process.stderr.close()

    def test_hub_download_resumes_the_partial_file_with_an_http_range(self):
        body = b"x" * (24 * 1024 * 1024)
        digest = hashlib.sha256(body).hexdigest()
        manifest = {"repository": "fixture/model", "revision": "a" * 40, "files": [
            {"path": "weights.safetensors", "size": len(body), "algorithm": "sha256", "digest": digest}]}
        ranges = []
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_): pass
            def do_HEAD(self):
                self.send_response(200)
                self.send_header('X-Repo-Commit', manifest['revision'])
                self.send_header('ETag', digest)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
            def do_GET(self):
                value = self.headers.get('Range')
                ranges.append(value)
                start = int(value.split('=')[1].split('-')[0]) if value else 0
                self.send_response(206 if start else 200)
                self.send_header('Content-Length', str(len(body)-start))
                if start: self.send_header('Content-Range', f'bytes {start}-{len(body)-1}/{len(body)}')
                self.end_headers()
                try:
                    for offset in range(start, len(body), 1024 * 1024):
                        self.wfile.write(body[offset:offset+1024*1024]); self.wfile.flush(); time.sleep(.02)
                except (BrokenPipeError, ConnectionResetError): pass
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        def fetch(manifest, entry, path, partials, progress):
            return download_file(manifest, entry, path, partials, progress, lambda *_: f'http://127.0.0.1:{server.server_port}/weights')
        def interrupt(progress):
            if progress['phase'] == 'downloading' and progress['received'] > 0:
                raise InterruptedError('user paused')
        try:
            with self.assertRaises(InterruptedError):
                install(self.root, interrupt, manifest, fetch)
            self.assertFalse((self.root / 'complete.json').exists())
            install(self.root, self.progress.append, manifest, fetch)
            self.assertTrue(any(value and value != 'bytes=0-' for value in ranges))
            verify_model(self.root / 'model', manifest)
        finally:
            server.shutdown(); server.server_close()


if __name__ == "__main__":
    unittest.main()
