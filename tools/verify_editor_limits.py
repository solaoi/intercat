"""Complement V38/V50 with maximum input and process-residency measurements."""

import argparse
import json
import os
from pathlib import Path
import resource
import subprocess
import time

from verify_editor import Engine, PREFIX, Scheduler, payload
from intercat_editor.contract import DecisionRequest


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--model", required=True)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    save = lambda name, data: (args.output / name).write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
    holder = {}
    def factory():
        engine = Engine(args.model)
        holder["engine"] = engine
        return engine
    scheduler = Scheduler(factory)
    try:
        save("loaded.json", scheduler.ready.result(timeout=120))
        tokenizer = holder["engine"].tokenizer
        prefix = tokenizer.decode((tokenizer.encode(PREFIX, add_special_tokens=False) * 20)[:6000])
        value = payload(0, prefix)
        # Fill only diagnostic context, preserving the request; this is a bound probe,
        # not evidence for the classification quality of a repeated prefix.
        tail_ids = tokenizer.encode(value["tail"], add_special_tokens=False)
        filler = tokenizer.encode(" Context: dummy arithmetic tests. " * 200, add_special_tokens=False)
        value["tail"] = tokenizer.decode((tail_ids + filler)[:500])
        save("limit-request.json", value)
        rows = []
        for _ in range(3):
            row = scheduler.submit("decide", DecisionRequest.parse(value)).result(timeout=120)
            rows.append(row)
            print(json.dumps({key: row[key] for key in ("prefixTokens", "tailTokens", "inferenceMs", "totalMs")}), flush=True)
        save("limit-results.json", rows)
        samples = []
        for _ in range(3):
            sample = scheduler.submit("memory", None).result(timeout=10)
            rss = subprocess.check_output(["/bin/ps", "-o", "rss=", "-p", str(os.getpid())], text=True)
            sample.update(rssBytes=int(rss.strip()) * 1024, peakRssBytes=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss)
            samples.append(sample)
            time.sleep(1)
        save("memory.json", samples)
    finally:
        scheduler.close()


if __name__ == "__main__":
    main()
