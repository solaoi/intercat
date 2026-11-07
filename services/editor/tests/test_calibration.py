import copy
import hashlib
import json
import math
from pathlib import Path
import sqlite3
import socket
import subprocess
import sys
import tempfile
import threading
import unittest

from intercat_editor.calibration import Observation, fit, metrics, nll, validate_partitions
from intercat_editor.evaluation_data import export, text_digest


def row(label="a", **changes):
    value = {"profile": "profile", "group": "group", "partition": "calibration", "stage": "decision",
             "source": "curated", "scores": {"a": -1.0, "b": -3.0}, "selected": "a", "label": label, "weight": 1.0}
    return Observation.parse(value | changes)


class TemperatureTests(unittest.TestCase):
    def test_binary_optimum_matches_the_closed_form(self):
        observations = [row() for _ in range(3)] + [row("b")]
        result = fit(observations)
        self.assertAlmostEqual(result["temperature"], 2 / math.log(3), places=10)
        self.assertLess(result["nllAfter"], result["nllBefore"])
        self.assertFalse(result["atBound"])

    def test_weights_and_constant_logit_shifts_are_preserved(self):
        observations = [row(weight=.3), row("b", weight=.1)]
        shifted = [row(weight=.3, scores={"a": -10001, "b": -10003}), row("b", weight=.1, scores={"a": -10001, "b": -10003})]
        self.assertAlmostEqual(fit(observations)["temperature"], fit(shifted)["temperature"], places=7)

    def test_one_sided_evidence_reports_a_bounded_fit(self):
        self.assertEqual(fit([row()])["temperature"], .05)
        self.assertEqual(fit([row("b")])["temperature"], 20)
        self.assertTrue(fit([row()])["atBound"])

    def test_evaluation_and_other_model_profiles_cannot_be_fitted(self):
        for observations in ([row(partition="evaluation")], [row(), row(profile="different")]):
            with self.assertRaises(ValueError):
                fit(observations)
        with self.assertRaises(ValueError):
            validate_partitions([row(), row(partition="evaluation")])

    def test_invalid_observations_are_rejected(self):
        for change in ({"weight": 0}, {"weight": True}, {"scores": {"a": float("nan"), "b": -1}}, {"label": "c"}, {"scores": {"a": -1}}):
            with self.subTest(change=change), self.assertRaises(ValueError):
                row(**change)

    def test_metrics_keep_stage_errors_separate_and_do_not_change_the_labels(self):
        rows = [row(), row("b", group="second", stage="lane", partition="evaluation")]
        before, after = metrics(rows, 1), metrics(rows, 2)
        self.assertEqual(before["accuracy"], after["accuracy"])
        self.assertEqual(after["stageErrors"], {"decision": {"samples": 1, "errors": 0}, "lane": {"samples": 1, "errors": 1}})
        self.assertLess(after["ece"], before["ece"])
        self.assertEqual(metrics([], 1), {"observations": 0})
        with self.assertRaises(ValueError):
            nll([], 1)

    def test_tied_scores_keep_the_recorded_model_choice(self):
        tied = row("b", selected="b", scores={"a": -1, "b": -1})
        self.assertEqual(metrics([tied], 2)["accuracy"], 1)
        with self.assertRaises(ValueError):
            row(selected="b")


class DatasetTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.path = Path(self.directory.name) / "events.sqlite"
        self.db = sqlite3.connect(self.path)
        self.db.executescript("CREATE TABLE utterances(id TEXT PRIMARY KEY,text TEXT,state TEXT,decision TEXT); CREATE TABLE events(seq INTEGER PRIMARY KEY,kind TEXT,payload TEXT);")

    def tearDown(self):
        self.db.close()
        self.directory.cleanup()

    def event(self, kind, payload):
        self.db.execute("INSERT INTO events(kind,payload) VALUES(?,?)", (kind, json.dumps(payload)))

    def model(self, key, text=None, state="landed", profile="prefix", prepared=None):
        text = key if text is None else text
        decision = {"kind": "extend", "trackId": "track-a", "boundary": None, "command": None,
            "scores": {"cachePrefix": False, "calibration": {"id": key, "backend": "GlimmerLogits(local)", "cachePrefix": False, "modelRevision": "model", "promptVersion": profile,
                "requestHash": text_digest(text if prepared is None else prepared), "labelTracks": {"1": "track-a", "2": "track-b"}},
                "stages": {"decision": {"selected": " A", "logprobs": {" A": -1, " B": -2}}, "lane": {"selected": "1", "logprobs": {"1": -1, "2": -3}}}}}
        self.db.execute("INSERT INTO utterances VALUES(?,?,?,?)", (key, text, state, json.dumps(decision)))
        self.event("utterance.classified", {"id": key, "revision": 1, "decision": decision})
        return decision

    def snapshot(self):
        self.db.commit()
        return export(self.path)

    def test_explicit_confirmed_and_weak_labels_have_distinct_weights(self):
        confirmed = self.model("confirmed")
        self.event("utterance.confirmed", {"inputId": "confirmed", "decision": confirmed})
        corrected = self.model("corrected")
        after = copy.deepcopy(corrected); after["trackId"] = "track-b"
        self.event("utterance.corrected", {"inputId": "corrected", "kind": "explicit", "before": corrected, "after": after})
        self.model("weak")
        dataset = self.snapshot()
        rows = dataset["observations"]
        self.assertEqual({row["source"] for row in rows}, {"confirmed", "explicit", "uncorrected"})
        self.assertEqual(next(row["label"] for row in rows if row["source"] == "explicit" and row["stage"] == "lane"), "2")
        self.assertEqual(sum(row["weight"] for row in rows if row["source"] == "uncorrected"), .1)
        self.assertEqual(sum(row["weight"] for row in rows if row["source"] == "confirmed"), 1)

    def test_unlabelled_misreads_never_become_weak_correct_examples(self):
        decision = self.model("wrong")
        self.event("utterance.corrected", {"inputId": "wrong", "kind": "explicit", "before": decision, "after": None})
        dataset = self.snapshot()
        self.assertEqual(dataset["observations"], [])
        self.assertEqual(dataset["excluded"]["unlabelled_decision"], 1)

    def test_related_corrections_cannot_cross_the_partition_boundary(self):
        texts = {}
        for i in range(100):
            text = f"request {i}"
            fold = int(text_digest(text)[:8], 16) % 5 == 0
            texts.setdefault(fold, text)
        original = self.model("original", texts[False], state="cut")
        replacement = self.model("replacement", texts[True])
        self.event("utterance.corrected", {"inputId": "original", "replacementInputId": "replacement", "kind": "inferred", "before": original, "after": replacement})
        dataset = self.snapshot()
        self.assertEqual(dataset["observations"], [])
        self.assertEqual(dataset["excluded"]["related_inputs_cross_partitions"], 1)

    def test_repeated_publication_of_the_same_model_result_is_one_example(self):
        decision = self.model("same")
        self.event("utterance.classified", {"id": "same", "revision": 2, "decision": decision})
        self.event("utterance.confirmed", {"inputId": "same", "decision": decision})
        self.assertEqual(len(self.snapshot()["observations"]), 2)
        altered = copy.deepcopy(decision); altered["scores"]["stages"]["lane"]["logprobs"]["2"] = -4
        self.event("utterance.classified", {"id": "same", "revision": 3, "decision": altered})
        with self.assertRaises(ValueError):
            self.snapshot()

    def test_identical_texts_stay_together_and_old_unrecorded_profiles_are_reported(self):
        self.model("one", " Ａ  request ")
        self.model("two", "a request")
        self.event("utterance.classified", {"id": "legacy", "decision": {"scores": {}}})
        dataset = self.snapshot()
        self.assertEqual(len({row["group"] for row in dataset["observations"]}), 1)
        self.assertEqual(dataset["excluded"]["unrecorded_model_profile"], 1)

    def test_unmatched_loading_modes_are_not_used_for_calibration(self):
        decision = self.model("cached")
        self.db.execute("DELETE FROM events")
        decision["scores"]["cachePrefix"] = True
        self.event("utterance.classified", {"id": "cached", "decision": decision})
        dataset = self.snapshot()
        self.assertEqual(dataset["observations"], [])
        self.assertEqual(dataset["excluded"]["unmatched_loading_mode"], 1)

    def test_cached_and_uncached_scores_have_distinct_profiles(self):
        self.model("uncached")
        decision = self.model("cached")
        self.db.execute("DELETE FROM events WHERE json_extract(payload,'$.id')='cached'")
        decision["scores"]["cachePrefix"] = True
        decision["scores"]["calibration"]["cachePrefix"] = True
        self.event("utterance.classified", {"id": "cached", "decision": decision})
        dataset = self.snapshot()
        self.assertEqual(len(dataset["profiles"]), 2)
        self.assertEqual({profile["cachePrefix"] for profile in dataset["profiles"].values()}, {False, True})
        self.assertEqual(len(dataset["observations"]), 4)

    def test_an_unrecorded_loading_mode_cannot_be_guessed_from_a_legacy_event(self):
        decision = self.model("unversioned")
        self.db.execute("DELETE FROM events")
        del decision["scores"]["calibration"]["cachePrefix"]
        self.event("utterance.classified", {"id": "unversioned", "decision": decision})
        dataset = self.snapshot()
        self.assertEqual(dataset["observations"], [])
        self.assertEqual(dataset["excluded"]["unmatched_loading_mode"], 1)

    def test_pasted_requests_use_the_prepared_body_fingerprint(self):
        self.model("one", "", prepared="the entire pasted request")
        self.model("two", "", prepared="the entire pasted request")
        dataset = self.snapshot()
        self.assertEqual(len({row["group"] for row in dataset["observations"]}), 1)
        self.assertEqual(len({row["partition"] for row in dataset["observations"]}), 1)

    def test_export_is_repeatable_and_never_changes_the_database(self):
        self.model("readonly")
        self.db.commit()
        before = hashlib.sha256(self.path.read_bytes()).hexdigest()
        first, second = export(self.path), export(self.path)
        self.assertEqual(first, second)
        self.assertEqual(hashlib.sha256(self.path.read_bytes()).hexdigest(), before)
        self.assertEqual(first["snapshotSequence"], 1)

    def test_cli_fits_only_calibration_groups_and_preserves_existing_outputs(self):
        for index in range(30):
            key = f"request-{index}"
            decision = self.model(key)
            if index % 4 == 0:
                corrected = copy.deepcopy(decision); corrected["kind"] = "fork"
                self.event("utterance.corrected", {"inputId": key, "kind": "explicit", "before": decision, "after": corrected})
            elif index % 3 == 0:
                self.event("utterance.confirmed", {"inputId": key, "decision": decision})
        self.db.commit()
        tool = Path(__file__).resolve().parents[3] / "tools/calibrate_editor.py"
        dataset_path = Path(self.directory.name) / "dataset.json"
        fitted_path = Path(self.directory.name) / "fitted.json"
        def run(*arguments):
            return subprocess.run([sys.executable, str(tool), *map(str, arguments)], capture_output=True, text=True)
        exported = run("export", "--database", self.path, "--output", dataset_path)
        self.assertEqual(exported.returncode, 0, exported.stderr)
        fitted = run("fit", "--input", dataset_path, "--output", fitted_path)
        self.assertEqual(fitted.returncode, 0, fitted.stderr)
        original = fitted_path.read_bytes()
        self.assertNotEqual(run("fit", "--input", dataset_path, "--output", fitted_path).returncode, 0)
        self.assertEqual(fitted_path.read_bytes(), original)
        dataset = json.loads(dataset_path.read_text())
        held_out = next(row for row in dataset["observations"] if row["partition"] == "evaluation")
        held_out["label"] = next(label for label in held_out["scores"] if label != held_out["label"])
        altered_path = Path(self.directory.name) / "altered.json"
        altered_path.write_text(json.dumps(dataset))
        second_path = Path(self.directory.name) / "second.json"
        result = run("fit", "--input", altered_path, "--output", second_path)
        self.assertEqual(result.returncode, 0, result.stderr)
        first = next(iter(json.loads(original)["profiles"].values()))
        second = next(iter(json.loads(second_path.read_text())["profiles"].values()))
        self.assertEqual(first["fit"], second["fit"])
        self.assertEqual(first["calibrationHash"], second["calibrationHash"])
        self.assertNotEqual(first["evaluationHash"], second["evaluationHash"])


class ApplyTests(unittest.TestCase):
    def run_apply(self, reject=False):
        with tempfile.TemporaryDirectory(prefix="ic-cal-", dir="/private/tmp") as directory:
            root = Path(directory)
            endpoint = root / "rpc.sock"
            artifact = {"format": 1, "datasetHash": "a" * 64, "profiles": {}}
            (root / "fit.json").write_text(json.dumps(artifact))
            received = []
            with socket.socket(socket.AF_UNIX) as server:
                server.bind(str(endpoint))
                server.listen()
                server.settimeout(10)

                def serve():
                    client, _ = server.accept()
                    with client, client.makefile("rwb") as wire:
                        for identifier in (1, 2):
                            request = json.loads(wire.readline())
                            received.append(request)
                            wire.write(b'{"jsonrpc":"2.0","method":"changed","params":{}}\n')
                            reply = {"jsonrpc": "2.0", "id": identifier}
                            if reject and identifier == 2:
                                reply["error"] = {"code": -32002, "message": "rejected"}
                            else:
                                reply["result"] = {"revision": 3, "profiles": {"profile": {}}}
                            wire.write((json.dumps(reply) + "\n").encode())
                            wire.flush()

                thread = threading.Thread(target=serve)
                thread.start()
                tool = Path(__file__).resolve().parents[3] / "tools/calibrate_editor.py"
                result = subprocess.run([
                    sys.executable, str(tool), "apply", "--input", str(root / "fit.json"),
                    "--socket", str(endpoint), "--token-fd", "0", "--revision", "2",
                    "--op-id", "019b1d36-59b0-7000-8000-000000000001",
                ], input="d" * 64 + "\n", text=True, capture_output=True, timeout=15)
                thread.join(10)
                self.assertFalse(thread.is_alive())
            self.assertEqual(received[0]["method"], "rpc.authenticate")
            self.assertEqual(received[1]["params"]["artifact"], artifact)
            self.assertEqual(received[1]["params"]["revision"], 2)
            self.assertNotIn("d" * 64, result.stdout + result.stderr)
            return result

    def test_apply_authenticates_with_a_pipe_and_ignores_notifications(self):
        result = self.run_apply()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {"command": "apply", "revision": 3, "profiles": 1})

    def test_rejected_installation_fails_without_exposing_the_credential(self):
        result = self.run_apply(reject=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("-32002", result.stderr)


if __name__ == "__main__":
    unittest.main()
