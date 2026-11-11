//! A completed conversation turn and an empty conversation's files are distinct.
use crate::{Result, checkpoint::Checkpoint, preparation::Workspace};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Selection {
    Turn { turn_id: Uuid },
    Initial { two_ways_id: Uuid },
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Point {
    Turn { checkpoint: Checkpoint },
    Initial { snapshot: Initial },
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Initial {
    pub operation: Uuid,
    pub sha: String,
    pub reference: String,
}
impl Point {
    pub fn sha(&self) -> &str {
        match self {
            Self::Turn { checkpoint } => &checkpoint.sha,
            Self::Initial { snapshot } => &snapshot.sha,
        }
    }
    pub fn reference(&self) -> &str {
        match self {
            Self::Turn { checkpoint } => &checkpoint.reference,
            Self::Initial { snapshot } => &snapshot.reference,
        }
    }
    pub fn checkpoint(&self) -> Option<&Checkpoint> {
        match self {
            Self::Turn { checkpoint } => Some(checkpoint),
            Self::Initial { .. } => None,
        }
    }
    pub fn matches(&self, selection: &Selection) -> bool {
        match (self, selection) {
            (Self::Turn { checkpoint }, Selection::Turn { turn_id }) => {
                checkpoint.turn_id == *turn_id
            }
            (Self::Initial { snapshot }, Selection::Initial { two_ways_id }) => {
                snapshot.operation == initial_operation(*two_ways_id)
            }
            _ => false,
        }
    }
}
pub(crate) fn initial_operation(job: Uuid) -> Uuid {
    Uuid::new_v5(&job, b"initial-files")
}

pub(crate) fn resolve(
    db: &Connection,
    request: &crate::branch::Request,
) -> Result<(String, Point, Workspace)> {
    match request.point {
        Selection::Turn { turn_id } => {
            let source = crate::branch::source(db, &request.parent_track_id, turn_id)?;
            Ok((
                source.session_id,
                Point::Turn {
                    checkpoint: source.checkpoint,
                },
                source.workspace,
            ))
        }
        Selection::Initial { two_ways_id } => {
            let job = crate::two_ways_store::read(db, two_ways_id)?
                .ok_or("Missing initial paired source.")?;
            let Some(Point::Initial { snapshot }) = job.point else {
                return Err("The initial files have not been saved.".into());
            };
            if job.branch_id != request.op_id
                || job.source_track != request.parent_track_id
                || !matches!(
                    job.phase,
                    crate::two_ways::Phase::Forking
                        | crate::two_ways::Phase::AwaitingReceipt
                        | crate::two_ways::Phase::Queued
                )
                || request.instructions != crate::branch::InstructionMode::Checkpoint
            {
                return Err("This initial source belongs to another paired branch.".into());
            }
            verify_initial(
                db,
                &job.source_track,
                &job.source_session,
                two_ways_id,
                &snapshot,
            )?;
            let raw: String = db.query_row(
                "SELECT record FROM workspaces WHERE track_id=?1",
                [&job.source_track],
                |row| row.get(0),
            )?;
            let workspace: Workspace = serde_json::from_str(&raw)?;
            if workspace.state != "ready"
                || snapshot.reference != format!("{}-initial/{two_ways_id}", workspace.reference)
            {
                return Err("The initial source workspace changed.".into());
            }
            Ok((job.source_session, Point::Initial { snapshot }, workspace))
        }
    }
}
pub(crate) fn verify_initial(
    db: &Connection,
    track: &str,
    session: &str,
    job: Uuid,
    snapshot: &Initial,
) -> Result<()> {
    let raw: String = db.query_row("SELECT payload FROM operations WHERE id=?1 AND kind='two-ways.initial' AND state='complete'", [initial_operation(job).to_string()], |row| row.get(0))?;
    let intent: crate::two_ways_initial::Intent = serde_json::from_str(&raw)?;
    if intent.job != job
        || intent.track_id != track
        || intent.session_id != session
        || &intent.snapshot != snapshot
        || snapshot.operation != initial_operation(job)
    {
        return Err("The saved initial source changed.".into());
    }
    intercat_git::ObjectId::from_hex(snapshot.sha.as_bytes())?;
    let count: i64 = db.query_row("SELECT count(*) FROM checkpoint_lineage WHERE operation=?1 AND track=?2 AND sha=?3 AND complete=1", params![snapshot.operation.to_string(), track, snapshot.sha], |row| row.get(0))?;
    if count != 1 {
        return Err("The initial file ancestry is incomplete.".into());
    }
    Ok(())
}

/// MSP cursors are opaque. The session metadata establishes an empty boundary.
pub(crate) fn verify_empty(value: &serde_json::Value, expected: &str) -> Result<()> {
    let session = &value["session"];
    if session["sessionId"] != expected
        || session["turnCount"].as_u64() != Some(0)
        || session.get("activeTurnId") != Some(&serde_json::Value::Null)
        || !value["pendingRequests"]
            .as_array()
            .is_some_and(|pending| pending.is_empty())
    {
        return Err("The initial conversation is no longer empty and idle.".into());
    }
    Ok(())
}
