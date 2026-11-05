"""The /decide wire contract, independent of MLX and HTTP."""

from dataclasses import dataclass
import math


@dataclass(frozen=True)
class Stage:
    name: str
    prompt: str
    candidates: tuple[str, ...]
    when: dict[str, tuple[str, ...]]


@dataclass(frozen=True)
class DecisionRequest:
    prefix: str
    tail: str
    stages: tuple[Stage, ...]
    temperature: float

    @classmethod
    def parse(cls, value: dict) -> "DecisionRequest":
        if not isinstance(value, dict):
            raise ValueError("request must be an object")
        if value.keys() - {"prefix", "tail", "stages", "temperature"}:
            raise ValueError("unknown request field")
        if not isinstance(value.get("prefix"), str) or not isinstance(value.get("tail"), str):
            raise ValueError("prefix and tail must be strings")
        raw_stages = value.get("stages")
        if not isinstance(raw_stages, list) or not 1 <= len(raw_stages) <= 8:
            raise ValueError("stages must contain 1–8 entries")
        stages = []
        known = {}
        for raw in raw_stages:
            if not isinstance(raw, dict) or raw.keys() - {"name", "prompt", "candidates", "when"}:
                raise ValueError("invalid stage")
            name, prompt, candidates = raw.get("name"), raw.get("prompt"), raw.get("candidates")
            if not isinstance(name, str) or not name or name in known:
                raise ValueError("stage names must be nonempty and unique")
            if not isinstance(prompt, str) or not prompt:
                raise ValueError("stage prompt must be nonempty")
            if not isinstance(candidates, list) or not 1 <= len(candidates) <= 301:
                raise ValueError("stage must have 1–301 candidates")
            if any(not isinstance(c, str) or not c for c in candidates) or len(set(candidates)) != len(candidates):
                raise ValueError("candidates must be distinct nonempty strings")
            when = raw.get("when", {})
            if not isinstance(when, dict):
                raise ValueError("when must be an object")
            for previous, labels in when.items():
                if previous not in known or not isinstance(labels, list) or not labels:
                    raise ValueError("conditions must reference a previous stage")
                if any(label not in known[previous] for label in labels):
                    raise ValueError("unknown condition candidate")
            stages.append(Stage(name, prompt, tuple(candidates), {k: tuple(v) for k, v in when.items()}))
            known[name] = candidates
        temperature = value.get("temperature", 1.0)
        if isinstance(temperature, bool) or not isinstance(temperature, (int, float)) or not math.isfinite(temperature) or temperature <= 0:
            raise ValueError("temperature must be positive and finite")
        return cls(value["prefix"], value["tail"], tuple(stages), float(temperature))


def distribution(logprobs: list[float], temperature: float) -> list[float]:
    """Normalize only the allowed labels; preserve raw vocabulary scores separately."""
    if not logprobs or any(not math.isfinite(x) for x in logprobs):
        raise ValueError("non-finite model scores")
    maximum = max(logprobs)
    weights = [math.exp((x - maximum) / temperature) for x in logprobs]
    total = sum(weights)
    return [x / total for x in weights]
