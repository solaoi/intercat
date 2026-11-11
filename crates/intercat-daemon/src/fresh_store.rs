//! Transactional source boundary, protected context and dispatch hold for Start fresh.
use crate::{
    Result,
    fresh::{Boundary, Change, Job, Phase, Request},
    preparation::Workspace,
};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Option<Job>> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    row.map(|(kind, payload)| {
        if kind != "worker.fresh" {
            return Err("Start fresh operation ID reused.".into());
        }
        Ok(serde_json::from_str(&payload)?)
    })
    .transpose()
}
pub(crate) fn snapshot(db: &Connection) -> Result<Vec<Job>> {
    let mut query =
        db.prepare("SELECT payload FROM operations WHERE kind='worker.fresh' ORDER BY rowid")?;
    query
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|raw| Ok(serde_json::from_str(&raw?)?))
        .collect()
}
pub(crate) fn holds(db: &Connection, track: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind='worker.fresh' AND state='prepared' AND json_extract(payload,'$.request.trackId')=?1)", [track], |row| row.get(0))?)
}
pub(crate) fn retire(db: &Connection, track: &str, session: &str) -> Result<()> {
    if snapshot(db)?.iter().any(|job| {
        job.request.track_id == track
            && job.request.session_id == session
            && matches!(
                job.phase,
                Phase::PublishingOwner | Phase::OwnerHeld | Phase::Cancelling
            )
    }) {
        return Err(
            "Finish the workspace transfer or cancellation before retiring its source history."
                .into(),
        );
    }

    let ids = snapshot(db)?
        .into_iter()
        .filter(|job| job.request.track_id == track && job.request.session_id == session)
        .map(|job| job.request.op_id.to_string())
        .collect::<Vec<_>>();
    for id in ids {
        db.execute("DELETE FROM operations WHERE (id=?1 AND kind='worker.fresh') OR (kind IN ('worker.fresh.cancel','worker.fresh.access','worker.fresh.retry') AND json_extract(payload,'$.id')=?1)",[&id])?;
        db.execute(
            "DELETE FROM events WHERE kind='worker.fresh' AND json_extract(payload,'$.id')=?1",
            [&id],
        )?;
    }
    Ok(())
}
fn save(db: &Connection, job: &Job) -> Result<()> {
    let payload = serde_json::to_string(job)?;
    db.execute("INSERT INTO operations VALUES(?1,'worker.fresh',?2,?3) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,state=excluded.state", params![job.request.op_id.to_string(),payload,if matches!(job.phase, Phase::Cancelled | Phase::Transferred) { "complete" } else { "prepared" }])?;
    db.execute("INSERT INTO events(ts,kind,payload) VALUES(?1,'worker.fresh',?2)", params![crate::journal::timestamp(),serde_json::json!({"id":job.request.op_id,"trackId":job.request.track_id,"revision":job.revision,"phase":job.phase}).to_string()])?;
    Ok(())
}
fn worker(db: &Connection, request: &Request) -> Result<crate::runtime::WorkerState> {
    crate::session_retirement::ensure_open(db, &request.session_id)?;
    let raw: String = db.query_row(
        "SELECT record FROM workers WHERE track_id=?1",
        [&request.track_id],
        |row| row.get(0),
    )?;
    let worker: crate::runtime::WorkerState = serde_json::from_str(&raw)?;
    if worker.track_id != request.track_id
        || worker.session_id.as_deref() != Some(&request.session_id)
    {
        return Err("Start fresh no longer matches this worker's session.".into());
    }
    if ![
        "ready",
        "held",
        "working",
        "approval",
        "question",
        "checkpoint",
        "sleeping",
        "stopped",
    ]
    .contains(&worker.phase.as_str())
    {
        return Err("Recover this worker before starting fresh.".into());
    }
    Ok(worker)
}
fn workspace(db: &Connection, track: &str) -> Result<Workspace> {
    let raw: String = db.query_row(
        "SELECT record FROM workspaces WHERE track_id=?1",
        [track],
        |row| row.get(0),
    )?;
    let workspace: Workspace = serde_json::from_str(&raw)?;
    if workspace.track_id != track || workspace.state != "ready" {
        return Err("This workspace is unavailable for Start fresh.".into());
    }
    if crate::path_retirement::holds(db, track)?
        || crate::realign_queue::holds_work(db, track)?
        || crate::realign_store::pending(db, track)?
        || crate::inheritance_apply_store::pending(db, track)?.is_some()
        || db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind='deletion.restore' AND state<>'complete' AND json_extract(payload,'$.trackId')=?1 AND json_extract(payload,'$.repairRunId') IS NULL)",[track],|row|row.get::<_,bool>(0))?
        || crate::pickup::snapshot(db)?
            .iter()
            .any(|job| job.request.track_id == track && job.phase.active())
    {
        return Err("Finish this path's pending file operation before starting fresh.".into());
    }
    Ok(workspace)
}
fn request(db: &Connection, request: Request) -> Result<Job> {
    if request.op_id.get_version_num() != 7 {
        return Err("Start fresh needs a new operation ID.".into());
    }
    Uuid::parse_str(&request.track_id)?;
    Uuid::parse_str(&request.session_id)?;
    if let Some(old) = read(db, request.op_id)? {
        if old.request != request {
            return Err("Start fresh operation ID reused.".into());
        }
        return Ok(old);
    }
    if crate::two_ways_store::holds_track(db, &request.track_id)? {
        return Err("Cancel the paired preparation before starting a fresh conversation.".into());
    }
    if holds(db, &request.track_id)? {
        return Err("Start fresh is already waiting for this worker.".into());
    }
    let catalog = crate::secret_store::catalog(db)?;
    if catalog.current.revision > 0 && catalog.sealed.as_ref() != Some(&catalog.current) {
        return Err("Confirm the saved fingerprints before starting fresh.".into());
    }
    crate::continuation::ensure_writer(db, &request.track_id)?;
    let worker = worker(db, &request)?;
    let workspace = workspace(db, &request.track_id)?;
    if worker.active_turn.is_none() && !worker.checkpoint_pending && worker.checkpoint.is_none() {
        return Err("No checkpoint yet.".into());
    }
    let folder = crate::folders::track(db, &request.track_id)?;
    let session: String = db.query_row(
        "SELECT json_extract(value,'$.id') FROM settings WHERE key='session'",
        [],
        |row| row.get(0),
    )?;
    let slug: String = db.query_row(
        "SELECT slug FROM session_folders WHERE session=?1 AND folder=?2",
        params![session, folder.id],
        |row| row.get(0),
    )?;
    let job = Job {
        folder,
        session,
        slug,
        successor_track: Uuid::new_v5(&request.op_id, b"fresh-track").to_string(),
        successor_session: Uuid::now_v7().to_string(),
        request,
        revision: 1,
        phase: Phase::WaitingForTurn,
        workspace,
        boundary: None,
        inspection: None,
        access_consent: None,
        files: None,
        summary_id: None,
        summary_input: None,
        summary: None,
        error: None,
    };
    save(db, &job)?;
    Ok(job)
}
pub(crate) fn change(
    db: &mut Connection,
    change: Change,
    key: &crate::secrets::Key,
) -> Result<Job> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let job = match change {
        Change::Request(value) => request(&tx, value)?,
        Change::Capture { id, revision } => {
            let mut job = read(&tx, id)?.ok_or("Missing Start fresh request.")?;
            if job.revision != revision || job.phase != Phase::WaitingForTurn {
                return Err("Start fresh changed before its turn boundary was captured.".into());
            }
            original_scope(&tx, &job)?;
            let worker = worker(&tx, &job.request)?;
            if serde_json::to_value(workspace(&tx, &job.request.track_id)?)?
                != serde_json::to_value(&job.workspace)?
            {
                return Err(
                    "The Start fresh workspace changed. Cancel and review it again.".into(),
                );
            }
            let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind='turn.start' AND state IN ('prepared','dispatched') AND json_extract(payload,'$.sessionId')=?1)", [&job.request.session_id], |row| row.get(0))?;
            if active {
                return Err(crate::fresh::Waiting(
                    "Wait for the admitted turn to finish and save its checkpoint.",
                )
                .into());
            }
            let identity = crate::handoff::Identity {
                track_id: job.request.track_id.clone(),
                session_id: job.request.session_id.clone(),
            };
            let context = crate::handoff_context::capture(
                &tx,
                &identity,
                crate::handoff_context::Boundary::Completed,
                &worker,
                key,
            )?;
            let checkpoint = worker.checkpoint.ok_or("No checkpoint yet.")?;
            let saved: String = tx.query_row("SELECT payload FROM operations WHERE id=?1 AND kind='checkpoint.save' AND state='complete'",[Uuid::new_v5(&checkpoint.turn_id,b"checkpoint").to_string()],|row|row.get(0))?;
            let saved: serde_json::Value = serde_json::from_str(&saved)?;
            if saved["trackId"] != job.request.track_id
                || saved["checkpoint"] != serde_json::to_value(&checkpoint)?
                || checkpoint.reference
                    != format!(
                        "{}-checkpoints/{}",
                        job.workspace.reference, checkpoint.turn_id
                    )
                || !checkpoint.unmerged_paths.is_empty()
            {
                return Err(
                    "Start fresh needs this worker's completed, resolved checkpoint.".into(),
                );
            }
            job.boundary = Some(Boundary {
                checkpoint,
                context,
            });
            job.phase = Phase::Captured;
            job.revision = job
                .revision
                .checked_add(1)
                .ok_or("Start fresh revision overflow.")?;
            save(&tx, &job)?;
            job
        }
        Change::Release { id, revision } => {
            let mut job = expect(&tx, id, revision, &[Phase::Captured])?;
            source_boundary(&tx, &job, false)?;
            job.phase = Phase::Releasing;
            advance(&tx, &mut job)?;
            job
        }
        Change::Released { id, revision } => {
            let mut job = expect(&tx, id, revision, &[Phase::Releasing])?;
            source_boundary(&tx, &job, true)?;
            let worker = worker(&tx, &job.request)?;
            job.boundary
                .as_mut()
                .ok_or("Missing Start fresh boundary.")?
                .context = crate::handoff_context::capture(
                &tx,
                &crate::handoff::Identity {
                    track_id: job.request.track_id.clone(),
                    session_id: job.request.session_id.clone(),
                },
                crate::handoff_context::Boundary::Completed,
                &worker,
                key,
            )?;
            job.phase = Phase::Inspecting;
            advance(&tx, &mut job)?;
            job
        }
        Change::Inspect {
            id,
            revision,
            inspection,
        } => {
            let mut job = expect(&tx, id, revision, &[Phase::Inspecting, Phase::NeedsAccess])?;
            source_boundary(&tx, &job, true)?;
            if job
                .inspection
                .as_ref()
                .is_some_and(|old| old.fingerprint == inspection.fingerprint)
                && job.phase == Phase::NeedsAccess
            {
                job
            } else {
                let permitted = inspection.clear()
                    || job.access_consent.as_deref() == Some(&inspection.fingerprint);
                job.access_consent = permitted.then(|| inspection.fingerprint.clone());
                job.inspection = Some(inspection);
                job.phase = if permitted {
                    Phase::CapturingFiles
                } else {
                    Phase::NeedsAccess
                };
                advance(&tx, &mut job)?;
                job
            }
        }
        Change::ApproveAccess {
            op_id,
            id,
            revision,
            fingerprint,
        } => {
            if op_id.get_version_num() != 7 {
                return Err("File access confirmation needs a new operation ID.".into());
            }
            let payload =
                serde_json::json!({"id":id,"revision":revision,"fingerprint":fingerprint})
                    .to_string();
            let prior: Option<(String, String)> = tx
                .query_row(
                    "SELECT kind,payload FROM operations WHERE id=?1",
                    [op_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((kind, old)) = prior {
                if kind != "worker.fresh.access" || old != payload {
                    return Err("File access operation ID reused.".into());
                }
                read(&tx, id)?.ok_or("Missing Start fresh request.")?
            } else {
                let mut job = expect(&tx, id, revision, &[Phase::NeedsAccess])?;
                source_boundary(&tx, &job, true)?;
                if job
                    .inspection
                    .as_ref()
                    .is_none_or(|value| value.fingerprint != fingerprint)
                {
                    return Err("The file-access confirmation changed. Review again.".into());
                }
                job.access_consent = Some(fingerprint);
                job.phase = Phase::Inspecting;
                advance(&tx, &mut job)?;
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'worker.fresh.access',?2,'complete')",
                    params![op_id.to_string(), payload],
                )?;
                job
            }
        }
        Change::Files {
            id,
            revision,
            files,
        } => {
            let mut job = expect(&tx, id, revision, &[Phase::CapturingFiles])?;
            source_boundary(&tx, &job, true)?;
            intercat_git::ObjectId::from_hex(files.checkpoint.sha.as_bytes())?;
            if files.checkpoint.live_processes
                || !files.checkpoint.unmerged_paths.is_empty()
                || files.checkpoint.turn_id != crate::fresh_files::turn(id, revision)
                || files.checkpoint.reference
                    != crate::fresh_files::reference(&job, &files.checkpoint.sha)
                || files.summary_id
                    != Uuid::new_v5(&id, format!("summary:{}", files.checkpoint.sha).as_bytes())
            {
                return Err("Invalid Start fresh file snapshot.".into());
            }
            job.files = Some(files);
            job.phase = Phase::PublishingFiles;
            advance(&tx, &mut job)?;
            job
        }
        Change::PrepareSummary {
            id,
            revision,
            changes,
        } => {
            let mut job = expect(&tx, id, revision, &[Phase::SnapshotReady])?;
            source_boundary(&tx, &job, true)?;
            if crate::dispatch_control::paused(&tx)? {
                return Err(
                    crate::fresh::Waiting("Resume before starting the handoff summary.").into(),
                );
            }
            let pending:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM reports WHERE track_id=?1 AND json_extract(record,'$.sessionId')=?2 AND pending=1)",params![job.request.track_id,job.request.session_id],|row|row.get(0))?;
            if pending {
                return Err(crate::fresh::Waiting(
                    "Waiting for the source report to finish analysis.",
                )
                .into());
            }
            let worker = worker(&tx, &job.request)?;
            let mut context = crate::handoff_context::capture(
                &tx,
                &crate::handoff::Identity {
                    track_id: job.request.track_id.clone(),
                    session_id: job.request.session_id.clone(),
                },
                crate::handoff_context::Boundary::Completed,
                &worker,
                key,
            )?;
            context["snapshotChanges"] = changes;
            key.mask(crate::secret_store::read(
                &tx,
                &job.request.track_id,
                &job.request.session_id,
            )?)?
            .redact_value(&mut context)?;
            if serde_json::to_vec(&context)?.len() > crate::handoff::CONTEXT_BYTES
                || context.get("_intercatTruncated").is_some()
            {
                return Err("The protected handoff input exceeds its limit.".into());
            }
            use sha2::{Digest, Sha256};
            let digest = Sha256::digest(serde_json::to_vec(&context)?);
            job.summary_id = Some(Uuid::new_v5(
                &job.files.as_ref().ok_or("Missing final files.")?.summary_id,
                &digest,
            ));
            job.summary_input = Some(context);
            job.phase = Phase::Summarizing;
            advance(&tx, &mut job)?;
            job
        }
        Change::Summarized {
            id,
            revision,
            document,
        } => {
            let mut job = expect(&tx, id, revision, &[Phase::Summarizing])?;
            source_boundary(&tx, &job, true)?;
            if crate::dispatch_control::paused(&tx)? {
                return Err(
                    crate::fresh::Waiting("Resume before accepting the handoff summary.").into(),
                );
            }
            document.fields.validate()?;
            let context = job
                .summary_input
                .as_ref()
                .ok_or("Missing frozen handoff input.")?;
            if document.fields.purpose.trim().is_empty()
                || document.source_addresses
                    != crate::handoff::addresses(context, &job.request.track_id)
            {
                return Err(
                    "The handoff summary does not match its original source addresses.".into(),
                );
            }
            let mut value = serde_json::to_value(document)?;
            key.mask(crate::secret_store::read(
                &tx,
                &job.request.track_id,
                &job.request.session_id,
            )?)?
            .redact_value(&mut value)?;
            job.summary = Some(serde_json::from_value(value)?);
            job.phase = Phase::SummaryReady;
            advance(&tx, &mut job)?;
            job
        }
        Change::Transfer { id, revision } => {
            let mut job = expect(&tx, id, revision, &[Phase::SummaryReady])?;
            source_boundary(&tx, &job, true)?;
            if crate::dispatch_control::paused(&tx)? {
                return Err(
                    crate::fresh::Waiting("Resume before transferring this workspace.").into(),
                );
            }
            crate::continuation::transfer(&tx, &job)?;
            job.phase = Phase::PublishingOwner;
            advance(&tx, &mut job)?;
            job
        }
        Change::OwnerPublished { id, revision } => {
            let mut job = expect(&tx, id, revision, &[Phase::PublishingOwner])?;
            original_scope(&tx, &job)?;
            let owner = crate::continuation::start(&tx, &job.successor_track)?
                .ok_or("Missing workspace ownership transfer.")?;
            if owner.operation != id
                || owner.source != job.request.track_id
                || owner.session_id != job.successor_session
            {
                return Err("The workspace ownership transfer changed.".into());
            }
            let raw: String = tx.query_row(
                "SELECT record FROM workspaces WHERE track_id=?1",
                [&job.successor_track],
                |row| row.get(0),
            )?;
            let mut workspace: Workspace = serde_json::from_str(&raw)?;
            if workspace.state != "fresh-publishing"
                || workspace.path != job.workspace.path
                || workspace.base != job.workspace.base
            {
                return Err("The new workspace reservation changed.".into());
            }
            workspace.state = "ready".into();
            tx.execute(
                "UPDATE workspaces SET record=?2 WHERE track_id=?1",
                params![workspace.track_id, serde_json::to_string(&workspace)?],
            )?;
            job.phase = Phase::Transferred;
            advance(&tx, &mut job)?;
            job
        }
        Change::RefreshFiles {
            id,
            revision,
            inspection,
        } => {
            let mut job = expect(&tx, id, revision, &[Phase::SummaryReady])?;
            source_boundary(&tx, &job, true)?;
            job.files = None;
            job.summary_id = None;
            job.summary_input = None;
            job.summary = None;
            job.access_consent = None;
            job.inspection = Some(inspection);
            job.phase = Phase::Inspecting;
            advance(&tx, &mut job)?;
            job
        }
        Change::Published { id, revision } => {
            let mut job = expect(&tx, id, revision, &[Phase::PublishingFiles])?;
            source_boundary(&tx, &job, true)?;
            if job.files.is_none() {
                return Err("Missing Start fresh file snapshot.".into());
            }
            job.phase = Phase::SnapshotReady;
            advance(&tx, &mut job)?;
            job
        }
        Change::FilesChanged {
            id,
            revision,
            inspection,
        } => {
            let mut job = expect(&tx, id, revision, &[Phase::CapturingFiles])?;
            job.access_consent = None;
            job.phase = if inspection.clear() {
                Phase::Inspecting
            } else {
                Phase::NeedsAccess
            };
            job.inspection = Some(inspection);
            advance(&tx, &mut job)?;
            job
        }
        Change::Retry {
            op_id,
            id,
            revision,
        } => {
            if op_id.get_version_num() != 7 {
                return Err("Retry needs a new operation ID.".into());
            }
            let payload = serde_json::json!({"id":id,"revision":revision}).to_string();
            let prior: Option<(String, String)> = tx
                .query_row(
                    "SELECT kind,payload FROM operations WHERE id=?1",
                    [op_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((kind, old)) = prior {
                if kind != "worker.fresh.retry" || old != payload {
                    return Err("Retry operation ID reused.".into());
                }
                read(&tx, id)?.ok_or("Missing Start fresh request.")?
            } else {
                let mut job = expect(
                    &tx,
                    id,
                    revision,
                    &[Phase::FilesHeld, Phase::SummaryHeld, Phase::OwnerHeld],
                )?;
                if job.phase != Phase::OwnerHeld {
                    source_boundary(&tx, &job, true)?;
                }
                job.phase = if job.phase == Phase::OwnerHeld {
                    Phase::PublishingOwner
                } else if job.phase == Phase::SummaryHeld {
                    Phase::Summarizing
                } else if job.files.is_some() {
                    Phase::PublishingFiles
                } else {
                    Phase::Inspecting
                };
                advance(&tx, &mut job)?;
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'worker.fresh.retry',?2,'complete')",
                    params![op_id.to_string(), payload],
                )?;
                job
            }
        }
        Change::Failed {
            id,
            revision,
            error,
        } => {
            let mut job = read(&tx, id)?.ok_or("Missing Start fresh request.")?;
            let error = crate::privacy::truncate(
                &key.mask(crate::secret_store::read(
                    &tx,
                    &job.request.track_id,
                    &job.request.session_id,
                )?)?
                .redact(&error),
                1024,
            );
            if job.revision == revision
                && !matches!(job.phase, Phase::Cancelled | Phase::Transferred)
                && job.error.as_deref() != Some(&error)
            {
                if job.phase == Phase::Cancelling {
                    // A cleanup error never makes cancelled work retryable as
                    // a file or summary operation. Keep its dispatch fence.
                } else if job.phase == Phase::PublishingOwner {
                    job.phase = Phase::OwnerHeld;
                    job.revision = job
                        .revision
                        .checked_add(1)
                        .ok_or("Start fresh revision overflow.")?;
                } else if job.phase == Phase::Summarizing {
                    job.phase = Phase::SummaryHeld;
                    job.revision = job
                        .revision
                        .checked_add(1)
                        .ok_or("Start fresh revision overflow.")?;
                } else if matches!(job.phase, Phase::CapturingFiles | Phase::PublishingFiles) {
                    job.phase = Phase::FilesHeld;
                    job.revision = job
                        .revision
                        .checked_add(1)
                        .ok_or("Start fresh revision overflow.")?;
                }
                job.error = Some(error);
                save(&tx, &job)?;
            }
            job
        }
        Change::Cancel {
            op_id,
            id,
            revision,
        } => {
            if op_id.get_version_num() != 7 {
                return Err("Cancellation needs a new operation ID.".into());
            }
            let mut job = read(&tx, id)?.ok_or("Missing Start fresh request.")?;
            let payload = serde_json::json!({"id":id,"revision":revision}).to_string();
            let prior: Option<(String, String)> = tx
                .query_row(
                    "SELECT kind,payload FROM operations WHERE id=?1",
                    [op_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((kind, old)) = prior {
                if kind != "worker.fresh.cancel" || old != payload {
                    return Err("Cancellation operation ID reused.".into());
                }
            } else {
                if job.revision != revision
                    || matches!(
                        job.phase,
                        Phase::Cancelled
                            | Phase::Cancelling
                            | Phase::PublishingOwner
                            | Phase::OwnerHeld
                            | Phase::Transferred
                    )
                {
                    return Err("Start fresh changed before cancellation.".into());
                }
                crate::journal::hold_inputs_at(
                    &tx,
                    &Uuid::new_v5(&op_id, b"fresh-cancel-hold").to_string(),
                    &job.request.track_id,
                    false,
                )?;
                job.phase = if job.summary_id.is_some()
                    || matches!(
                        job.phase,
                        Phase::Releasing | Phase::CapturingFiles | Phase::PublishingFiles
                    ) {
                    Phase::Cancelling
                } else {
                    Phase::Cancelled
                };
                job.revision = job
                    .revision
                    .checked_add(1)
                    .ok_or("Start fresh revision overflow.")?;
                save(&tx, &job)?;
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'worker.fresh.cancel',?2,?3)",
                    params![
                        op_id.to_string(),
                        payload,
                        if job.phase == Phase::Cancelled {
                            "complete"
                        } else {
                            "prepared"
                        }
                    ],
                )?;
            }
            job
        }
        Change::Cancelled { id, revision } => {
            let mut job = read(&tx, id)?.ok_or("Missing Start fresh request.")?;
            if job.phase != Phase::Cancelled {
                if job.phase != Phase::Cancelling || job.revision != revision {
                    return Err("Start fresh cancellation changed before cleanup finished.".into());
                }
                source_boundary(&tx, &job, true)?;
                job.phase = Phase::Cancelled;
                advance(&tx, &mut job)?;
                tx.execute("UPDATE operations SET state='complete' WHERE kind='worker.fresh.cancel' AND json_extract(payload,'$.id')=?1",[id.to_string()])?;
            }
            job
        }
    };
    tx.commit()?;
    Ok(job)
}

fn expect(db: &Connection, id: Uuid, revision: u64, phases: &[Phase]) -> Result<Job> {
    let job = read(db, id)?.ok_or("Missing Start fresh request.")?;
    if job.revision != revision || !phases.contains(&job.phase) {
        return Err("Start fresh changed before this action completed.".into());
    }
    Ok(job)
}
fn advance(db: &Connection, job: &mut Job) -> Result<()> {
    job.revision = job
        .revision
        .checked_add(1)
        .ok_or("Start fresh revision overflow.")?;
    job.error = None;
    save(db, job)
}
fn source_boundary(db: &Connection, job: &Job, released: bool) -> Result<()> {
    original_scope(db, job)?;
    let worker = worker(db, &job.request)?;
    if serde_json::to_value(workspace(db, &job.request.track_id)?)?
        != serde_json::to_value(&job.workspace)?
        || worker.active_turn.is_some()
        || worker.checkpoint_pending
        || !worker.approvals.is_empty()
        || !worker.prompts.is_empty()
        || serde_json::to_value(worker.checkpoint)?
            != serde_json::to_value(job.boundary.as_ref().map(|value| &value.checkpoint))?
        || (released && !matches!(worker.phase.as_str(), "sleeping" | "stopped"))
    {
        return Err(
            "Wait for this source's stopped, saved boundary before continuing Start fresh.".into(),
        );
    }
    Ok(())
}

fn original_scope(db: &Connection, job: &Job) -> Result<()> {
    let session: String = db.query_row(
        "SELECT json_extract(value,'$.id') FROM settings WHERE key='session'",
        [],
        |row| row.get(0),
    )?;
    let slug: Option<String> = db
        .query_row(
            "SELECT slug FROM session_folders WHERE session=?1 AND folder=?2",
            params![job.session, job.folder.id],
            |row| row.get(0),
        )
        .optional()?;
    if session != job.session
        || slug.as_deref() != Some(&job.slug)
        || serde_json::to_value(crate::folders::track(db, &job.request.track_id)?)?
            != serde_json::to_value(&job.folder)?
    {
        return Err(
            "Return to the original session and folder before continuing Start fresh.".into(),
        );
    }
    Ok(())
}
