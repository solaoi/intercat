//! Shared validation and persistence for starting a held or go-ahead request.
use crate::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

#[derive(Clone, Copy)]
pub(crate) enum Permission {
    Button,
    GoAhead,
}

pub(crate) fn unavailable(
    db: &Connection,
    input: &str,
    revision: i64,
    permission: Permission,
) -> Result<Option<&'static str>> {
    if crate::dispatch_control::paused(db)? {
        return Ok(Some("Turns are stopped. Resume before starting a request."));
    }
    if crate::edit_hold::active(db, input)? {
        return Ok(Some("Finish editing this request first."));
    }
    let row: Option<(String, i64, Option<String>)> = db
        .query_row(
            "SELECT state,revision,decision FROM utterances WHERE id=?1",
            [input],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((state, current, Some(decision))) = row else {
        return Ok(Some("This request needs a destination first."));
    };
    if crate::confirmation::pending(db, input)?.is_some() {
        return Ok(Some("Confirm this interpretation before starting it."));
    }
    let decision: Value = serde_json::from_str(&decision)?;
    let waiting = state == "placed" && decision["boundary"] == "when-told";
    let permitted = match permission {
        Permission::Button => state == "held" || waiting,
        Permission::GoAhead => waiting,
    };
    if !permitted
        || current != revision
        || decision["scores"].get("review").is_some()
        || !matches!(
            decision["kind"].as_str(),
            Some("fork" | "extend" | "schedule")
        )
        || !decision["trackId"].is_string()
    {
        return Ok(Some("This request changed or cannot be started here."));
    }
    Ok(None)
}

pub(crate) fn start(
    db: &Connection,
    operation: &str,
    input: &str,
    revision: i64,
    permission: Permission,
) -> Result<()> {
    if let Some(reason) = unavailable(db, input, revision, permission)? {
        return Err(reason.into());
    }
    let payload = json!({"utteranceId":input,"revision":revision}).to_string();
    db.execute("UPDATE utterances SET state='placed',revision=revision+1,reason=NULL,decision=json_set(decision,'$.boundary','now') WHERE id=?1",[input])?;
    db.execute(
        "INSERT INTO operations VALUES(?1,'utterance.start',?2,'complete')",
        params![operation, payload],
    )?;
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.started',?2)",
        params![crate::journal::timestamp(), payload],
    )?;
    Ok(())
}
