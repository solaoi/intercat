//! Durable ownership independent of the current Commit review. Completed Runs
//! must remain controllable after their parent job has been committed.
use crate::{
    Result,
    repair_run::{Phase, Run},
    run_processes::{Command, Remaining, State},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

fn replay(db: &Connection, op: Uuid, kind: &str, request: &Value) -> Result<Option<Run>> {
    let old: Option<(String, String)> = db
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [op.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((saved_kind, payload)) = old else {
        return Ok(None);
    };
    let payload: Value = serde_json::from_str(&payload)?;
    if saved_kind != kind || &payload["request"] != request {
        return Err("Operation ID was used for a different Run process operation.".into());
    }
    Ok(Some(crate::repair_run_store::read(
        db,
        serde_json::from_value(payload["run"].clone())?,
    )?))
}
fn record(db: &Connection, op: Uuid, kind: &str, request: Value, run: &Run) -> Result<()> {
    db.execute(
        "INSERT INTO operations VALUES(?1,?2,?3,'complete')",
        params![
            op.to_string(),
            kind,
            json!({"request":request,"run":run.id}).to_string()
        ],
    )?;
    crate::repair_run_store::save(db, run)?;
    crate::repair_run_store::record(db, kind, run)
}
pub(crate) fn retired(db: &mut Connection, id: Uuid, remaining: Remaining) -> Result<Run> {
    remaining.validate()?;
    let tx = db.transaction()?;
    let op = Uuid::new_v5(&id, remaining.nonce.as_bytes());
    let request = serde_json::to_value(&remaining)?;
    if let Some(run) = replay(&tx, op, "repair.retired", &request)? {
        return Ok(run);
    }
    let mut run = crate::repair_run_store::read(&tx, id)?;
    run.retired(remaining)?;
    run.revision = run
        .revision
        .checked_add(1)
        .ok_or("Repair revision overflow.")?;
    record(&tx, op, "repair.retired", request, &run)?;
    tx.commit()?;
    Ok(run)
}
pub(crate) fn decide(db: &mut Connection, command: Command) -> Result<Run> {
    if !matches!(command.op_id.get_version_num(), 5 | 7) {
        return Err("Invalid Run process operation.".into());
    }
    let tx = db.transaction()?;
    let request = serde_json::to_value(&command)?;
    if let Some(run) = replay(&tx, command.op_id, "repair.processes", &request)? {
        return Ok(run);
    }
    let mut run = crate::repair_run_store::read(&tx, command.id)?;
    run.remaining
        .as_mut()
        .ok_or("This Run no longer has remaining processes.")?
        .decide(&command)?;
    record(&tx, command.op_id, "repair.processes", request, &run)?;
    tx.commit()?;
    Ok(run)
}
pub(crate) fn cleaning(db: &mut Connection, id: Uuid, nonce: Uuid) -> Result<Run> {
    let tx = db.transaction()?;
    let mut run = crate::repair_run_store::read(&tx, id)?;
    let remaining = run
        .remaining
        .as_mut()
        .ok_or("Run processes were already cleaned up.")?;
    if remaining.nonce != nonce {
        return Err("The Run process owner changed.".into());
    }
    if remaining.state != State::Cleaning {
        remaining.state = State::Cleaning;
        remaining.error = None;
        remaining.revision = remaining
            .revision
            .checked_add(1)
            .ok_or("Process revision overflow.")?;
        crate::repair_run_store::save(&tx, &run)?;
        crate::repair_run_store::record(&tx, "repair.processes-cleaning", &run)?;
    }
    tx.commit()?;
    Ok(run)
}
pub(crate) fn clear(db: &mut Connection, id: Uuid, nonce: Uuid) -> Result<()> {
    let tx = db.transaction()?;
    let mut run = crate::repair_run_store::read(&tx, id)?;
    if let Some(remaining) = &run.remaining {
        if remaining.nonce != nonce || remaining.state != State::Cleaning {
            return Err("The Run process cleanup is not confirmed.".into());
        }
        if matches!(
            run.phase,
            Phase::Captured | Phase::Applying | Phase::NeedsRecovery
        ) {
            return Err("Wait for the saved repair before releasing its workspace.".into());
        }
        run.remaining = None;
        if run.phase == Phase::Stopping {
            run.phase = Phase::Held;
            run.revision = run
                .revision
                .checked_add(1)
                .ok_or("Repair revision overflow.")?;
        }
        crate::repair_run_store::save(&tx, &run)?;
        crate::repair_run_store::record(&tx, "repair.processes-cleared", &run)?;
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn failed(db: &mut Connection, id: Uuid, nonce: Uuid) -> Result<()> {
    let tx = db.transaction()?;
    let mut run = crate::repair_run_store::read(&tx, id)?;
    if let Some(remaining) = &mut run.remaining
        && remaining.nonce == nonce
    {
        let message = if remaining.state == State::Cleaning {
            "The run's files could not be released. They have been kept for recovery. Try cleanup again."
        } else {
            "The processes could not be checked or stopped. Their records have been kept. Try Stop it again."
        };
        if remaining.error.as_deref() != Some(message) {
            remaining.error = Some(message.into());
            crate::repair_run_store::save(&tx, &run)?;
            crate::repair_run_store::record(&tx, "repair.processes-failed", &run)?;
        }
    }
    tx.commit()?;
    Ok(())
}
