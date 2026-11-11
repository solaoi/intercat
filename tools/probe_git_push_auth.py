"""V60: sandboxed Git authentication against generated loopback fixtures only."""
import argparse
import base64
from contextlib import ExitStack
import http.server
import json
import os
from pathlib import Path
import pwd
import shlex
import socket
import ssl
import subprocess
import threading
import time


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    home = output / "home"
    home.mkdir()
    env = {"HOME": str(home), "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
           "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1", "GIT_TERMINAL_PROMPT": "0"}
    git = subprocess.check_output(["/usr/bin/xcrun", "--find", "git"], text=True).strip()
    calls = []
    result = {"probe": "V60", "paidTurns": 0, "verified": False}
    owned = []

    def run(command, cwd=output, extra=None, data=None, checked=True, sandbox=None):
        command = list(map(str, command))
        if sandbox:
            command = ["/usr/bin/sandbox-exec", "-p", sandbox, *command]
        p = subprocess.run(command, cwd=cwd, env={**env, **(extra or {})}, input=data,
                           capture_output=True, timeout=30)
        calls.append({"args": command, "code": p.returncode, "stderr": p.stderr.decode(errors="replace")})
        if checked and p.returncode:
            raise RuntimeError(calls[-1])
        return p.stdout.decode().strip()

    def start(command, log):
        stream = (output / log).open("wb")
        p = subprocess.Popen(list(map(str, command)), env=env, stdout=stream, stderr=stream)
        owned.append((p, stream))
        return p

    def profile(network):
        paths = [output, Path(git).parents[2]]
        return ('(version 1)(deny default)(allow process-fork)(allow process-exec)'
                '(allow process-info*)(allow sysctl-read)(allow mach-lookup)(allow signal (target self))'
                '(allow file-read-metadata)(allow file-read* (literal "/") (subpath "/System")'
                ' (subpath "/usr") (subpath "/bin") (subpath "/sbin") (subpath "/dev")'
                ' (subpath "/private/var/db/dyld") (subpath "/private/preboot")'
                ' (subpath "/private/etc/ssl")'
                + ''.join(f' (subpath {json.dumps(str(path))})' for path in paths) + ')'
                + f'(allow file-write* (subpath {json.dumps(str(output))}) (literal "/dev/null"))'
                + ('(allow network*)' if network else ''))

    def free_port():
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            return s.getsockname()[1]

    try:
        source = output / "source"
        source.mkdir()
        run([git, "init", "-q", "-b", "main"], cwd=source)
        run([git, "config", "user.name", "Dummy"], cwd=source)
        run([git, "config", "user.email", "dummy@example.invalid"], cwd=source)
        (source / "dummy.txt").write_text("DUMMY_AUTH_PUSH\n")
        run([git, "add", "-A"], cwd=source)
        run([git, "commit", "-qm", "Generated authentication fixture"], cwd=source)
        sha = run([git, "rev-parse", "HEAD"], cwd=source)
        remotes = output / "remotes"
        remotes.mkdir()
        for name in ["https.git", "ssh.git"]:
            run([git, "init", "--bare", "-q", remotes / name])
        cert, key = output / "localhost.crt", output / "localhost.key"
        run(["/usr/bin/openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
             "-keyout", key, "-out", cert, "-subj", "/CN=localhost", "-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"])
        key.chmod(0o600)
        backend = Path(run([git, "--exec-path"])) / "git-http-backend"
        authorized = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                self.handle_git()

            def do_POST(self):
                self.handle_git()

            def handle_git(self):
                expected = "Basic " + base64.b64encode(b"dummy:dummy-local-only").decode()
                if self.headers.get("Authorization") != expected:
                    self.send_response(401)
                    self.send_header("WWW-Authenticate", 'Basic realm="Intercat dummy"')
                    self.end_headers()
                    return
                path, _, query = self.path.partition("?")
                if not path.startswith("/https.git/"):
                    self.send_error(404)
                    return
                authorized.append(path)
                body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                cgi = {**env, "GIT_PROJECT_ROOT": str(remotes), "GIT_HTTP_EXPORT_ALL": "1",
                       "PATH_INFO": path, "QUERY_STRING": query, "REQUEST_METHOD": self.command,
                       "CONTENT_TYPE": self.headers.get("Content-Type", ""), "CONTENT_LENGTH": str(len(body)),
                       "REMOTE_USER": "dummy", "SERVER_PROTOCOL": "HTTP/1.1"}
                response = subprocess.run([backend], env=cgi, input=body, capture_output=True, timeout=20)
                headers, _, content = response.stdout.partition(b"\r\n\r\n")
                pairs = [line.decode().split(":", 1) for line in headers.split(b"\r\n") if b":" in line]
                status = next((int(value.strip().split()[0]) for name, value in pairs if name.lower() == "status"), 200)
                self.send_response(status)
                for name, value in pairs:
                    if name.lower() != "status":
                        self.send_header(name, value.strip())
                self.end_headers()
                self.wfile.write(content)

        with ExitStack() as stack:
            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.load_cert_chain(cert, key)
            server.socket = context.wrap_socket(server.socket, server_side=True)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            stack.callback(server.server_close)
            stack.callback(server.shutdown)
            url = f"https://127.0.0.1:{server.server_port}/https.git"
            helper = output / "credential-helper.sh"
            helper.write_text('#!/bin/sh\nif [ "$1" = get ]; then printf "username=dummy\\npassword=dummy-local-only\\n"; fi\n')
            helper.chmod(0o700)
            run([git, "-c", f"http.sslCAInfo={cert}", "-c", "credential.helper=",
                 "-c", f"credential.helper={helper}", "push", "--no-verify", "--no-follow-tags", url,
                 f"{sha}:refs/heads/main"], cwd=source, sandbox=profile(True))
            assert authorized and run([git, "--git-dir", remotes / "https.git", "rev-parse", "refs/heads/main"]) == sha
            result["httpsCredentialHelper"] = True
            # Exact pre-push argv and stdin, with all network operations denied.
            hook = output / "pre-push"
            hook.write_text('#!/bin/sh\nprintf "%s\\n%s\\n" "$1" "$2" > "$INTERCAT_DUMMY_HOOK_ARGS"\n/bin/cat > "$INTERCAT_DUMMY_HOOK_INPUT"\n')
            hook.chmod(0o700)
            stdin = f"refs/heads/main {sha} refs/heads/main {'0' * 40}\n"
            hook_env = {"GIT_LFS_SKIP_PUSH": "1", "INTERCAT_DUMMY_HOOK_ARGS": str(output / "hook.args"),
                        "INTERCAT_DUMMY_HOOK_INPUT": str(output / "hook.input")}
            run([hook, "origin", url], cwd=source, extra=hook_env, data=stdin.encode(), sandbox=profile(False))
            assert (output / "hook.args").read_text() == f"origin\n{url}\n"
            assert (output / "hook.input").read_text() == stdin
            denied = run(["/usr/bin/curl", "--max-time", "2", "--cacert", cert, url],
                         checked=False, sandbox=profile(False))
            # An empty stdout alone is insufficient; inspect the actual recorded exit code.
            assert calls[-1]["code"] != 0
            result["offlinePrePush"] = True
            result["prePushNetworkDenied"] = True
        for name in ["client", "host"]:
            run(["/usr/bin/ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", output / name])
        sock = output / "agent.sock"
        start(["/usr/bin/ssh-agent", "-D", "-a", sock], "agent.log")
        for _ in range(50):
            if sock.exists():
                break
            time.sleep(.05)
        run(["/usr/bin/ssh-add", output / "client"], extra={"SSH_AUTH_SOCK": str(sock)})
        port = free_port()
        config = output / "sshd.conf"
        user = pwd.getpwuid(os.getuid()).pw_name
        config.write_text(f"Port {port}\nListenAddress 127.0.0.1\nHostKey {output / 'host'}\n"
                          f"PidFile {output / 'sshd.pid'}\nAuthorizedKeysFile {output / 'client.pub'}\n"
                          f"AllowUsers {user}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n"
                          "UsePAM no\nStrictModes no\nLogLevel ERROR\n")
        run(["/usr/sbin/sshd", "-t", "-f", config])
        daemon = start(["/usr/sbin/sshd", "-D", "-e", "-f", config], "sshd.log")
        for _ in range(50):
            if daemon.poll() is not None:
                raise RuntimeError((output / "sshd.log").read_text())
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=.1):
                    break
            except OSError:
                time.sleep(.05)
        public = (output / "host.pub").read_text().split()
        known = output / "known_hosts"
        known.write_text(f"[127.0.0.1]:{port} {public[0]} {public[1]}\n")
        ssh = shlex.join(["/usr/bin/ssh", "-F", "/dev/null", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
                          "-o", f"IdentityAgent={sock}", "-o", f"UserKnownHostsFile={known}", "-o", "StrictHostKeyChecking=yes",
                          "-i", str(output / "client.pub")])
        ssh_url = f"ssh://{user}@127.0.0.1:{port}{remotes / 'ssh.git'}"
        run([git, "push", "--no-verify", "--no-follow-tags", ssh_url, f"{sha}:refs/heads/main"], cwd=source,
            extra={"GIT_SSH_COMMAND": ssh, "SSH_AUTH_SOCK": str(sock)}, sandbox=profile(True))
        assert run([git, "--git-dir", remotes / "ssh.git", "rev-parse", "refs/heads/main"]) == sha
        result.update(sshAgent=True, verified=True)
    except Exception as error:
        result["error"] = str(error)
    finally:
        for process, stream in reversed(owned):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            stream.close()
        (output / "calls.json").write_text(json.dumps(calls, indent=2) + "\n")
        (output / "verification.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result), flush=True)
    raise SystemExit(0 if result["verified"] else 1)


if __name__ == "__main__":
    main()
