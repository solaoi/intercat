import threading
import unittest
import json
import subprocess
import sys
import urllib.error
import urllib.request

from intercat_editor.contract import DecisionRequest, distribution
from intercat_editor.scheduler import Scheduler
from intercat_editor.server import create_server


class LifetimeTests(unittest.TestCase):
    def test_server_exits_when_the_daemons_lifetime_pipe_closes(self):
        code = "from unittest.mock import patch; import sys; from intercat_editor import server; sys.argv=['server','--model','unused']; patcher=patch.object(server,'Engine',lambda _: object()); patcher.start(); server.main()"
        process = subprocess.Popen([sys.executable, '-c', code], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            endpoint = json.loads(process.stdout.readline())
            self.assertEqual(endpoint['host'], '127.0.0.1')
            process.stdin.close()
            self.assertEqual(process.wait(timeout=5), 2)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            process.stdout.close()
            process.stderr.close()


def request(**overrides):
    value = {"prefix": "rules\n", "tail": "request", "stages": [
        {"name": "decision", "prompt": "\nDecision:", "candidates": [" A", " B"]},
        {"name": "lane", "prompt": "\nLane: ", "candidates": ["1", "9"], "when": {"decision": [" A"]}},
    ]}
    return DecisionRequest.parse(value | overrides)


class ContractTests(unittest.TestCase):
    def test_conditional_stage_is_validated(self):
        self.assertEqual(request().stages[1].when, {"decision": (" A",)})
        with self.assertRaises(ValueError):
            request(stages=[{"name": "lane", "prompt": "x", "candidates": ["1"], "when": {"future": [" A"]}}])

    def test_temperature_and_labels_reject_ambiguous_inputs(self):
        for bad in [True, 0, -1, float("nan"), float("inf"), "1"]:
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                request(temperature=bad)
        with self.assertRaises(ValueError):
            request(stages=[{"name": "x", "prompt": "x", "candidates": [" A", " A"]}])

    def test_probabilities_are_stable_and_use_temperature(self):
        cold = distribution([-10001, -10000], 1)
        warm = distribution([-10001, -10000], 2)
        self.assertAlmostEqual(sum(cold), 1)
        self.assertGreater(cold[1], warm[1])


class SchedulerTests(unittest.TestCase):
    def test_decision_preempts_background_and_serializes_gpu_access(self):
        entered = threading.Event()
        order = []

        class FakeEngine:
            load_ms = 0
            def memory(self):
                return {}
            def generate(self, payload, max_tokens, preempted):
                entered.set()
                deadline = __import__("time").monotonic() + 2
                while not preempted():
                    if __import__("time").monotonic() > deadline:
                        raise TimeoutError("decision did not preempt generation")
                    threading.Event().wait(0.001)
                order.append("preempted")
                return {"preempted": True}
            def decide(self, payload):
                order.append(payload)
                return {}

        scheduler = Scheduler(FakeEngine)
        try:
            scheduler.ready.result(timeout=2)
            background = scheduler.submit("generate", {"prompt": "x", "maxTokens": 5})
            self.assertTrue(entered.wait(2))
            first = scheduler.submit("decide", "first")
            second = scheduler.submit("decide", "second")
            self.assertTrue(background.result(timeout=2)["preempted"])
            first.result(timeout=2)
            second.result(timeout=2)
            self.assertEqual(order, ["preempted", "first", "second"])
        finally:
            scheduler.close()

    def test_load_failure_rejects_queued_work(self):
        release = threading.Event()
        def fail():
            release.wait(2)
            raise RuntimeError("model unavailable")
        scheduler = Scheduler(fail)
        queued = scheduler.submit("decide", "x")
        release.set()
        with self.assertRaisesRegex(RuntimeError, "model unavailable"):
            scheduler.ready.result(timeout=2)
        with self.assertRaisesRegex(RuntimeError, "model unavailable"):
            queued.result(timeout=2)
        scheduler.close()


class TransportTests(unittest.TestCase):
    def test_response_headers_announce_inference_before_the_body(self):
        import http.client
        release = threading.Event()

        class FakeEngine:
            load_ms = 0
            def memory(self): return {}
            def decide(self, request):
                release.wait(3)
                return {"stages": {"decision": {"selected": " A"}}}

        scheduler = Scheduler(FakeEngine)
        scheduler.ready.result(timeout=2)
        server = create_server(scheduler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        client = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=1)
        try:
            value = {"prefix": "rules", "tail": "hi", "stages": [{"name": "decision", "prompt": "Decision:", "candidates": [" A"]}]}
            client.request("POST", "/decide", json.dumps(value), {"Content-Type": "application/json"})
            response = client.getresponse()
            self.assertEqual(response.getheader("X-Intercat-Inference"), "started")
            self.assertFalse(release.is_set())
            release.set()
            self.assertEqual(json.load(response)["stages"]["decision"]["selected"], " A")
        finally:
            release.set()
            client.close()
            server.shutdown()
            server.server_close()
            scheduler.close()

    def test_http_rejects_bad_requests_and_reports_loading(self):
        ready = threading.Event()
        class FakeEngine:
            load_ms = 0
            def __init__(self):
                ready.wait(2)
            def memory(self):
                return {}
            def decide(self, request):
                return {"stages": {"decision": {"selected": " A"}}}

        scheduler = Scheduler(FakeEngine)
        server = create_server(scheduler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        url = f"http://127.0.0.1:{server.server_port}"
        def post(value, headers=None):
            return urllib.request.urlopen(urllib.request.Request(url + "/decide", data=json.dumps(value).encode(), headers={"Content-Type": "application/json", **(headers or {})}), timeout=2)
        value = {"prefix": "rules", "tail": "hi", "stages": [{"name": "decision", "prompt": "Decision:", "candidates": [" A"]}]}
        try:
            with self.assertRaises(urllib.error.HTTPError) as error:
                post(value)
            self.assertEqual(error.exception.code, 503)
            ready.set()
            scheduler.ready.result(timeout=2)
            with self.assertRaises(urllib.error.HTTPError) as error:
                post(value, {"Origin": "https://example.invalid"})
            self.assertEqual(error.exception.code, 403)
            with self.assertRaises(urllib.error.HTTPError) as error:
                post({"prefix": "missing fields"})
            self.assertEqual(error.exception.code, 400)
            with post(value) as response:
                self.assertEqual(json.load(response)["stages"]["decision"]["selected"], " A")
        finally:
            ready.set()
            server.shutdown()
            server.server_close()
            scheduler.close()


if __name__ == "__main__":
    unittest.main()
