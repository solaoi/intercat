"""One GPU owner, isolated request caches, and explicit prefix reuse."""

import copy
import hashlib
import json
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
        self._prefix_ids = None
        self._prefix_cache = None
        # These are every label used by the designed decision stages, including Skill 0.
        for label in [*(" " + c for c in "ABCDEFGH"), *(str(i) for i in range(301))]:
            self.label_id(label)

    def label_id(self, label: str) -> int:
        ids = self.tokenizer.encode(label, add_special_tokens=False)
        if len(ids) != 1:
            raise ValueError(f"candidate must be one token: {label!r}")
        return ids[0]

    def count_tokens(self, text: str) -> dict:
        return {"tokens": len(self.tokenizer.encode(text, add_special_tokens=False))}

    def memory(self) -> dict:
        return {
            "activeBytes": self.mx.get_active_memory(),
            "cacheBytes": self.mx.get_cache_memory(),
            "peakBytes": self.mx.get_peak_memory(),
        }

    def _initial_cache(self, prefix_ids, initial_ids, reuse):
        if not reuse:
            return self.make_cache(self.model), initial_ids, False, 0
        shared = 0
        for left, right in zip(prefix_ids, initial_ids[:-1]):
            if left != right:
                break
            shared += 1
        key = tuple(prefix_ids[:shared])
        if not key:
            return self.make_cache(self.model), initial_ids, False, 0
        hit = self._prefix_ids == key and self._prefix_cache is not None
        if not hit:
            base = self.make_cache(self.model)
            self.model(self.mx.array([list(key)]), cache=base)
            self.mx.eval([entry.state for entry in base])
            # Keep only one immutable prefix; each request gets its own cache.
            self._prefix_ids, self._prefix_cache = key, base
        return copy.deepcopy(self._prefix_cache), initial_ids[shared:], hit, shared

    def decide(self, request: DecisionRequest, preempted=None) -> dict:
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
        cache = None
        results = {}
        prefill_tokens = None
        prefix_hit = False
        cached_tokens = 0
        try:
            for stage in request.stages:
                if preempted and preempted():
                    raise Preempted()
                if any(results.get(key, {}).get("selected") not in labels for key, labels in stage.when.items()):
                    continue
                ids = encode(pending + stage.prompt)
                if prefill_tokens is None:
                    prefill_tokens = len(ids)
                    cache, ids, prefix_hit, cached_tokens = self._initial_cache(prefix_ids, ids, request.cache_prefix)
                logits = self.model(mx.array([ids]), cache=cache)[:, -1, :].astype(mx.float32)
                logprobs = logits - mx.logsumexp(logits, axis=-1, keepdims=True)
                scores = logprobs[0, mx.array(candidate_ids[stage.name])]
                mx.eval(scores, [entry.state for entry in cache])
                if preempted and preempted():
                    raise Preempted()
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
                "cachePrefix": request.cache_prefix,
                "prefixCacheHit": prefix_hit,
                "prefixCacheTokens": cached_tokens,
            }
        except Preempted:
            return {"preempted": True}
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
        decoder = json.JSONDecoder()
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
                try:
                    candidate = "".join(parts).lstrip()
                    value, end = decoder.raw_decode(candidate)
                    if isinstance(value, dict):
                        parts = [candidate[:end]]
                        break
                except json.JSONDecodeError:
                    pass
            return {"text": "".join(parts), "tokens": generated, "preempted": False}
        except Preempted:
            # Partial background output never becomes a completed title or summary.
            return {"text": "", "tokens": generated, "preempted": True}
        finally:
            stream.close()
            self.mx.synchronize()
            self.mx.clear_cache()
