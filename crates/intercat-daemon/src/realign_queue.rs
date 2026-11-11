//! User requests survive closed windows and worker turns. File application
//! has a separate immutable intent, created only at the idle write boundary.
use crate::{
    Result,
    realign::{Control, ControlAction, Job, Phase, Request},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

pub(crate) enum Change {
    Request(Request),
    Control(Control),
    Claim {
        id: Uuid,
        revision: u64,
    },
    Wake {
        id: Uuid,
        revision: u64,
    },
    Inspection {
        id: Uuid,
        revision: u64,
        value: crate::workspace_access::Inspection,
    },
    Finish {
        id: Uuid,
        revision: u64,
        phase: Phase,
        error: Option<String>,
        inspection: Option<crate::workspace_access::Inspection>,
        consume_stop: bool,
    },
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS realign_jobs(id TEXT PRIMARY KEY,track TEXT NOT NULL REFERENCES tracks(id),phase TEXT NOT NULL,record TEXT NOT NULL);
        CREATE UNIQUE INDEX IF NOT EXISTS one_active_realignment ON realign_jobs(track) WHERE phase NOT IN ('done','discarded');")?;
    Ok(())
}
pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Option<Job>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM realign_jobs WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}
pub(crate) fn save(db: &Connection, job: &Job) -> Result<()> {
    let phase = serde_json::to_value(job.phase)?
        .as_str()
        .ok_or("Invalid realignment phase.")?
        .to_owned();
    db.execute("INSERT INTO realign_jobs VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET phase=excluded.phase,record=excluded.record",params![job.request.op_id.to_string(),job.request.track_id,phase,serde_json::to_string(job)?])?;
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'realign.job',?2)",
        params![
            crate::journal::timestamp(),
            json!({"id":job.request.op_id,"revision":job.revision,"phase":job.phase}).to_string()
        ],
    )?;
    Ok(())
}
pub(crate) fn interrupt(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    for mut job in snapshot(&tx)? {
        if job.phase != Phase::Preparing {
            continue;
        }
        let complete:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND kind='workspace.realign' AND state='complete')",[crate::realign::file_operation(&job.request).to_string()],|row|row.get(0))?;
        job.phase = if complete {
            Phase::Done
        } else if job.merge_input.is_some() || job.shared_input.is_some() {
            Phase::NeedsMerge
        } else {
            Phase::Held
        };
        job.revision = job
            .revision
            .checked_add(1)
            .ok_or("Realignment revision overflow.")?;
        job.error = (!complete).then(|| "Realignment was interrupted · Continue when ready".into());
        save(&tx, &job)?;
    }
    tx.commit()?;
    Ok(())
}
pub(crate) fn can_apply(db: &Connection, request: &Request) -> Result<bool> {
    let Some(job) = read(db, request.op_id)? else {
        return Ok(true);
    };
    let session: Option<String> = db.query_row(
        "SELECT json_extract(record,'$.sessionId') FROM workers WHERE track_id=?1",
        [&request.track_id],
        |row| row.get(0),
    )?;
    Ok(job.request == *request
        && job.phase == Phase::Preparing
        && session.as_deref() == Some(&job.session_id))
}
pub(crate) fn holds_work(db: &Connection, track: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM realign_jobs WHERE track=?1 AND phase IN ('queued','waiting-for-worker','preparing','stopping-processes','needs-merge'))",[track],|row|row.get(0))?)
}
pub(crate) fn owns_review(db: &Connection, job: Uuid) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM realign_jobs WHERE phase NOT IN ('done','discarded') AND json_extract(record,'$.request.source.kind')='reparent' AND json_extract(record,'$.request.source.jobId')=?1) OR EXISTS(SELECT 1 FROM operations WHERE kind='workspace.realign' AND state<>'complete' AND json_extract(payload,'$.request.source.kind')='reparent' AND json_extract(payload,'$.request.source.jobId')=?1)",[job.to_string()],|row|row.get(0))?)
}
pub(crate) fn snapshot(db: &Connection) -> Result<Vec<Job>> {
    let mut query=db.prepare("SELECT record FROM realign_jobs WHERE phase NOT IN ('done','discarded') OR id IN (SELECT id FROM realign_jobs ORDER BY rowid DESC LIMIT 20) ORDER BY rowid")?;
    query
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|raw| Ok(serde_json::from_str(&raw?)?))
        .collect()
}
pub(crate) fn change(db: &mut Connection, change: Change) -> Result<Option<Job>> {
    let tx = db.transaction()?;
    let result = match change {
        Change::Request(request) => Some(request_in(&tx, request)?),
        Change::Claim { id, revision } => {
            let mut job = read(&tx, id)?.ok_or("Missing realignment request.")?;
            if job.revision != revision
                || !matches!(
                    job.phase,
                    Phase::Queued
                        | Phase::WaitingForWorker
                        | Phase::WaitingProcesses
                        | Phase::WaitingAccess
                        | Phase::StoppingProcesses
                )
            {
                None
            } else {
                job.phase = Phase::Preparing;
                job.revision = job
                    .revision
                    .checked_add(1)
                    .ok_or("Realignment revision overflow.")?;
                save(&tx, &job)?;
                Some(job)
            }
        }
        Change::Wake { id, revision } => {
            let mut job = read(&tx, id)?.ok_or("Missing realignment request.")?;
            let raw: String = tx.query_row(
                "SELECT record FROM workers WHERE track_id=?1",
                [&job.request.track_id],
                |row| row.get(0),
            )?;
            let worker: crate::runtime::WorkerState = serde_json::from_str(&raw)?;
            // Only a cleanly released host establishes that no owned children
            // remain. A lost or failed host needs its normal recovery first.
            if job.revision != revision
                || !matches!(job.phase, Phase::WaitingProcesses | Phase::WaitingAccess)
                || !matches!(worker.phase.as_str(), "sleeping" | "stopped")
                || worker.session_id.as_deref() != Some(&job.session_id)
                || worker.active_turn.is_some()
                || worker.checkpoint_pending
                || !worker.prompts.is_empty()
                || !worker.approvals.is_empty()
            {
                None
            } else {
                job.phase = Phase::Queued;
                job.revision = job
                    .revision
                    .checked_add(1)
                    .ok_or("Realignment revision overflow.")?;
                save(&tx, &job)?;
                Some(job)
            }
        }
        Change::Inspection {
            id,
            revision,
            value,
        } => {
            let mut job = read(&tx, id)?.ok_or("Missing realignment request.")?;
            if job.revision != revision
                || job.phase != Phase::WaitingAccess
                || job
                    .inspection
                    .as_ref()
                    .is_some_and(|v| v.fingerprint == value.fingerprint)
            {
                None
            } else {
                job.inspection = Some(value);
                job.revision = job
                    .revision
                    .checked_add(1)
                    .ok_or("Realignment revision overflow.")?;
                save(&tx, &job)?;
                Some(job)
            }
        }
        Change::Finish {
            id,
            revision,
            phase,
            error,
            inspection,
            consume_stop,
        } => {
            let mut job = read(&tx, id)?.ok_or("Missing realignment request.")?;
            if job.revision != revision || job.phase != Phase::Preparing {
                None
            } else {
                job.phase = phase;
                job.error = error;
                job.inspection = inspection;
                if consume_stop {
                    job.stop_command = None;
                    job.stop_checkpoint = None;
                }
                job.revision = job
                    .revision
                    .checked_add(1)
                    .ok_or("Realignment revision overflow.")?;
                save(&tx, &job)?;
                Some(job)
            }
        }
        Change::Control(control) => {
            if control.op_id.get_version_num() != 7 {
                return Err("Invalid realignment control.".into());
            }
            let old: Option<(String, String)> = tx
                .query_row(
                    "SELECT kind,payload FROM operations WHERE id=?1",
                    [control.op_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let payload = serde_json::to_value(&control)?;
            if let Some((kind, raw)) = old {
                if kind != "realign.control" || serde_json::from_str::<Value>(&raw)? != payload {
                    return Err("Realignment control ID reused.".into());
                }
                read(&tx, control.id)?
            } else {
                let mut job = read(&tx, control.id)?.ok_or("Missing realignment request.")?;
                if job.revision != control.revision || job.phase == Phase::Preparing {
                    return Err("This realignment changed. Review its current choices.".into());
                }
                match &control.action {
                    ControlAction::KeepProcesses {}
                        if matches!(job.phase, Phase::NeedsProcesses | Phase::WaitingProcesses) =>
                    {
                        job.keep_processes = true;
                        job.phase = Phase::WaitingProcesses;
                    }
                    ControlAction::StopProcesses {}
                        if matches!(job.phase, Phase::NeedsProcesses | Phase::WaitingProcesses) =>
                    {
                        let raw: String = tx.query_row(
                            "SELECT record FROM workers WHERE track_id=?1",
                            [&job.request.track_id],
                            |row| row.get(0),
                        )?;
                        let worker: crate::runtime::WorkerState = serde_json::from_str(&raw)?;
                        if worker.active_turn.is_some()
                            || worker.checkpoint_pending
                            || worker.session_id.as_deref() != Some(&job.session_id)
                        {
                            return Err("Wait for the worker's current turn and checkpoint before stopping its remaining processes.".into());
                        }
                        job.stop_command = Some(control.op_id);
                        job.stop_checkpoint = worker.checkpoint.map(|cp| cp.turn_id);
                        job.phase = Phase::StoppingProcesses;
                    }
                    ControlAction::Wait {}
                        if matches!(job.phase, Phase::NeedsAccess | Phase::WaitingAccess) =>
                    {
                        job.wait_for_access = true;
                        job.phase = Phase::WaitingAccess;
                    }
                    ControlAction::GoAhead { fingerprint }
                        if matches!(job.phase, Phase::NeedsAccess | Phase::WaitingAccess) =>
                    {
                        if job
                            .inspection
                            .as_ref()
                            .is_none_or(|v| v.fingerprint != *fingerprint)
                        {
                            return Err(
                                "The file-access confirmation changed. Review again.".into()
                            );
                        }
                        crate::realign_store::save_access(
                            &tx,
                            &crate::workspace_access::Consent {
                                op_id: Uuid::now_v7(),
                                request: job.request.clone(),
                                fingerprint: fingerprint.clone(),
                            },
                        )?;
                        job.wait_for_access = false;
                        job.phase = Phase::Queued;
                    }
                    ControlAction::Retry {}
                        if matches!(
                            job.phase,
                            Phase::Held | Phase::Failed | Phase::WaitingForWorker
                        ) =>
                    {
                        if !crate::realign_store::pending(&tx, &job.request.track_id)? {
                            job.merge_input = None;
                            job.merge_boundary = None;
                            job.shared_input = None;
                            job.resolutions.clear();
                        }
                        job.phase = Phase::Queued;
                    }
                    ControlAction::Discard {} => {
                        let shared_active = job
                            .shared_input
                            .as_ref()
                            .map(|input| crate::repair_run_store::active(&tx, input.id))
                            .transpose()?
                            .is_some_and(|active| active);
                        if crate::repair_run_store::active(&tx, job.request.op_id)? || shared_active
                        {
                            return Err(
                                "Finish or discard this realignment's merge Run first.".into()
                            );
                        }
                        if crate::realign_store::pending(&tx, &job.request.track_id)? {
                            return Err(
                                "Finish the saved file update before discarding this request."
                                    .into(),
                            );
                        }
                        job.phase = Phase::Discarded;
                    }
                    _ => {
                        return Err(
                            "This choice is unavailable for the current realignment.".into()
                        );
                    }
                }
                job.revision = job
                    .revision
                    .checked_add(1)
                    .ok_or("Realignment revision overflow.")?;
                job.error = None;
                save(&tx, &job)?;
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'realign.control',?2,'complete')",
                    params![control.op_id.to_string(), payload.to_string()],
                )?;
                Some(job)
            }
        }
    };
    tx.commit()?;
    Ok(result)
}

pub(crate) fn request_in(db: &Connection, request: Request) -> Result<Job> {
    if let Some(old) = read(db, request.op_id)? {
        if old.request != request {
            return Err("Realignment operation ID reused.".into());
        }
        Ok(old)
    } else {
        let used: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1)",
            [request.op_id.to_string()],
            |row| row.get(0),
        )?;
        if used {
            return Err("Realignment operation ID reused.".into());
        }
        let active:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM realign_jobs WHERE track=?1 AND phase NOT IN ('done','discarded'))",[&request.track_id],|row|row.get(0))?;
        if active {
            return Err("A realignment is already waiting for this worker.".into());
        }
        let context = crate::realign_store::request_context(db, &request)?;
        db.execute(
            "INSERT INTO operations VALUES(?1,'realign.request',?2,'complete')",
            params![request.op_id.to_string(), serde_json::to_string(&request)?],
        )?;
        let job = Job {
            request,
            session_id: context.session,
            revision: 1,
            phase: Phase::Queued,
            keep_processes: false,
            wait_for_access: false,
            stop_command: None,
            stop_checkpoint: None,
            inspection: None,
            error: None,
            merge_input: None,
            merge_boundary: None,
            shared_input: None,
            resolutions: vec![],
        };
        save(db, &job)?;
        Ok(job)
    }
}
