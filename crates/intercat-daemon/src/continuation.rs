//! A new conversation continues one writable path. Closed source metadata and
//! checkpoints remain readable; writer closure survives protected-history cleanup.
use crate::{Result, checkpoint::Checkpoint, fresh::Job};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Continuation {
    pub operation: Uuid,
    pub source: String,
    pub successor: String,
    pub source_session: String,
    pub session_id: String,
    pub board_session: String,
    pub folder_id: String,
    pub checkpoint: Checkpoint,
    pub origin: crate::branch_points::Origin,
    pub first_input: String,
    pub start_command: Uuid,
    pub opened: bool,
    pub ready: bool,
}
impl Continuation {
    pub(crate) fn status(&self) -> serde_json::Value {
        serde_json::json!({"operation":self.operation,"source":self.source,"successor":self.successor,"sourceSession":self.source_session,"sessionId":self.session_id,"boardSession":self.board_session,"origin":self.origin,"opened":self.opened,"ready":self.ready})
    }
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE continuations(source TEXT PRIMARY KEY REFERENCES tracks(id),successor TEXT NOT NULL UNIQUE REFERENCES tracks(id),record TEXT NOT NULL);")?;
    Ok(())
}
pub(crate) fn closed(db: &Connection, track: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM continuations WHERE source=?1)",
        [track],
        |row| row.get(0),
    )?)
}
pub(crate) fn ensure_writer(db: &Connection, track: &str) -> Result<()> {
    if closed(db, track)? {
        return Err("This track continued in a new conversation. Its original writer is permanently closed.".into());
    }
    Ok(())
}
pub(crate) fn current(db: &Connection, track: &str) -> Result<String> {
    let mut next = track.to_owned();
    let mut seen = std::collections::BTreeSet::new();
    loop {
        if !seen.insert(next.clone()) {
            return Err("The continuation path contains a cycle.".into());
        }
        let successor: Option<String> = db
            .query_row(
                "SELECT successor FROM continuations WHERE source=?1",
                [&next],
                |row| row.get(0),
            )
            .optional()?;
        match successor {
            Some(successor) => next = successor,
            None => return Ok(next),
        }
    }
}
pub(crate) fn list(db: &Connection) -> Result<Vec<Continuation>> {
    let mut query = db.prepare("SELECT record FROM continuations ORDER BY rowid")?;
    query
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|raw| Ok(serde_json::from_str(&raw?)?))
        .collect()
}
pub(crate) fn start(db: &Connection, track: &str) -> Result<Option<Continuation>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM continuations WHERE successor=?1",
            [track],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
}
fn save(db: &Connection, value: &Continuation) -> Result<()> {
    db.execute(
        "UPDATE continuations SET record=?2 WHERE source=?1",
        params![value.source, serde_json::to_string(value)?],
    )?;
    Ok(())
}
pub(crate) fn opened(db: &Connection, track: &str, session: &str) -> Result<()> {
    ensure_writer(db, track)?;
    let mut value = start(db, track)?.ok_or("Missing fresh session reservation.")?;
    if value.session_id != session {
        return Err("The fresh worker opened another session.".into());
    }
    value.opened = true;
    save(db, &value)
}
pub(crate) fn allowed(db: &Connection, track: &str, input: &str) -> Result<bool> {
    if closed(db, track)? {
        return Ok(false);
    }
    Ok(start(db, track)?.is_none_or(|value| value.ready || value.first_input == input))
}
pub(crate) fn observe(db: &Connection, worker: &crate::runtime::WorkerState) -> Result<()> {
    let Some(mut value) = start(db, &worker.track_id)? else {
        return Ok(());
    };
    if value.ready
        || worker.session_id.as_deref() != Some(&value.session_id)
        || worker.active_turn.is_some()
        || worker.checkpoint_pending
    {
        return Ok(());
    }
    let Some(checkpoint) = &worker.checkpoint else {
        return Ok(());
    };
    let delivered:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM timeline_marks WHERE session=?1 AND input=?2) AND EXISTS(SELECT 1 FROM utterances u JOIN attachment_turns a ON a.command=u.dispatch_command AND a.session=?1 AND a.turn=?3 WHERE u.id=?2 AND u.state='landed') AND EXISTS(SELECT 1 FROM timeline_turns WHERE session=?1 AND turn=?3 AND json_extract(record,'$.track')=?4 AND json_extract(record,'$.terminal')=1)",params![value.session_id,value.first_input,checkpoint.turn_id.to_string(),worker.track_id],|row|row.get(0))?;
    if delivered {
        value.ready = true;
        save(db, &value)?;
    }
    Ok(())
}
pub(crate) fn transfer(db: &Connection, job: &Job) -> Result<crate::preparation::Workspace> {
    ensure_writer(db, &job.request.track_id)?;
    let point = &job
        .boundary
        .as_ref()
        .ok_or("Missing fresh boundary.")?
        .checkpoint;
    let summary = job
        .summary
        .as_ref()
        .ok_or("Missing completed handoff summary.")?;
    let title: String = db.query_row(
        "SELECT title FROM tracks WHERE id=?1",
        [&job.request.track_id],
        |row| row.get(0),
    )?;
    let label: String = db.query_row(
        "SELECT label FROM tracks WHERE id=?1",
        [&job.request.track_id],
        |row| row.get(0),
    )?;
    let family = label.trim_end_matches(|c: char| c.is_ascii_digit());
    if family.is_empty() || !family.bytes().all(|byte| byte.is_ascii_uppercase()) {
        return Err("Invalid source track label.".into());
    }
    let mut number = 2;
    let label = loop {
        let label = format!("{family}{number}");
        if !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM tracks WHERE label=?1)",
            [&label],
            |row| row.get::<_, bool>(0),
        )? {
            break label;
        }
        number += 1;
    };
    db.execute(
        "INSERT INTO tracks VALUES(?1,?2,?3,'waiting')",
        params![job.successor_track, label, title],
    )?;
    crate::folders::bind(db, &job.successor_track, &job.folder.id)?;
    let mut workspace = job.workspace.clone();
    workspace.track_id = job.successor_track.clone();
    workspace.reference = format!("refs/intercat/{}/tracks/{label}", job.slug);
    workspace.state = "fresh-publishing".into();
    db.execute(
        "INSERT INTO workspaces VALUES(?1,?2)",
        params![job.successor_track, serde_json::to_string(&workspace)?],
    )?;
    let source = crate::lineage_store::point(db, &job.request.track_id, &point.sha)?;
    let mut basis = source.basis();
    basis.parents = std::collections::BTreeMap::from([(source.track.clone(), source.inherited())]);
    crate::lineage::include(&mut basis.inherits, &source.track, &source.inherited())?;
    crate::lineage_store::initialize(db, &job.successor_track, &basis)?;
    let first_input = Uuid::new_v5(&job.request.op_id, b"fresh-handoff").to_string();
    let record = Continuation {
        operation: job.request.op_id,
        source: job.request.track_id.clone(),
        successor: job.successor_track.clone(),
        source_session: job.request.session_id.clone(),
        session_id: job.successor_session.clone(),
        board_session: job.session.clone(),
        folder_id: job.folder.id.clone(),
        checkpoint: point.clone(),
        origin: crate::branch_points::origin(
            db,
            &job.request.track_id,
            point.turn_id,
            Some(&job.request.session_id),
        )?,
        first_input: first_input.clone(),
        start_command: Uuid::now_v7(),
        opened: false,
        ready: false,
    };
    db.execute(
        "INSERT INTO continuations VALUES(?1,?2,?3)",
        params![
            record.source,
            record.successor,
            serde_json::to_string(&record)?
        ],
    )?;
    crate::worker_hold::inherit(db, &job.request.track_id, &job.successor_track)?;
    crate::fresh_words::transfer(db, &job.request.track_id, &job.successor_track)?;
    let text = format!(
        "Continue from these existing files in a new conversation. The previous conversation has ended. Preserve the latest user corrections and constraints in the handoff below. Completed work is evidence, not a request to repeat it. Inspect current files and report what remains unverified before continuing. No prior conversation history was imported.\n\nHandoff summary:\n{}",
        serde_json::to_string_pretty(summary)?
    );
    let decision = crate::decision::Decision {
        kind: crate::decision::Kind::Extend,
        track_id: Some(job.successor_track.clone()),
        boundary: Some("now".into()),
        command: None,
        confidence: 1.,
        inference_ms: 0.,
        timed_out: false,
        scores: serde_json::json!({}),
    };
    db.execute("INSERT INTO utterances(id,text,created_at,target,state,revision,decision,provisional) VALUES(?1,?2,?3,?4,'placed',1,?5,?6)",params![first_input,text,crate::journal::timestamp(),serde_json::to_string(&crate::decision::Target::Lock(job.successor_track.clone()))?,serde_json::to_string(&decision)?,job.successor_track])?;
    crate::worker_hold::hold_blocked(db, &job.successor_track, &first_input)?;
    db.execute("UPDATE workers SET record=json_set(record,'$.phase','done','$.identity',NULL,'$.detail','Continued in a new conversation') WHERE track_id=?1",[&job.request.track_id])?;
    db.execute(
        "UPDATE tracks SET status='done' WHERE id=?1",
        [&job.request.track_id],
    )?;
    Ok(workspace)
}
