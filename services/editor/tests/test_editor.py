import threading
import unittest
import json
import subprocess
import sys
import urllib.error
import urllib.request

from intercat_editor.contract import DecisionRequest, distribution
from intercat_editor.engine import Engine
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
    value = {"cachePrefix": False, "prefix": "rules\n", "tail": "request", "stages": [
        {"name": "decision", "prompt": "\nDecision:", "candidates": [" A", " B"]},
        {"name": "lane", "prompt": "\nLane: ", "candidates": ["1", "9"], "when": {"decision": [" A"]}},
    ]}
    return DecisionRequest.parse(value | overrides)


class ContractTests(unittest.TestCase):
    def test_batched_report_labels_keep_independent_stages(self):
        stages = [{"name": f"question_{index}", "prompt": f"Report {index}:",
                   "candidates": ["0", "1", "2"]} for index in range(301)]
        self.assertEqual(len(request(stages=stages).stages), 301)
        with self.assertRaises(ValueError):
            request(stages=stages + [{"name": "overflow", "prompt": "Report:", "candidates": ["0"]}])

    def test_cache_mode_must_be_an_explicit_boolean(self):
        self.assertFalse(request().cache_prefix)
        self.assertTrue(request(cachePrefix=True).cache_prefix)
        for value in [None, 0, 1, 'false', []]:
            with self.subTest(value=value), self.assertRaises(ValueError):
                request(cachePrefix=value)
        with self.assertRaises(ValueError):
            DecisionRequest.parse({'prefix': 'x', 'tail': 'y', 'stages': [
                {'name': 'decision', 'prompt': 'Choose:', 'candidates': [' A', ' B']} ]})

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


class PrefixCacheTests(unittest.TestCase):
    def setUp(self):
        from types import SimpleNamespace
        self.engine = Engine.__new__(Engine)
        self.engine._prefix_ids = self.engine._prefix_cache = None
        self.engine.mx = SimpleNamespace(array=lambda value: value, eval=lambda *_: None)
        self.engine.make_cache = lambda _: [SimpleNamespace(state=[])]
        self.prefills = []
        def model(tokens, cache):
            self.prefills.append(tokens[0])
            cache[0].state.extend(tokens[0])
        self.engine.model = model

    def test_reused_prefix_does_not_include_the_previous_request(self):
        first, tail, hit, count = self.engine._initial_cache([1, 2], [1, 2, 3], True)
        self.assertEqual((tail, hit, count), ([3], False, 2))
        first[0].state.append(3)
        second, tail, hit, count = self.engine._initial_cache([1, 2], [1, 2, 4], True)
        self.assertEqual((second[0].state, tail, hit, count), ([1, 2], [4], True, 2))
        self.assertEqual(self.prefills, [[1, 2]])
        changed, _, hit, _ = self.engine._initial_cache([8, 9], [8, 9, 4], True)
        self.assertFalse(hit)
        self.assertEqual(changed[0].state, [8, 9])

    def test_boundary_merges_are_excluded_and_uncached_requests_keep_all_tokens(self):
        cache, tail, hit, count = self.engine._initial_cache([1, 2, 3], [1, 2, 30, 4], True)
        self.assertEqual((cache[0].state, tail, hit, count), ([1, 2], [30, 4], False, 2))
        cache, tail, hit, count = self.engine._initial_cache([1, 2, 3], [1, 2, 30, 4], False)
        self.assertEqual((cache[0].state, tail, hit, count), ([], [1, 2, 30, 4], False, 0))
        _, tail, _, count = self.engine._initial_cache([1, 2], [1, 2], True)
        self.assertEqual((tail, count), ([2], 1))
        cache, tail, hit, count = self.engine._initial_cache([9], [8, 7], True)
        self.assertEqual((cache[0].state, tail, hit, count), ([], [8, 7], False, 0))


class LogProbabilityTests(unittest.TestCase):
    def test_vocabulary_normalization_uses_float32_before_subtraction(self):
        import math
        from types import SimpleNamespace
        import numpy as np

        engine = Engine.__new__(Engine)
        engine.mx = SimpleNamespace(
            array=np.array, float32=np.float32,
            logsumexp=lambda values, axis, keepdims: np.logaddexp.reduce(values, axis=axis, keepdims=keepdims),
            eval=lambda *values: None, clear_cache=lambda: None,
        )
        engine.tokenizer = SimpleNamespace(encode=lambda text, **options: [1, 2])
        engine.make_cache = lambda model: []
        engine.label_id = {" A": 0, " B": 1, " C": 2}.__getitem__
        engine.model = lambda *args, **kwargs: np.array([[[20, 20, 15]]], dtype=np.float16)
        payload = DecisionRequest.parse({
            "prefix": "rules", "tail": "request", "temperature": 1.0, "cachePrefix": False,
            "stages": [{"name": "decision", "prompt": "Decision:", "candidates": [" A", " B", " C"]}],
        })
        stage = engine.decide(payload)["stages"]["decision"]
        self.assertAlmostEqual(sum(math.exp(value) for value in stage["logprobs"].values()), 1.0, places=4)
        self.assertEqual(stage["selected"], " A")
        self.assertAlmostEqual(stage["logprobs"][" A"] - stage["logprobs"][" C"], 5.0)


class GenerationTests(unittest.TestCase):
    def generate(self, chunks, preempted=lambda: False):
        from types import SimpleNamespace
        from unittest.mock import Mock, patch
        engine = Engine.__new__(Engine)
        engine.tokenizer = SimpleNamespace(encode=lambda *args, **kwargs: [1])
        engine.model = object()
        engine.mx = SimpleNamespace(synchronize=Mock(), clear_cache=Mock())
        closed = []
        emitted = []

        def stream(*args, **kwargs):
            try:
                for index, text in enumerate(chunks, 1):
                    emitted.append(text)
                    yield SimpleNamespace(text=text, generation_tokens=index)
            finally:
                closed.append(True)

        with patch.dict(sys.modules, {'mlx_lm': SimpleNamespace(stream_generate=stream)}):
            result = engine.generate('Return JSON', 512, preempted)
        self.assertEqual(closed, [True])
        engine.mx.synchronize.assert_called_once()
        engine.mx.clear_cache.assert_called_once()
        return result, emitted

    def test_json_end_can_share_a_chunk_with_the_next_message(self):
        result, emitted = self.generate(['  {"assumptions":[]}<|message|>more', 'not consumed'])
        self.assertEqual(result, {'text': '{"assumptions":[]}', 'tokens': 1, 'preempted': False})
        self.assertEqual(len(emitted), 1)

    def test_nested_json_and_escaped_braces_are_not_early_boundaries(self):
        result, emitted = self.generate(['{ "text": "brace } and ', '\\"quote\\"", "nested": [', '{"value": 1}]}trailing'])
        self.assertEqual(json.loads(result['text']), {'text': 'brace } and "quote"', 'nested': [{'value': 1}]})
        self.assertEqual(result['tokens'], 3)
        self.assertEqual(len(emitted), 3)

    def test_leading_prose_and_incomplete_objects_are_not_repaired(self):
        for text in ['Explanation {"ok":true}', '{"unfinished":', '[{"ok":true}]']:
            with self.subTest(text=text):
                result, _ = self.generate([text])
                self.assertEqual(result['text'], text)
                self.assertFalse(result['preempted'])

    def test_preemption_discards_the_partial_object_and_closes_the_stream(self):
        calls = []
        def preempted():
            calls.append(True)
            return len(calls) >= 3
        result, _ = self.generate(['{"unfinished":', '1}'], preempted)
        self.assertEqual(result, {'text': '', 'tokens': 1, 'preempted': True})


class SchedulerTests(unittest.TestCase):
    def test_new_readings_cancel_queued_older_requests_and_ignore_late_cancellation(self):
        release = threading.Event()
        seen = []
        class FakeEngine:
            load_ms = 0
            def __init__(self): release.wait(2)
            def memory(self): return {}
            def decide(self, payload, preempted=None):
                seen.append(payload)
                return {}
        scheduler = Scheduler(FakeEngine)
        try:
            old = scheduler.submit('decide', 'old', 'park', reading=('input', 1))
            current = scheduler.submit('decide', 'current', 'park', reading=('input', 2))
            self.assertTrue(old.cancelled())
            scheduler.cancel_reading('input', 1)
            self.assertFalse(current.cancelled())
            release.set()
            current.result(timeout=2)
            self.assertEqual(seen, ['current'])
            scheduler.cancel_reading('input', 3)
            self.assertTrue(scheduler.submit('decide', 'late', 'park', reading=('input', 3)).cancelled())
        finally:
            release.set()
            scheduler.close()

    def test_new_readings_interrupt_running_older_requests(self):
        entered = threading.Event()
        class FakeEngine:
            load_ms = 0
            def memory(self): return {}
            def decide(self, payload, preempted=None):
                if payload == 'old':
                    entered.set()
                    deadline = __import__('time').monotonic() + 2
                    while not preempted():
                        if __import__('time').monotonic() > deadline:
                            raise TimeoutError('old reading did not yield')
                        threading.Event().wait(.001)
                return {'preempted': preempted()}
        scheduler = Scheduler(FakeEngine)
        try:
            scheduler.ready.result(timeout=2)
            old = scheduler.submit('decide', 'old', 'park', reading=('input', 1))
            self.assertTrue(entered.wait(2))
            current = scheduler.submit('decide', 'current', 'park', reading=('input', 2))
            self.assertTrue(old.result(timeout=2)['preempted'])
            self.assertFalse(current.result(timeout=2)['preempted'])
        finally:
            scheduler.close()

    def test_queued_priorities_and_cancelled_requests(self):
        release = threading.Event()
        order = []

        class FakeEngine:
            load_ms = 0
            def __init__(self): release.wait(2)
            def memory(self): return {}
            def decide(self, payload, preempted=None):
                order.append(payload)
                return {}

        scheduler = Scheduler(FakeEngine)
        try:
            jobs = [scheduler.submit('decide', priority, priority)
                    for priority in ('background', 'park', 'question', 'input')]
            cancelled = scheduler.submit('decide', 'cancelled', 'input')
            self.assertTrue(cancelled.cancel())
            for invalid in ('', 'urgent', 1):
                with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                    scheduler.submit('decide', 'invalid', invalid)
            release.set()
            for job in jobs: job.result(timeout=2)
            self.assertEqual(order, ['input', 'question', 'park', 'background'])
        finally:
            release.set()
            scheduler.close()

    def test_running_question_gate_finishes_before_new_input(self):
        entered = threading.Event()
        release = threading.Event()
        order = []

        class FakeEngine:
            load_ms = 0
            def memory(self): return {}
            def decide(self, payload, preempted=None):
                if payload == 'question':
                    entered.set()
                    release.wait(2)
                order.append(payload)
                return {'preempted': preempted()}

        scheduler = Scheduler(FakeEngine)
        try:
            scheduler.ready.result(timeout=2)
            question = scheduler.submit('decide', 'question', 'question')
            self.assertTrue(entered.wait(2))
            incoming = scheduler.submit('decide', 'input', 'input')
            release.set()
            self.assertFalse(question.result(timeout=2)['preempted'])
            incoming.result(timeout=2)
            self.assertEqual(order, ['question', 'input'])
        finally:
            release.set()
            scheduler.close()

    def test_expired_running_question_releases_the_next_input(self):
        entered = threading.Event()
        order = []

        class FakeEngine:
            load_ms = 0
            def memory(self): return {}
            def decide(self, payload, preempted=None):
                if payload == 'question':
                    entered.set()
                    deadline = __import__('time').monotonic() + 1
                    while not preempted():
                        if __import__('time').monotonic() > deadline:
                            raise TimeoutError('expired question did not yield')
                        threading.Event().wait(.001)
                order.append(payload)
                return {'preempted': preempted()}

        scheduler = Scheduler(FakeEngine)
        try:
            scheduler.ready.result(timeout=2)
            question = scheduler.submit('decide', 'question', 'question', timeout=.05)
            self.assertTrue(entered.wait(2))
            incoming = scheduler.submit('decide', 'input', 'input')
            self.assertTrue(question.result(timeout=2)['preempted'])
            self.assertFalse(incoming.result(timeout=2)['preempted'])
            self.assertEqual(order, ['question', 'input'])
        finally:
            scheduler.close()

    def test_background_report_can_yield_to_new_input(self):
        entered = threading.Event()
        release = threading.Event()
        order = []

        class FakeEngine:
            load_ms = 0
            def memory(self): return {}
            def decide(self, payload, preempted=None):
                if payload == 'report':
                    entered.set()
                    release.wait(2)
                order.append(payload)
                return {'preempted': preempted()}

        scheduler = Scheduler(FakeEngine)
        try:
            scheduler.ready.result(timeout=2)
            report = scheduler.submit('decide', 'report', 'background')
            self.assertTrue(entered.wait(2))
            incoming = scheduler.submit('decide', 'input', 'input')
            release.set()
            self.assertTrue(report.result(timeout=2)['preempted'])
            incoming.result(timeout=2)
            self.assertEqual(order, ['report', 'input'])
        finally:
            release.set()
            scheduler.close()

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
            def decide(self, payload, preempted=None):
                order.append(payload)
                return {}

        scheduler = Scheduler(FakeEngine)
        try:
            scheduler.ready.result(timeout=2)
            background = scheduler.submit("generate", {"prompt": "x", "maxTokens": 5})
            self.assertTrue(entered.wait(2))
            first = scheduler.submit("decide", "first")
            second = scheduler.submit("decide", "second")
            result = background.result(timeout=2)
            self.assertTrue(result["preempted"])
            self.assertEqual(result["preemptReason"], "priority")
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
    def test_privacy_generation_uses_the_requested_priority_and_its_own_suffix(self):
        seen = []
        class FakeEngine:
            load_ms = 0
            def memory(self): return {}
            def generate(self, prompt, max_tokens, preempted):
                seen.append(prompt)
                return {"text": '{"findings":[]}', "preempted": False}
        scheduler = Scheduler(FakeEngine)
        scheduler.ready.result(timeout=2)
        server = create_server(scheduler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        try:
            for priority in ["input", "background"]:
                with urllib.request.urlopen(urllib.request.Request(
                    f'http://127.0.0.1:{server.server_port}/privacy',
                    data=json.dumps({"prefix": "Inspect\n", "tail": '[]', "maxTokens": 100}).encode(),
                    headers={'Content-Type': 'application/json', 'X-Intercat-Priority': priority}), timeout=2) as response:
                    self.assertEqual(json.load(response)['priority'], priority)
            self.assertEqual(seen, ['Inspect\n[]\nFindings:\n'] * 2)
        finally:
            server.shutdown()
            server.server_close()
            scheduler.close()

    def test_generation_is_background_only_and_validates_its_contract(self):
        seen = []
        class FakeEngine:
            load_ms = 0
            def memory(self): return {}
            def generate(self, prompt, max_tokens, preempted):
                seen.append((prompt, max_tokens))
                return {"text": '{"detail":"why","line":"placed","title":"Tests"}', "preempted": False}
        scheduler = Scheduler(FakeEngine)
        scheduler.ready.result(timeout=2)
        server = create_server(scheduler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        def post(value):
            return urllib.request.urlopen(urllib.request.Request(
                f'http://127.0.0.1:{server.server_port}/generate', data=json.dumps(value).encode(),
                headers={'Content-Type': 'application/json', 'X-Intercat-Priority': 'input'}), timeout=2)
        value = {'prefix': 'Explain\n', 'tail': '{"request":"tests"}', 'maxTokens': 100}
        try:
            with post(value) as response:
                result = json.load(response)
            self.assertEqual(result['priority'], 'background')
            self.assertEqual(seen, [('Explain\n{"request":"tests"}\nDescription:\n', 100)])
            for invalid in ({**value, 'maxTokens': True}, {**value, 'maxTokens': 513},
                            {**value, 'tail': {}}, {**value, 'extra': 1}):
                with self.subTest(invalid=invalid), self.assertRaises(urllib.error.HTTPError) as error:
                    post(invalid)
                self.assertEqual(error.exception.code, 400)
        finally:
            server.shutdown()
            server.server_close()
            scheduler.close()

    def test_cancellation_before_submission_rejects_the_old_reading(self):
        seen = []
        class FakeEngine:
            load_ms = 0
            def memory(self): return {}
            def decide(self, request, preempted=None):
                seen.append(request.tail)
                return {}
        scheduler = Scheduler(FakeEngine)
        scheduler.ready.result(timeout=2)
        server = create_server(scheduler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        key = '00000000-0000-7000-8000-000000000001'
        def post(route, body, headers=None):
            return urllib.request.urlopen(urllib.request.Request(
                f'http://127.0.0.1:{server.server_port}/{route}', data=json.dumps(body).encode(),
                headers={'Content-Type': 'application/json', **(headers or {})}), timeout=2)
        value = {'cachePrefix': False, 'prefix': 'rules', 'tail': 'old', 'stages': [{'name': 'decision', 'prompt': 'Choose:', 'candidates': [' A']}]}
        try:
            with post('cancel-reading', {'input': key, 'index': 3}) as response:
                self.assertTrue(json.load(response)['cancelled'])
            with self.assertRaises(urllib.error.HTTPError) as error:
                post('decide', value, {'X-Intercat-Reading': key, 'X-Intercat-Reading-Index': '2'})
            self.assertEqual(error.exception.code, 503)
            self.assertEqual(seen, [])
            for invalid in ({'input': None, 'index': None}, {'input': key, 'index': True}, {'input': key, 'index': 1.5}):
                with self.subTest(invalid=invalid), self.assertRaises(urllib.error.HTTPError) as error:
                    post('cancel-reading', invalid)
                self.assertEqual(error.exception.code, 400)
            with post('decide', value, {'X-Intercat-Reading': key, 'X-Intercat-Reading-Index': '4'}) as response:
                json.load(response)
            self.assertEqual(seen, ['old'])
        finally:
            server.shutdown(); server.server_close(); scheduler.close()

    def test_priority_and_queue_deadline_are_enforced_before_inference(self):
        entered = threading.Event()
        release = threading.Event()
        seen = []

        class FakeEngine:
            load_ms = 0
            def memory(self): return {}
            def decide(self, request, preempted=None):
                seen.append(request.tail)
                if request.tail == 'block':
                    entered.set()
                    release.wait(3)
                return {}

        scheduler = Scheduler(FakeEngine)
        scheduler.ready.result(timeout=2)
        server = create_server(scheduler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        value = {'cachePrefix': False, 'prefix': 'rules', 'tail': 'expired',
                 'stages': [{'name': 'decision', 'prompt': 'Decision:', 'candidates': [' A']}]}

        def post(headers):
            return urllib.request.urlopen(urllib.request.Request(
                f'http://127.0.0.1:{server.server_port}/decide',
                data=json.dumps(value).encode(),
                headers={'Content-Type': 'application/json', **headers}), timeout=2)

        try:
            for headers in [{'X-Intercat-Priority': 'unknown'},
                            *({'X-Intercat-Timeout-Ms': bad} for bad in ('0', '-1', '120001', 'nan'))]:
                with self.subTest(headers=headers), self.assertRaises(urllib.error.HTTPError) as error:
                    post(headers)
                self.assertEqual(error.exception.code, 400)
            blocked = scheduler.submit('decide', request(tail='block'), 'question')
            self.assertTrue(entered.wait(2))
            with self.assertRaises(urllib.error.HTTPError) as error:
                post({'X-Intercat-Priority': 'input', 'X-Intercat-Timeout-Ms': '30'})
            self.assertEqual(error.exception.code, 504)
            release.set()
            blocked.result(timeout=2)
            with post({'X-Intercat-Priority': 'background'}) as response:
                self.assertEqual(json.load(response)['priority'], 'background')
            self.assertEqual(seen, ['block', 'expired'])
        finally:
            release.set()
            server.shutdown()
            server.server_close()
            scheduler.close()

    def test_token_count_uses_the_engine_and_keeps_the_transport_boundary(self):
        seen = []

        class FakeEngine:
            load_ms = 0
            def memory(self): return {}
            def count_tokens(self, text):
                seen.append(text)
                return {"tokens": 7}

        scheduler = Scheduler(FakeEngine)
        scheduler.ready.result(timeout=2)
        server = create_server(scheduler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        url = f"http://127.0.0.1:{server.server_port}/tokens"
        try:
            for value in [{"text": 12}, {"text": "hello", "extra": True}]:
                with self.assertRaises(urllib.error.HTTPError) as error:
                    urllib.request.urlopen(urllib.request.Request(url, data=json.dumps(value).encode(), headers={"Content-Type": "application/json"}), timeout=2)
                self.assertEqual(error.exception.code, 400)
            with urllib.request.urlopen(urllib.request.Request(url, data=json.dumps({"text": "日本語も数える"}).encode(), headers={"Content-Type": "application/json"}), timeout=2) as response:
                self.assertEqual(response.getheader("X-Intercat-Inference"), "started")
                self.assertEqual(json.load(response)["tokens"], 7)
            with self.assertRaises(urllib.error.HTTPError) as error:
                urllib.request.urlopen(urllib.request.Request(url, data=b'{"text":"hello"}', headers={"Content-Type": "application/json", "Origin": "https://example.invalid"}), timeout=2)
            self.assertEqual(error.exception.code, 403)
            long_text = "字" * 400_000 + "\x01" * 200_000 + "keep the final instruction"
            with urllib.request.urlopen(urllib.request.Request(url, data=json.dumps({"text": long_text}).encode(), headers={"Content-Type": "application/json"}), timeout=5) as response:
                self.assertEqual(json.load(response)["tokens"], 7)
            self.assertEqual(seen, ["日本語も数える", long_text])
            import http.client
            connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=2)
            connection.request("POST", "/decide", headers={"Content-Type": "application/json", "Content-Length": str(1024 * 1024 + 1)})
            self.assertEqual(connection.getresponse().status, 400)
            connection.close()
        finally:
            server.shutdown()
            server.server_close()
            scheduler.close()

    def test_response_headers_announce_inference_before_the_body(self):
        import http.client
        release = threading.Event()

        class FakeEngine:
            load_ms = 0
            def memory(self): return {}
            def decide(self, request, preempted=None):
                release.wait(3)
                return {"stages": {"decision": {"selected": " A"}}}

        scheduler = Scheduler(FakeEngine)
        scheduler.ready.result(timeout=2)
        server = create_server(scheduler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        client = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=1)
        try:
            value = {"cachePrefix": False, "prefix": "rules", "tail": "hi", "stages": [{"name": "decision", "prompt": "Decision:", "candidates": [" A"]}]}
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
            def decide(self, request, preempted=None):
                return {"stages": {"decision": {"selected": " A"}}}

        scheduler = Scheduler(FakeEngine)
        server = create_server(scheduler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        url = f"http://127.0.0.1:{server.server_port}"
        def post(value, headers=None):
            return urllib.request.urlopen(urllib.request.Request(url + "/decide", data=json.dumps(value).encode(), headers={"Content-Type": "application/json", **(headers or {})}), timeout=2)
        value = {"cachePrefix": False, "prefix": "rules", "tail": "hi", "stages": [{"name": "decision", "prompt": "Decision:", "candidates": [" A"]}]}
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
