//! Assumption extraction and answers share Journal's single transaction writer.
use crate::{
    Result,
    assumptions::{Answer, Assumption, Command, Finish, Source, Ticket},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS assumption_sources(
        id TEXT PRIMARY KEY, clip TEXT NOT NULL, text TEXT NOT NULL,
        state TEXT NOT NULL, request TEXT, attempts INTEGER NOT NULL);
        CREATE INDEX IF NOT EXISTS pending_assumption_sources ON assumption_sources(state,attempts);
        CREATE TABLE IF NOT EXISTS assumptions(
        id TEXT PRIMARY KEY, source TEXT NOT NULL, clip TEXT NOT NULL,
        state TEXT NOT NULL, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS clip_assumptions ON assumptions(clip,state);
        CREATE INDEX IF NOT EXISTS timed_assumptions ON assumptions(json_extract(record,'$.observedAt.timelineMs'),clip);
        CREATE TRIGGER IF NOT EXISTS close_assumed_questions AFTER UPDATE ON timeline_clips
        WHEN json_type(OLD.record,'$.end')='null' AND json_type(NEW.record,'$.end')='object' BEGIN
            UPDATE assumptions SET record=json_set(record,'$.source','question-timeout')
            WHERE clip=NEW.id AND json_type(record,'$.questionId')='text' AND json_extract(record,'$.source')<>'question-timeout';
        END;
        CREATE TRIGGER IF NOT EXISTS assumption_source_insert AFTER INSERT ON timeline_items
        WHEN json_extract(NEW.record,'$.kind')='agentMessage' AND length(json_extract(NEW.record,'$.text'))>0 BEGIN
            INSERT INTO assumption_sources VALUES(json_array(NEW.session,NEW.item),NEW.clip,json_extract(NEW.record,'$.text'),'pending',NULL,0);
        END;
        CREATE TRIGGER IF NOT EXISTS assumption_source_update AFTER UPDATE ON timeline_items
        WHEN json_extract(NEW.record,'$.kind')='agentMessage' AND length(json_extract(NEW.record,'$.text'))>0 BEGIN
            INSERT INTO assumption_sources VALUES(json_array(NEW.session,NEW.item),NEW.clip,json_extract(NEW.record,'$.text'),'pending',NULL,0)
            ON CONFLICT(id) DO UPDATE SET clip=excluded.clip,text=excluded.text,state='pending',attempts=0
                WHERE text<>excluded.text OR clip<>excluded.clip;
        END;
        CREATE TRIGGER IF NOT EXISTS assumption_source_cleared AFTER UPDATE ON timeline_items
        WHEN json_extract(NEW.record,'$.kind')<>'agentMessage' OR coalesce(length(json_extract(NEW.record,'$.text')),0)=0 BEGIN
            DELETE FROM assumption_sources WHERE id=json_array(NEW.session,NEW.item);
        END;
        CREATE TRIGGER IF NOT EXISTS assumption_source_delete AFTER DELETE ON timeline_items BEGIN
            DELETE FROM assumption_sources WHERE id=json_array(OLD.session,OLD.item);
        END;
        INSERT OR IGNORE INTO assumption_sources
            SELECT json_array(session,item),clip,json_extract(record,'$.text'),'pending',NULL,0 FROM timeline_items
            WHERE json_extract(record,'$.kind')='agentMessage' AND length(json_extract(record,'$.text'))>0;
        UPDATE assumption_sources SET state='pending' WHERE state='running';")?;
    crate::integration_assumptions::install(db)?;
    Ok(())
}

pub fn claim(db: &mut Connection) -> Result<Option<Ticket>> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let sources = tx
        .prepare(
            "SELECT s.id,s.text,s.clip,c.track FROM assumption_sources s
        JOIN timeline_clips c ON c.id=s.clip
        WHERE s.state='pending' AND s.attempts<2 AND json_extract(c.record,'$.repairRunId') IS NULL
        ORDER BY s.rowid",
        )?
        .query_map([], |row| {
            Ok(Source {
                id: row.get(0)?,
                text: row.get(1)?,
                clip: row.get(2)?,
                track: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if sources.is_empty() {
        return Ok(None);
    }
    let request = crate::input_readings::Ticket {
        input: Uuid::now_v7().to_string(),
        revision: 0,
        index: 1,
    };
    for source in &sources {
        tx.execute("UPDATE assumption_sources SET state='running',request=?2,attempts=attempts+1 WHERE id=?1", params![source.id, request.input])?;
    }
    tx.commit()?;
    Ok(Some(Ticket { sources, request }))
}

pub fn finish(db: &mut Connection, ticket: &Ticket, result: Finish) -> Result<bool> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let mut touched = std::collections::BTreeSet::new();
    let mut outcomes = Vec::new();
    let observed_at = crate::clock::Stamp::now();
    for source in &ticket.sources {
        let current: Option<(String, String)> = tx
            .query_row(
                "SELECT text,clip FROM assumption_sources WHERE id=?1 AND request=?2",
                params![source.id, ticket.request.input],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((current, current_clip)) = current else {
            outcomes.push(json!({"source":source.id,"clip":source.clip,"state":"stale"}));
            continue;
        };
        let valid = current_clip == source.clip && current.starts_with(&source.text);
        let outcome = match &result {
            _ if !valid => "stale",
            Finish::Complete(_) => "applied",
            Finish::Interrupted => "interrupted",
            Finish::Failed => "failed",
        };
        outcomes.push(json!({"source":source.id,"clip":source.clip,"state":outcome}));
        if let Finish::Complete(extracted) = &result
            && valid
        {
            let mut ids = Vec::new();
            for item in extracted.iter().filter(|item| item.source == source.id) {
                let id = Uuid::new_v5(
                    &Uuid::NAMESPACE_OID,
                    &serde_json::to_vec(&(
                        "assumption",
                        &source.id,
                        &item.evidence,
                        &item.assumed,
                    ))?,
                )
                .to_string();
                let existing: Option<String> = tx
                    .query_row("SELECT record FROM assumptions WHERE id=?1", [&id], |row| {
                        row.get(0)
                    })
                    .optional()?;
                let existing = existing
                    .map(|value| serde_json::from_str::<Assumption>(&value))
                    .transpose()?;
                let fresh = existing
                    .as_ref()
                    .is_none_or(|value| value.state == "superseded");
                let mut assumption = if fresh {
                    Assumption {
                        id: id.clone(),
                        source: "default-placement".into(),
                        source_id: source.id.clone(),
                        track_id: source.track.clone(),
                        clip_id: source.clip.clone(),
                        question_id: None,
                        question: item.question.clone(),
                        options: item.options.to_vec(),
                        text: item.assumed.clone(),
                        evidence: item.evidence.clone(),
                        revision: existing.as_ref().map_or(1, |value| value.revision + 1),
                        observed_at: Some(observed_at.clone()),
                        state: "open".into(),
                        kept_at: None,
                        changed_at: None,
                        change_input_id: None,
                    }
                } else {
                    existing.unwrap()
                };
                let previous = (
                    assumption.source.clone(),
                    assumption.question_id.clone(),
                    assumption.question.clone(),
                );
                if source.text.contains(&item.question) {
                    let asked = &item.question;
                    let (id, closed) =
                        crate::assumption_questions::record(&tx, source, asked, &item.assumed)?;
                    assumption.question_id = Some(id);
                    assumption.question = asked.clone();
                    assumption.source = if closed {
                        "question-timeout"
                    } else {
                        "default-placement"
                    }
                    .into();
                } else if assumption.question_id.is_some()
                    && !source.text.contains(&assumption.question)
                {
                    assumption.question_id = None;
                    assumption.question = item.question.clone();
                    assumption.source = "default-placement".into();
                }
                let changed = previous
                    != (
                        assumption.source.clone(),
                        assumption.question_id.clone(),
                        assumption.question.clone(),
                    );
                if fresh || changed {
                    if !fresh {
                        assumption.revision += 1;
                    }
                    tx.execute(
                        "INSERT INTO assumptions VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET clip=excluded.clip,state=excluded.state,record=excluded.record",
                        params![id,source.id,source.clip,assumption.state,serde_json::to_string(&assumption)?],
                    )?;
                }
                if fresh {
                    crate::editor_log::assumed(&tx, &assumption)?;
                }
                ids.push(id);
            }
            tx.execute("UPDATE assumptions SET state='superseded',record=json_set(record,'$.state','superseded','$.revision',json_extract(record,'$.revision')+1)
                    WHERE source=?1 AND state='open' AND id NOT IN (SELECT value FROM json_each(?2))
                    AND instr(?3,json_extract(record,'$.evidence'))>0",params![source.id,serde_json::to_string(&ids)?,source.text])?;
            touched.insert(source.clip.clone());
        }
        let (state, retry) = match &result {
            _ if !valid || current != source.text => ("pending", false),
            Finish::Complete(_) => ("complete", false),
            Finish::Interrupted => ("pending", true),
            Finish::Failed => ("pending", false),
        };
        tx.execute("UPDATE assumption_sources SET state=?3,request=NULL,attempts=max(0,attempts-?4) WHERE id=?1 AND request=?2",
            params![source.id,ticket.request.input,state,i64::from(retry)])?;
    }
    for clip in &touched {
        tx.execute(
            "UPDATE timeline_clips SET record=record WHERE id=?1",
            [clip],
        )?;
    }
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'assumptions.extracted',?2)",
        params![
            crate::journal::timestamp(),
            json!({"request":ticket.request.input,"clips":touched,"sources":outcomes}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(true)
}

pub fn list(db: &Connection, clip: Option<&str>) -> Result<Vec<Assumption>> {
    db.prepare(
        "SELECT a.record FROM assumptions a JOIN assumption_sources s ON s.id=a.source AND s.clip=a.clip
        JOIN timeline_clips c ON c.id=a.clip
        WHERE a.state<>'superseded' AND instr(s.text,json_extract(a.record,'$.evidence'))>0
        AND ((?1 IS NULL AND a.state='open') OR a.clip=?1) ORDER BY a.rowid",
    )?
    .query_map([clip], |row| row.get::<_, String>(0))?
    .map(|row| Ok(serde_json::from_str(&row?)?))
    .collect()
}

pub fn respond(db: &mut Connection, command: &Command) -> Result<Value> {
    crate::session_retirement::ensure_input(db, &command.op_id.to_string())?;
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let payload = serde_json::to_string(command)?;
    let previous: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [command.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, previous)) = previous {
        if kind != "assumption.respond" || previous != payload {
            return Err("Operation ID reused.".into());
        }
        let record: String = tx.query_row(
            "SELECT record FROM assumptions WHERE id=?1",
            [command.assumption_id.to_string()],
            |row| row.get(0),
        )?;
        return Ok(serde_json::from_str(&record)?);
    }
    let mut assumption = list(&tx, None)?
        .into_iter()
        .find(|item| item.id == command.assumption_id.to_string())
        .ok_or("The assumption is no longer available.")?;
    if assumption.revision != command.revision || assumption.state != "open" {
        return Err("The assumption changed or was already answered.".into());
    }
    let now = crate::journal::timestamp();
    match &command.answer {
        Answer::Keep => {
            assumption.state = "kept".into();
            assumption.kept_at = Some(now);
        }
        Answer::Change { text } => {
            if text.trim().is_empty() || text.len() > 32 * 1024 {
                return Err("A correction must contain 1–32768 bytes.".into());
            }
            let input = Uuid::now_v7().to_string();
            let text = format!(
                "For the earlier assumption ‘{}’ in ‘{}’, use this correction:\n{}",
                assumption.text, assumption.question, text
            );
            crate::journal::insert_utterance(
                &tx,
                &input,
                &text,
                &crate::decision::Target::Lock(assumption.track_id.clone()),
                None,
                &[],
                None,
                true,
                None,
            )?;
            let (session, turn): (String, Option<String>) = tx.query_row(
                "SELECT json_extract(record,'$.sessionId'),json_extract(record,'$.turnId') FROM timeline_clips WHERE id=?1",
                [&assumption.clip_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
            if let Some(turn) = turn {
                crate::plan_continuation::attach_to_turn(&tx, &input, &session, &turn)?;
            }
            assumption.state = "changed".into();
            assumption.changed_at = Some(now);
            assumption.change_input_id = Some(input);
            crate::editor_log::assumption_changed(
                &tx,
                &format!("assumption-change/{}", command.op_id),
                &assumption.track_id,
                false,
            )?;
        }
    }
    assumption.revision += 1;
    let value = serde_json::to_value(&assumption)?;
    tx.execute(
        "UPDATE assumptions SET state=?2,record=?3 WHERE id=?1",
        params![assumption.id, assumption.state, value.to_string()],
    )?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'assumption.respond',?2,'complete')",
        params![command.op_id.to_string(), payload],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'assumption.answered',?2)",
        params![now, value.to_string()],
    )?;
    tx.execute(
        "UPDATE timeline_clips SET record=record WHERE id=?1",
        [assumption.clip_id],
    )?;
    tx.commit()?;
    Ok(value)
}

/// Find markers independently of their source clips' time spans.
pub fn in_window(
    db: &Connection,
    start: i64,
    end: i64,
) -> Result<std::collections::HashSet<String>> {
    db.prepare("SELECT a.clip FROM assumptions a JOIN assumption_sources s ON s.id=a.source AND s.clip=a.clip
        WHERE json_extract(a.record,'$.observedAt.timelineMs') BETWEEN ?1 AND ?2
        AND a.state<>'superseded' AND instr(s.text,json_extract(a.record,'$.evidence'))>0")?
        .query_map(params![start,end], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>().map_err(Into::into)
}
