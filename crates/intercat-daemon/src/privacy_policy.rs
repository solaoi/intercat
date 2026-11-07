//! Persisted privacy preference; a Full setting never silently becomes Basic.
use crate::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Basic,
    Full,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct State {
    pub revision: u64,
    pub mode: Mode,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Change {
    pub op_id: Uuid,
    pub revision: u64,
    pub mode: Mode,
}

pub(crate) fn state(db: &Connection) -> Result<State> {
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key='privacy'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?
        .unwrap_or_default())
}

pub(crate) fn change(db: &mut Connection, request: &Change, local_ready: bool) -> Result<State> {
    if request.op_id.get_version_num() != 7 {
        return Err("Invalid privacy setting operation.".into());
    }
    let tx = db.transaction()?;
    let payload = serde_json::to_string(request)?;
    let previous: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, saved)) = previous {
        if kind != "privacy.change" || saved != payload {
            return Err("Privacy operation ID reused.".into());
        }
        return state(&tx);
    }
    let current = state(&tx)?;
    if current.revision != request.revision {
        return Err("The privacy setting changed. Refresh it before trying again.".into());
    }
    if request.mode == Mode::Full && !local_ready {
        return Err("Start the local Editor before choosing Full privacy.".into());
    }
    let next = State {
        revision: current
            .revision
            .checked_add(1)
            .ok_or("Privacy revision overflow.")?,
        mode: request.mode,
    };
    let raw = serde_json::to_string(&next)?;
    tx.execute("INSERT INTO settings VALUES('privacy',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&raw])?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'privacy.changed',?2)",
        params![crate::journal::timestamp(), raw],
    )?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'privacy.change',?2,'complete')",
        params![request.op_id.to_string(), payload],
    )?;
    tx.commit()?;
    Ok(next)
}
