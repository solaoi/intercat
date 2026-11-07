//! Read the fixed result footer from the final message of the saved work turn.
use crate::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Done,
    Partial,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Footer {
    pub kind: Kind,
    pub reason: String,
}

/// Only one standalone footer is valid; quoted examples and earlier prose are not results.
pub fn parse(text: &str) -> Option<Footer> {
    let mut fence = None::<(char, usize)>;
    let mut candidate = None;
    let mut markers = 0;
    let mut last_content = None;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        let marker = trimmed.chars().next().filter(|c| matches!(c, '`' | '~'));
        let length = marker.map_or(0, |c| trimmed.chars().take_while(|x| *x == c).count());
        if let Some((open, count)) = fence {
            if marker == Some(open)
                && length >= count
                && trimmed.chars().skip(length).all(char::is_whitespace)
            {
                fence = None;
            }
            continue;
        }
        if length >= 3 {
            fence = Some((marker.unwrap(), length));
            last_content = Some(index);
            continue;
        }
        if trimmed.is_empty()
            || line
                .strip_prefix("Waiting for you:")
                .is_some_and(|s| !s.trim().is_empty())
        {
            continue;
        }
        last_content = Some(index);
        if let Some(value) = line.strip_prefix("Result:") {
            markers += 1;
            candidate = value
                .strip_prefix(' ')
                .and_then(|value| value.split_once(" — "))
                .and_then(|(kind, reason)| {
                    let kind = match kind {
                        "done" => Kind::Done,
                        "partial" => Kind::Partial,
                        "failed" => Kind::Failed,
                        _ => return None,
                    };
                    let reason = reason.trim();
                    (!reason.is_empty()).then(|| {
                        (
                            index,
                            Footer {
                                kind,
                                reason: reason.into(),
                            },
                        )
                    })
                });
        }
    }
    let (index, footer) = candidate?;
    (markers == 1 && last_content == Some(index) && fence.is_none()).then_some(footer)
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE INDEX IF NOT EXISTS timeline_final_reports ON timeline_items(session,json_extract(record,'$.turnId'))
        WHERE json_extract(record,'$.kind')='agentMessage' AND json_extract(record,'$.messagePhase')='finalAnswer';")?;
    Ok(())
}

fn final_report(db: &Connection, session: &str, turn: &str) -> Result<Option<Value>> {
    let row: Option<(String, i64, String)> = db.query_row(
        "SELECT item,revision,record FROM timeline_items WHERE session=?1 AND json_extract(record,'$.turnId')=?2
         AND json_extract(record,'$.kind')='agentMessage' AND json_extract(record,'$.messagePhase')='finalAnswer'
         ORDER BY rowid DESC LIMIT 1", params![session, turn], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    let Some((item, revision, record)) = row else {
        return Ok(None);
    };
    let record: Value = serde_json::from_str(&record)?;
    if record["status"] != "completed"
        || record["retracted"] == true
        || record.get("_intercatTruncated").is_some()
    {
        return Ok(None);
    }
    Ok(record["text"].as_str().and_then(parse).map(|footer| json!({
        "itemId":item,"revision":revision,"kind":footer.kind,"reason":crate::privacy::truncate(&footer.reason,240),
    })))
}

pub(crate) fn project(db: &Connection, tracks: &mut [Value], workers: &[Value]) -> Result<()> {
    let workers: HashMap<_, _> = workers
        .iter()
        .filter_map(|worker| Some((worker["trackId"].as_str()?, worker)))
        .collect();
    for track in tracks {
        track["completion"] = Value::Null;
        if track["status"] != "done" {
            continue;
        }
        let Some(worker) = track["id"].as_str().and_then(|id| workers.get(id)) else {
            continue;
        };
        if !worker["activeTurn"].is_null() || worker["checkpointPending"] != false {
            continue;
        }
        let (Some(session), Some(turn)) = (
            worker["sessionId"].as_str(),
            worker["checkpoint"]["turnId"].as_str(),
        ) else {
            continue;
        };
        track["completion"] =
            json!({"sessionId":session,"turnId":turn,"result":final_report(db,session,turn)?});
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_is_last_content_before_waiting_questions() {
        for (label, kind) in [
            ("done", Kind::Done),
            ("partial", Kind::Partial),
            ("failed", Kind::Failed),
        ] {
            assert_eq!(
                parse(&format!(
                    "変更と検証の報告。\nResult: {label} — 要求した範囲を確認しました。\nWaiting for you: 次の保存先はどちらですか。\n"
                )),
                Some(Footer {
                    kind,
                    reason: "要求した範囲を確認しました。".into()
                })
            );
        }
    }

    #[test]
    fn examples_missing_or_ambiguous_footers_are_not_success() {
        for text in [
            "No result",
            "Result: done",
            "Result: done — ",
            "Result: Done — reason",
            "Result: done - reason",
            "> Result: done — quoted",
            "    Result: done — code",
            "```text\nResult: done — code\n```",
            "~~~\nResult: done — code\n~~~",
            "Result: done — first\nResult: failed — last",
            "Result: done — first\nmore prose",
            "Result: done — reason\nWaiting for you:",
            "Result: done — reason\n```\nmore\n```",
            "Result: done — reason\n~~~",
        ] {
            assert_eq!(parse(text), None, "{text}");
        }
        assert_eq!(
            parse("```\nResult: failed — example\n```\nResult: done — actual"),
            Some(Footer {
                kind: Kind::Done,
                reason: "actual".into()
            })
        );
    }
}
