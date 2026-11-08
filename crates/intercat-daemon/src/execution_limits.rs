//! Durable application-wide worker and Planner/Reviewer turn limits.
use crate::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Setting {
    pub revision: u64,
    pub parallel_workers: u16,
    pub planner_reviewer_turns: u16,
}
impl Default for Setting {
    fn default() -> Self {
        Self {
            revision: 0,
            parallel_workers: 4,
            planner_reviewer_turns: 2,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Change {
    pub op_id: Uuid,
    pub revision: u64,
    pub parallel_workers: u16,
    pub planner_reviewer_turns: u16,
}
pub(crate) fn read(db: &Connection) -> Result<Setting> {
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key='execution_limits'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?
        .unwrap_or_default())
}
pub(crate) fn change(db: &mut Connection, request: &Change) -> Result<Setting> {
    if request.op_id.get_version_num() != 7
        || request.parallel_workers == 0
        || request.planner_reviewer_turns == 0
    {
        return Err("Choose positive whole numbers for both turn limits.".into());
    }
    let tx = db.transaction()?;
    let payload = serde_json::to_string(request)?;
    if let Some((kind, old)) = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if kind != "execution-limits.change" || old != payload {
            return Err("Operation ID reused.".into());
        }
        return read(&tx);
    }
    let current = read(&tx)?;
    if current.revision != request.revision {
        return Err("Execution limits changed in another window. Reload before saving.".into());
    }
    let next = Setting {
        revision: current
            .revision
            .checked_add(1)
            .ok_or("Worker limit revision overflow.")?,
        parallel_workers: request.parallel_workers,
        planner_reviewer_turns: request.planner_reviewer_turns,
    };
    let value = serde_json::to_string(&next)?;
    tx.execute("INSERT INTO settings VALUES('execution_limits',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&value])?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'execution-limits.change',?2,'complete')",
        params![request.op_id.to_string(), payload],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'execution-limits.changed',?2)",
        params![crate::journal::timestamp(), value],
    )?;
    tx.commit()?;
    Ok(next)
}
