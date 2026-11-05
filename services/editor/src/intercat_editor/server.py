"""Loopback HTTP transport for the local /decide service."""

import argparse
from concurrent.futures import TimeoutError
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json

from .contract import DecisionRequest
from .engine import Engine
from .lifetime import watch_parent
from .scheduler import Scheduler


def create_server(scheduler, port=0):
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
            if self.path != "/decide":
                self.reply(404, {"error": "not found"})
                return
            if not scheduler.ready.done() or scheduler.ready.exception() or scheduler.closed:
                self.reply(503, {"error": "editor unavailable"})
                return
            try:
                size = int(self.headers.get("Content-Length", "0"))
                if not 0 < size <= 1024 * 1024:
                    raise ValueError("request body must contain 1–1048576 bytes")
                if self.headers.get("Content-Type", "").split(";")[0] != "application/json":
                    raise ValueError("Content-Type must be application/json")
                value = json.loads(self.rfile.read(size))
                request = DecisionRequest.parse(value)
                future = scheduler.submit("decide", request)
                if not future.started.wait(timeout=120):
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
