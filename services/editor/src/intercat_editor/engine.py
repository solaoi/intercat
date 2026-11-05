"""One GPU owner, one fresh KV cache per request, all candidate logits."""

import hashlib
from pathlib import Path
import time

from .contract import DecisionRequest, distribution
from .model_store import verify_model


class Preempted(Exception):
    pass


class Engine:
    def __init__(self, model_path: str):
        if not Path(model_path).is_dir():
            raise ValueError("model must be an existing local directory")
        import mlx.core as mx
        from mlx_lm import load
        from mlx_lm.models.cache import make_prompt_cache

        self.mx = mx
        self.make_cache = make_prompt_cache
        started = time.perf_counter()
        verify_model(model_path)
        self.model, self.tokenizer = load(model_path)
        self.load_ms = (time.perf_counter() - started) * 1000
        # These are every label used by the designed decision stages, including Skill 0.
        for label in [*(" " + c for c in "ABCDEFGH"), *(str(i) for i in range(301))]:
            self.label_id(label)

    def label_id(self, label: str) -> int:
        ids = self.tokenizer.encode(label, add_special_tokens=False)
        if len(ids) != 1:
            raise ValueError(f"candidate must be one token: {label!r}")
        return ids[0]

    def memory(self) -> dict:
        return {
            "activeBytes": self.mx.get_active_memory(),
            "cacheBytes": self.mx.get_cache_memory(),
            "peakBytes": self.mx.get_peak_memory(),
        }

    def decide(self, request: DecisionRequest) -> dict:
        mx = self.mx
        started = time.perf_counter()
        encode = lambda text: self.tokenizer.encode(text, add_special_tokens=False)
        prefix_ids, tail_ids = encode(request.prefix), encode(request.tail)
        if len(prefix_ids) > 6000 or len(tail_ids) > 500:
            raise ValueError("prefix exceeds 6000 tokens or tail exceeds 500 tokens")
        candidate_ids = {s.name: [self.label_id(c) for c in s.candidates] for s in request.stages}
        if any(len(set(ids)) != len(ids) for ids in candidate_ids.values()):
            raise ValueError("candidates must map to distinct tokens")
        # Encode the joined text, never concatenate separately tokenized prefix/tail.
        pending = request.prefix + request.tail
        cache = self.make_cache(self.model)
        results = {}
        prefill_tokens = None
        try:
            for stage in request.stages:
                if any(results.get(key, {}).get("selected") not in labels for key, labels in stage.when.items()):
                    continue
                ids = encode(pending + stage.prompt)
                prefill_tokens = len(ids) if prefill_tokens is None else prefill_tokens
                logits = self.model(mx.array([ids]), cache=cache)[:, -1, :]
                logprobs = logits - mx.logsumexp(logits, axis=-1, keepdims=True)
                scores = logprobs[0, mx.array(candidate_ids[stage.name])]
                mx.eval(scores, [entry.state for entry in cache])
                raw = scores.tolist()
                probabilities = distribution(raw, request.temperature)
                selected = stage.candidates[max(range(len(raw)), key=raw.__getitem__)]
                results[stage.name] = {
                    "selected": selected,
                    "logprobs": dict(zip(stage.candidates, raw)),
                    "probabilities": dict(zip(stage.candidates, probabilities)),
                    "confidence": max(probabilities),
                }
                pending = selected
            return {
                "stages": results,
                "inferenceMs": (time.perf_counter() - started) * 1000,
                "prefixTokens": len(prefix_ids),
                "tailTokens": len(tail_ids),
                "prefillTokens": prefill_tokens,
                "promptVersion": hashlib.sha256(request.prefix.encode()).hexdigest(),
                "tailHash": hashlib.sha256(request.tail.encode()).hexdigest(),
                "temperature": request.temperature,
            }
        finally:
            del cache
            mx.clear_cache()

    def generate(self, prompt: str, max_tokens: int, preempted) -> dict:
        from mlx_lm import stream_generate

        ids = self.tokenizer.encode(prompt, add_special_tokens=False)
        if not ids or len(ids) > 8000 or not 1 <= max_tokens <= 512:
            raise ValueError("generation requires 1–8000 input tokens and 1–512 output tokens")

        def checkpoint(*_):
            if preempted():
                raise Preempted()

        parts = []
        generated = 0
        stream = stream_generate(
            self.model, self.tokenizer, ids, max_tokens=max_tokens,
            prefill_step_size=128, prompt_progress_callback=checkpoint,
        )
        try:
            checkpoint()
            for response in stream:
                checkpoint()
                parts.append(response.text)
                generated = response.generation_tokens
            return {"text": "".join(parts), "tokens": generated, "preempted": False}
        except Preempted:
            # Partial background output never becomes a completed title or summary.
            return {"text": "", "tokens": generated, "preempted": True}
        finally:
            stream.close()
            self.mx.synchronize()
            self.mx.clear_cache()
