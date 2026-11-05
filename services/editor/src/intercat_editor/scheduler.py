"""Serialize MLX calls; foreground decisions interrupt background generation."""

from collections import deque
from concurrent.futures import Future
from threading import Condition, Event, Thread
import time


class Scheduled(Future):
    def __init__(self):
        super().__init__()
        self.started = Event()
        self.add_done_callback(lambda _: self.started.set())


class Scheduler:
    def __init__(self, engine_factory):
        self.condition = Condition()
        self.decisions = deque()
        self.background = deque()
        self.closed = False
        self.ready = Future()
        self.thread = Thread(target=self._run, args=(engine_factory,), daemon=True)
        self.thread.start()

    def submit(self, kind, payload) -> Future:
        if kind not in {"decide", "generate", "memory"}:
            raise ValueError("unknown job kind")
        future = Scheduled()
        with self.condition:
            if self.closed:
                raise RuntimeError("editor has stopped")
            queue = self.background if kind == "generate" else self.decisions
            queue.append((kind, payload, future, time.perf_counter()))
            self.condition.notify()
        return future

    def _preempted(self):
        with self.condition:
            return self.closed or bool(self.decisions)

    def _run(self, engine_factory):
        try:
            engine = engine_factory()
            self.ready.set_result({"loadMs": engine.load_ms, "memory": engine.memory()})
        except BaseException as error:
            with self.condition:
                self.closed = True
                self.ready.set_exception(error)
                for _, _, future, _ in (*self.decisions, *self.background):
                    future.set_exception(error)
            return
        while True:
            with self.condition:
                self.condition.wait_for(lambda: self.closed or self.decisions or self.background)
                if self.closed:
                    return
                kind, payload, future, submitted = (self.decisions or self.background).popleft()
            if not future.set_running_or_notify_cancel():
                continue
            started = time.perf_counter()
            future.started.set()
            try:
                if kind == "decide":
                    result = engine.decide(payload)
                elif kind == "generate":
                    result = engine.generate(payload["prompt"], payload["maxTokens"], self._preempted)
                else:
                    result = engine.memory()
                result.update(queueMs=(started - submitted) * 1000, totalMs=(time.perf_counter() - submitted) * 1000)
                future.set_result(result)
            except Exception as error:
                future.set_exception(error)

    def close(self):
        with self.condition:
            self.closed = True
            for _, _, future, _ in (*self.decisions, *self.background):
                future.cancel()
            self.condition.notify_all()
        self.thread.join()
