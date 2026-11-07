use crate::{
    Result, integration, integration_store,
    repair_run::{Command, Event, Phase, Request, Run},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE repair_runs(id TEXT PRIMARY KEY, job TEXT NOT NULL, active INTEGER NOT NULL, record TEXT NOT NULL);
    CREATE UNIQUE INDEX one_repair_per_job ON repair_runs(job) WHERE active=1;")?;
    Ok(())
}

pub(crate) fn active(db: &Connection, job: Uuid) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM repair_runs WHERE job=?1 AND active=1)",
        [job.to_string()],
        |r| r.get(0),
    )?)
}

pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Run> {
    let value: String = db.query_row(
        "SELECT record FROM repair_runs WHERE id=?1",
        [id.to_string()],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str(&value)?)
}

pub(crate) fn save(db: &Connection, run: &Run) -> Result<()> {
    if run.phase == Phase::Discarded
        && let crate::repair_run::Purpose::Assumption { id, .. } = run.purpose
    {
        crate::integration_assumptions::finish_change(db, run.job_id, id, false)?;
    }
    db.execute("INSERT INTO repair_runs VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET active=excluded.active,record=excluded.record",
        params![run.id.to_string(), run.job_id.to_string(), run.phase != Phase::Discarded && (run.phase != Phase::Done || run.refresh.as_ref().is_some_and(|r| r.error.is_none())), serde_json::to_string(run)?])?;
    Ok(())
}

pub(crate) fn record(db: &Connection, kind: &str, run: &Run) -> Result<()> {
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,?2,?3)",
        params![
            crate::journal::timestamp(),
            kind,
            serde_json::to_string(run)?
        ],
    )?;
    Ok(())
}

fn replay(db: &Connection, op: Uuid, kind: &str, request: &Value) -> Result<Option<Run>> {
    let previous: Option<(String, String)> = db
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [op.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((previous_kind, payload)) = previous else {
        return Ok(None);
    };
    let payload: Value = serde_json::from_str(&payload)?;
    if previous_kind != kind || &payload["request"] != request {
        return Err(crate::repair_run::Rejected(
            "Operation ID was used for a different repair request.",
        )
        .into());
    }
    Ok(Some(read(
        db,
        serde_json::from_value(payload["run"].clone())?,
    )?))
}

fn operation(db: &Connection, op: Uuid, kind: &str, request: Value, run: &Run) -> Result<()> {
    db.execute(
        "INSERT INTO operations VALUES(?1,?2,?3,'complete')",
        params![
            op.to_string(),
            kind,
            json!({"request":request,"run":run.id}).to_string()
        ],
    )?;
    save(db, run)?;
    record(db, kind, run)
}

fn current(db: &Connection, run: &Run) -> Result<integration::Job> {
    let job = integration_store::read(db, run.job_id)?;
    let valid_status = if run.phase == Phase::Applying {
        job.status == integration::Status::Staging
    } else {
        matches!(
            job.status,
            integration::Status::Failed | integration::Status::Checked
        )
    };
    if job.revision != run.job_revision || !valid_status {
        return Err(crate::repair_run::Rejected(
            "The integration changed while its repair Run was active.",
        )
        .into());
    }
    Ok(job)
}

pub(crate) fn start(db: &mut Connection, request: Request) -> Result<Run> {
    let tx = db.transaction()?;
    let value = serde_json::to_value(&request)?;
    if let Some(run) = replay(&tx, request.op_id, "repair.start", &value)? {
        return Ok(run);
    }
    let mut job = integration_store::read(&tx, request.job_id)?;
    Run::validate_request(&job, &request)?;
    if active(&tx, job.id)? {
        return Err(crate::repair_run::Rejected("This candidate already has a repair Run.").into());
    }
    let settings = integration_store::settings(&tx, job.id, job.revision)?
        .ok_or("Missing frozen Check settings.")?;
    if job.candidate.as_ref().unwrap().settings_hash != settings.hash()? {
        return Err("The candidate settings changed.".into());
    }
    if let crate::repair_run::Purpose::Assumption { id, revision, .. } = &request.purpose {
        crate::integration_assumptions::begin_change(&tx, &mut job, *id, *revision)?;
    }
    job.revision = job
        .revision
        .checked_add(1)
        .ok_or("Integration revision overflow.")?;
    job.candidate.as_mut().unwrap().revision = job.revision;
    integration_store::copy_settings(&tx, job.id, request.revision, job.revision, None)?;
    integration_store::save(&tx, &job)?;
    let run = Run {
        id: Uuid::new_v5(&request.op_id, b"repair"),
        revision: 1,
        job_id: job.id,
        job_revision: job.revision,
        track: request.track,
        selection: match &request.purpose {
            crate::repair_run::Purpose::Edit { selection, .. } => Some(selection.clone()),
            _ => None,
        },
        purpose: request.purpose,
        phase: Phase::Queued,
        created_at: crate::journal::timestamp(),
        host: None,
        host_nonce: None,
        secret_revision: None,
        secret_input: None,
        checkpoint: None,
        session: None,
        session_ready: false,
        workspace_ready: false,
        start_command: None,
        turn: None,
        failure: None,
        approvals: vec![],
        prompts: vec![],
        output: None,
        instruction: None,
        refresh: None,
        restoring: None,
    };
    operation(&tx, request.op_id, "repair.start", value, &run)?;
    tx.commit()?;
    Ok(run)
}

pub(crate) fn control(db: &mut Connection, command: Command) -> Result<Run> {
    let tx = db.transaction()?;
    let value = serde_json::to_value(&command)?;
    if let Some(run) = replay(&tx, command.op_id, "repair.control", &value)? {
        return Ok(run);
    }
    let mut run = read(&tx, command.id)?;
    current(&tx, &run)?;
    if run.revision != command.revision {
        return Err("The repair Run changed. Review it again.".into());
    }
    if command.action != crate::repair_run::Control::Stop
        && crate::secret_input_store::repair_pending(&tx, run.id)?
    {
        return Err("A secret is being prepared. Stop the repair before changing it.".into());
    }
    run.control(command.action)?;
    run.revision = run
        .revision
        .checked_add(1)
        .ok_or("Repair revision overflow.")?;
    operation(&tx, command.op_id, "repair.control", value, &run)?;
    tx.commit()?;
    Ok(run)
}

pub(crate) fn begin_refresh(
    db: &mut Connection,
    request: crate::repair_run::RefreshRequest,
) -> Result<Run> {
    use crate::repair_run::{Control, Refresh, Rejected};
    let tx = db.transaction()?;
    let value = serde_json::to_value(&request)?;
    if let Some(run) = replay(&tx, request.op_id, "repair.refresh", &value)? {
        return Ok(run);
    }
    let job = integration_store::read(&tx, request.job_id)?;
    let mut run = read(&tx, request.repair_id)?;
    if job.revision != request.revision || run.job_id != job.id || run.job_revision != job.revision
    {
        return Err(Rejected("The review changed. Confirm Refresh again.").into());
    }
    if !matches!(
        job.status,
        integration::Status::Failed | integration::Status::Checked
    ) || run.restoring.is_some()
        || matches!(
            run.phase,
            Phase::Applying | Phase::NeedsRecovery | Phase::Discarded
        )
        || run
            .refresh
            .as_ref()
            .is_some_and(|refresh| refresh.error.is_none())
    {
        return Err(Rejected(
            "This repair cannot be refreshed yet. Wait for it to finish or recover.",
        )
        .into());
    }
    if matches!(
        run.phase,
        Phase::Queued | Phase::Preparing | Phase::Running | Phase::Stopping | Phase::Held
    ) {
        run.control(Control::Stop)?;
    }
    run.refresh = Some(Refresh {
        operation: request.op_id,
        error: None,
    });
    run.revision = run
        .revision
        .checked_add(1)
        .ok_or("Repair revision overflow.")?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'repair.refresh',?2,'prepared')",
        params![
            request.op_id.to_string(),
            json!({"request":value,"run":run.id}).to_string()
        ],
    )?;
    save(&tx, &run)?;
    record(&tx, "repair.refresh-requested", &run)?;
    tx.commit()?;
    Ok(run)
}

pub(crate) fn finish_refresh(
    db: &mut Connection,
    operation: Uuid,
) -> Result<Option<integration::Job>> {
    let mut tx = db.transaction()?;
    let (kind, payload, state): (String, String, String) = tx.query_row(
        "SELECT kind,payload,state FROM operations WHERE id=?1",
        [operation.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if kind != "repair.refresh" {
        return Err("This operation is not a repair Refresh.".into());
    }
    if state == "complete" {
        return Ok(None);
    }
    let mut payload: Value = serde_json::from_str(&payload)?;
    let request: crate::repair_run::RefreshRequest =
        serde_json::from_value(payload["request"].clone())?;
    if state == "dispatched" {
        let job = integration_store::read(&tx, request.job_id)?;
        if Some(job.revision) == payload["capturedRevision"].as_u64()
            && matches!(
                job.status,
                integration::Status::Preparing | integration::Status::Open
            )
        {
            return Ok(Some(job));
        }
        complete_refresh_in(&tx, operation, request.repair_id)?;
        tx.commit()?;
        return Ok(None);
    }
    if state != "prepared" {
        return Err("The repair Refresh has an invalid state.".into());
    }
    let mut run = read(&tx, request.repair_id)?;
    if run
        .refresh
        .as_ref()
        .is_none_or(|refresh| refresh.operation != operation || refresh.error.is_some())
        || run.host.is_some()
        || !matches!(
            run.phase,
            Phase::Held | Phase::Captured | Phase::Failed | Phase::Done
        )
    {
        return Err("The repair has not finished stopping for Refresh.".into());
    }
    let mut discarded = run.clone();
    discarded.phase = Phase::Discarded;
    discarded.revision = discarded
        .revision
        .checked_add(1)
        .ok_or("Repair revision overflow.")?;
    let result = {
        let savepoint = tx.savepoint()?;
        save(&savepoint, &discarded)?;
        let result = integration_store::command_in(
            &savepoint,
            &integration::Command {
                op_id: Uuid::new_v5(&operation, b"integration-refresh"),
                action: integration::Action::Refresh {
                    id: request.job_id,
                    revision: request.revision,
                },
            },
        );
        match result {
            Ok(job) => {
                savepoint.commit()?;
                Ok(job)
            }
            Err(error) => Err(error),
        }
    };
    let job = match result {
        Ok(job) => {
            run = discarded;
            payload["capturedRevision"] = json!(job.revision);
            Some(job)
        }
        Err(_) => {
            if run.phase == Phase::Captured {
                run.phase = Phase::Held;
            }
            run.refresh.as_mut().unwrap().error = Some(
                "Refresh could not finish. Review the current candidate before trying again."
                    .into(),
            );
            run.revision = run
                .revision
                .checked_add(1)
                .ok_or("Repair revision overflow.")?;
            None
        }
    };
    save(&tx, &run)?;
    record(&tx, "repair.refreshed", &run)?;
    tx.execute(
        "UPDATE operations SET state=?2,payload=?3 WHERE id=?1",
        params![
            operation.to_string(),
            if job.is_some() {
                "dispatched"
            } else {
                "complete"
            },
            payload.to_string()
        ],
    )?;
    tx.commit()?;
    Ok(job)
}

pub(crate) fn complete_refresh(db: &mut Connection, operation: Uuid) -> Result<()> {
    let tx = db.transaction()?;
    let (payload, state): (String, String) = tx.query_row(
        "SELECT payload,state FROM operations WHERE id=?1 AND kind='repair.refresh'",
        [operation.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if state == "complete" {
        return Ok(());
    }
    if state != "dispatched" {
        return Err("The repair Refresh has not captured its candidate.".into());
    }
    let payload: Value = serde_json::from_str(&payload)?;
    let id = serde_json::from_value(payload["run"].clone())?;
    complete_refresh_in(&tx, operation, id)?;
    tx.commit()?;
    Ok(())
}

fn complete_refresh_in(db: &Connection, operation: Uuid, id: Uuid) -> Result<()> {
    let mut run = read(db, id)?;
    if run
        .refresh
        .as_ref()
        .is_some_and(|refresh| refresh.operation == operation)
    {
        run.refresh = None;
        run.revision = run
            .revision
            .checked_add(1)
            .ok_or("Repair revision overflow.")?;
        save(db, &run)?;
        record(db, "repair.refresh-completed", &run)?;
    }
    db.execute(
        "UPDATE operations SET state='complete' WHERE id=?1",
        [operation.to_string()],
    )?;
    Ok(())
}

pub(crate) fn followup(db: &mut Connection, request: crate::repair_run::FollowUp) -> Result<Run> {
    let tx = db.transaction()?;
    let run = followup_in(&tx, request)?;
    tx.commit()?;
    Ok(run)
}
pub(crate) fn followup_in(
    tx: &rusqlite::Transaction<'_>,
    request: crate::repair_run::FollowUp,
) -> Result<Run> {
    let value = serde_json::to_value(&request)?;
    if let Some(run) = replay(tx, request.op_id, "repair.followup", &value)? {
        return Ok(run);
    }
    crate::repair_run::validate_instruction(&request.text)?;
    let mut run = read(tx, request.id)?;
    if crate::secret_input_store::repair_pending(tx, run.id)? {
        return Err(
            "A secret is being prepared. Stop the repair before sending another instruction."
                .into(),
        );
    }
    let job = current(tx, &run)?;
    if let Some(selection) = &request.selection {
        Run::validate_selection(&job, &run.track, selection)?;
    }
    if run.revision != request.revision {
        return Err(crate::repair_run::Rejected(
            "The repair changed. Review it before sending more instructions.",
        )
        .into());
    }
    if run.phase != Phase::Held || run.host.is_some() {
        return Err(crate::repair_run::Rejected(
            "Stop the repair before sending another instruction.",
        )
        .into());
    }
    if run
        .refresh
        .as_ref()
        .is_some_and(|refresh| refresh.error.is_none())
    {
        return Err(crate::repair_run::Rejected(
            "Refresh is finishing. Wait before sending another instruction.",
        )
        .into());
    }
    run.control(crate::repair_run::Control::Resume)?;
    run.instruction = Some(request.text);
    if let Some(selection) = request.selection {
        run.selection = Some(selection);
    }
    run.revision = run
        .revision
        .checked_add(1)
        .ok_or("Repair revision overflow.")?;
    operation(tx, request.op_id, "repair.followup", value, &run)?;
    Ok(run)
}

pub(crate) fn transition(
    db: &mut Connection,
    op: Uuid,
    id: Uuid,
    revision: u64,
    event: Event,
) -> Result<Run> {
    let tx = db.transaction()?;
    let value = json!({"id":id,"revision":revision,"event":event});
    if let Some(run) = replay(&tx, op, "repair.transition", &value)? {
        return Ok(run);
    }
    let mut run = read(&tx, id)?;
    current(&tx, &run)?;
    if run.revision != revision {
        return Err("The repair transition was superseded.".into());
    }
    if matches!(event, Event::ReserveStart { .. }) {
        crate::project_secrets::validate_revision(&tx, run.secret_revision)?;
        if let Some(id) = run.secret_input {
            crate::secret_input_store::validate_repair_start(&tx, id, &run)?;
        }
    }
    run.transition(&event)?;
    run.revision = run
        .revision
        .checked_add(1)
        .ok_or("Repair revision overflow.")?;
    operation(&tx, op, "repair.transition", value, &run)?;
    tx.commit()?;
    Ok(run)
}

pub(crate) fn input(
    db: &Connection,
    id: Uuid,
) -> Result<(Run, integration::Job, crate::candidate::Settings)> {
    let run = read(db, id)?;
    let job = current(db, &run)?;
    let settings =
        integration_store::settings(db, job.id, job.revision)?.ok_or("Missing repair settings.")?;
    Ok((run, job, settings))
}

pub(crate) fn begin_restore(
    db: &mut Connection,
    operation: Uuid,
    id: Uuid,
    track: String,
    snapshot: Uuid,
) -> Result<Run> {
    use crate::repair_run::Rejected;
    let tx = db.transaction()?;
    let request = json!({"id":id,"trackId":track,"snapshotId":snapshot});
    if let Some(run) = replay(&tx, operation, "repair.restore", &request)? {
        return Ok(run);
    }
    let mut run = read(&tx, id)?;
    current(&tx, &run)?;
    if crate::secret_input_store::repair_pending(&tx, run.id)? {
        return Err("A secret is being prepared. Stop the repair before restoring files.".into());
    }
    let source = crate::deletion::read(&tx, snapshot)?;
    if run.track != track || source.track_id != track || source.repair_run_id != Some(id) {
        return Err(Rejected("These saved files belong to another worker or repair.").into());
    }
    if !matches!(run.phase, Phase::Held | Phase::Failed)
        || run.host.is_some()
        || !run.workspace_ready
        || run.restoring.is_some()
        || run.refresh.as_ref().is_some_and(|r| r.error.is_none())
    {
        return Err(Rejected("Stop this repair before restoring its saved files.").into());
    }
    run.phase = Phase::Held;
    run.restoring = Some(operation);
    run.revision = run
        .revision
        .checked_add(1)
        .ok_or("Repair revision overflow.")?;
    tx.execute("INSERT INTO operations VALUES(?1,'repair.restore',?2,'prepared')", params![operation.to_string(), json!({"request":request,"run":id,"trackId":track,"repairRunId":id,"snapshotId":snapshot}).to_string()])?;
    save(&tx, &run)?;
    record(&tx, "repair.restore-requested", &run)?;
    tx.commit()?;
    Ok(run)
}

pub(crate) fn finish_restore(
    db: &mut Connection,
    operation: Uuid,
    result: crate::deletion::Restored,
) -> Result<()> {
    let tx = db.transaction()?;
    let (payload, state): (String, String) = tx.query_row(
        "SELECT payload,state FROM operations WHERE id=?1 AND kind='repair.restore'",
        [operation.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if state == "complete" {
        return Ok(());
    }
    let mut payload: Value = serde_json::from_str(&payload)?;
    if payload["snapshotId"] != result.snapshot_id.to_string() {
        return Err("The restore result belongs to another snapshot.".into());
    }
    let mut run = read(&tx, serde_json::from_value(payload["run"].clone())?)?;
    if run.restoring != Some(operation) || run.host.is_some() || run.phase != Phase::Held {
        return Err("The restoring repair changed.".into());
    }
    run.restoring = None;
    run.failure = None;
    run.revision = run
        .revision
        .checked_add(1)
        .ok_or("Repair revision overflow.")?;
    payload["result"] = serde_json::to_value(result)?;
    save(&tx, &run)?;
    record(&tx, "repair.restored", &run)?;
    tx.execute(
        "UPDATE operations SET state='complete',payload=?2 WHERE id=?1",
        params![operation.to_string(), payload.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

/// Reserve the already allocated repair revision before any rebuilt candidate
/// can be staged. The returned failed input is immutable build input; the live
/// job is Staging and all user edits remain blocked by the active repair. The
/// normal integration coordinator must not launch a second build on Open.
pub(crate) fn begin_apply(
    db: &mut Connection,
    id: Uuid,
    revision: u64,
) -> Result<(Run, integration::Job, crate::candidate::Settings)> {
    let tx = db.transaction()?;
    let (mut run, input, settings) = input(&tx, id)?;
    if run.revision != revision
        || run.phase != Phase::Captured
        || run.host.is_some()
        || run.refresh.is_some()
        || run.restoring.is_some()
    {
        return Err("The captured repair changed before rebuilding.".into());
    }
    if !crate::integration_assumptions::current(&tx, &input)? {
        return Err("Assumptions changed · Refresh to review them.".into());
    }
    run.phase = Phase::Applying;
    run.revision = run
        .revision
        .checked_add(1)
        .ok_or("Repair revision overflow.")?;
    let mut live = input.clone();
    live.status = integration::Status::Staging;
    integration_store::save(&tx, &live)?;
    save(&tx, &run)?;
    record(&tx, "repair.applying", &run)?;
    tx.commit()?;
    Ok((run, input, settings))
}

pub(crate) fn applied(
    db: &mut Connection,
    id: Uuid,
    revision: u64,
    result: std::result::Result<crate::candidate::Candidate, String>,
) -> Result<Run> {
    let tx = db.transaction()?;
    let mut run = read(&tx, id)?;
    let mut job = current(&tx, &run)?;
    if run.revision != revision || run.phase != Phase::Applying || run.host.is_some() {
        return Err("The repair result was superseded.".into());
    }
    if let Ok(candidate) = &result {
        let plan = candidate
            .repair
            .as_ref()
            .ok_or("Missing repair attribution.")?;
        let settings = integration_store::settings(&tx, job.id, job.revision)?
            .ok_or("Missing repair settings.")?;
        let mut input = job.clone();
        input.status = if input
            .candidate
            .as_ref()
            .is_some_and(|c| c.state == crate::candidate::State::Checked)
        {
            integration::Status::Checked
        } else {
            integration::Status::Failed
        };
        plan.validate(&input, &settings)?;
        if plan.requested_track != run.track || candidate.generation != job.revision {
            return Err("The rebuilt candidate belongs to another repair.".into());
        }
    }
    let failed = result.is_err();
    integration_store::apply_candidate(&tx, &mut job, run.job_revision, result)?;
    if !failed && let crate::repair_run::Purpose::Assumption { id, .. } = run.purpose {
        crate::integration_assumptions::finish_change(&tx, run.job_id, id, true)?;
    }
    run.phase = if failed { Phase::Failed } else { Phase::Done };
    run.failure = failed.then_some(crate::repair_run::Failure::Capture);
    run.revision = run
        .revision
        .checked_add(1)
        .ok_or("Repair revision overflow.")?;
    save(&tx, &run)?;
    record(&tx, "repair.applied", &run)?;
    tx.commit()?;
    Ok(run)
}

pub(crate) fn snapshot(db: &Connection) -> Result<Value> {
    let mut statement =
        db.prepare("SELECT record FROM repair_runs WHERE active=1 OR (json_extract(record,'$.refresh.operation') IS NOT NULL AND json_extract(record,'$.refresh.error') IS NULL) OR rowid IN (SELECT rowid FROM repair_runs WHERE active=0 ORDER BY rowid DESC LIMIT 100) ORDER BY rowid DESC")?;
    let values = statement
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(serde_json::to_value(
        values
            .into_iter()
            .map(|s| serde_json::from_str::<Run>(&s))
            .collect::<serde_json::Result<Vec<_>>>()?,
    )?)
}

pub(crate) fn interrupt(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    let rows = {
        let mut stmt = tx.prepare("SELECT record FROM repair_runs WHERE active=1")?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for value in rows {
        let mut run: Run = serde_json::from_str(&value)?;
        if run.phase == Phase::Queued && run.secret_input.is_some() {
            run.phase = Phase::Held;
            run.revision += 1;
            save(&tx, &run)?;
            record(&tx, "repair.secret-interrupted", &run)?;
        }
        if matches!(
            run.phase,
            Phase::Preparing | Phase::Running | Phase::Stopping | Phase::Applying
        ) {
            run.phase = Phase::NeedsRecovery;
            run.revision = run
                .revision
                .checked_add(1)
                .ok_or("Repair revision overflow.")?;
            save(&tx, &run)?;
            record(&tx, "repair.interrupted", &run)?;
        }
    }
    tx.commit()?;
    Ok(())
}
