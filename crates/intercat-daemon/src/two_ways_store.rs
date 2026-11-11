//! The source cannot dispatch until its exact instruction has a second path.
use crate::{
    Result,
    runtime::WorkerState,
    two_ways::{Action, Budget, Change, Command, Job, Phase, Request},
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use uuid::Uuid;
const KIND: &str = "utterance.two-ways";

pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Option<Job>> {
    db.query_row(
        "SELECT kind,payload FROM operations WHERE id=?1",
        [id.to_string()],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    )
    .optional()?
    .map(|(kind, raw)| {
        if kind != KIND {
            return Err("Two-way operation ID reused.".into());
        }
        Ok(serde_json::from_str(&raw)?)
    })
    .transpose()
}
pub(crate) fn list(db: &Connection) -> Result<Vec<Job>> {
    db.prepare("SELECT payload FROM operations WHERE kind=?1 ORDER BY rowid")?
        .query_map([KIND], |r| r.get::<_, String>(0))?
        .map(|r| Ok(serde_json::from_str(&r?)?))
        .collect()
}
fn save(db: &Connection, job: &Job) -> Result<()> {
    let terminal = matches!(
        job.phase,
        Phase::Queued | Phase::Cancelled | Phase::Declined
    );
    db.execute("INSERT INTO operations VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,state=excluded.state",params![job.request.op_id.to_string(),KIND,serde_json::to_string(job)?,if terminal{"complete"}else{"prepared"}])?;
    db.execute("INSERT INTO events(ts,kind,payload) VALUES(?1,'two-ways.state',?2)",params![crate::journal::timestamp(),json!({"id":job.request.op_id,"inputId":job.request.input_id,"revision":job.revision,"phase":job.phase}).to_string()])?;
    Ok(())
}
fn current_word(db: &Connection, job: &Job) -> Result<(String, Value)> {
    crate::session_retirement::ensure_input(db, &job.request.input_id)?;
    let (text, revision, state, raw): (String, i64, String, String) = db.query_row(
        "SELECT text,revision,state,decision FROM utterances WHERE id=?1",
        [&job.request.input_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let decision: Value = serde_json::from_str(&raw)?;
    if revision != job.request.revision
        || state != "placed"
        || decision["trackId"] != job.source_track
        || !matches!(
            decision["kind"].as_str(),
            Some("fork" | "extend" | "schedule")
        )
    {
        return Err(
            "The instruction changed. Cancel this comparison before editing or sending it again."
                .into(),
        );
    }
    Ok((text, decision))
}
fn worker(db: &Connection, track: &str) -> Result<WorkerState> {
    Ok(serde_json::from_str(&db.query_row(
        "SELECT record FROM workers WHERE track_id=?1",
        [track],
        |r| r.get::<_, String>(0),
    )?)?)
}
fn boundary(db: &Connection, job: &Job) -> Result<Option<crate::branch_start::Point>> {
    current_word(db, job)?;
    if crate::edit_hold::active(db, &job.request.input_id)? {
        return Err("Finish editing before trying two ways.".into());
    }
    crate::continuation::ensure_writer(db, &job.source_track)?;
    crate::start_condition::validate_dispatch(db, &job.request.input_id)?;
    if crate::dispatch_control::paused(db)?
        || crate::fresh_store::holds(db, &job.source_track)?
        || crate::realign_queue::holds_work(db, &job.source_track)?
        || crate::path_retirement::holds(db, &job.source_track)?
        || !crate::worker_hold::allowed(db, &job.source_track, &job.request.input_id)?
    {
        return Err("This instruction is held. Start it explicitly before trying two ways.".into());
    }
    let worker = worker(db, &job.source_track)?;
    if worker.session_id.as_deref() != Some(&job.source_session)
        || worker.active_turn.is_some()
        || worker.checkpoint_pending
        || !worker.approvals.is_empty()
        || !worker.prompts.is_empty()
    {
        return Err("Wait for the selected instruction's saved turn boundary.".into());
    }
    use crate::branch_start::Point;
    let point = if let Some(point) = worker.checkpoint {
        if !point.unmerged_paths.is_empty() {
            return Err("Resolve this checkpoint before trying two ways.".into());
        }
        let source = crate::branch::source(db, &job.source_track, point.turn_id)?;
        if source.session_id != job.source_session || source.checkpoint.sha != point.sha {
            return Err("The source session or checkpoint changed.".into());
        }
        Point::Turn { checkpoint: point }
    } else if let Some(Point::Initial { snapshot }) = &job.point {
        crate::branch_start::verify_initial(
            db,
            &job.source_track,
            &job.source_session,
            job.request.op_id,
            snapshot,
        )?;
        Point::Initial {
            snapshot: snapshot.clone(),
        }
    } else if job.point.is_some() {
        return Err("The source lost its paired boundary.".into());
    } else {
        return Ok(None);
    };
    if let Some(captured) = &job.point
        && captured != &point
    {
        return Err("The source moved beyond the paired boundary.".into());
    }
    Ok(Some(point))
}
pub(crate) fn validate_initial_intent(
    db: &Connection,
    intent: &crate::two_ways_initial::Intent,
) -> Result<()> {
    let job = read(db, intent.job)?.ok_or("Missing initial paired request.")?;
    if job.phase != Phase::CapturingInitial
        || job.point.is_some()
        || job.source_track != intent.track_id
        || job.source_session != intent.session_id
        || intent.snapshot.operation != crate::branch_start::initial_operation(intent.job)
        || boundary(db, &job)?.is_some()
    {
        return Err("This request is no longer capturing its initial files.".into());
    }
    let raw: String = db.query_row(
        "SELECT record FROM workspaces WHERE track_id=?1",
        [&intent.track_id],
        |row| row.get(0),
    )?;
    let workspace: crate::preparation::Workspace = serde_json::from_str(&raw)?;
    if workspace.state != "ready"
        || intent.snapshot.reference != format!("{}-initial/{}", workspace.reference, intent.job)
    {
        return Err("The initial-file reference does not belong to this workspace.".into());
    }
    intercat_git::ObjectId::from_hex(intent.snapshot.sha.as_bytes())?;
    Ok(())
}

pub(crate) fn options_source(
    db: &Connection,
    query: &crate::two_ways::OptionsQuery,
) -> Result<crate::two_ways::OptionsSource> {
    if Uuid::parse_str(&query.input_id)?.get_version_num() != 7 || query.revision < 1 {
        return Err("Read the current instruction before choosing a second effort.".into());
    }
    crate::session_retirement::ensure_input(db, &query.input_id)?;
    let (revision, state, raw): (i64, String, Option<String>) = db.query_row(
        "SELECT revision,state,decision FROM utterances WHERE id=?1",
        [&query.input_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let decision: Value =
        serde_json::from_str(&raw.ok_or("Choose a destination before trying two ways.")?)?;
    if revision != query.revision
        || state != "placed"
        || !matches!(
            decision["kind"].as_str(),
            Some("fork" | "extend" | "schedule")
        )
    {
        return Err("The instruction changed. Reopen it before choosing a second effort.".into());
    }
    if crate::plan_execution::owner(db, &query.input_id)?.is_some()
        || crate::review_fix_store::for_input(db, &query.input_id)?.is_some()
        || db.query_row(
            "SELECT question_id IS NOT NULL FROM utterances WHERE id=?1",
            [&query.input_id],
            |r| r.get::<_, bool>(0),
        )?
    {
        return Err("Choose an ordinary instruction to try two ways.".into());
    }
    let track = decision["trackId"]
        .as_str()
        .ok_or("Choose a track before trying two ways.")?
        .to_owned();
    crate::continuation::ensure_writer(db, &track)?;
    let worker = worker(db, &track)?;
    if matches!(
        worker.phase.as_str(),
        "failed" | "lost" | "needs-recovery" | "after-connection-loss"
    ) {
        return Err("Recover this worker before choosing a second effort.".into());
    }
    let released = worker.released();
    let session = worker
        .session_id
        .ok_or("Connect this worker before trying two ways.")?;
    Ok(crate::two_ways::OptionsSource {
        query: query.clone(),
        track,
        session,
        released,
    })
}
pub(crate) fn prepare(db: &Transaction<'_>, request: Request) -> Result<Job> {
    if request.op_id.get_version_num() != 7 || request.revision < 1 {
        return Err("Two ways needs a new operation and the current instruction revision.".into());
    }
    Uuid::parse_str(&request.input_id)?;
    if let Some(job) = read(db, request.op_id)? {
        if job.request != request {
            return Err("Two-way operation ID reused.".into());
        }
        return Ok(job);
    }
    if list(db)?.iter().any(|job| {
        job.request.input_id == request.input_id
            && !matches!(job.phase, Phase::Cancelled | Phase::Declined)
    }) {
        return Err("This instruction already has a paired attempt.".into());
    }
    let source = options_source(
        db,
        &crate::two_ways::OptionsQuery {
            input_id: request.input_id.clone(),
            revision: request.revision,
        },
    )?;
    if request.selection.track_id != source.track
        || request.selection.session_id != source.session
        || request.selection.variation.primary != crate::two_ways::PRIMARY_EFFORT
        || request.selection.variation.alternate == request.selection.variation.primary
    {
        return Err("The selected worker or effort changed. Read its options again.".into());
    }
    let job = Job {
        request,
        revision: 1,
        phase: Phase::Waiting,
        source_track: source.track,
        source_session: source.session,
        branch_id: Uuid::now_v7(),
        child_input: Uuid::now_v7(),
        point: None,
        variation: None,
        usage_observed_at: None,
        error: None,
        resume: None,
    };
    current_word(db, &job)?;
    crate::continuation::ensure_writer(db, &job.source_track)?;
    save(db, &job)?;
    Ok(job)
}
pub(crate) fn allowed(db: &Connection, input: &str, track: &str) -> Result<bool> {
    Ok(!db.query_row("SELECT EXISTS(SELECT 1 FROM operations o LEFT JOIN branches b ON b.id=json_extract(o.payload,'$.branchId') WHERE o.kind=?1 AND o.state='prepared' AND (json_extract(o.payload,'$.sourceTrack')=?2 OR json_extract(b.record,'$.trackId')=?2) AND (json_extract(o.payload,'$.phase')!='waiting' OR json_extract(o.payload,'$.request.inputId')=?3))",params![KIND,track,input],|row|row.get::<_,bool>(0))?)
}
pub(crate) fn holds_track(db: &Connection, track: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind=?1 AND state='prepared' AND json_extract(payload,'$.sourceTrack')=?2)",params![KIND,track],|row|row.get(0))?)
}
pub(crate) fn fenced(db: &Connection, input: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind=?1 AND state='prepared' AND json_extract(payload,'$.request.inputId')=?2)",params![KIND,input],|row|row.get(0))?)
}
pub(crate) fn for_input(db: &Connection, input: &str) -> Result<Option<Job>> {
    db.query_row("SELECT payload FROM operations WHERE kind=?1 AND json_extract(payload,'$.request.inputId')=?2 AND json_extract(payload,'$.phase') NOT IN ('cancelled','declined') ORDER BY rowid DESC LIMIT 1",params![KIND,input],|row|row.get::<_,String>(0)).optional()?.map(|raw|Ok(serde_json::from_str(&raw)?)).transpose()
}
pub(crate) fn effort(db: &Connection, input: &str) -> Result<muse_code_sdk::msp::ReasoningEffort> {
    let raw=db.query_row("SELECT payload FROM operations WHERE kind=?1 AND state='complete' AND json_extract(payload,'$.phase')='queued' AND json_extract(payload,'$.childInput')=?2",params![KIND,input],|row|row.get::<_,String>(0)).optional()?;
    raw.map(|raw| {
        let job: Job = serde_json::from_str(&raw)?;
        Ok(job.variation.ok_or("Missing paired effort.")?.alternate)
    })
    .unwrap_or(Ok(crate::two_ways::PRIMARY_EFFORT))
}
fn publish(tx: &Transaction<'_>, job: &Job) -> Result<()> {
    boundary(tx, job)?;
    let branches = crate::branch::snapshot(tx)?;
    let branch = branches
        .iter()
        .find(|branch| branch.request.op_id == job.branch_id)
        .ok_or("The paired branch is unavailable.")?;
    if branch.phase != crate::branch::Phase::Ready || branch.request != job.branch_request()? {
        return Err("Finish preparing the exact paired branch before dispatch.".into());
    }
    let (text, mut decision) = current_word(tx, job)?;
    let attachments = crate::attachment_store::records(tx, &job.request.input_id)?
        .into_iter()
        .map(|item| item.attachment.id)
        .collect::<Vec<_>>();
    let sources = crate::request_sources::read(tx, &job.request.input_id)?;
    crate::journal::insert_utterance(
        tx,
        &job.child_input.to_string(),
        &text,
        &crate::decision::Target::Lock(branch.track_id.clone()),
        Some(&branch.track_id),
        &attachments,
        sources.as_ref(),
        true,
        None,
    )?;
    decision["trackId"] = branch.track_id.clone().into();
    decision["kind"] = "extend".into();
    decision["boundary"] = "now".into();
    tx.execute(
        "UPDATE utterances SET state='placed',decision=?2 WHERE id=?1",
        params![job.child_input.to_string(), decision.to_string()],
    )?;
    crate::worker_hold::hold_blocked(tx, &branch.track_id, &job.child_input.to_string())?;
    let runnable = crate::worker_hold::allowed(tx, &branch.track_id, &job.child_input.to_string())?;
    let mut child = worker(tx, &branch.track_id)?;
    if child.active_turn.is_some() || child.checkpoint_pending {
        return Err("The paired branch already started another instruction.".into());
    }
    child.phase = if runnable { "ready" } else { "held" }.into();
    tx.execute(
        "UPDATE workers SET record=?2 WHERE track_id=?1",
        params![child.track_id, serde_json::to_string(&child)?],
    )?;
    Ok(())
}
pub(crate) fn change(db: &mut Connection, change: Change) -> Result<Job> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let mut job = match change {
        Change::Command(command) => {
            let job = control(&tx, command)?;
            tx.commit()?;
            return Ok(job);
        }
        Change::Prepare(request) => {
            let job = prepare(&tx, request)?;
            tx.commit()?;
            return Ok(job);
        }
        Change::Capture { id, revision }
        | Change::InitialCaptured { id, revision, .. }
        | Change::Inspect { id, revision, .. }
        | Change::BranchReady { id, revision }
        | Change::Publish { id, revision }
        | Change::Hold { id, revision, .. }
        | Change::Cancelled { id, revision } => {
            let job = read(&tx, id)?.ok_or("The two-way request is unavailable.")?;
            if job.revision != revision {
                return Err("The two-way request changed. Review its latest state.".into());
            }
            job
        }
    };
    match change {
        Change::Capture { .. } => {
            if job.phase != Phase::Waiting {
                return Err("The paired boundary is already fixed.".into());
            }
            job.point = boundary(&tx, &job)?;
            job.phase = if job.point.is_some() {
                Phase::Inspecting
            } else {
                Phase::CapturingInitial
            };
        }
        Change::InitialCaptured { snapshot, .. } => {
            if job.phase != Phase::CapturingInitial || boundary(&tx, &job)?.is_some() {
                return Err("This initial-file capture is stale.".into());
            }
            crate::branch_start::verify_initial(
                &tx,
                &job.source_track,
                &job.source_session,
                job.request.op_id,
                &snapshot,
            )?;
            job.point = Some(crate::branch_start::Point::Initial { snapshot });
            job.phase = Phase::Inspecting;
        }
        Change::Inspect { usage, models, .. } => {
            if !matches!(
                job.phase,
                Phase::Inspecting | Phase::InspectingConfirmedUsage
            ) {
                return Err("The usage result belongs to an old paired boundary.".into());
            }
            boundary(&tx, &job)?;
            match crate::two_ways::budget(&usage, crate::journal::timestamp())? {
                Budget::Unknown if job.phase == Phase::Inspecting => {
                    job.phase = Phase::AwaitingUsageConfirmation;
                }
                Budget::Insufficient { observed_at, .. } => {
                    job.phase = Phase::Declined;
                    job.usage_observed_at = Some(observed_at);
                    job.error = Some(
                        "Not enough plan for two. The original instruction takes it alone.".into(),
                    );
                }
                budget @ (Budget::Available { .. } | Budget::Unknown) => {
                    let current = crate::two_ways::Variation::from_catalog(&models)?;
                    if current != job.request.selection.variation {
                        return Err("The available model or effort changed. Cancel and reopen Try it two ways to review its options.".into());
                    }
                    job.variation = Some(current);
                    job.usage_observed_at = match budget {
                        Budget::Available { observed_at, .. } => Some(observed_at),
                        _ => None,
                    };
                    job.phase = Phase::Forking;
                }
            }
        }
        Change::BranchReady { .. } => {
            if job.phase != Phase::Forking {
                return Err("The paired branch result is stale.".into());
            }
            job.phase = Phase::AwaitingReceipt;
        }
        Change::Publish { .. } => {
            if job.phase != Phase::AwaitingReceipt {
                return Err("This paired attempt cannot dispatch yet.".into());
            }
            publish(&tx, &job)?;
            job.phase = Phase::Queued;
        }
        Change::Hold { error, .. } => {
            if matches!(
                job.phase,
                Phase::Queued | Phase::Cancelled | Phase::Declined
            ) {
                return Err("This paired attempt has already finished preparation.".into());
            }
            if job.phase != Phase::Cancelling {
                job.resume = Some(job.phase);
                job.phase = Phase::Held;
            }
            job.error = Some(error);
        }
        Change::Cancelled { .. } => {
            if job.phase != Phase::Cancelling {
                return Err("This paired attempt is not cancelling.".into());
            }
            crate::branch::cancel(&tx, job.branch_id)?;
            job.phase = Phase::Cancelled;
            job.error = None;
            job.resume = None;
            tx.execute("UPDATE operations SET state='complete' WHERE kind='two-ways.command' AND json_extract(payload,'$.id')=?1 AND json_extract(payload,'$.action')='cancel'",[job.request.op_id.to_string()])?;
        }
        Change::Command(_) => unreachable!(),
        Change::Prepare(_) => unreachable!(),
    }
    job.revision = job
        .revision
        .checked_add(1)
        .ok_or("Paired revision overflow.")?;
    save(&tx, &job)?;
    tx.commit()?;
    Ok(job)
}

pub(crate) fn validate_turn(
    db: &Connection,
    input: &str,
    track: &str,
    params: &Value,
    paired_model: Option<&crate::two_ways::Variation>,
) -> Result<()> {
    let raw=db.query_row("SELECT payload FROM operations WHERE kind=?1 AND json_extract(payload,'$.phase')='queued' AND (json_extract(payload,'$.request.inputId')=?2 OR json_extract(payload,'$.childInput')=?2)",params![KIND,input],|row|row.get::<_,String>(0)).optional()?;
    let Some(raw) = raw else {
        if paired_model.is_some() {
            return Err(crate::journal::StartSuperseded.into());
        }
        return Ok(());
    };
    let job: Job = serde_json::from_str(&raw)?;
    let variation = job
        .variation
        .as_ref()
        .ok_or("Missing paired model selection.")?;
    let session = params["sessionId"]
        .as_str()
        .ok_or("Missing paired session.")?;
    let (declared, expected_session, expected) = if input == job.request.input_id {
        (
            job.source_track.clone(),
            job.source_session.clone(),
            variation.primary,
        )
    } else {
        let branch = crate::branch::snapshot(db)?
            .into_iter()
            .find(|branch| branch.request.op_id == job.branch_id)
            .ok_or("Missing paired branch.")?;
        (
            branch.track_id,
            branch.session_id.ok_or("Missing paired branch session.")?,
            variation.alternate,
        )
    };
    let current = crate::continuation::current(db, &declared)?;
    let current_session = if current == declared {
        expected_session
    } else {
        crate::continuation::start(db, &current)?
            .ok_or("Missing paired continuation.")?
            .session_id
    };
    if track != current || session != current_session {
        return Err(
            "The paired destination changed. Review this attempt before sending it.".into(),
        );
    }
    if paired_model != Some(variation)
        || params["reasoningEffort"] != serde_json::to_value(expected)?
    {
        return Err(
            "The paired model or effort changed. Review this attempt before sending it.".into(),
        );
    }
    Ok(())
}

pub(crate) fn start(
    db: &Connection,
    input: &str,
    track: &str,
) -> Result<Option<crate::branch_start::Point>> {
    let raw=db.query_row("SELECT payload FROM operations WHERE kind=?1 AND json_extract(payload,'$.phase')='queued' AND (json_extract(payload,'$.request.inputId')=?2 OR json_extract(payload,'$.childInput')=?2)",params![KIND,input],|row|row.get::<_,String>(0)).optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let job: Job = serde_json::from_str(&raw)?;
    let declared = if input == job.request.input_id {
        job.source_track.clone()
    } else {
        crate::branch::snapshot(db)?
            .into_iter()
            .find(|branch| branch.request.op_id == job.branch_id)
            .ok_or("Missing paired branch.")?
            .track_id
    };
    let current = crate::continuation::current(db, &declared)?;
    if current != track {
        return Err("The paired starting path changed.".into());
    }
    // An explicit later Start fresh has its own verified current-file handoff.
    // It does not turn an old branch checkpoint back into the current files.
    Ok(if current == declared { job.point } else { None })
}

fn control(tx: &Transaction<'_>, command: Command) -> Result<Job> {
    if command.op_id.get_version_num() != 7
        || command.id.get_version_num() != 7
        || command.revision == 0
    {
        return Err("Invalid paired control request.".into());
    }
    let payload = serde_json::to_string(&command)?;
    if let Some((kind, old)) = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [command.op_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if kind != "two-ways.command" || old != payload {
            return Err("Paired control ID reused.".into());
        }
        return read(tx, command.id)?.ok_or_else(|| "The paired request is unavailable.".into());
    }
    let mut job = read(tx, command.id)?.ok_or("The paired request is unavailable.")?;
    if job.revision != command.revision {
        return Err("The paired request changed. Review its saved state.".into());
    }
    match command.action {
        Action::Cancel => {
            if matches!(job.phase, Phase::Queued | Phase::Declined) {
                return Err(
                    "This instruction has left preparation. Use the worker's Stop controls.".into(),
                );
            }
            if !matches!(job.phase, Phase::Cancelled | Phase::Cancelling) {
                if job.variation.is_some()
                    || job.phase == Phase::CapturingInitial
                    || job.resume == Some(Phase::CapturingInitial)
                    || matches!(job.point, Some(crate::branch_start::Point::Initial { .. }))
                {
                    job.phase = Phase::Cancelling;
                } else {
                    job.phase = Phase::Cancelled;
                }
                job.error = None;
                job.resume = None;
            }
        }
        Action::Retry => {
            if job.phase != Phase::Held {
                return Err("Only a held paired attempt can be retried.".into());
            }
            current_word(tx, &job)?;
            let phase = job
                .resume
                .ok_or("The paired attempt has no saved retry stage.")?;
            if !matches!(
                phase,
                Phase::Waiting
                    | Phase::CapturingInitial
                    | Phase::Inspecting
                    | Phase::InspectingConfirmedUsage
                    | Phase::Forking
                    | Phase::AwaitingReceipt
            ) {
                return Err("The paired attempt cannot retry this stage.".into());
            }
            job.phase = phase;
            job.resume = None;
            job.error = None;
        }
        Action::ConfirmUsage => {
            if job.phase != Phase::AwaitingUsageConfirmation {
                return Err("This request is not waiting for usage confirmation.".into());
            }
            boundary(tx, &job)?;
            job.phase = Phase::InspectingConfirmedUsage;
        }
    }
    job.revision = job
        .revision
        .checked_add(1)
        .ok_or("Paired revision overflow.")?;
    save(tx, &job)?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'two-ways.command',?2,?3)",
        params![
            command.op_id.to_string(),
            payload,
            if job.phase == Phase::Cancelling {
                "prepared"
            } else {
                "complete"
            }
        ],
    )?;
    Ok(job)
}
