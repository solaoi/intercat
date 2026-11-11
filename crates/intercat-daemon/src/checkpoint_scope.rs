//! Checkpoint exclusions are explicit, durable decisions for one track.
use crate::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Issue {
    pub id: Uuid,
    pub track_id: String,
    pub session_id: String,
    pub turn_id: Uuid,
    pub revision: i64,
    pub path: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Exclude {
    pub op_id: Uuid,
    pub issue_id: Uuid,
    pub revision: i64,
}

pub enum Change {
    Block {
        track: String,
        session: String,
        turn: Uuid,
        path: String,
    },
    Exclude(Exclude),
    Clear {
        track: String,
        turn: Uuid,
    },
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS checkpoint_exclusions(track TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,path TEXT NOT NULL,PRIMARY KEY(track,path));
        CREATE TABLE IF NOT EXISTS checkpoint_issues(track TEXT PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,record TEXT NOT NULL);")?;
    Ok(())
}

pub(crate) fn exclusions(db: &Connection, track: &str) -> Result<Vec<String>> {
    Ok(db
        .prepare("SELECT path FROM checkpoint_exclusions WHERE track=?1 ORDER BY path")?
        .query_map([track], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?)
}

pub(crate) fn snapshot(db: &Connection) -> Result<Vec<Issue>> {
    let rows = db
        .prepare("SELECT record FROM checkpoint_issues ORDER BY track")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|raw| Ok(serde_json::from_str(&raw)?))
        .collect()
}

pub(crate) fn change(db: &mut Connection, action: Change) -> Result<bool> {
    let tx = db.transaction()?;
    let changed = match action {
        Change::Block {
            track,
            session,
            turn,
            path,
        } => {
            intercat_git::validate_exclusion(&path)?;
            let old: Option<String> = tx
                .query_row(
                    "SELECT record FROM checkpoint_issues WHERE track=?1",
                    [&track],
                    |row| row.get(0),
                )
                .optional()?;
            let old: Option<Issue> = old.map(|raw| serde_json::from_str(&raw)).transpose()?;
            if old.as_ref().is_some_and(|old| {
                old.turn_id == turn && old.path == path && old.session_id == session
            }) {
                return Ok(false);
            }
            let issue = Issue {
                id: Uuid::new_v5(&turn, path.as_bytes()),
                track_id: track.clone(),
                session_id: session,
                turn_id: turn,
                path,
                revision: old.as_ref().map_or(Ok(1), |old| {
                    old.revision
                        .checked_add(1)
                        .ok_or("Checkpoint issue revision overflow.")
                })?,
            };
            let record = serde_json::to_string(&issue)?;
            tx.execute("INSERT INTO checkpoint_issues VALUES(?1,?2) ON CONFLICT(track) DO UPDATE SET record=excluded.record", params![track,record])?;
            tx.execute(
                "INSERT INTO events(ts,kind,payload) VALUES(?2,'checkpoint.blocked',?1)",
                params![record, crate::journal::timestamp()],
            )?;
            true
        }
        Change::Exclude(command) => {
            if command.op_id.get_version_num() != 7 {
                return Err("A checkpoint decision needs a new operation ID.".into());
            }
            let old: Option<(String, String)> = tx
                .query_row(
                    "SELECT kind,payload FROM operations WHERE id=?1",
                    [command.op_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((kind, before)) = old {
                if kind != "checkpoint.exclude"
                    || serde_json::from_str::<serde_json::Value>(&before)?["request"]
                        != serde_json::to_value(&command)?
                {
                    return Err("This operation belongs to a different checkpoint decision.".into());
                }
                return Ok(false);
            }
            let raw: String = tx
                .query_row(
                    "SELECT record FROM checkpoint_issues WHERE json_extract(record,'$.id')=?1",
                    [command.issue_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or("This checkpoint issue was already resolved. Review the current card.")?;
            let issue: Issue = serde_json::from_str(&raw)?;
            if issue.revision != command.revision {
                return Err("The checkpoint issue changed. Review it again.".into());
            }
            let worker: String = tx.query_row(
                "SELECT record FROM workers WHERE track_id=?1",
                [&issue.track_id],
                |row| row.get(0),
            )?;
            let worker: crate::runtime::WorkerState = serde_json::from_str(&worker)?;
            if worker.session_id.as_deref() != Some(issue.session_id.as_str())
                || !worker.checkpoint_pending
                || worker.active_turn.is_some()
            {
                return Err("The worker has moved on. Review its checkpoint again.".into());
            }
            intercat_git::validate_exclusion(&issue.path)?;
            tx.execute(
                "INSERT OR IGNORE INTO checkpoint_exclusions VALUES(?1,?2)",
                params![issue.track_id, issue.path],
            )?;
            tx.execute(
                "DELETE FROM checkpoint_issues WHERE track=?1",
                [&issue.track_id],
            )?;
            let payload =
                serde_json::json!({"request":command,"issue":issue,"retryId":Uuid::now_v7()});
            tx.execute(
                "INSERT INTO operations VALUES(?1,'checkpoint.exclude',?2,'prepared')",
                params![command.op_id.to_string(), payload.to_string()],
            )?;
            tx.execute("INSERT INTO events(ts,track_id,kind,payload) VALUES(?3,?1,'checkpoint.excluded',?2)", params![issue.track_id,serde_json::to_string(&issue)?,crate::journal::timestamp()])?;
            true
        }
        Change::Clear { track, turn } => tx.execute(
            "DELETE FROM checkpoint_issues WHERE track=?1 AND json_extract(record,'$.turnId')=?2",
            params![track, turn.to_string()],
        )? != 0,
    };
    tx.commit()?;
    Ok(changed)
}
