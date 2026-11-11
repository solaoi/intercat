//! Restore owns its stopped path until files, lineage and checkpoint are saved.
//! Replaying an operation never captures a different source or another head.
use crate::{
    Result,
    checkpoint::Checkpoint,
    path_restore::{Job, Phase, Request, Restored},
    path_restore_source::Context,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Files {
    pub prepared: crate::path_restore::Prepared,
    pub after: String,
    pub anchor: Anchor,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Anchor {
    pub expected_head: String,
    pub expected_base: Option<String>,
    pub checkpoint_turn: Uuid,
}
impl Anchor {
    pub(crate) fn validate(&self, request: &Request) -> Result<()> {
        if self.checkpoint_turn
            != Uuid::new_v5(
                &crate::path_restore::file_operation(request),
                b"restored-checkpoint",
            )
        {
            return Err("Invalid restored checkpoint identity.".into());
        }
        self.expected_head.parse::<intercat_git::ObjectId>()?;
        if let Some(base) = &self.expected_base {
            base.parse::<intercat_git::ObjectId>()?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum Plan {
    Reopen,
    Files(Box<Files>),
    Conflict {
        inputs: Box<crate::path_restore::Inputs>,
        conflict: Box<crate::path_restore::Conflict>,
        anchor: Anchor,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Application {
    pub request: Request,
    pub source: Context,
    pub plan: Plan,
}
pub(crate) enum Change {
    Request(Request),
    Control(crate::path_restore::Control),
    ApproveAccess(crate::path_restore::AccessConsent),
    Begin(Box<Application>),
    Checkpoint {
        operation: Uuid,
        checkpoint: Checkpoint,
        lfs_directory: std::path::PathBuf,
    },
    Complete(Uuid),
    Progress {
        request: Request,
        phase: Phase,
        error: Option<String>,
        inspection: Option<crate::workspace_access::Inspection>,
    },
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE INDEX path_restore_requests ON operations(json_extract(payload,'$.request.retirementId')) WHERE kind='path.restore' AND state<>'complete';
        CREATE INDEX path_restore_files ON operations(json_extract(payload,'$.source.entry.track')) WHERE kind='workspace.restorePath' AND state<>'complete';")?;
    Ok(())
}
pub(crate) fn read(db: &Connection, request: &Request) -> Result<Job> {
    let (kind, raw): (String, String) = db.query_row(
        "SELECT kind,payload FROM operations WHERE id=?1",
        [request.op_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if kind != "path.restore" {
        return Err("Restore operation ID reused.".into());
    }
    let job: Job = serde_json::from_str(&raw)?;
    if job.request != *request {
        return Err("Restore operation ID reused.".into());
    }
    Ok(job)
}
pub(crate) fn read_id(db: &Connection, id: Uuid) -> Result<Job> {
    let raw: String = db.query_row(
        "SELECT payload FROM operations WHERE id=?1 AND kind='path.restore'",
        [id.to_string()],
        |row| row.get(0),
    )?;
    let job: Job = serde_json::from_str(&raw)?;
    if job.request.op_id != id {
        return Err("Restore operation ID reused.".into());
    }
    Ok(job)
}
pub(crate) fn application(
    db: &Connection,
    request: &Request,
) -> Result<Option<(Application, String)>> {
    let old: Option<(String, String, String)> = db
        .query_row(
            "SELECT kind,payload,state FROM operations WHERE id=?1",
            [crate::path_restore::file_operation(request).to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    old.map(|(kind, raw, state)| {
        if kind != "workspace.restorePath" {
            return Err("Restore file operation ID reused.".into());
        }
        let app: Application = serde_json::from_str(&raw)?;
        if app.request != *request {
            return Err("Restore file operation ID reused.".into());
        }
        Ok((app, state))
    })
    .transpose()
}
pub(crate) fn snapshot(db: &Connection) -> Result<Vec<Job>> {
    db.prepare("SELECT payload FROM operations WHERE kind='path.restore' ORDER BY rowid")?
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|raw| Ok(serde_json::from_str(&raw?)?))
        .collect()
}
pub(crate) fn owns_folder(db: &Connection, session: &str, folder: &str) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind='workspace.restorePath' AND state<>'complete' AND json_extract(payload,'$.source.entry.session')=?1 AND json_extract(payload,'$.source.entry.folder')=?2)", params![session,folder], |row| row.get(0))?)
}
pub(crate) fn access_allowed(
    db: &Connection,
    request: &Request,
    fingerprint: &str,
) -> Result<bool> {
    let mut query=db.prepare("SELECT payload FROM operations WHERE kind='path.restore-access' AND state='complete' AND json_extract(payload,'$.request.opId')=?1 AND json_extract(payload,'$.fingerprint')=?2")?;
    for raw in query.query_map(params![request.op_id.to_string(), fingerprint], |row| {
        row.get::<_, String>(0)
    })? {
        let consent: crate::path_restore::AccessConsent = serde_json::from_str(&raw?)?;
        if consent.request == *request {
            return Ok(true);
        }
    }
    Ok(false)
}
fn event(db: &Connection, kind: &str, value: Value) -> Result<()> {
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,?2,?3)",
        params![crate::journal::timestamp(), kind, value.to_string()],
    )?;
    Ok(())
}
pub(crate) fn save_job(db: &Connection, job: &mut Job) -> Result<()> {
    job.revision = job
        .revision
        .checked_add(1)
        .ok_or("Restore revision overflow.")?;
    db.execute(
        "UPDATE operations SET payload=?2,state=?3 WHERE id=?1 AND kind='path.restore'",
        params![
            job.request.op_id.to_string(),
            serde_json::to_string(job)?,
            if job.phase == Phase::Done {
                "complete"
            } else {
                "prepared"
            }
        ],
    )?;
    event(
        db,
        "path.restore-progress",
        json!({"request":job.request,"trackId":job.track_id,"phase":job.phase,"revision":job.revision}),
    )
}
fn worker(db: &Connection, source: &Context) -> Result<Option<crate::runtime::WorkerState>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM workers WHERE track_id=?1",
            [&source.entry.track],
            |row| row.get(0),
        )
        .optional()?;
    let worker = raw
        .map(|raw| serde_json::from_str::<crate::runtime::WorkerState>(&raw))
        .transpose()?;
    let saved = source
        .entry
        .saved
        .as_ref()
        .ok_or("Missing retired path boundary.")?;
    if let Some(worker) = &worker {
        if worker.session_id != saved.session_id
            || worker.active_turn.is_some()
            || worker.checkpoint_pending
            || !worker.prompts.is_empty()
            || !worker.approvals.is_empty()
        {
            return Err("Wait for the original worker's stopped boundary before Restore.".into());
        }
    } else if saved.session_id.is_some() {
        return Err("The original worker session is unavailable.".into());
    }
    Ok(worker)
}
fn invalidate_reviews(db: &Connection, source: &Context) -> Result<()> {
    let ids = db
        .prepare("SELECT id FROM integration_jobs WHERE active=1")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let mut job = crate::integration_store::read(db, id.parse()?)?;
        if job.session_id != source.entry.session || job.project.id != source.entry.folder {
            continue;
        }
        if matches!(
            job.status,
            crate::integration::Status::Committing | crate::integration::Status::CommitBlocked
        ) {
            return Err("Finish the saved Commit before restoring this path.".into());
        }
        let repair:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM repair_runs WHERE json_extract(record,'$.owner.kind')='commit' AND json_extract(record,'$.owner.id')=?1 AND json_extract(record,'$.phase') NOT IN ('done','discarded'))",[job.id.to_string()],|row|row.get(0))?;
        if repair {
            return Err("Finish or discard the review's repair before Restore.".into());
        }
        job.revision = job
            .revision
            .checked_add(1)
            .ok_or("Review revision overflow.")?;
        job.status = crate::integration::Status::Failed;
        job.error = Some(format!("{} was restored · Refresh", source.entry.label));
        job.candidate = None;
        job.kept_secrets.clear();
        job.publication = None;
        crate::integration_store::save(db, &job)?;
    }
    Ok(())
}
pub(crate) fn change(db: &mut Connection, change: Change) -> Result<Job> {
    let tx = db.transaction()?;
    let job = match change {
        Change::Control(control) => crate::path_restore_control::apply(&tx, control)?,
        Change::ApproveAccess(consent) => {
            if consent.op_id.get_version_num() != 7
                || consent.fingerprint.len() != 64
                || !consent
                    .fingerprint
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            {
                return Err("Invalid Restore file-access confirmation.".into());
            }
            let mut job = read(&tx, &consent.request)?;
            let old: Option<(String, String, String)> = tx
                .query_row(
                    "SELECT kind,payload,state FROM operations WHERE id=?1",
                    [consent.op_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((kind, raw, state)) = old {
                if kind != "path.restore-access"
                    || state != "complete"
                    || serde_json::from_str::<crate::path_restore::AccessConsent>(&raw)? != consent
                {
                    return Err("Restore access operation ID reused.".into());
                }
            } else {
                if job.phase == Phase::Done {
                    return Err("This Restore already finished.".into());
                }
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'path.restore-access',?2,'complete')",
                    params![consent.op_id.to_string(), serde_json::to_string(&consent)?],
                )?;
                if job.phase == Phase::WaitingForAccess {
                    job.phase = Phase::Queued;
                    job.error = None;
                    job.inspection = None;
                    save_job(&tx, &mut job)?;
                }
            }
            job
        }
        Change::Request(request) => {
            if request.op_id.get_version_num() != 7 {
                return Err("Restore needs a version 7 operation ID.".into());
            }
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1)",
                [request.op_id.to_string()],
                |row| row.get(0),
            )?;
            if exists {
                read(&tx, &request)?
            } else {
                let source = crate::path_restore_source::capture(&tx, request.retirement_id)?;
                let busy:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind='path.restore' AND state<>'complete' AND json_extract(payload,'$.request.retirementId')=?1)",[request.retirement_id.to_string()],|row|row.get(0))?;
                if busy {
                    return Err("A Restore already owns this set-aside path.".into());
                }
                let job = Job {
                    request,
                    track_id: source.entry.track,
                    session_id: source.entry.session,
                    folder_id: source.entry.folder,
                    revision: 1,
                    phase: Phase::Queued,
                    keep_processes: false,
                    error: None,
                    inspection: None,
                    restored: None,
                    merge_input: None,
                };
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'path.restore',?2,'prepared')",
                    params![job.request.op_id.to_string(), serde_json::to_string(&job)?],
                )?;
                event(
                    &tx,
                    "path.restore-requested",
                    json!({"request":job.request,"trackId":job.track_id}),
                )?;
                job
            }
        }
        Change::Begin(app) => {
            let mut job = read(&tx, &app.request)?;
            if let Some((old, _)) = application(&tx, &app.request)? {
                if serde_json::to_value(&old)? != serde_json::to_value(&app)? {
                    return Err("Restore file operation ID reused.".into());
                }
            } else {
                if matches!(job.phase, Phase::Done | Phase::Held) {
                    return Err("This Restore is not ready to apply files.".into());
                }
                let source = crate::path_restore_source::capture(&tx, app.request.retirement_id)?;
                if crate::aside_processes::pending(&tx, app.request.retirement_id)? {
                    return Err(crate::path_restore::Waiting(
                        "Waiting for this path's processes to stop.",
                    )
                    .into());
                }
                if serde_json::to_value(&source)? != serde_json::to_value(&app.source)? {
                    return Err(
                        "The Restore source changed before its file intent was saved.".into(),
                    );
                }
                let previous = worker(&tx, &source)?;
                if let Some(worker) = &previous
                    && serde_json::to_value(&worker.checkpoint)?
                        != serde_json::to_value(&source.entry.saved.as_ref().unwrap().checkpoint)?
                {
                    return Err("The stopped worker checkpoint changed before Restore.".into());
                }
                if owns_folder(&tx, &source.entry.session, &source.entry.folder)? {
                    return Err(crate::path_restore::Waiting(
                        "Waiting for the other Restore in this folder.",
                    )
                    .into());
                }
                invalidate_reviews(&tx, &source)?;
                let (phase, state) = match &app.plan {
                    Plan::Reopen if source.cancelled.is_empty() => (Phase::Applying, "prepared"),
                    Plan::Files(files) if !source.cancelled.is_empty() => {
                        validate_inputs(&source, &files.prepared.inputs)?;
                        files.anchor.validate(&app.request)?;
                        (Phase::Applying, "prepared")
                    }
                    Plan::Conflict { inputs, anchor, .. } if !source.cancelled.is_empty() => {
                        validate_inputs(&source, inputs)?;
                        anchor.validate(&app.request)?;
                        (Phase::NeedsMerge, "prepared")
                    }
                    _ => {
                        return Err(
                            "The Restore plan does not match its cancelled publications.".into(),
                        );
                    }
                };
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'workspace.restorePath',?2,?3)",
                    params![
                        crate::path_restore::file_operation(&app.request).to_string(),
                        serde_json::to_string(&app)?,
                        state
                    ],
                )?;
                job.phase = phase;
                job.error = None;
                job.inspection = None;
                save_job(&tx, &mut job)?;
            }
            job
        }
        Change::Progress {
            request,
            phase,
            error,
            inspection,
        } => {
            let mut job = read(&tx, &request)?;
            if !matches!(job.phase, Phase::Done | Phase::Held) {
                if phase == Phase::Done {
                    return Err("Save the restored path before completing Restore.".into());
                }
                if job.phase != phase
                    || job.error != error
                    || serde_json::to_value(&job.inspection)? != serde_json::to_value(&inspection)?
                {
                    job.phase = phase;
                    job.error = error;
                    job.inspection = inspection;
                    save_job(&tx, &mut job)?;
                }
            }
            job
        }
        Change::Checkpoint {
            operation,
            checkpoint,
            lfs_directory,
        } => {
            let (app, state) = read_application(&tx, operation)?;
            let Plan::Files(files) = &app.plan else {
                return Err("This Restore has no file checkpoint.".into());
            };
            if state != "dispatched" || checkpoint.turn_id != files.anchor.checkpoint_turn {
                return Err("The Restore checkpoint cannot be saved in this state.".into());
            }
            let id = Uuid::new_v5(&checkpoint.turn_id, b"checkpoint").to_string();
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1)",
                [&id],
                |row| row.get(0),
            )?;
            if !exists {
                if crate::lineage_store::basis(&tx, &app.source.entry.track)? != app.source.basis {
                    return Err("The track's baseline changed during Restore.".into());
                }
                crate::lineage_store::set_basis(
                    &tx,
                    &app.source.entry.track,
                    &files.prepared.basis,
                )?;
                let payload = json!({"trackId":app.source.entry.track,"checkpoint":checkpoint,"latestRef":format!("{}-checkpoint",app.source.entry.workspace.reference),"expected":app.source.entry.saved.as_ref().unwrap().checkpoint.as_ref().map(|cp|&cp.sha),"lfsDirectory":lfs_directory});
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
            }
            read(&tx, &app.request)?
        }
        Change::Complete(operation) => {
            let (app, state) = read_application(&tx, operation)?;
            let mut job = read(&tx, &app.request)?;
            if state != "complete" {
                if crate::aside_processes::pending(&tx, app.request.retirement_id)? {
                    return Err(crate::path_restore::Waiting(
                        "Waiting for this path's processes to stop.",
                    )
                    .into());
                }
                let worker = worker(&tx, &app.source)?;
                let mut entry = crate::path_retirement::read(&tx, app.source.entry.id)?;
                if serde_json::to_value(&entry)? != serde_json::to_value(&app.source.entry)? {
                    return Err("The retired path changed during Restore.".into());
                }
                let target =
                    crate::lineage_store::read_published(&tx, &job.session_id, &job.folder_id)?
                        .ok_or("The Restore target has no published coverage.")?
                        .head;
                if target != app.source.target {
                    return Err("The published target changed during Restore.".into());
                }
                let (basis, checkpoint, suggest_realign) = match &app.plan {
                    Plan::Reopen => {
                        if crate::lineage_store::basis(&tx, &entry.track)? != app.source.basis {
                            return Err("The set-aside track's baseline changed.".into());
                        }
                        (
                            app.source.basis.clone(),
                            None,
                            app.source.basis.base != app.source.target,
                        )
                    }
                    Plan::Files(files) => {
                        let raw:String=tx.query_row("SELECT payload FROM operations WHERE id=?1 AND kind='checkpoint.save' AND state='complete'",[Uuid::new_v5(&files.anchor.checkpoint_turn,b"checkpoint").to_string()],|row|row.get(0))?;
                        let checkpoint: Checkpoint = serde_json::from_value(
                            serde_json::from_str::<Value>(&raw)?["checkpoint"].clone(),
                        )?;
                        if crate::lineage_store::basis(&tx, &entry.track)? != files.prepared.basis
                            || worker.as_ref().is_some_and(|worker| {
                                worker.checkpoint.as_ref().map(|cp| &cp.sha)
                                    != Some(&checkpoint.sha)
                            })
                        {
                            return Err("Save the original worker's restored checkpoint before releasing this path.".into());
                        }
                        let raw: Option<String> = tx
                            .query_row(
                                "SELECT record FROM integrated_lineage WHERE track=?1",
                                [&entry.track],
                                |row| row.get(0),
                            )
                            .optional()?;
                        let integrated = raw
                            .map(|raw| serde_json::from_str::<crate::lineage::Inherited>(&raw))
                            .transpose()?;
                        if integrated != app.source.graph.integrated.get(&entry.track).cloned() {
                            return Err(
                                "The track's publication cursor changed during Restore.".into()
                            );
                        }
                        if let Some(mark) = &files.prepared.integrated {
                            tx.execute("INSERT INTO integrated_lineage VALUES(?1,?2) ON CONFLICT(track) DO UPDATE SET record=excluded.record",params![entry.track,serde_json::to_string(mark)?])?;
                        } else {
                            tx.execute(
                                "DELETE FROM integrated_lineage WHERE track=?1",
                                [&entry.track],
                            )?;
                        }
                        (files.prepared.basis.clone(), Some(checkpoint), false)
                    }
                    Plan::Conflict { .. } => {
                        return Err(
                            "Resolve the Restore conflict before releasing this path.".into()
                        );
                    }
                };
                entry.phase = crate::path_retirement::Phase::Released;
                entry.error = None;
                crate::path_retirement::save(&tx, &entry)?;
                job.phase = Phase::Done;
                job.error = None;
                job.inspection = None;
                job.restored = Some(Restored {
                    track_id: entry.track.clone(),
                    session_id: entry.saved.as_ref().unwrap().session_id.clone(),
                    previous_base: app.source.basis.base.clone(),
                    base: basis.base.clone(),
                    checkpoint,
                    suggest_realign,
                });
                tx.execute(
                    "UPDATE operations SET state='complete' WHERE id=?1",
                    [operation.to_string()],
                )?;
                save_job(&tx, &mut job)?;
                event(
                    &tx,
                    "path.restored",
                    json!({"request":app.request,"trackId":entry.track,"old":app.source.basis.base,"new":basis.base,"suggestRealign":suggest_realign}),
                )?;
            }
            job
        }
    };
    // Admission holds and held utterances are deliberately independent.
    tx.commit()?;
    Ok(job)
}
fn read_application(db: &Connection, operation: Uuid) -> Result<(Application, String)> {
    let (kind, raw, state): (String, String, String) = db.query_row(
        "SELECT kind,payload,state FROM operations WHERE id=?1",
        [operation.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if kind != "workspace.restorePath" {
        return Err("Invalid Restore file operation.".into());
    }
    let app: Application = serde_json::from_str(&raw)?;
    if crate::path_restore::file_operation(&app.request) != operation {
        return Err("Restore file operation ID reused.".into());
    }
    Ok((app, state))
}
fn validate_inputs(source: &Context, inputs: &crate::path_restore::Inputs) -> Result<()> {
    if inputs.track != source.entry.track
        || inputs.basis != source.basis
        || inputs.target != source.target
        || inputs.coverage != source.coverage
        || inputs.prefix != source.prefix
        || inputs.cancelled != source.cancelled
    {
        return Err("The Restore calculation has different publication inputs.".into());
    }
    Ok(())
}
