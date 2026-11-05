//! A correction interrupts the turn captured at classification, never a later
//! turn. Its text becomes dispatchable only after that turn's saved checkpoint.
use crate::{Result, decision::Target, runtime::WorkerState};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Stopping,
    Ready,
    Sent,
    Done,
    Held,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Redirect {
    pub input_id: String,
    pub revision: i64,
    pub track_id: String,
    pub session_id: String,
    pub turn_id: Uuid,
    pub phase: Phase,
}

pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE redirects(input TEXT PRIMARY KEY REFERENCES utterances(id),record TEXT NOT NULL);")?;
    Ok(())
}
fn read(db: &Connection, input: &str) -> Result<Option<Redirect>> {
    db.query_row(
        "SELECT record FROM redirects WHERE input=?1",
        [input],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|s| serde_json::from_str(&s).map_err(Into::into))
    .transpose()
}
fn save(db: &Connection, redirect: &Redirect) -> Result<()> {
    db.execute("INSERT INTO redirects VALUES(?1,?2) ON CONFLICT(input) DO UPDATE SET record=excluded.record", params![redirect.input_id, serde_json::to_string(redirect)?])?;
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'redirect.state',?2)",
        params![
            crate::journal::timestamp(),
            serde_json::to_string(redirect)?
        ],
    )?;
    Ok(())
}
pub fn snapshot(db: &Connection) -> Result<Vec<Redirect>> {
    db.prepare("SELECT record FROM redirects ORDER BY rowid")?
        .query_map([], |r| r.get::<_, String>(0))?
        .map(|s| Ok(serde_json::from_str(&s?)?))
        .collect()
}
pub fn prepare(db: &Connection, input: &str, revision: i64, track: &str) -> Result<bool> {
    let state = db
        .query_row(
            "SELECT record FROM workers WHERE track_id=?1",
            [track],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let Some(state) = state else {
        return Ok(false);
    };
    let state: WorkerState = serde_json::from_str(&state)?;
    let (Some(session_id), Some(turn_id)) = (state.session_id, state.active_turn) else {
        return Ok(false);
    };
    if !["working", "approval", "question"].contains(&state.phase.as_str())
        || state.checkpoint_pending
    {
        return Ok(false);
    }
    save(
        db,
        &Redirect {
            input_id: input.into(),
            revision,
            track_id: track.into(),
            session_id,
            turn_id,
            phase: Phase::Stopping,
        },
    )?;
    Ok(true)
}
pub fn forget(db: &Connection, input: &str) -> Result<()> {
    db.execute("DELETE FROM redirects WHERE input=?1", [input])?;
    Ok(())
}
fn hold(db: &Connection, redirect: &mut Redirect, reason: &str) -> Result<()> {
    redirect.phase = Phase::Held;
    db.execute("UPDATE utterances SET state='held',reason=?2 WHERE id=?1 AND state IN ('redirecting','placed','starting')", params![redirect.input_id, reason])?;
    save(db, redirect)
}
fn checkpoint_ready(db: &Connection, redirect: &Redirect) -> Result<bool> {
    let id = Uuid::new_v5(&redirect.turn_id, b"checkpoint");
    let operation: Option<String> = db.query_row("SELECT payload FROM operations WHERE id=?1 AND kind='checkpoint.save' AND state='complete'", [id.to_string()], |r| r.get(0)).optional()?;
    let Some(operation) = operation else {
        return Ok(false);
    };
    let payload: Value = serde_json::from_str(&operation)?;
    Ok(payload["trackId"] == redirect.track_id
        && payload["checkpoint"]["turnId"] == redirect.turn_id.to_string())
}
pub fn worker_changed(db: &Connection, worker: &WorkerState) -> Result<()> {
    for mut redirect in snapshot(db)?.into_iter().filter(|r| {
        r.track_id == worker.track_id && matches!(r.phase, Phase::Stopping | Phase::Ready)
    }) {
        if worker.session_id.as_deref() != Some(&redirect.session_id)
            || ["held", "stopped", "failed", "lost", "needs-recovery"]
                .contains(&worker.phase.as_str())
        {
            hold(
                db,
                &mut redirect,
                "Correction held. Review the worker before sending it.",
            )?;
        } else if redirect.phase == Phase::Stopping
            && worker.active_turn.is_none()
            && !worker.checkpoint_pending
        {
            if worker
                .checkpoint
                .as_ref()
                .is_some_and(|c| c.turn_id == redirect.turn_id)
                && checkpoint_ready(db, &redirect)?
            {
                redirect.phase = Phase::Ready;
                db.execute("UPDATE utterances SET state='placed',reason=NULL WHERE id=?1 AND revision=?2 AND state='redirecting'", params![redirect.input_id, redirect.revision])?;
                save(db, &redirect)?;
            } else {
                hold(
                    db,
                    &mut redirect,
                    "The interrupted turn has no verified checkpoint. Review before sending the correction.",
                )?;
            }
        } else if redirect.phase == Phase::Stopping
            && worker
                .active_turn
                .is_some_and(|turn| turn != redirect.turn_id)
        {
            hold(
                db,
                &mut redirect,
                "The worker moved to another turn. The correction did not stop that turn.",
            )?;
        }
    }
    Ok(())
}
pub fn reserve(
    db: &Connection,
    input: &str,
    revision: i64,
    track: &str,
    session: &Value,
) -> Result<()> {
    let kind: Option<String> = db.query_row(
        "SELECT json_extract(decision,'$.kind') FROM utterances WHERE id=?1",
        [input],
        |r| r.get(0),
    )?;
    if kind.as_deref() != Some("trim") {
        return Ok(());
    }
    let mut redirect = read(db, input)?.ok_or("Missing correction target.")?;
    if redirect.phase != Phase::Ready
        || redirect.revision != revision
        || redirect.track_id != track
        || session.as_str() != Some(&redirect.session_id)
    {
        return Err("The correction has not reached its saved turn boundary.".into());
    }
    redirect.phase = Phase::Sent;
    save(db, &redirect)
}
pub fn input_state(db: &Connection, input: &str, state: &str) -> Result<()> {
    if let Some(mut redirect) = read(db, input)? {
        let phase = match state {
            "landed" => Phase::Done,
            "held" => Phase::Held,
            _ => return Ok(()),
        };
        if redirect.phase != phase {
            redirect.phase = phase;
            save(db, &redirect)?;
        }
    }
    Ok(())
}
pub fn retry(db: &Connection, input: &str, target: &Target) -> Result<bool> {
    let Some(mut redirect) = read(db, input)?.filter(|r| r.phase == Phase::Held) else {
        return Ok(false);
    };
    if matches!(target, Target::None) {
        return Err("Choose the correction's existing track to resume it.".into());
    }
    if !matches!(target, Target::Lock(track) if *track == redirect.track_id) {
        return Ok(false);
    }
    let session: Option<String> = db
        .query_row(
            "SELECT json_extract(record,'$.sessionId') FROM workers WHERE track_id=?1",
            [&redirect.track_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if session.as_deref() != Some(&redirect.session_id) {
        return Ok(false);
    }
    if !checkpoint_ready(db, &redirect)? {
        return Err("Finish checkpoint recovery before resuming this correction.".into());
    }
    let current: i64 = db.query_row(
        "SELECT revision FROM utterances WHERE id=?1",
        [input],
        |r| r.get(0),
    )?;
    redirect.revision = current + 1;
    redirect.phase = Phase::Ready;
    db.execute(
        "UPDATE utterances SET state='placed',revision=?2,target=?3,reason=NULL WHERE id=?1",
        params![
            input,
            redirect.revision,
            serde_json::to_string(&Target::Lock(redirect.track_id.clone()))?
        ],
    )?;
    save(db, &redirect)?;
    Ok(true)
}
pub fn interrupt(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    for mut redirect in snapshot(&tx)?
        .into_iter()
        .filter(|r| matches!(r.phase, Phase::Stopping | Phase::Ready))
    {
        hold(
            &tx,
            &mut redirect,
            "Correction held after restart. Nothing was sent automatically.",
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn stopping(snapshot: &Value, track: &str) -> Vec<Redirect> {
    snapshot["redirects"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|value| serde_json::from_value::<Redirect>(value.clone()).ok())
        .filter(|r| r.track_id == track && r.phase == Phase::Stopping)
        .collect()
}
