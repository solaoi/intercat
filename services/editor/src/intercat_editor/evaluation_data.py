"""Read recorded decisions and labels without writing to the application log."""

from collections import Counter
from dataclasses import asdict
import hashlib
import json
from pathlib import Path
import sqlite3
import unicodedata

from .calibration import Observation, validate_partitions


WEIGHTS = {"explicit": 1.0, "confirmed": 1.0, "inferred": 0.25, "uncorrected": 0.1}
KINDS = dict(zip(("extend", "fork", "schedule", "trim", "park", "ask", "plan", "command"), (" " + value for value in "ABCDEFGH")))
BOUNDARIES = {"now": " A", "after-turn": " B", "after-clip": " C", "when-told": " D"}
COMMANDS = ["commit", "push", "open-pr", "export", "branch-from", "restore", "stage-mode", "settings", "plan-only", "start-waiting"]


def digest(value):
    return hashlib.sha256(json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def text_digest(text):
    lowercase = "".join(character.lower() for character in unicodedata.normalize("NFKC", text))
    normalized = " ".join(lowercase.split())
    return hashlib.sha256(normalized.encode()).hexdigest()


def identity(decision):
    return (decision or {}).get("scores", {}).get("calibration", {}).get("id")


def labels(decision, metadata):
    result = {}
    kind = decision.get("kind")
    if kind in KINDS:
        result["decision"] = KINDS[kind]
    if decision.get("boundary") in BOUNDARIES:
        result["boundary"] = BOUNDARIES[decision["boundary"]]
    target = decision.get("trackId")
    lane = next((label for label, track in metadata["labelTracks"].items() if track == target), None)
    if lane is not None:
        result["lane"] = lane
    if decision.get("command") in COMMANDS:
        result["command"] = str(COMMANDS.index(decision["command"]) + 1)
    return result


def export(database):
    connection = sqlite3.connect(Path(database).resolve().as_uri() + "?mode=ro", uri=True)
    try:
        connection.execute("PRAGMA query_only=ON")
        connection.execute("BEGIN")
        sequence = connection.execute("SELECT coalesce(max(seq),0) FROM events").fetchone()[0]
        inputs = {}
        for key, text, state, raw in connection.execute("SELECT id,text,state,decision FROM utterances"):
            inputs[key] = {"text": text, "state": state, "decision": json.loads(raw) if raw else None}
        models, evidence, corrections, links = {}, {}, set(), []
        request_hashes = {}
        excluded = Counter()
        for seq, kind, raw in connection.execute("SELECT seq,kind,payload FROM events WHERE kind IN ('utterance.classified','utterance.corrected','utterance.confirmed') ORDER BY seq"):
            value = json.loads(raw)
            if kind == "utterance.classified":
                decision = value.get("decision") or {}
                metadata = decision.get("scores", {}).get("calibration")
                if metadata is None:
                    excluded["unrecorded_model_profile"] += 1
                    continue
                if not isinstance(metadata.get("cachePrefix"), bool) or metadata["cachePrefix"] != decision["scores"].get("cachePrefix"):
                    excluded["unmatched_loading_mode"] += 1
                    continue
                key = identity(decision)
                if not key:
                    raise ValueError("model decision identity is missing")
                request_hash = metadata.get("requestHash")
                if not isinstance(request_hash, str) or len(request_hash) != 64 or any(character not in "0123456789abcdef" for character in request_hash):
                    raise ValueError("recorded model request fingerprint is invalid")
                if key in models:
                    previous = models[key]["decision"]["scores"]
                    if previous["calibration"] != metadata or previous["stages"] != decision["scores"]["stages"]:
                        raise ValueError("model decision identity was reused with different scores")
                    continue
                models[key] = {"sequence": seq, "input": value["id"], "decision": decision, "metadata": metadata}
                request_hashes.setdefault(value["id"], []).append(request_hash)
            elif kind == "utterance.confirmed":
                key = identity(value.get("decision"))
                if key in models and evidence.get(key, (None,))[0] != "explicit":
                    evidence[key] = ("confirmed", value["decision"])
            else:
                corrections.add(value["inputId"])
                if value.get("replacementInputId"):
                    corrections.add(value["replacementInputId"])
                    links.append((value["inputId"], value["replacementInputId"]))
                key = identity(value.get("before"))
                if key not in models:
                    excluded["correction_without_model_identity"] += 1
                    continue
                source = value["kind"]
                if source not in ("explicit", "inferred"):
                    raise ValueError("unknown recorded correction kind")
                if source == "explicit" or evidence.get(key, (None,))[0] not in ("explicit", "confirmed"):
                    evidence[key] = (source, value.get("after"))

        parents = {key: key for key in inputs}
        def find(key):
            while parents[key] != key:
                parents[key] = parents[parents[key]]
                key = parents[key]
            return key
        def join(left, right):
            if left in parents and right in parents:
                parents[find(right)] = find(left)

        bodies = {}
        partitions = {}
        for key, item in inputs.items():
            hashes = request_hashes.get(key) or [text_digest(item["text"] or key)]
            group_hash = hashes[0]
            partitions[key] = "evaluation" if int(group_hash[:8], 16) % 5 == 0 else "calibration"
            for request_hash in hashes:
                if request_hash in bodies:
                    join(key, bodies[request_hash])
                bodies[request_hash] = key
        for left, right in links:
            join(left, right)
        members = {}
        for key in inputs:
            members.setdefault(find(key), []).append(key)

        profiles, rows = {}, []
        for key, model in models.items():
            item = inputs.get(model["input"])
            if item is None:
                excluded["removed_input"] += 1
                continue
            proof = evidence.get(key)
            if proof is None and item["state"] == "landed" and identity(item["decision"]) == key and model["input"] not in corrections:
                proof = ("uncorrected", item["decision"])
            if proof is None or proof[1] is None:
                excluded["unlabelled_decision"] += 1
                continue
            family = members[find(model["input"])]
            folds = {partitions[member] for member in family}
            if len(folds) != 1:
                excluded["related_inputs_cross_partitions"] += 1
                continue
            metadata = model["metadata"]
            profile = {name: metadata[name] for name in ("backend", "cachePrefix", "modelRevision", "promptVersion")}
            if any(not isinstance(profile[name], str) or not profile[name] for name in ("backend", "modelRevision", "promptVersion")):
                raise ValueError("invalid recorded model profile")
            profile_id = digest(profile)
            profiles[profile_id] = profile
            expected = labels(proof[1], metadata)
            stage_rows = []
            for stage, result in model["decision"]["scores"]["stages"].items():
                scores = result["logprobs"]
                if len(scores) < 2 or expected.get(stage) not in scores:
                    excluded["unlabelled_or_single_candidate_stage"] += 1
                    continue
                stage_rows.append(Observation.parse({"profile": profile_id, "group": digest(sorted(family)), "partition": next(iter(folds)),
                    "stage": stage, "source": proof[0], "scores": scores, "selected": result["selected"], "label": expected[stage], "weight": WEIGHTS[proof[0]]}))
            # A decision with multiple observed stages still contributes one label weight.
            for row in stage_rows:
                record = asdict(row)
                record["weight"] /= len(stage_rows)
                rows.append(record)
        validate_partitions([Observation.parse(row) for row in rows])
        return {"format": 1, "snapshotSequence": sequence, "profiles": profiles, "weights": WEIGHTS,
                "excluded": dict(excluded), "observations": rows}
    finally:
        connection.close()
