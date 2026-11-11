//! Protected local records for interrupted recovery and completed-turn handoff.
use crate::{Result, runtime::WorkerState};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

#[derive(Clone, Copy)]
pub(crate) enum Boundary {
    Interrupted,
    Completed,
}

pub(crate) fn capture(
    tx: &Connection,
    source: &crate::handoff::Identity,
    boundary: Boundary,
    previous: &WorkerState,
    key: &crate::secrets::Key,
) -> Result<Value> {
    if previous.track_id != source.track_id
        || previous.session_id.as_deref() != Some(&source.session_id)
    {
        return Err("The handoff history belongs to another worker.".into());
    }
    let receipt_turn = match boundary {
        Boundary::Interrupted => previous.active_turn,
        Boundary::Completed => {
            if previous.active_turn.is_some()
                || previous.checkpoint_pending
                || !previous.approvals.is_empty()
                || !previous.prompts.is_empty()
            {
                return Err(
                    "Finish the current turn and save its checkpoint before starting fresh.".into(),
                );
            }
            previous.checkpoint.as_ref().ok_or("No checkpoint yet.")?;
            // Realignment and restored-file checkpoints are saved points too,
            // but have no SDK tool turn. Collect receipts from the last ended
            // turn of this source session instead of inventing such a turn.
            // Checkpoint order survives out-of-order SDK history replay.
            tx.query_row("SELECT t.turn FROM timeline_turns t JOIN operations o ON json_extract(o.payload,'$.checkpoint.turnId')=t.turn WHERE t.session=?1 AND json_extract(t.record,'$.track')=?2 AND json_extract(t.record,'$.terminal')=1 AND o.kind='checkpoint.save' AND o.state='complete' AND json_extract(o.payload,'$.trackId')=?2 ORDER BY o.rowid DESC LIMIT 1",params![source.session_id,source.track_id],|row|row.get::<_,String>(0)).optional()?.map(|turn|turn.parse()).transpose()?
        }
    };
    let receipt_turn = receipt_turn.map(|turn| turn.to_string());
    let item_bytes:i64=tx.query_row("SELECT coalesce(sum(length(i.record)),0) FROM timeline_items i JOIN timeline_clips c ON c.id=i.clip WHERE i.session=?1 AND (json_extract(i.record,'$.kind')='userMessage' OR (json_extract(i.record,'$.kind')='toolCall' AND json_extract(i.record,'$.status')<>'inProgress' AND json_extract(c.record,'$.turnId')=?2))",params![source.session_id,receipt_turn],|r|r.get(0))?;
    let clip_bytes:i64=tx.query_row("SELECT coalesce(sum(length(i.summary)),0) FROM timeline_items i WHERE i.clip IN (SELECT id FROM timeline_clips WHERE track=?1 AND json_extract(record,'$.sessionId')=?2 AND json_extract(record,'$.repairRunId') IS NULL ORDER BY ordinal DESC LIMIT 3)",params![source.track_id,source.session_id],|r|r.get(0))?;
    let question_bytes:i64=tx.query_row("SELECT coalesce(sum(length(record)),0) FROM report_questions WHERE track_id=?1 AND json_extract(record,'$.sessionId')=?2 AND state IN ('open','queued','held')",params![source.track_id,source.session_id],|r|r.get(0))?;
    if item_bytes
        + clip_bytes
        + question_bytes
        + previous.output.as_ref().map_or(0, |t| t.len() as i64)
        > crate::handoff::CONTEXT_BYTES as i64
    {
        return Err(
            "The local history exceeds the protected summary input limit. It remains held.".into(),
        );
    }
    let mut clips = Vec::new();
    let mut query = tx.prepare("SELECT id,record FROM timeline_clips WHERE track=?1 AND json_extract(record,'$.sessionId')=?2 AND json_extract(record,'$.repairRunId') IS NULL ORDER BY ordinal DESC LIMIT 3")?;
    for row in query.query_map(params![source.track_id, source.session_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (id, record) = row?;
        let mut items =
            tx.prepare("SELECT summary FROM timeline_items WHERE clip=?1 ORDER BY rowid")?;
        let items = items
            .query_map([&id], |r| r.get::<_, String>(0))?
            .map(|r| Ok(serde_json::from_str::<Value>(&r?)?))
            .collect::<Result<Vec<_>>>()?;
        clips.push(
            json!({"address":id,"clip":serde_json::from_str::<Value>(&record)?,"items":items}),
        );
    }
    clips.reverse();
    let mut query=tx.prepare("SELECT i.record,c.id,c.record FROM timeline_items i JOIN timeline_clips c ON c.id=i.clip WHERE i.session=?1 AND (json_extract(i.record,'$.kind')='userMessage' OR (json_extract(i.record,'$.kind')='toolCall' AND json_extract(i.record,'$.status')<>'inProgress' AND json_extract(c.record,'$.turnId')=?2)) ORDER BY i.rowid")?;
    let mut instructions = Vec::new();
    let mut completed_tools = Vec::new();
    for row in query.query_map(params![source.session_id, receipt_turn], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })? {
        let (item, address, clip) = row?;
        let item: Value = serde_json::from_str(&item)?;
        if item["kind"] == "userMessage" {
            instructions.push(json!({"address":address,"message":item}));
        } else if item["kind"] == "toolCall"
            && item["status"] != "inProgress"
            && receipt_turn.as_ref().is_some_and(|turn| {
                serde_json::from_str::<Value>(&clip).is_ok_and(|clip| clip["turnId"] == *turn)
            })
        {
            completed_tools.push(json!({"address":address,"tool":item}));
        }
    }
    let title: String = tx.query_row(
        "SELECT title FROM tracks WHERE id=?1",
        [&source.track_id],
        |r| r.get(0),
    )?;
    let mut questions=tx.prepare("SELECT record FROM report_questions WHERE track_id=?1 AND json_extract(record,'$.sessionId')=?2 AND state IN ('open','queued','held') ORDER BY rowid")?;
    let questions = questions
        .query_map(params![source.track_id, source.session_id], |r| {
            r.get::<_, String>(0)
        })?
        .map(|r| Ok(serde_json::from_str::<Value>(&r?)?))
        .collect::<Result<Vec<_>>>()?;
    let unverified = match boundary {
        Boundary::Interrupted => {
            "The previous turn did not finish. Inspect the current files before continuing. Unanswered questions must remain unresolved until the user answers them."
        }
        Boundary::Completed => {
            "The previous turn ended and its checkpoint was saved. A saved checkpoint and tool receipts do not establish that every check passed. Inspect the transferred files before continuing. Unanswered questions must remain unresolved until the user answers them."
        }
    };
    let unfinished = match boundary {
        Boundary::Interrupted => previous.active_turn,
        Boundary::Completed => None,
    };
    let mut value = json!({"purpose":title,"instructions":instructions,"lastClips":clips,"unfinishedTurn":unfinished,"completedTools":completed_tools,"lastReport":previous.output,"unresolvedQuestions":questions,"unverified":unverified});
    key.mask(crate::secret_store::read(
        tx,
        &source.track_id,
        &source.session_id,
    )?)?
    .redact_value(&mut value)?;
    if value.get("_intercatTruncated").is_some() {
        return Err(
            "A protected history field is too large to summarize without losing evidence.".into(),
        );
    }
    if serde_json::to_vec(&value)?.len() > crate::handoff::CONTEXT_BYTES {
        return Err(
            "The local history exceeds the protected summary input limit. It remains held.".into(),
        );
    }
    Ok(value)
}
