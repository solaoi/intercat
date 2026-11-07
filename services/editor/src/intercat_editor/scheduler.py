"""Serialize MLX calls by input, question gate, park, and background priority."""

from collections import deque
from concurrent.futures import Future
from threading import Condition, Event, Thread
import time

PRIORITIES = ("input", "question", "park", "background")


class Scheduled(Future):
    def __init__(self, timeout=None, reading=None):
        super().__init__()
        self.started = Event()
        self.deadline = time.monotonic() + timeout if timeout is not None else None
        self.reading = reading
        self.superseded = False
        self.add_done_callback(lambda _: self.started.set())


class Scheduler:
    def __init__(self, engine_factory):
        self.condition = Condition()
        self.queues = [deque() for _ in PRIORITIES]
        self.readings = {}
        self.reading_jobs = {}
        self.closed = False
        self.ready = Future()
        self.thread = Thread(target=self._run, args=(engine_factory,), daemon=True)
        self.thread.start()

    def submit(self, kind, payload, priority=None, timeout=None, reading=None) -> Future:
        if kind not in {"decide", "tokens", "generate", "memory"}:
            raise ValueError("unknown job kind")
        future = Scheduled(timeout, reading)
        if priority is None:
            priority = "background" if kind == "generate" else "input"
        priority = PRIORITIES.index(priority)
        with self.condition:
            if self.closed:
                raise RuntimeError("editor has stopped")
            if reading is not None:
                key, index = reading
                current, cancelled = self.readings.get(key, (0, False))
                if index < current or index == current and cancelled:
                    future.cancel()
                    return future
                if index > current:
                    self._cancel_reading_jobs(key, index - 1)
                    self.readings[key] = (index, False)
                self.reading_jobs.setdefault(key, set()).add(future)
                future.add_done_callback(self._forget_reading_job)
            self.queues[priority].append((kind, payload, future, time.perf_counter()))
            self.condition.notify()
        return future

    def _forget_reading_job(self, future):
        with self.condition:
            jobs = self.reading_jobs.get(future.reading[0])
            if jobs is not None:
                jobs.discard(future)
                if not jobs:
                    self.reading_jobs.pop(future.reading[0])

    def _cancel_reading_jobs(self, key, index):
        for future in list(self.reading_jobs.get(key, ())):
            if future.reading[1] <= index:
                future.superseded = True
                future.cancel()

    def cancel_reading(self, key, index):
        with self.condition:
            current, _ = self.readings.get(key, (0, False))
            if index >= current:
                self.readings[key] = (index, True)
            self._cancel_reading_jobs(key, index)
            self.condition.notify_all()

    def _preemption_reason(self, priority, future):
        with self.condition:
            if self.closed:
                return "closed"
            if future.deadline is not None and time.monotonic() >= future.deadline:
                return "timeout"
            if future.superseded:
                return "superseded"
            if priority >= 2 and any(
                not future.cancelled()
                for queue in self.queues[:priority]
                for _, _, future, _ in queue
            ):
                return "priority"
            return None

    def _preempted(self, priority, future):
        return self._preemption_reason(priority, future) is not None

    def _run(self, engine_factory):
        try:
            engine = engine_factory()
            self.ready.set_result({"loadMs": engine.load_ms, "memory": engine.memory()})
        except BaseException as error:
            with self.condition:
                self.closed = True
                self.ready.set_exception(error)
                for queue in self.queues:
                    for _, _, future, _ in queue:
                        if not future.done():
                            future.set_exception(error)
            return
        while True:
            with self.condition:
                self.condition.wait_for(lambda: self.closed or any(self.queues))
                if self.closed:
                    return
                priority = next(i for i, queue in enumerate(self.queues) if queue)
                kind, payload, future, submitted = self.queues[priority].popleft()
            if not future.set_running_or_notify_cancel():
                continue
            started = time.perf_counter()
            future.started.set()
            try:
                if kind == "decide":
                    result = engine.decide(payload, lambda: self._preempted(priority, future))
                elif kind == "tokens":
                    result = engine.count_tokens(payload)
                elif kind == "generate":
                    result = engine.generate(payload["prompt"], payload["maxTokens"], lambda: self._preempted(priority, future))
                else:
                    result = engine.memory()
                if result.get("preempted") is True:
                    result["preemptReason"] = self._preemption_reason(priority, future) or "interrupted"
                result.update(priority=PRIORITIES[priority], queueMs=(started - submitted) * 1000, totalMs=(time.perf_counter() - submitted) * 1000)
                future.set_result(result)
            except Exception as error:
                future.set_exception(error)

    def close(self):
        with self.condition:
            self.closed = True
            for queue in self.queues:
                for _, _, future, _ in queue:
                    future.cancel()
            self.condition.notify_all()
        self.thread.join()
