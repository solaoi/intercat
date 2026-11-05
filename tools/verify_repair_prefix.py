"""Verify a dummy repair's saved preview evidence without calling Muse."""
import argparse
import json
import sqlite3
from pathlib import Path


def verify(root: Path) -> dict:
    samples = json.loads((root / "previews.json").read_text())
    result = json.loads((root / "result.json").read_text())
    with sqlite3.connect(root / "events.sqlite") as db:
        turns = db.execute(
            "SELECT count(*) FROM operations WHERE kind='turn.start'"
        ).fetchone()[0]
        raw_deltas = db.execute(
            "SELECT count(*) FROM events WHERE "
            "json_extract(payload,'$.method')='item/delta' AND "
            "json_type(payload,'$.params.delta') IS NOT NULL"
        ).fetchone()[0]
        delta_items = dict(db.execute(
            "SELECT json_extract(payload,'$.params.itemId'), count(*) FROM events "
            "WHERE kind='msp.applied' AND json_extract(payload,'$.method')='item/delta' "
            "AND json_extract(payload,'$.params.field')='text' GROUP BY 1"
        ))
        prefixes = 0
        for sample in samples:
            preview = sample["preview"]
            rows = db.execute(
                "SELECT json_extract(payload,'$.params.item.text') FROM events "
                "WHERE kind='msp.applied' AND "
                "json_extract(payload,'$.method')='item/completed' AND "
                "json_extract(payload,'$.params.item.itemId')=?",
                (preview["itemId"],),
            )
            if preview["itemId"] in delta_items and sample["completedItems"] == 0 and any(
                text and 0 < len(preview["text"]) < len(text)
                and text.startswith(preview["text"]) for (text,) in rows
            ):
                prefixes += 1
    assert turns == 1, turns
    assert raw_deltas == 0, raw_deltas
    assert delta_items, "Host did not emit text deltas"
    assert prefixes > 0, "No incomplete prefix observed before recorded completion"
    assert result["sourceUnchanged"] is True
    assert result["candidate"]["state"] == "checked"
    return dict(directory=str(root), paidTurns=turns, samples=len(samples),
                textDeltas=sum(delta_items.values()), incompletePrefixes=prefixes, rawRecordedDeltas=raw_deltas,
                candidateChecked=True, sourceUnchanged=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    print(json.dumps(verify(args.directory.resolve()), ensure_ascii=False, indent=2))
