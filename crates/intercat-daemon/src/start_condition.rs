//! A new track waits for the turn named by its original request context.
use crate::{
    Result,
    decision::{Decision, Kind},
    decision_review::{Review, Stage},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Turn {
    pub track_id: String,
    pub label: String,
    pub session_id: String,
    pub turn_id: Uuid,
}

#[derive(Default, Deserialize, Serialize)]
struct Context {
    candidates: Vec<Turn>,
    source: Option<Turn>,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS input_start_conditions(
        input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE,
        record TEXT NOT NULL);",
    )?;
    Ok(())
}

pub(crate) fn capture(db: &Connection, input: &str) -> Result<()> {
    let mut query = db.prepare(
        "SELECT w.track_id,t.label,
        json_extract(w.record,'$.sessionId'),json_extract(w.record,'$.activeTurn')
        FROM workers w JOIN tracks t ON t.id=w.track_id
        WHERE json_type(w.record,'$.sessionId')='text'
        AND json_type(w.record,'$.activeTurn')='text' ORDER BY t.rowid",
    )?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    let mut candidates = Vec::new();
    for row in rows {
        let (track_id, label, session_id, turn_id) = row?;
        candidates.push(Turn {
            track_id,
            label,
            session_id,
            turn_id: turn_id.parse()?,
        });
    }
    save(
        db,
        input,
        &Context {
            candidates,
            source: None,
        },
    )
}

fn read(db: &Connection, input: &str) -> Result<Context> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM input_start_conditions WHERE input=?1",
            [input],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?
        .unwrap_or_default())
}

fn save(db: &Connection, input: &str, context: &Context) -> Result<()> {
    db.execute(
        "INSERT INTO input_start_conditions VALUES(?1,?2)
        ON CONFLICT(input) DO UPDATE SET record=excluded.record",
        params![input, serde_json::to_string(context)?],
    )?;
    Ok(())
}

fn waits(decision: &Decision) -> bool {
    decision.kind == Kind::Fork && decision.boundary.as_deref() == Some("after-turn")
}

pub(crate) fn prepare(db: &Connection, input: &str, decision: &mut Decision) -> Result<()> {
    if !waits(decision) || decision.timed_out {
        return Ok(());
    }
    let mut context = read(db, input)?;
    if context.source.is_none() && context.candidates.len() == 1 {
        context.source = context.candidates.first().cloned();
        save(db, input, &context)?;
    }
    if context.source.is_none() && crate::decision_review::read(decision)?.is_none() {
        crate::decision_review::write(
            decision,
            Review {
                stage: Stage::StartTurn,
                turn: None,
            },
        )?;
    }
    Ok(())
}

pub(crate) fn choose(db: &Connection, input: &str, source: &Turn) -> Result<()> {
    let mut context = read(db, input)?;
    if !context.candidates.contains(source) {
        return Err("Choose a turn recorded with this request.".into());
    }
    context.source = Some(source.clone());
    save(db, input, &context)
}

fn completed(db: &Connection, source: &Turn) -> Result<bool> {
    Ok(db
        .query_row(
            "SELECT coalesce(json_extract(record,'$.terminal'),0)
        FROM timeline_turns WHERE session=?1 AND turn=?2 AND json_extract(record,'$.track')=?3",
            params![
                source.session_id,
                source.turn_id.to_string(),
                source.track_id
            ],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(false))
}

pub(crate) fn projection(
    db: &Connection,
    input: &str,
    decision: &Decision,
) -> Result<Option<Value>> {
    if !waits(decision) {
        return Ok(None);
    }
    let context = read(db, input)?;
    let ready = context
        .source
        .as_ref()
        .map(|source| completed(db, source))
        .transpose()?
        .unwrap_or(false);
    Ok(Some(
        json!({"source":context.source,"candidates":context.candidates,"ready":ready}),
    ))
}

pub(crate) fn validate_dispatch(db: &Connection, input: &str) -> Result<()> {
    let raw: Option<String> = db
        .query_row(
            "SELECT decision FROM utterances WHERE id=?1",
            [input],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let raw = raw.ok_or(crate::journal::StartSuperseded)?;
    let decision = serde_json::from_str(&raw)?;
    if projection(db, input, &decision)?.is_some_and(|condition| condition["ready"] != true) {
        return Err(crate::journal::StartSuperseded.into());
    }
    Ok(())
}
