//! Process operations are bound to a confirmed retired path. Its admission
//! hold and the runtime's file-operation mutex keep it idle until Stop finishes.
use crate::{
    Result,
    path_retirement::{Entry, Kind, Phase},
    runtime::WorkerState,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub retirement_id: Uuid,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Stop {
    pub request: Request,
    pub track: String,
    pub session: Option<String>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Status {
    pub record: Stop,
    pub state: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub retirement_id: Uuid,
    pub groups: Option<usize>,
    pub error: Option<String>,
    pub stop: Option<Status>,
}
pub(crate) enum Change {
    Begin(Request),
    Failed { operation: Uuid, error: String },
    Complete(Uuid),
}
fn event(db: &Connection, record: &Stop, state: &str) -> Result<()> {
    db.execute("INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'path.process-stop',?3)",params![crate::journal::timestamp(),record.session,serde_json::json!({"request":record.request,"track":record.track,"state":state,"error":record.error}).to_string()])?;
    Ok(())
}
pub(crate) fn scope(db: &Connection, id: Uuid) -> Result<(Entry, Option<WorkerState>)> {
    let entry = crate::path_retirement::read(db, id)?;
    let session: Option<String> = db
        .query_row(
            "SELECT json_extract(value,'$.id') FROM settings WHERE key='session'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if session.as_deref() != Some(entry.session.as_str()) {
        return Err("Return to the original session before managing this path's processes.".into());
    }
    if entry.phase != Phase::Aside
        || entry.kind != Kind::Aside
        || !crate::path_retirement::holds(db, &entry.track)?
    {
        return Err("This path is no longer set aside.".into());
    }
    let saved = entry.saved.as_ref().ok_or("Missing saved path boundary.")?;
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM workers WHERE track_id=?1",
            [&entry.track],
            |row| row.get(0),
        )
        .optional()?;
    let worker = raw
        .map(|raw| serde_json::from_str::<WorkerState>(&raw))
        .transpose()?;
    if let Some(worker) = &worker {
        if worker.session_id != saved.session_id
            || worker.active_turn.is_some()
            || worker.checkpoint_pending
            || !worker.approvals.is_empty()
            || !worker.prompts.is_empty()
        {
            return Err(
                "Wait for the original worker's saved stop before managing its processes.".into(),
            );
        }
    } else if saved.session_id.is_some() {
        return Err("The original worker session is unavailable.".into());
    }
    Ok((entry, worker))
}
pub(crate) fn pending(db: &Connection, id: Uuid) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind='path.process-stop' AND state<>'complete' AND json_extract(payload,'$.request.retirementId')=?1)",[id.to_string()],|row|row.get(0))?)
}
pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Status> {
    let (raw, state): (String, String) = db.query_row(
        "SELECT payload,state FROM operations WHERE id=?1 AND kind='path.process-stop'",
        [id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let record: Stop = serde_json::from_str(&raw)?;
    if record.request.op_id != id {
        return Err("Process Stop operation ID reused.".into());
    }
    Ok(Status { record, state })
}
pub(crate) fn snapshot(db: &Connection) -> Result<Vec<Status>> {
    db.prepare("SELECT id FROM operations WHERE kind='path.process-stop' AND state<>'complete' ORDER BY rowid")?.query_map([],|row|row.get::<_,String>(0))?.map(|id|read(db,id?.parse()?)).collect()
}
pub(crate) fn change(db: &mut Connection, change: Change) -> Result<Status> {
    let tx = db.transaction()?;
    let status = match change {
        Change::Begin(request) => {
            if request.op_id.get_version_num() != 7 {
                return Err("Invalid process Stop operation ID.".into());
            }
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1)",
                [request.op_id.to_string()],
                |row| row.get(0),
            )?;
            if exists {
                let old = read(&tx, request.op_id)?;
                if old.record.request != request {
                    return Err("Process Stop operation ID reused.".into());
                }
                old
            } else {
                let (entry, _) = scope(&tx, request.retirement_id)?;
                if pending(&tx, request.retirement_id)? {
                    return Err(
                        "A Stop already owns these processes. Review its current state.".into(),
                    );
                }
                let record = Stop {
                    request,
                    track: entry.track,
                    session: entry.saved.unwrap().session_id,
                    error: None,
                };
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'path.process-stop',?2,'prepared')",
                    params![
                        record.request.op_id.to_string(),
                        serde_json::to_string(&record)?
                    ],
                )?;
                event(&tx, &record, "prepared")?;
                Status {
                    record,
                    state: "prepared".into(),
                }
            }
        }
        Change::Failed { operation, error } => {
            let mut status = read(&tx, operation)?;
            if status.state != "complete" && status.record.error.as_deref() != Some(&error) {
                status.record.error = Some(error);
                tx.execute(
                    "UPDATE operations SET payload=?2 WHERE id=?1",
                    params![
                        operation.to_string(),
                        serde_json::to_string(&status.record)?
                    ],
                )?;
                event(&tx, &status.record, &status.state)?;
            }
            status
        }
        Change::Complete(operation) => {
            let mut status = read(&tx, operation)?;
            if status.state != "complete" {
                let (entry, _) = scope(&tx, status.record.request.retirement_id)?;
                if entry.track != status.record.track
                    || entry.saved.as_ref().unwrap().session_id != status.record.session
                {
                    return Err("The stopped process scope changed.".into());
                }
                status.state = "complete".into();
                status.record.error = None;
                tx.execute(
                    "UPDATE operations SET state='complete',payload=?2 WHERE id=?1",
                    params![
                        operation.to_string(),
                        serde_json::to_string(&status.record)?
                    ],
                )?;
                event(&tx, &status.record, &status.state)?;
            }
            status
        }
    };
    tx.commit()?;
    Ok(status)
}
pub(crate) fn count(scan: crate::process::Scan, host: Option<crate::process::Identity>) -> usize {
    let mut groups = scan
        .processes
        .iter()
        .filter(|process| Some(process.identity) != host)
        .map(|process| process.group)
        .collect::<std::collections::HashSet<_>>();
    groups.extend(
        scan.groups
            .into_iter()
            .filter(|group| Some(*group) != host)
            .map(|group| group.pid),
    );
    groups.len()
}
