"""V52: observe one real MSP deadline using only a generated dummy workspace.

Each invocation starts at most one paid turn. Results and raw frames stay under
the explicitly supplied private output directory; credentials are only linked.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import signal
import time

from verify_host_hooks import Host, command_id


PINNED_SHA = "1c96114408acbeade9a8fb79e3b4425e5b64d50425938568aef01bdc04ce2e03"


def initialize(host):
    host.request("initialize", {"clientInfo": {"name": "intercat_deadline_probe", "version": "0.1.0"},
                                "capabilities": {"userInputDialogs": True}})
    host.send({"jsonrpc": "2.0", "method": "initialized"})


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--auth", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--scenario", choices=["question", "approval"], required=True)
    parser.add_argument("--engaged", action="store_true")
    parser.add_argument("--wait-seconds", type=float, default=180)
    parser.add_argument("--restart-after", type=float)
    parser.add_argument("--answer-restored", action="store_true")
    parser.add_argument("--questions", type=int, choices=[1, 2, 3], default=1)
    parser.add_argument("--partial-answer", action="store_true")
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    if hashlib.sha256(binary.read_bytes()).hexdigest() != PINNED_SHA:
        raise RuntimeError("The probe requires the verified Muse 1.4.14 binary.")
    if args.wait_seconds <= 0 or args.wait_seconds > 900:
        raise RuntimeError("The observation budget must be between 0 and 900 seconds.")
    if args.restart_after is not None and not 0 < args.restart_after < args.wait_seconds:
        raise RuntimeError("The restart must happen during the observation budget.")
    if args.answer_restored and (args.restart_after is None or args.scenario != "question"):
        raise RuntimeError("Answering a restored prompt requires the question restart probe.")
    if (args.questions != 1 or args.partial_answer) and args.scenario != "question":
        raise RuntimeError("Question counts and partial answers require the question scenario.")
    if args.partial_answer and args.questions < 2:
        raise RuntimeError("A partial-answer probe needs at least two questions.")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False, mode=0o700)
    (output / "probe-source.py").write_bytes(Path(__file__).read_bytes())
    root = output / "workspace"
    root.mkdir(mode=0o700)
    config = root / "config"
    muse_config = config / "muse"
    muse_config.mkdir(parents=True, mode=0o700)
    (muse_config / "auth.json").symlink_to(args.auth.resolve(strict=True))
    (muse_config / "settings.json").write_text(json.dumps({
        "schema_version": 1, "provider": "meta", "model": "muse-spark-1.3-contributor",
        "contributor_mode_enabled": False, "agents": {"execution_capacity": 1},
        "run": {"toolset": ["request_user_input"] if args.scenario == "question" else ["bash"]},
        "permissions": {"schema_version": 1, "default_profile": "intercat-worker",
                        "profiles": {"intercat-worker": {"extends": ":ask-me"}}},
    }))
    if args.scenario == "question":
        prompt = (f'This is a disposable protocol test. Use request_user_input exactly once with {args.questions} questions. '
                  'Ask a dummy choice for each question, with options "Red" and "Blue". '
                  'Do not choose an answer yourself. Do not use any other tool. '
                  'After the question is settled, reply only "PROBE COMPLETE".')
        wanted = "userInput/request"
    else:
        marker = output / "dummy-outside-workspace.txt"
        prompt = (f'This is a disposable protocol test. Use bash exactly once to execute '
                  f'printf V52 > "{marker}". This path was created for this test outside the workspace. '
                  'Do not use another tool or modify another file. After the tool settles, '
                  'reply only "PROBE COMPLETE".')
        wanted = "approval/request"
    result = {"scenario": args.scenario, "engaged": args.engaged, "startedTurns": 0,
              "requested": False, "settled": None, "terminal": None, "observedSeconds": 0,
              "restartAfter": args.restart_after, "restarted": False,
              "requestedQuestionCount": args.questions, "partialAnswerProbe": args.partial_answer}
    host = Host(binary, root, config, output, False)
    session = None
    turn = None
    try:
        initialize(host)
        opened = host.request("session/start", {"commandId": command_id(), "workspaceRoot": str(root),
                                                "providerId": "meta", "approvalMode": "promptUnmatched"})
        session = opened["session"]["sessionId"]
        result["startedTurns"] = 1
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        ack = host.request("turn/start", {"commandId": command_id(), "sessionId": session,
                                          "reasoningEffort": "minimal", "input": [{"type": "text", "text": prompt}]})
        turn = ack["turnId"]
        pending = list(host.events)
        host.events.clear()
        request_at = None
        restart_at = None
        limit = time.monotonic() + 90
        while True:
            try:
                wake = min(limit, restart_at) if restart_at is not None else limit
                frame = pending.pop(0) if pending else host.receive(wake)
            except queue.Empty:
                if restart_at is not None and time.monotonic() >= restart_at:
                    # The child owns this process group and has not been reaped,
                    # so this cannot target a reused PID or another worker.
                    if host.process.poll() is None:
                        os.killpg(host.process.pid, signal.SIGKILL)
                    host.close()
                    restarted = output / "restarted"
                    restarted.mkdir(mode=0o700)
                    host = Host(binary, root, config, restarted, False)
                    initialize(host)
                    result["resumedSession"] = host.request("session/resume", {
                        "commandId": command_id(), "sessionId": session, "excludeItems": True})
                    result["resumedPending"] = host.request("approval/listPending", {"sessionId": session})
                    result["restarted"] = True
                    pending.extend(host.events)
                    host.events.clear()
                    if args.engaged and args.scenario == "question":
                        host.send({"jsonrpc": "2.0", "method": "userInput/engaged", "params": {
                            "sessionId": session, "userInputId": result["request"]["userInputId"]}})
                    restart_at = None
                    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
                    print(json.dumps({"phase": "resumed", "scenario": args.scenario}), flush=True)
                    continue
                break
            method, params = frame.get("method"), frame.get("params", {})
            if method == wanted and request_at is None:
                request_at = time.monotonic()
                result["requested"] = True
                result["request"] = params
                auto_ms = params.get("autoResolutionMs")
                result["declaredTimeoutMs"] = auto_ms
                result["questionCount"] = len(params.get("questions", []))
                if args.scenario == "question" and result["questionCount"] != args.questions:
                    raise RuntimeError("The model did not provide the requested number of dummy questions.")
                if args.partial_answer:
                    first = params["questions"][0]
                    partial = [{"questionId": first["id"], "selectedLabel": first["options"][0]["label"]}]
                    result["partialAnswers"] = partial
                    try:
                        result["partialAnswerResult"] = host.request("userInput/answer", {
                            "commandId": command_id(), "sessionId": session,
                            "userInputId": params["userInputId"], "answers": partial})
                    except RuntimeError as error:
                        result["partialAnswerError"] = json.loads(str(error))
                    pending.extend(host.events)
                    host.events.clear()
                duration = args.wait_seconds
                if isinstance(auto_ms, (int, float)) and auto_ms > 0:
                    duration = min(duration, auto_ms * max(1, result["questionCount"]) / 1000 + 5)
                limit = request_at + duration
                if args.restart_after is not None:
                    restart_at = request_at + args.restart_after
                (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
                print(json.dumps({"phase": "observing", "scenario": args.scenario,
                                  "engaged": args.engaged, "declaredTimeoutMs": auto_ms,
                                  "budgetSeconds": duration}), flush=True)
            if method == wanted:
                if args.engaged and args.scenario == "question":
                    host.send({"jsonrpc": "2.0", "method": "userInput/engaged", "params": {
                        "sessionId": session, "userInputId": params["userInputId"]}})
                if "id" in frame:
                    host.send({"jsonrpc": "2.0", "id": frame["id"], "result": {}})
            if method in ["userInput/settled", "approval/resolved"] and request_at is not None:
                result["settled"] = params
                result["settledAfterSeconds"] = time.monotonic() - request_at
            if method == "turn/completed" and params.get("turnId") == turn:
                result["terminal"] = params
                break
        if request_at is not None:
            result["observedSeconds"] = time.monotonic() - request_at
        result["sessionBeforeCleanup"] = host.request("session/read", {"sessionId": session, "excludeItems": True})
        result["pendingBeforeCleanup"] = host.request("approval/listPending", {"sessionId": session})
        if args.partial_answer and result.get("partialAnswers"):
            try:
                result["partialInterrupt"] = host.request("userInput/interrupt", {
                    "commandId": command_id(), "sessionId": session,
                    "userInputId": result["request"]["userInputId"],
                    "answers": result["partialAnswers"]})
                buffered = list(host.events)
                host.events.clear()
                observed = []
                deadline = time.monotonic() + 10
                while True:
                    try:
                        frame = buffered.pop(0) if buffered else host.receive(deadline)
                    except queue.Empty:
                        break
                    if frame.get("method") in ["userInput/settled", "turn/completed"]:
                        observed.append(frame)
                    if frame.get("method") == "turn/completed" and frame.get("params", {}).get("turnId") == turn:
                        break
                result["afterPartialInterrupt"] = observed
            except RuntimeError as error:
                result["partialInterruptError"] = json.loads(str(error))
        if args.answer_restored and result["restarted"]:
            prompts = result["pendingBeforeCleanup"].get("userInputs", [])
            if len(prompts) == 1:
                restored = prompts[0]
                answers = [{"questionId": question["id"], "selectedLabel": question["options"][0]["label"]}
                           for question in restored["questions"]]
                host.events.clear()
                try:
                    result["restoredAnswer"] = host.request("userInput/answer", {
                        "commandId": command_id(), "sessionId": session,
                        "userInputId": restored["userInputId"], "answers": answers})
                except RuntimeError as error:
                    result["restoredAnswerError"] = json.loads(str(error))
                observed = []
                buffered = list(host.events)
                host.events.clear()
                deadline = time.monotonic() + 10
                while True:
                    try:
                        frame = buffered.pop(0) if buffered else host.receive(deadline)
                    except queue.Empty:
                        break
                    if frame.get("method") in ["turn/started", "turn/completed", "userInput/settled"]:
                        observed.append(frame)
                result["afterRestoredAnswer"] = observed
                result["sessionAfterRestoredAnswer"] = host.request("session/read", {"sessionId": session, "excludeItems": True})
            else:
                result["restoredAnswerSkipped"] = "Expected exactly one remaining dummy prompt."
    finally:
        if session and turn:
            try:
                host.request("turn/interrupt", {"commandId": command_id(), "sessionId": session, "turnId": turn})
            except (RuntimeError, queue.Empty):
                pass
        host.close()
        (muse_config / "auth.json").unlink(missing_ok=True)
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({"phase": "complete", "scenario": args.scenario, "engaged": args.engaged,
                      "requested": result["requested"], "observedSeconds": result["observedSeconds"],
                      "settled": result["settled"] is not None, "terminal": result["terminal"] is not None}), flush=True)


if __name__ == "__main__":
    main()
