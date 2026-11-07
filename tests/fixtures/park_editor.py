"""Deterministic model, with production HTTP, scheduling and parent lifetime."""
import json
import math
from pathlib import Path
import sys
import time
from types import ModuleType


class FakeEngine:
    load_ms = 0

    def memory(self):
        return {}

    def count_tokens(self, text):
        return {"tokens": 40}

    def generate(self, prompt, max_tokens, preempted):
        context = json.loads(prompt.rsplit("\nContext:\n", 1)[1].split("\nDescription:\n", 1)[0])
        clip = "items" in context
        text = json.dumps(context["items"]) if clip else context["request"]
        with Path("model-generations.jsonl").open("a") as stream:
            stream.write(json.dumps({"text": text, "phase": context.get("phase")}) + "\n")
        if text == "slow explanation" or (clip and "slow summary" in text):
            operation = "summary" if clip else "description"
            Path(operation + "-running").touch()
            while not Path("release-" + operation).exists():
                if preempted():
                    Path(operation + "-cancelled").touch()
                    return {"text": "", "preempted": True, "tokens": 0}
                time.sleep(.005)
        extra = Path("description-secret").read_text() if Path("description-secret").exists() else ""
        result = {"title": "Zero-case test", "summary": "Added a zero-case test without running the suite." + extra} if clip else {
            "detail": "The recorded request is: " + text + extra,
            "line": ("New track for " + context["destination"]["worker"] + ".") if context.get("decision", {}).get("operation") == "fork" and context.get("destination", {}).get("worker") else "Interpretation recorded.", "title": "Arithmetic tests"}
        if clip and context.get("phase") == "thinking":
            result = {"title": "Examining retry counter", "summary": "Examining the retry path for duplicate increments." + extra}
        return {"text": json.dumps(result),
            "preempted": False, "tokens": 32}

    def decide(self, request, preempted=None):
        tail = json.loads(request.tail)
        text = tail["request"]
        context = tail.get("laterRequest", {}).get("text")
        with Path("model-calls.jsonl").open("a") as stream:
            stream.write(json.dumps({"text": text, "context": context}) + "\n")
        if context == "slow context":
            Path("park-running").touch()
            deadline = time.monotonic() + 10
            while not Path("release-park").exists():
                if preempted():
                    return {"preempted": True}
                if time.monotonic() > deadline:
                    raise TimeoutError("fixture park was not interrupted")
                time.sleep(.005)
        operation = " E" if text.startswith("park") else " B"
        if text.startswith(("Start waiting work", "Open settings", "Open commit", "Stage mode", "Open branch")):
            operation = " H"
        if context and context.startswith("unlock"):
            operation = " A"
        selected = {}
        stages = {}
        for stage in request.stages:
            if any(selected.get(key) not in values for key, values in stage.when.items()):
                continue
            choice = operation if stage.name == "decision" else "1" if stage.name == "lane" else " A"
            if stage.name == "command":
                choice = "8" if text.startswith("Open settings") else "1" if text.startswith("Open commit") else "7" if text.startswith("Stage mode") else "5" if text.startswith("Open branch") else "10"
            if stage.name == "boundary" and text.startswith("When I say"):
                choice = " D"
            if stage.name == "boundary" and text.startswith("After this turn"):
                choice = " B"
            if choice not in stage.candidates:
                choice = stage.candidates[0]
            selected[stage.name] = choice
            stages[stage.name] = {"selected": choice,
                "logprobs": {label: 0 if label == choice else -100 for label in stage.candidates},
                "probabilities": {label: 1 if label == choice else 0 for label in stage.candidates}}
        for stage in stages.values():
            stage["logprobs"] = {label: math.log(max(probability, 1e-300)) * request.temperature
                                 for label, probability in stage["probabilities"].items()}
        return {"cachePrefix": False, "temperature": request.temperature, "stages": stages, "inferenceMs": 1}


module = ModuleType("intercat_editor.engine")
module.Engine = FakeEngine
sys.modules[module.__name__] = module
from intercat_editor.lifetime import watch_parent
from intercat_editor.scheduler import Scheduler
from intercat_editor.server import create_server

watch_parent()
scheduler = Scheduler(FakeEngine)
server = create_server(scheduler)
print(json.dumps({"host": "127.0.0.1", "port": server.server_port}), flush=True)
try:
    server.serve_forever()
finally:
    server.server_close()
    scheduler.close()
