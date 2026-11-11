//! An explicit realignment owns its file boundary until its checkpoint and
//! worker state are saved. It never borrows an ordinary worker input.
use crate::{
    Result,
    checkpoint::Checkpoint,
    lineage::{Ancestors, Basis, Frozen},
    preparation::Workspace,
    realign::{Plan, Request},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Application {
    pub operation: Uuid,
    pub request: Request,
    pub workspace: Workspace,
    pub session: String,
    pub previous: Option<Checkpoint>,
    pub target_ref: String,
    pub plan: Plan,
    pub after: String,
    pub expected_base: Option<String>,
    pub expected_head: String,
    pub checkpoint_turn: Uuid,
}
pub(crate) struct Context {
    pub common_directory: std::path::PathBuf,
    pub workspace: Workspace,
    pub session: String,
    pub previous: Option<Checkpoint>,
    pub target_ref: String,
    pub target: String,
    pub origin: Origin,
    pub basis: Basis,
    pub graph: Frozen,
    pub saved: Option<(Application, String)>,
    pub merge: Option<crate::realign_merge::Frozen>,
    pub resolutions: Vec<crate::merge_choices::Solved>,
    pub source: crate::integration::Job,
    pub label: String,
}
pub(crate) enum Origin {
    Commit(Ancestors),
    Reparent {
        coverage: Ancestors,
        input: Box<crate::realign::Reparent>,
    },
    Shared(crate::shared_context_store::Request),
}
impl Origin {
    pub(crate) fn coverage(&self) -> Option<&Ancestors> {
        match self {
            Self::Commit(coverage) | Self::Reparent { coverage, .. } => Some(coverage),
            Self::Shared(_) => None,
        }
    }
}
pub(crate) enum Change {
    FreezeMerge(Box<crate::realign_merge::Frozen>),
    MergeReady(Uuid),
    BeginMergeResult(Box<crate::realign_merge_store::Applied>),
    CompleteMergeResult(Uuid),
    Begin(Box<Application>),
    Checkpoint {
        operation: Uuid,
        checkpoint: Checkpoint,
        lfs_directory: std::path::PathBuf,
    },
    Complete(Uuid),
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE INDEX IF NOT EXISTS pending_realignments ON operations(json_extract(payload,'$.request.trackId')) WHERE kind='workspace.realign' AND state<>'complete';")?;
    db.execute_batch("CREATE INDEX IF NOT EXISTS realign_access_approvals ON operations(json_extract(payload,'$.request.opId'),json_extract(payload,'$.fingerprint')) WHERE kind='realign.access' AND state='complete';")?;
    Ok(())
}
pub(crate) fn approve_access(
    db: &mut Connection,
    consent: &crate::workspace_access::Consent,
) -> Result<()> {
    let tx = db.transaction()?;
    save_access(&tx, consent)?;
    tx.commit()?;
    Ok(())
}
pub(crate) fn save_access(
    db: &Connection,
    consent: &crate::workspace_access::Consent,
) -> Result<()> {
    let payload = serde_json::to_value(consent)?;
    let old: Option<(String, String, String)> = db
        .query_row(
            "SELECT kind,payload,state FROM operations WHERE id=?1",
            [consent.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((kind, raw, state)) = old {
        if kind != "realign.access"
            || serde_json::from_str::<Value>(&raw)? != payload
            || state != "complete"
        {
            return Err("Access confirmation operation ID reused.".into());
        }
    } else {
        if consent.op_id.get_version_num() != 7
            || consent.fingerprint.len() != 64
            || !consent
                .fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("Invalid file-access confirmation.".into());
        }
        request_context(db, &consent.request)?;
        db.execute(
            "INSERT INTO operations VALUES(?1,'realign.access',?2,'complete')",
            params![consent.op_id.to_string(), payload.to_string()],
        )?;
        event(
            db,
            "realign.access-approved",
            json!({"operation":consent.op_id,"realignment":consent.request.op_id,"trackId":consent.request.track_id}),
        )?;
    }
    Ok(())
}
pub(crate) fn access_allowed(
    db: &Connection,
    request: &Request,
    fingerprint: &str,
) -> Result<bool> {
    let mut query=db.prepare("SELECT payload FROM operations WHERE kind='realign.access' AND state='complete' AND json_extract(payload,'$.request.opId')=?1 AND json_extract(payload,'$.fingerprint')=?2")?;
    for row in query.query_map(params![request.op_id.to_string(), fingerprint], |row| {
        row.get::<_, String>(0)
    })? {
        let consent: crate::workspace_access::Consent = serde_json::from_str(&row?)?;
        if consent.request == *request {
            return Ok(true);
        }
    }
    Ok(false)
}
pub(crate) fn pending(db: &Connection, track: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind='workspace.realign' AND state<>'complete' AND json_extract(payload,'$.request.trackId')=?1)", [track], |row| row.get(0))?)
}
fn read(db: &Connection, operation: Uuid) -> Result<Option<(Application, String)>> {
    let row: Option<(String, String, String)> = db
        .query_row(
            "SELECT kind,payload,state FROM operations WHERE id=?1",
            [operation.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(kind, raw, state)| {
        if kind != "workspace.realign" {
            return Err("Operation ID reused.".into());
        }
        Ok((serde_json::from_str(&raw)?, state))
    })
    .transpose()
}
fn worker(db: &Connection, track: &str) -> Result<crate::runtime::WorkerState> {
    let raw: String = db.query_row(
        "SELECT record FROM workers WHERE track_id=?1",
        [track],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&raw)?)
}
pub(crate) fn context(db: &Connection, request: &Request) -> Result<Context> {
    context_at(db, request, true)
}
pub(crate) fn request_context(db: &Connection, request: &Request) -> Result<Context> {
    context_at(db, request, false)
}
fn context_at(db: &Connection, request: &Request, idle: bool) -> Result<Context> {
    crate::continuation::ensure_writer(db, &request.track_id)?;
    if crate::fresh_store::holds(db, &request.track_id)? {
        return Err("Cancel Start fresh before realigning this workspace.".into());
    }
    if request.op_id.get_version_num() != 7 {
        return Err("Invalid realignment operation.".into());
    }
    let raw: String = db.query_row(
        "SELECT record FROM workspaces WHERE track_id=?1",
        [&request.track_id],
        |row| row.get(0),
    )?;
    let workspace: Workspace = serde_json::from_str(&raw)?;
    if workspace.state != "ready" {
        return Err("The worker workspace is not ready.".into());
    }
    if crate::path_retirement::holds(db, &request.track_id)? {
        return Err("Restore this path before realigning it.".into());
    }
    let worker = worker(db, &request.track_id)?;
    let session = worker
        .session_id
        .clone()
        .ok_or("Connect this worker before realigning it.")?;
    if idle
        && (worker.active_turn.is_some()
            || worker.checkpoint_pending
            || !worker.approvals.is_empty()
            || !worker.prompts.is_empty())
    {
        return Err("Wait for this worker's turn and checkpoint before realigning it.".into());
    }
    let saved = read(db, crate::realign::file_operation(request))?;
    if let Some((app, _)) = &saved
        && (&app.request != request
            || app.workspace.path != workspace.path
            || app.workspace.reference != workspace.reference
            || app.session != session)
    {
        return Err("The saved realignment belongs to another request or worker session.".into());
    }
    let (job_id, revision) = request.source.review();
    let job = crate::integration_store::read(db, job_id)?;
    match &request.source {
        crate::realign::Source::Commit { .. } if job.revision != revision => {
            return Err("The completed review changed. Review its realignment again.".into());
        }
        crate::realign::Source::Reparent { .. } => {
            crate::reparent_store::validate_review(db, &job, request)?;
        }
        crate::realign::Source::Commit { .. } => {}
    }
    if crate::folders::track(db, &request.track_id)?.id() != job.project.id()
        || !workspace
            .reference
            .starts_with(&format!("refs/intercat/{}/tracks/", job.slug))
    {
        return Err("This review belongs to another folder or worker session.".into());
    }
    let graph = crate::lineage_store::freeze(db)?;
    let (target_ref, target, origin) = match &request.source {
        crate::realign::Source::Commit { .. } => {
            if job.status != crate::integration::Status::Done {
                return Err("Choose a completed Commit before realigning this worker.".into());
            }
            let plan = job.plan.as_ref().ok_or("Missing committed plan.")?;
            let candidate = job.candidate.as_ref().ok_or("Missing committed result.")?;
            let (target, coverage): (String, String) = db.query_row(
                "SELECT head,coverage FROM published_lineage WHERE reference=?1",
                [&plan.target_ref],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if target != candidate.head && saved.is_none() {
                return Err("A newer Commit is available. Review its realignment instead.".into());
            }
            (
                plan.target_ref.clone(),
                target,
                Origin::Commit(serde_json::from_str(&coverage)?),
            )
        }
        crate::realign::Source::Reparent { parent, .. } => {
            if idle
                && matches!(
                    job.status,
                    crate::integration::Status::Preparing
                        | crate::integration::Status::Building
                        | crate::integration::Status::Staging
                )
            {
                return Err(Box::new(crate::realign::Waiting));
            }
            let source = crate::reparent_store::capture(
                db,
                &job,
                parent,
                &worker,
                &graph,
                saved.as_ref().map(|(app, _)| app),
            )?;
            (source.reference, source.sha, source.origin)
        }
    };
    let queued = crate::realign_queue::read(db, request.op_id)?;
    let merge = queued
        .as_ref()
        .and_then(|job| job.merge_boundary)
        .map(|id| crate::realign_merge_store::read(db, id))
        .transpose()?;
    Ok(Context {
        label: db.query_row(
            "SELECT label FROM tracks WHERE id=?1",
            [&request.track_id],
            |row| row.get(0),
        )?,
        merge,
        resolutions: queued.map(|job| job.resolutions).unwrap_or_default(),
        common_directory: job.project.common_dir.clone(),
        source: job,
        workspace,
        session,
        previous: worker.checkpoint,
        target_ref,
        target,
        origin,
        basis: crate::lineage_store::basis(db, &request.track_id)?,
        graph,
        saved,
    })
}
fn event(db: &Connection, kind: &str, value: Value) -> Result<()> {
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,?2,?3)",
        params![crate::journal::timestamp(), kind, value.to_string()],
    )?;
    Ok(())
}
pub(crate) fn change(db: &mut Connection, change: Change) -> Result<Option<Checkpoint>> {
    let tx = db.transaction()?;
    let result = match change {
        Change::FreezeMerge(input) => {
            crate::realign_merge_store::freeze(&tx, &input)?;
            None
        }
        Change::MergeReady(id) => {
            crate::realign_merge_store::ready(&tx, id)?;
            None
        }
        Change::BeginMergeResult(result) => {
            crate::realign_merge_store::begin_result(&tx, &result)?;
            None
        }
        Change::CompleteMergeResult(id) => {
            crate::realign_merge_store::complete_result(&tx, id)?;
            None
        }
        Change::Begin(app) => {
            if app.operation != crate::realign::file_operation(&app.request) {
                return Err("Invalid realignment file operation.".into());
            }
            if let Some((saved, _)) = read(&tx, app.operation)? {
                if serde_json::to_value(&saved)? != serde_json::to_value(&app)? {
                    return Err("Realignment operation ID reused.".into());
                }
            } else {
                let current = context(&tx, &app.request)?;
                if !crate::realign_queue::can_apply(&tx, &app.request)?
                    || pending(&tx, &app.request.track_id)?
                    || crate::inheritance_apply_store::pending(&tx, &app.request.track_id)?
                        .is_some()
                    || current.basis != app.plan.before_basis
                    || current.target != app.plan.target
                    || current.origin.coverage() != Some(&app.plan.basis.base_inherits)
                    || current.session != app.session
                    || serde_json::to_value(current.previous)?
                        != serde_json::to_value(&app.previous)?
                    || serde_json::to_value(current.workspace)?
                        != serde_json::to_value(&app.workspace)?
                    || current.target_ref != app.target_ref
                    || app.plan.track != app.request.track_id
                {
                    return Err("The worker or realignment target changed. Review again.".into());
                }
                // A saved review must not silently survive a different worker
                // checkpoint. Its original inputs stay available for Refresh.
                let mut reviews = tx.prepare("SELECT id FROM integration_jobs WHERE active=1")?;
                let ids = reviews
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(reviews);
                for id in ids {
                    let mut job = crate::integration_store::read(&tx, id.parse()?)?;
                    if matches!(app.request.source, crate::realign::Source::Reparent { .. })
                        && job.id == current.source.id
                    {
                        continue;
                    }
                    if job.project.id() != crate::folders::track(&tx, &app.request.track_id)?.id()
                        || job.session_id != current.source.session_id
                    {
                        continue;
                    }
                    if matches!(
                        job.status,
                        crate::integration::Status::Committing
                            | crate::integration::Status::CommitBlocked
                    ) {
                        return Err("Finish the saved Commit before realigning this worker.".into());
                    }
                    let repair: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM repair_runs WHERE json_extract(record,'$.owner.kind')='commit' AND json_extract(record,'$.owner.id')=?1 AND json_extract(record,'$.phase') NOT IN ('done','discarded'))", [job.id.to_string()], |row| row.get(0))?;
                    if repair {
                        return Err(
                            "Finish or discard this review's repair before realigning the worker."
                                .into(),
                        );
                    }
                    let label: String = tx.query_row(
                        "SELECT label FROM tracks WHERE id=?1",
                        [&app.request.track_id],
                        |row| row.get(0),
                    )?;
                    job.revision = job
                        .revision
                        .checked_add(1)
                        .ok_or("Review revision overflow.")?;
                    job.status = crate::integration::Status::Failed;
                    job.error = Some(format!("{label} was realigned · Refresh"));
                    job.candidate = None;
                    job.kept_secrets.clear();
                    job.publication = None;
                    crate::integration_store::save(&tx, &job)?;
                }
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'workspace.realign',?2,'prepared')",
                    params![app.operation.to_string(), serde_json::to_string(&app)?],
                )?;
                event(
                    &tx,
                    "realign.prepared",
                    json!({"operation":app.operation,"realignment":app.request.op_id,"trackId":app.request.track_id}),
                )?;
            }
            None
        }
        Change::Checkpoint {
            operation,
            checkpoint,
            lfs_directory,
        } => {
            let (app, state) = read(&tx, operation)?.ok_or("Missing realignment.")?;
            if state != "dispatched" || checkpoint.turn_id != app.checkpoint_turn {
                return Err("The realigned checkpoint cannot be saved in this state.".into());
            }
            let id = Uuid::new_v5(&checkpoint.turn_id, b"checkpoint").to_string();
            let saved: Option<String> = tx
                .query_row("SELECT payload FROM operations WHERE id=?1", [&id], |row| {
                    row.get(0)
                })
                .optional()?;
            if let Some(saved) = saved {
                Some(serde_json::from_value(
                    serde_json::from_str::<Value>(&saved)?["checkpoint"].clone(),
                )?)
            } else {
                if crate::lineage_store::basis(&tx, &app.request.track_id)? != app.plan.before_basis
                {
                    return Err("The worker base changed during realignment.".into());
                }
                crate::lineage_store::set_basis(&tx, &app.request.track_id, &app.plan.basis)?;
                let payload = json!({"trackId":app.request.track_id,"checkpoint":checkpoint,"latestRef":format!("{}-checkpoint",app.workspace.reference),"expected":app.previous.as_ref().map(|p|&p.sha),"lfsDirectory":lfs_directory});
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'checkpoint.save',?2,'prepared')",
                    params![id, payload.to_string()],
                )?;
                crate::lineage_store::reserve(
                    &tx,
                    &id,
                    payload["trackId"]
                        .as_str()
                        .ok_or("Missing checkpoint track.")?,
                    payload["checkpoint"]["sha"]
                        .as_str()
                        .ok_or("Missing checkpoint tree.")?,
                )?;
                Some(checkpoint)
            }
        }
        Change::Complete(operation) => {
            let (app, state) = read(&tx, operation)?.ok_or("Missing realignment.")?;
            if state != "complete" {
                let checkpoint = Uuid::new_v5(&app.checkpoint_turn, b"checkpoint").to_string();
                let raw: String = tx.query_row(
                    "SELECT payload FROM operations WHERE id=?1 AND state='complete'",
                    [checkpoint],
                    |row| row.get(0),
                )?;
                let saved: Value = serde_json::from_str(&raw)?;
                let worker = worker(&tx, &app.request.track_id)?;
                if worker.session_id.as_deref() != Some(&app.session)
                    || worker.active_turn.is_some()
                    || worker.checkpoint_pending
                    || worker.checkpoint.as_ref().map(|cp| cp.sha.as_str())
                        != saved["checkpoint"]["sha"].as_str()
                {
                    return Err(
                        "Save the realigned worker checkpoint before starting another turn.".into(),
                    );
                }
                tx.execute(
                    "UPDATE operations SET state='complete' WHERE id=?1",
                    [operation.to_string()],
                )?;
                let checkpoint: Checkpoint = serde_json::from_value(saved["checkpoint"].clone())?;
                crate::reparent_store::complete(&tx, &app, &checkpoint)?;
                event(
                    &tx,
                    "realign.complete",
                    json!({"operation":operation,"trackId":app.request.track_id}),
                )?;
            }
            None
        }
    };
    tx.commit()?;
    Ok(result)
}
pub(crate) fn snapshot(db: &Connection) -> Result<Vec<Value>> {
    let mut query = db.prepare("SELECT id,payload,state FROM operations WHERE kind='workspace.realign' AND state<>'complete' ORDER BY rowid")?;
    query
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .map(|row| {
            let (id, raw, state) = row?;
            let app: Application = serde_json::from_str(&raw)?;
            Ok(json!({"operationId":id,"request":app.request,"state":state}))
        })
        .collect()
}
