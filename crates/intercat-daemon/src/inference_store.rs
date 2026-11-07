//! Durable work and provenance for inferred corrections.
use crate::{
    Result,
    clock::Stamp,
    decision::{Decision, Kind, Target},
    input_inference::{Candidate, Evaluation, TakenBack, Text, Ticket},
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::json;

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS input_inference_jobs(input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE,revision INTEGER NOT NULL,done INTEGER NOT NULL,retry_at INTEGER NOT NULL,failed INTEGER NOT NULL);
        CREATE TRIGGER IF NOT EXISTS input_inference_landed AFTER UPDATE OF state ON utterances
        WHEN NEW.state='landed' AND OLD.state<>'landed' AND json_type(NEW.decision,'$.trackId')='text' BEGIN
          INSERT INTO input_inference_jobs VALUES(NEW.id,NEW.revision,0,0,0)
          ON CONFLICT(input) DO UPDATE SET revision=excluded.revision,done=0,retry_at=0,failed=0 WHERE revision<>excluded.revision;
        END;
        CREATE INDEX IF NOT EXISTS input_inference_pending ON input_inference_jobs(done,retry_at,input);
        CREATE INDEX IF NOT EXISTS input_takeback_mono ON events(boot,mono,seq) WHERE kind='utterance.taken-back';
        CREATE INDEX IF NOT EXISTS input_takeback_wall ON events(ts,seq) WHERE kind='utterance.taken-back';
        CREATE INDEX IF NOT EXISTS input_reworded ON events(json_extract(payload,'$.id'),seq DESC) WHERE kind IN ('utterance.staged','utterance.reworded');
        CREATE UNIQUE INDEX IF NOT EXISTS input_inferred_source ON events(json_extract(payload,'$.takeBackSequence')) WHERE kind='utterance.corrected' AND json_extract(payload,'$.kind')='inferred';")?;
    Ok(())
}

pub(crate) fn text(db: &Connection, input: &str) -> Result<Text> {
    Ok(Text {
        text: db.query_row("SELECT text FROM utterances WHERE id=?1", [input], |row| {
            row.get(0)
        })?,
        sources: crate::request_sources::read(db, input)?
            .map(|record| record.sources)
            .unwrap_or_default(),
    })
}

pub(crate) fn reworded(db: &Transaction<'_>, input: &str, before: &Text) -> Result<()> {
    let after = text(db, input)?;
    if before.text == after.text && before.sources == after.sources {
        return Ok(());
    }
    let session: Option<String> = db
        .query_row(
            "SELECT json_extract(value,'$.id') FROM settings WHERE key='session'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.reworded',?2)",
        params![
            Stamp::now().epoch_ms,
            json!({"id":input,"text":after.text,"sources":after.sources,"sessionId":session})
                .to_string()
        ],
    )?;
    Ok(())
}

pub(crate) fn take_back(db: &Transaction<'_>, input: &str) -> Result<()> {
    let (target, decision, revision, state): (String, Option<String>, i64, String) = db.query_row(
        "SELECT target,decision,revision,state FROM utterances WHERE id=?1",
        [input],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    if !["placed", "held", "redirecting"].contains(&state.as_str())
        || !matches!(serde_json::from_str::<Target>(&target)?, Target::None)
    {
        return Ok(());
    }
    let Some(decision) = decision else {
        return Ok(());
    };
    let decision: Decision = serde_json::from_str(&decision)?;
    if !matches!(
        decision.kind,
        Kind::Fork | Kind::Extend | Kind::Schedule | Kind::Trim
    ) || decision.track_id.is_none()
        || decision.scores["human"] == true
    {
        return Ok(());
    }
    let session_id: Option<String> = db
        .query_row(
            "SELECT json_extract(value,'$.id') FROM settings WHERE key='session'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(session_id) = session_id else {
        return Ok(());
    };
    let taken = TakenBack {
        input_id: input.into(),
        revision: revision + 1,
        session_id,
        text: text(db, input)?,
        decision,
        at: Stamp::now(),
    };
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.taken-back',?2)",
        params![taken.at.epoch_ms, serde_json::to_string(&taken)?],
    )?;
    Ok(())
}

pub(crate) fn next(db: &Connection) -> Result<Option<Ticket>> {
    let row: Option<(String,i64,String)> = db.query_row("SELECT j.input,j.revision,u.decision FROM input_inference_jobs j JOIN utterances u ON u.id=j.input
        WHERE j.done=0 AND j.retry_at<=?1 AND u.state='landed' AND u.revision=j.revision ORDER BY j.rowid LIMIT 1",[Stamp::now().epoch_ms],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
    let Some((input, revision, decision)) = row else {
        return Ok(None);
    };
    let decision: Decision = serde_json::from_str(&decision)?;
    let origin: Option<(Stamp, Option<String>)> = db
        .query_row(
            "SELECT ts,mono,boot,position,json_extract(payload,'$.sessionId') FROM events
        WHERE kind IN ('utterance.staged','utterance.reworded') AND json_extract(payload,'$.id')=?1 ORDER BY seq DESC LIMIT 1",
            [&input],
            |row| {
                Ok((
                    Stamp {
                        epoch_ms: row.get(0)?,
                        mono_ms: row.get(1)?,
                        boot_id: row.get(2)?,
                        timeline_ms: row.get(3)?,
                    },
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let mut candidates = Vec::new();
    let (put_at, session) = origin.unwrap_or_else(|| (Stamp::now(), None));
    if let Some(session) = session {
        let rows = db.prepare("SELECT seq,payload FROM events WHERE kind='utterance.taken-back' AND boot=?1 AND mono BETWEEN ?2 AND ?3
            UNION ALL SELECT seq,payload FROM events WHERE kind='utterance.taken-back' AND boot<>?1 AND ts BETWEEN ?4 AND ?5 ORDER BY seq DESC")?
            .query_map(params![put_at.boot_id,put_at.mono_ms.saturating_sub(30_000),put_at.mono_ms,put_at.epoch_ms.saturating_sub(30_000),put_at.epoch_ms],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (sequence, raw) in rows {
            let original: TakenBack = serde_json::from_str(&raw)?;
            if original.session_id == session
                && original.input_id != input
                && original.decision.track_id != decision.track_id
                && eligible(db, sequence, &original)?
            {
                candidates.push(Candidate { sequence, original });
            }
        }
    }
    Ok(Some(Ticket {
        input: input.clone(),
        revision,
        text: text(db, &input)?,
        decision,
        put_at,
        candidates,
    }))
}

fn eligible(db: &Connection, sequence: i64, source: &TakenBack) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM utterances WHERE id=?1 AND state='cut' AND revision=?2)
        AND NOT EXISTS(SELECT 1 FROM events WHERE kind='utterance.corrected' AND json_extract(payload,'$.kind')='inferred' AND json_extract(payload,'$.takeBackSequence')=?3)",
        params![source.input_id,source.revision,sequence],|row|row.get(0))?)
}

pub(crate) fn finish(db: &mut Connection, ticket: &Ticket, result: Evaluation) -> Result<bool> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM input_inference_jobs j JOIN utterances u ON u.id=j.input
        WHERE j.input=?1 AND j.revision=?2 AND j.done=0 AND u.state='landed' AND u.revision=j.revision)",params![ticket.input,ticket.revision],|row|row.get(0))?;
    if !pending {
        return Ok(false);
    }
    if let Evaluation::Matched(matched) = &result {
        let source = ticket
            .candidates
            .iter()
            .find(|source| source.sequence == matched.sequence)
            .ok_or("Unknown inferred-correction source.")?;
        if matched.characters == 0 || matched.edits > matched.characters / 5 {
            return Err("Invalid inferred-correction distance.".into());
        }
        if !eligible(&tx, source.sequence, &source.original)? {
            return Ok(false);
        }
        let payload = json!({"inputId":source.original.input_id,"revision":source.original.revision,"kind":"inferred","reason":"resubmitted",
            "text":source.original.text.text,"sources":source.original.text.sources,"before":source.original.decision,"after":ticket.decision,
            "replacementInputId":ticket.input,"replacementRevision":ticket.revision,"takeBackSequence":source.sequence,"putAt":ticket.put_at,
            "distance":{"edits":matched.edits,"characters":matched.characters}});
        tx.execute(
            "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.corrected',?2)",
            params![Stamp::now().epoch_ms, payload.to_string()],
        )?;
    } else if matches!(result, Evaluation::Unavailable) {
        let failed: bool = tx.query_row(
            "SELECT failed FROM input_inference_jobs WHERE input=?1",
            [&ticket.input],
            |row| row.get(0),
        )?;
        if !failed {
            tx.execute("INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.correction-unavailable',?2)",params![Stamp::now().epoch_ms,json!({"inputId":ticket.input,"revision":ticket.revision,"reason":"Recorded request text could not be compared. Retrying."}).to_string()])?;
        }
        tx.execute(
            "UPDATE input_inference_jobs SET failed=1,retry_at=?2 WHERE input=?1",
            params![ticket.input, Stamp::now().epoch_ms + 1000],
        )?;
        tx.commit()?;
        return Ok(true);
    }
    tx.execute(
        "UPDATE input_inference_jobs SET done=1,retry_at=0,failed=0 WHERE input=?1 AND revision=?2",
        params![ticket.input, ticket.revision],
    )?;
    tx.commit()?;
    Ok(true)
}
