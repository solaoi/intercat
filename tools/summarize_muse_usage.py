"""Count unique paid usage notifications without printing prompt contents."""
import argparse
import json
from pathlib import Path
import sqlite3


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--date", required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    events, sessions, turns = set(), {}, set()

    def collect(frame):
        if not isinstance(frame, dict) or frame.get("method") != "session/tokenUsage":
            return
        p = frame.get("params", {})
        usage = p.get("cumulative", {})
        if not str(p.get("modelId", "")).startswith("muse-spark"):
            return
        sid = p["sessionId"]
        key = (sid, p.get("turnId"), usage.get("totalTokens"))
        events.add(key)
        if p.get("turnId"):
            turns.add((sid, p["turnId"]))
        if usage.get("totalTokens", 0) >= sessions.get(sid, {}).get("totalTokens", 0):
            sessions[sid] = usage

    for path in args.root.rglob("events.sqlite"):
        if args.date not in str(path):
            continue
        with sqlite3.connect(f"file:{path.resolve()}?mode=ro", uri=True) as connection:
            for (text,) in connection.execute("SELECT payload FROM events WHERE payload LIKE '%session/tokenUsage%'"):
                collect(json.loads(text))
    for path in args.root.rglob("*.jsonl"):
        if args.date not in str(path):
            continue
        with path.open() as source:
            for line in source:
                try:
                    collect(json.loads(line))
                except (json.JSONDecodeError, TypeError):
                    continue
    unpriced = [sid for sid,item in sessions.items() if not isinstance(item.get("cost",{}).get("usd"),(int,float))]
    result = {"authorization": "User approved stored Muse credentials and up to 1000 paid dummy-fixture verification turns", "workerTurns": len(turns), "modelUsageEvents": len(events), "reportedCostUsd": sum(item.get("cost",{}).get("usd",0) for item in sessions.values()), "unpricedSessions": unpriced, "costSource": "Sum of available Muse cumulative costs only; incomplete when unpricedSessions is nonempty; not an invoice", "sessions": sessions}
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps({key: value for key, value in result.items() if key != "sessions"}))


if __name__ == "__main__":
    main()
