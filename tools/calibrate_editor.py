"""Export recorded labels, fit a temperature and report a separate held-out set."""

import argparse
from dataclasses import asdict
import json
import os
from pathlib import Path
import socket
import sys
import tempfile
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "services/editor/src"))
from intercat_editor.calibration import Observation, fit, metrics, validate_partitions
from intercat_editor.evaluation_data import digest, export


def apply_fit(args):
    operation = uuid.UUID(args.op_id)
    if operation.version != 7:
        raise ValueError("a version 7 operation ID is required")
    artifact = json.loads(args.input.read_text())
    # Read the private credential from an inherited pipe, never argv or env.
    with os.fdopen(os.dup(args.token_fd), "rb") as token_stream:
        token = token_stream.readline(130).decode().strip()
    if len(token) != 64 or any(character not in "0123456789abcdef" for character in token):
        raise ValueError("invalid RPC credential")
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(15)
        client.connect(str(args.socket))
        with client.makefile("rwb") as wire:
            def call(identifier, method, params):
                message = {"jsonrpc": "2.0", "id": identifier, "method": method, "params": params}
                wire.write((json.dumps(message) + "\n").encode())
                wire.flush()
                while True:
                    raw = wire.readline(16 * 1024 * 1024)
                    if not raw or not raw.endswith(b"\n"):
                        raise RuntimeError("invalid calibration RPC response")
                    response = json.loads(raw)
                    if response.get("id") != identifier:
                        continue
                    if "error" in response:
                        raise RuntimeError(f"calibration RPC rejected the request ({response['error'].get('code')})")
                    return response["result"]

            call(1, "rpc.authenticate", {"token": token})
            return call(2, "editor.calibrate", {
                "opId": str(operation), "revision": args.revision, "artifact": artifact,
            })


def main():
    parser = argparse.ArgumentParser()
    commands = parser.add_subparsers(dest="command", required=True)
    extract = commands.add_parser("export")
    extract.add_argument("--database", type=Path, required=True)
    extract.add_argument("--output", type=Path, required=True)
    calibrate = commands.add_parser("fit")
    calibrate.add_argument("--input", type=Path, required=True)
    calibrate.add_argument("--output", type=Path, required=True)
    install = commands.add_parser("apply")
    install.add_argument("--input", type=Path, required=True)
    install.add_argument("--socket", type=Path, required=True)
    install.add_argument("--token-fd", type=int, required=True)
    install.add_argument("--revision", type=int, required=True)
    install.add_argument("--op-id", required=True)
    args = parser.parse_args()
    if args.command == "apply":
        result = apply_fit(args)
        print(json.dumps({"command": "apply", "revision": result["revision"], "profiles": len(result["profiles"])}))
        return
    if args.command == "export":
        result = export(args.database)
    else:
        dataset = json.loads(args.input.read_text())
        if dataset.get("format") != 1:
            raise ValueError("unknown calibration dataset format")
        observations = [Observation.parse(row) for row in dataset["observations"]]
        validate_partitions(observations)
        result = {"format": 1, "datasetHash": digest(dataset), "profiles": {}}
        for profile in sorted({row.profile for row in observations}):
            identity = dataset["profiles"][profile]
            if set(identity) != {"backend", "cachePrefix", "modelRevision", "promptVersion"} or not isinstance(identity["cachePrefix"], bool) or digest(identity) != profile:
                raise ValueError("calibration profile identity does not match its digest")
            rows = [row for row in observations if row.profile == profile]
            training = [row for row in rows if row.partition == "calibration"]
            evaluation = [row for row in rows if row.partition == "evaluation"]
            if not training or not evaluation:
                raise ValueError("each profile needs calibration and independent evaluation groups")
            fitted = fit(training)
            result["profiles"][profile] = {
                "identity": identity,
                "fit": fitted,
                "calibrationHash": digest([asdict(row) for row in training]),
                "evaluationHash": digest([asdict(row) for row in evaluation]),
                "evaluationBefore": metrics(evaluation, 1.0),
                "evaluationAfter": metrics(evaluation, fitted["temperature"]),
            }
        if not result["profiles"]:
            raise ValueError("no labelled model decisions are available")
    rendered = json.dumps(result, ensure_ascii=False, indent=2, allow_nan=False) + "\n"
    with tempfile.TemporaryDirectory(prefix=".intercat-calibration-", dir=args.output.parent) as temporary:
        staged = Path(temporary) / "result.json"
        staged.write_text(rendered)
        os.link(staged, args.output)
    print(json.dumps({"command": args.command, "output": str(args.output), "profiles": len(result["profiles"])}))


if __name__ == "__main__":
    main()
