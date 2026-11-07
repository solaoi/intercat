"""Loopback HTTP transport for the local /decide service."""

import argparse
from concurrent.futures import TimeoutError
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from uuid import UUID

from .contract import DecisionRequest
from .engine import Engine
from .lifetime import watch_parent
from .scheduler import PRIORITIES, Scheduler


def create_server(scheduler, port=0):
    def reading_identity(key, index):
        if key is None and index is None:
            return None
        if not isinstance(key, str) or len(key) > 64 or isinstance(index, bool) or not isinstance(index, (int, str)):
            raise ValueError("invalid reading identity")
        index = int(index)
        if not 1 <= index <= 2**63 - 1:
            raise ValueError("invalid reading index")
        return str(UUID(key)), index

    class Handler(BaseHTTPRequestHandler):
        def setup(self):
            super().setup()
            self.connection.settimeout(5)

        def log_message(self, *_):
            pass  # Never write prompts to HTTP logs.

        def reply(self, status, value):
            body = json.dumps(value, ensure_ascii=False, allow_nan=False).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def authorized(self):
            # Browser pages cannot use the local service as a cross-origin oracle.
            return not self.headers.get("Origin") and self.headers.get("Host") == f"127.0.0.1:{self.server.server_port}"

        def do_GET(self):
            if not self.authorized():
                self.reply(403, {"error": "loopback clients only"})
            elif self.path != "/health":
                self.reply(404, {"error": "not found"})
            else:
                loaded = scheduler.ready.done() and scheduler.ready.exception() is None and not scheduler.closed
                failure = scheduler.ready.exception() if scheduler.ready.done() else None
                self.reply(200, {"accepting": not scheduler.closed, "modelLoaded": loaded,
                                 "loadError": f"{type(failure).__name__}: {failure}"[:400] if failure else None})

        def do_POST(self):
            started = False

            def finish(status, value):
                if started:
                    self.wfile.write(json.dumps(value, ensure_ascii=False, allow_nan=False).encode())
                else:
                    self.reply(status, value)

            if not self.authorized():
                self.reply(403, {"error": "loopback clients only"})
                return
            if self.path not in {"/decide", "/tokens", "/generate", "/privacy", "/cancel-reading"}:
                self.reply(404, {"error": "not found"})
                return
            if not scheduler.ready.done() or scheduler.ready.exception() or scheduler.closed:
                self.reply(503, {"error": "editor unavailable"})
                return
            try:
                priority = self.headers.get("X-Intercat-Priority", "input")
                if priority not in PRIORITIES:
                    raise ValueError("invalid inference priority")
                timeout_ms = int(self.headers.get("X-Intercat-Timeout-Ms", "120000"))
                if not 1 <= timeout_ms <= 120000:
                    raise ValueError("timeout must be between 1 and 120000 milliseconds")
                size = int(self.headers.get("Content-Length", "0"))
                # The token probe must accept the complete 25 MiB request,
                # including escapes in both the JSON tail and HTTP envelope.
                limit = 176 * 1024 * 1024 if self.path == "/tokens" else 1024 * 1024
                if not 0 < size <= limit:
                    raise ValueError(f"request body must contain 1–{limit} bytes")
                if self.headers.get("Content-Type", "").split(";")[0] != "application/json":
                    raise ValueError("Content-Type must be application/json")
                value = json.loads(self.rfile.read(size))
                if self.path == "/cancel-reading":
                    if not isinstance(value, dict) or set(value) != {"input", "index"}:
                        raise ValueError("invalid reading cancellation")
                    identity = reading_identity(value["input"], value["index"])
                    if identity is None:
                        raise ValueError("missing reading identity")
                    scheduler.cancel_reading(*identity)
                    self.reply(200, {"cancelled": True})
                    return
                reading = reading_identity(self.headers.get("X-Intercat-Reading"), self.headers.get("X-Intercat-Reading-Index"))
                if self.path == "/tokens":
                    if not isinstance(value, dict) or set(value) != {"text"} or not isinstance(value["text"], str):
                        raise ValueError("token counting requires a text string")
                    future = scheduler.submit("tokens", value["text"], priority, timeout=timeout_ms / 1000, reading=reading)
                elif self.path in {"/generate", "/privacy"}:
                    if not isinstance(value, dict) or set(value) != {"prefix", "tail", "maxTokens"}:
                        raise ValueError("generation requires prefix, tail and maxTokens")
                    if any(not isinstance(value[field], str) for field in ("prefix", "tail")):
                        raise ValueError("generation context must be text")
                    if type(value["maxTokens"]) is not int or not 1 <= value["maxTokens"] <= 512:
                        raise ValueError("generation requires 1–512 output tokens")
                    suffix = "\nFindings:\n" if self.path == "/privacy" else "\nDescription:\n"
                    payload = {"prompt": value["prefix"] + value["tail"] + suffix, "maxTokens": value["maxTokens"]}
                    future = scheduler.submit("generate", payload, priority if self.path == "/privacy" else "background", timeout=timeout_ms / 1000, reading=reading)
                else:
                    request = DecisionRequest.parse(value)
                    future = scheduler.submit("decide", request, priority, timeout=timeout_ms / 1000, reading=reading)
                if not future.started.wait(timeout=timeout_ms / 1000):
                    future.cancel()
                    self.reply(504, {"error": "queue timed out"})
                    return
                # Headers signal GPU admission; queue time must not consume the
                # daemon's inference budget. JSON follows when work finishes.
                if future.cancelled():
                    self.reply(503, {"error": "editor unavailable"})
                    return
                self.send_response(200)
                self.send_header("Content-Type", "application/json; charset=utf-8")
                self.send_header("X-Intercat-Inference", "started")
                self.end_headers()
                self.wfile.flush()
                started = True
                finish(200, future.result(timeout=120))
            except (ValueError, UnicodeError) as error:
                finish(400, {"error": str(error)})
            except TimeoutError:
                future.cancel()
                finish(504, {"error": "inference timed out"})
            except (BrokenPipeError, ConnectionResetError):
                pass  # The caller can stop waiting without cancelling GPU work.
            except Exception:
                try:
                    finish(503, {"error": "inference failed"})
                except (BrokenPipeError, ConnectionResetError):
                    pass

    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.daemon_threads = True
    return server


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True, help="local model directory")
    parser.add_argument("--port", type=int, default=0)
    args = parser.parse_args()
    watch_parent()
    scheduler = Scheduler(lambda: Engine(args.model))
    server = create_server(scheduler, args.port)
    print(json.dumps({"host": "127.0.0.1", "port": server.server_port}), flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
        scheduler.close()


if __name__ == "__main__":
    main()
