//! Approval policy is read and recorded on the single journal writer.
use crate::{
    Result,
    policy::{self, Stops, Verdict},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::Path;

pub fn stops(db: &Connection) -> Result<Stops> {
    let value: Option<String> = db
        .query_row("SELECT value FROM settings WHERE key='stops'", [], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(value
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default())
}
pub fn set_stops(db: &mut Connection, id: &str, value: Stops) -> Result<Stops> {
    let tx = db.transaction()?;
    let payload = serde_json::to_string(&value)?;
    let previous: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, old)) = previous {
        if kind != "policy.stops" || old != payload {
            return Err("Operation ID reused.".into());
        }
        return stops(&tx);
    }
    tx.execute(
        "INSERT INTO operations VALUES(?1,'policy.stops',?2,'complete')",
        params![id, payload],
    )?;
    tx.execute("INSERT INTO settings VALUES('stops',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[&payload])?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'policy.stops',?2)",
        params![
            crate::journal::timestamp(),
            json!({"opId":id,"stops":value}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(value)
}
pub fn evaluate(
    db: &mut Connection,
    track: &str,
    workspace: &Path,
    hosts: &[String],
    request: &Value,
    deletion: Option<&crate::deletion::Snapshot>,
) -> Result<Verdict> {
    let tx = db.transaction()?;
    let stops = stops(&tx)?;
    let verdict = policy::evaluate(
        request,
        &policy::Context {
            stops,
            workspace,
            recoverable_deletion: deletion.is_some_and(|snapshot| snapshot.complete),
            remote_hosts: hosts,
        },
    );
    let session = request["sessionId"]
        .as_str()
        .ok_or("Approval session is missing.")?;
    let approval = request["approvalId"]
        .as_str()
        .ok_or("Approval ID is missing.")?;
    let requirement = request
        .get("currentRequirementId")
        .ok_or("Approval requirement is missing.")?
        .to_string();
    let record = json!({"trackId":track,"sessionId":session,"approvalId":approval,"requirement":request["currentRequirementId"],"itemId":request["itemId"],"turnId":request["turnId"],"at":crate::journal::timestamp(),"stops":stops,"verdict":verdict,"deletionSnapshotId":deletion.map(|s|s.id)});
    crate::effects::register(&tx, &record)?;
    tx.execute("INSERT INTO approval_policies VALUES(?1,?2,?3,?4) ON CONFLICT(session,approval,requirement) DO UPDATE SET record=excluded.record",params![session,approval,requirement,record.to_string()])?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'policy.evaluated',?2)",
        params![crate::journal::timestamp(), record.to_string()],
    )?;
    tx.commit()?;
    Ok(verdict)
}
