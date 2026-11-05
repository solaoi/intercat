//! Transactional integration-job snapshots. Git and project code never run on
//! the SQLite writer; the coordinator supplies object-only merge results later.
use crate::{
    Result,
    checkpoint::Checkpoint,
    integration::{Action, Command, Job, Plan, RowState, Status, Track},
    preparation::Workspace,
    project::Project,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE integration_jobs(id TEXT PRIMARY KEY,session TEXT NOT NULL,active INTEGER NOT NULL,record TEXT NOT NULL);
    CREATE TABLE integration_settings(job TEXT NOT NULL,revision INTEGER NOT NULL,record TEXT NOT NULL,PRIMARY KEY(job,revision));
    CREATE TABLE integration_counts(track TEXT PRIMARY KEY,count INTEGER NOT NULL);
    CREATE UNIQUE INDEX one_active_integration ON integration_jobs(session) WHERE active=1;")?;
    Ok(())
}

pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Job> {
    let record: String = db.query_row(
        "SELECT record FROM integration_jobs WHERE id=?1",
        [id.to_string()],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&record)?)
}
fn setting(db: &Connection, key: &str) -> Result<Value> {
    let value: String = db.query_row("SELECT value FROM settings WHERE key=?1", [key], |row| {
        row.get(0)
    })?;
    Ok(serde_json::from_str(&value)?)
}
pub(crate) fn save(db: &Connection, job: &Job) -> Result<()> {
    db.execute("INSERT INTO integration_jobs VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET active=excluded.active,record=excluded.record", params![job.id.to_string(),job.session_id,!matches!(job.status,Status::Discarded|Status::Done),serde_json::to_string(job)?])?;
    Ok(())
}
fn checkpoint_count(db: &Connection, track: &str) -> Result<u64> {
    let count: i64 = db.query_row("SELECT count(*) FROM operations WHERE kind='checkpoint.save' AND state='complete' AND json_extract(payload,'$.trackId')=?1", [track], |r| r.get(0))?;
    Ok(count.try_into()?)
}
fn capture(
    db: &Connection,
    id: Uuid,
    previous: Option<&Job>,
    selected: Option<&[String]>,
) -> Result<Job> {
    let session = setting(db, "session")?;
    if session["bound"] != true {
        return Err("The session is still being prepared.".into());
    }
    let project: Project = serde_json::from_value(setting(db, "project")?)?;
    let session_id = session["id"]
        .as_str()
        .ok_or("Missing session ID.")?
        .to_string();
    if previous.is_some_and(|previous| {
        previous.session_id != session_id
            || previous.project.root != project.root
            || previous.project.common_dir != project.common_dir
            || previous.project.base != project.base
    }) {
        return Err(
            "The project or session changed. Open its current review before refreshing.".into(),
        );
    }
    let slug = session["slug"]
        .as_str()
        .ok_or("Missing session slug.")?
        .to_string();
    let name = session["name"]
        .as_str()
        .ok_or("Missing session name.")?
        .to_string();
    let mut stmt = db.prepare("SELECT tracks.id,tracks.label,tracks.title,workspaces.record FROM tracks JOIN workspaces ON tracks.id=workspaces.track_id ORDER BY tracks.rowid")?;
    let records = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut tracks = Vec::new();
    for (track_id, label, title, workspace) in records {
        let workspace: Workspace = serde_json::from_str(&workspace)?;
        if workspace.state != "ready" {
            continue;
        }
        if label.is_empty() || !label.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err("Invalid track label.".into());
        }
        // A completed operation is authoritative even if the worker-state
        // notification is still queued, or the daemon died just before it.
        let payload: Option<String> = db.query_row("SELECT payload FROM operations WHERE kind='checkpoint.save' AND state='complete' AND json_extract(payload,'$.trackId')=?1 ORDER BY rowid DESC LIMIT 1", [&track_id], |row| row.get(0)).optional()?;
        let checkpoint: Option<Checkpoint> = payload
            .map(|record| -> Result<_> {
                Ok(serde_json::from_value(
                    serde_json::from_str::<Value>(&record)?["checkpoint"].clone(),
                )?)
            })
            .transpose()?;
        let last_clip = match &checkpoint {
            Some(checkpoint) => db.query_row("SELECT max(ordinal) FROM timeline_clips WHERE track=?1 AND json_extract(record,'$.turnId')=?2 AND json_extract(record,'$.category')='step'", params![track_id, checkpoint.turn_id.to_string()], |row| row.get(0))?,
            None => None,
        };
        let included = checkpoint.is_some()
            && match selected {
                Some(ids) => ids.contains(&track_id),
                None => previous
                    .and_then(|job| job.tracks.iter().find(|t| t.id == track_id))
                    .is_none_or(|t| t.included),
            };
        tracks.push(Track {
            integration_count: db
                .query_row(
                    "SELECT count FROM integration_counts WHERE track=?1",
                    [&track_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .unwrap_or(0)
                .try_into()?,
            checkpoint_count: checkpoint_count(db, &track_id)?,
            id: track_id,
            label,
            title,
            initial_base: workspace.base,
            checkpoint,
            last_clip,
            included,
        });
    }
    if let Some(selected) = selected {
        if selected
            .iter()
            .any(|id| !tracks.iter().any(|t| &t.id == id && t.checkpoint.is_some()))
        {
            return Err("A selected track has no saved checkpoint.".into());
        }
        let unique: std::collections::HashSet<_> = selected.iter().collect();
        if unique.len() != selected.len() {
            return Err("Duplicate track selection.".into());
        }
    }
    if tracks.is_empty() {
        return Err("No prepared tracks are available.".into());
    }
    Ok(Job {
        id,
        revision: previous.map_or(1, |p| p.revision + 1),
        status: Status::Preparing,
        opened_at: crate::journal::timestamp(),
        session_id,
        slug,
        session_name: name,
        project,
        tracks,
        plan: None,
        error: None,
        candidate: None,
        messages: previous.map(|job| job.messages.clone()).unwrap_or_default(),
        publication: None,
        kept_secrets: Default::default(),
        contributions: Default::default(),
    })
}
fn editable(job: &Job, revision: u64) -> Result<()> {
    if job.revision != revision {
        return Err("The sheet changed · review again".into());
    }
    if job.status == Status::Discarded {
        return Err("This integration job was discarded.".into());
    }
    if matches!(
        job.status,
        Status::Committing | Status::CommitBlocked | Status::Done
    ) {
        return Err("This Commit is finishing or has already finished.".into());
    }
    if job.status == Status::Staging {
        return Err(crate::integration::StagingBusy.into());
    }
    Ok(())
}

pub(crate) fn command(db: &mut Connection, command: &Command) -> Result<Job> {
    let tx = db.transaction()?;
    let job = command_in(&tx, command)?;
    tx.commit()?;
    Ok(job)
}

pub(crate) fn command_in(tx: &Connection, command: &Command) -> Result<Job> {
    let op_id = command.op_id.to_string();
    let request = serde_json::to_value(&command.action)?;
    let replay: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [&op_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, payload)) = replay {
        let payload: Value = serde_json::from_str(&payload)?;
        if kind != "integration.command" || payload["request"] != request {
            return Err("Operation ID was used for a different request.".into());
        }
        return read(
            tx,
            Uuid::parse_str(
                payload["job"]
                    .as_str()
                    .ok_or("Invalid saved integration command.")?,
            )?,
        );
    }
    if let Some(id) = request["id"].as_str()
        && crate::repair_run_store::active(tx, Uuid::parse_str(id)?)?
    {
        return Err(
            "A repair Run owns this candidate. Stop and discard it before changing the job.".into(),
        );
    }
    let job = match &command.action {
        Action::Open { tracks } => {
            let session = setting(tx, "session")?;
            let active: Option<String> = tx
                .query_row(
                    "SELECT id FROM integration_jobs WHERE session=?1 AND active=1",
                    [session["id"].as_str().ok_or("Missing session ID.")?],
                    |row| row.get(0),
                )
                .optional()?;
            match active {
                Some(id) => read(tx, Uuid::parse_str(&id)?)?,
                None => capture(
                    tx,
                    Uuid::new_v5(&command.op_id, b"integration"),
                    None,
                    tracks.as_deref(),
                )?,
            }
        }
        Action::Refresh { id, revision } => {
            let old = read(tx, *id)?;
            editable(&old, *revision)?;
            capture(tx, *id, Some(&old), None)?
        }
        Action::Select {
            id,
            revision,
            track,
            included,
        } => {
            let mut job = read(tx, *id)?;
            editable(&job, *revision)?;
            let row = job
                .tracks
                .iter_mut()
                .find(|row| row.id == *track)
                .ok_or("Unknown integration track.")?;
            if *included && row.checkpoint.is_none() {
                return Err("This track has no saved checkpoint.".into());
            }
            row.included = *included;
            job.revision += 1;
            job.status = if job.contributions.is_empty() {
                Status::Preparing
            } else {
                Status::Building
            };
            job.error = None;
            job.candidate = None;
            job.kept_secrets.clear();
            copy_settings(tx, *id, *revision, job.revision, None)?;
            // Preserve the original refs and checkpoint selection. Including
            // or excluding a row is not an implicit Refresh.
            job
        }
        Action::Discard { id, revision } => {
            let mut job = read(tx, *id)?;
            editable(&job, *revision)?;
            job.revision += 1;
            job.status = Status::Discarded;
            job
        }
        Action::Prepare { id, revision } => {
            let mut job = read(tx, *id)?;
            editable(&job, *revision)?;
            if job
                .plan
                .as_ref()
                .and_then(|plan| plan.tree.as_ref())
                .is_none()
            {
                return Err("The integration has no resolved tree to build.".into());
            }
            if job.status == Status::MergeConflict || job.status == Status::Preparing {
                return Err("The integration is not ready to build.".into());
            }
            if job.status == Status::Failed {
                job.revision += 1;
                copy_settings(tx, *id, *revision, job.revision, None)?;
                let resume = job.candidate.as_ref().is_some_and(|candidate| {
                    candidate.blocked_commit.is_some()
                        && candidate
                            .secret_scans
                            .last()
                            .is_some_and(|scan| scan.accepted(&job.kept_secrets))
                });
                if resume {
                    job.candidate.as_mut().unwrap().revision = job.revision;
                } else {
                    job.candidate = None;
                    job.kept_secrets.clear();
                }
            }
            if job.status != Status::Checked && job.status != Status::Building {
                job.status = Status::Building;
                job.error = None;
            }
            job
        }
        Action::SkipHooks { id, revision } => {
            let mut job = read(tx, *id)?;
            editable(&job, *revision)?;
            if job
                .candidate
                .as_ref()
                .is_none_or(|candidate| candidate.state != crate::candidate::State::HooksOutside)
            {
                return Err("There is no outside-hook decision to answer.".into());
            }
            job.revision += 1;
            copy_settings(tx, *id, *revision, job.revision, Some(true))?;
            job.status = Status::Building;
            job.error = None;
            job.candidate = None;
            job.kept_secrets.clear();
            job
        }
        Action::EditMessage {
            id,
            revision,
            track,
            message,
        } => {
            let mut job = read(tx, *id)?;
            editable(&job, *revision)?;
            if message.trim().is_empty() || message.len() > 16 * 1024 || message.contains('\0') {
                return Err("A commit message must be nonempty and at most 16 KiB.".into());
            }
            if !job
                .tracks
                .iter()
                .any(|row| row.id == *track && row.included && row.checkpoint.is_some())
            {
                return Err("Unknown or excluded commit track.".into());
            }
            if job
                .plan
                .as_ref()
                .and_then(|plan| plan.tree.as_ref())
                .is_none()
            {
                return Err("Resolve the integration before editing its messages.".into());
            }
            job.messages.insert(track.clone(), message.clone());
            job.revision += 1;
            copy_settings(tx, *id, *revision, job.revision, None)?;
            job.status = Status::Building;
            job.candidate = None;
            job.kept_secrets.clear();
            job.error = None;
            job
        }
        Action::KeepSecret {
            id,
            revision,
            finding,
        } => {
            let mut job = read(tx, *id)?;
            editable(&job, *revision)?;
            let candidate = job
                .candidate
                .as_ref()
                .ok_or("No secret finding to review.")?;
            if candidate.state != crate::candidate::State::SecretFound
                || candidate.blocked_commit.is_none()
                || !candidate
                    .secret_scans
                    .last()
                    .is_some_and(|scan| scan.findings.iter().any(|item| item.id == *finding))
            {
                return Err("The secret finding changed · review again".into());
            }
            job.kept_secrets.insert(finding.clone());
            job.revision += 1;
            copy_settings(tx, *id, *revision, job.revision, None)?;
            let candidate = job.candidate.as_mut().unwrap();
            candidate.revision = job.revision;
            if candidate
                .secret_scans
                .last()
                .unwrap()
                .accepted(&job.kept_secrets)
            {
                job.status = Status::Building;
                job.error = None;
            }
            job
        }
        Action::Commit {
            id,
            revision,
            head,
            tree,
            settings_hash,
        } => {
            let mut job = read(tx, *id)?;
            editable(&job, *revision)?;
            let candidate = job.candidate.as_ref().ok_or("No checked candidate.")?;
            if candidate
                .secret_scans
                .iter()
                .any(|scan| scan.binding.ruleset != crate::secret_scan::ruleset())
            {
                return Err("Secret detection rules changed. Refresh before committing.".into());
            }
            if job.status != Status::Checked
                || candidate.head != *head
                || candidate.tree != *tree
                || candidate.settings_hash != *settings_hash
                || settings(tx, *id, *revision)?
                    .ok_or("Missing Check settings.")?
                    .hash()?
                    != *settings_hash
            {
                return Err("The sheet changed · review again".into());
            }
            let publication = crate::publication::intent(
                &job,
                Uuid::new_v5(&command.op_id, b"integration.publish"),
            )?;
            tx.execute(
                "INSERT INTO operations VALUES(?1,'integration.publish',?2,'prepared')",
                params![
                    publication.operation.to_string(),
                    serde_json::to_string(&publication)?
                ],
            )?;
            job.publication = Some(publication);
            job.status = Status::Committing;
            job.error = None;
            job
        }
        Action::RetryCommit { id, revision } => {
            let mut job = read(tx, *id)?;
            if job.revision != *revision
                || job.status != Status::CommitBlocked
                || job.publication.is_none()
            {
                return Err("There is no blocked Commit to retry.".into());
            }
            job.status = Status::Committing;
            job.error = None;
            job
        }
    };
    save(tx, &job)?;
    let payload = json!({"request":request,"job":job.id});
    tx.execute(
        "INSERT INTO operations VALUES(?1,'integration.command',?2,'complete')",
        params![op_id, payload.to_string()],
    )?;
    tx.execute("INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'integration.command',?3)", params![crate::journal::timestamp(),job.session_id,json!({"opId":command.op_id,"request":command.action,"jobId":job.id,"revision":job.revision}).to_string()])?;
    Ok(job)
}

pub(crate) fn pending_publications(db: &Connection) -> Result<Vec<Job>> {
    let mut query=db.prepare("SELECT record FROM integration_jobs WHERE json_extract(record,'$.status')='committing' ORDER BY rowid")?;
    let records = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    records
        .into_iter()
        .map(|record| Ok(serde_json::from_str(&record)?))
        .collect()
}
pub(crate) fn published(
    db: &mut Connection,
    id: Uuid,
    revision: u64,
    outcome: crate::publication::Outcome,
) -> Result<Job> {
    let tx = db.transaction()?;
    let mut job = read(&tx, id)?;
    if job.revision != revision || job.status != Status::Committing {
        return Err("The publication result was superseded.".into());
    }
    let operation = job
        .publication
        .as_ref()
        .ok_or("Missing publication intent.")?
        .operation;
    match outcome {
        crate::publication::Outcome::Complete => {
            for commit in &job.candidate.as_ref().ok_or("Missing candidate.")?.commits {
                let track = job
                    .tracks
                    .iter()
                    .find(|track| track.id == commit.track)
                    .ok_or("Missing committed track.")?;
                tx.execute("INSERT INTO integration_counts VALUES(?1,?2) ON CONFLICT(track) DO UPDATE SET count=excluded.count",params![track.id,i64::try_from(track.integration_count.checked_add(1).ok_or("Integration count overflow.")?)?])?;
            }
            job.status = Status::Done;
            job.error = None;
            tx.execute(
                "UPDATE operations SET state='complete' WHERE id=?1",
                [operation.to_string()],
            )?;
        }
        crate::publication::Outcome::NotPublished(error) => {
            job.status = Status::Failed;
            job.error = Some(format!("Commit was not published · {error} · Refresh"));
            job.revision += 1;
            copy_settings(&tx, id, revision, job.revision, None)?;
            job.candidate = None;
            job.kept_secrets.clear();
            job.publication = None;
            tx.execute(
                "UPDATE operations SET state='superseded' WHERE id=?1",
                [operation.to_string()],
            )?;
        }
        crate::publication::Outcome::Blocked(error) => {
            job.status = Status::CommitBlocked;
            job.error = Some(format!("Commit needs recovery · {error}"));
        }
    }
    save(&tx, &job)?;
    tx.execute(
        "INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'integration.published',?3)",
        params![
            crate::journal::timestamp(),
            job.session_id,
            serde_json::to_string(&job)?
        ],
    )?;
    tx.commit()?;
    Ok(job)
}

pub(crate) fn copy_settings(
    db: &Connection,
    id: Uuid,
    old: u64,
    next: u64,
    skip_hooks: Option<bool>,
) -> Result<()> {
    if let Some(mut settings) = settings(db, id, old)? {
        if let Some(skip) = skip_hooks {
            settings.skip_hooks = skip;
        }
        db.execute(
            "INSERT INTO integration_settings VALUES(?1,?2,?3)",
            params![
                id.to_string(),
                i64::try_from(next)?,
                serde_json::to_string(&settings)?
            ],
        )?;
    }
    Ok(())
}
pub(crate) fn settings(
    db: &Connection,
    id: Uuid,
    revision: u64,
) -> Result<Option<crate::candidate::Settings>> {
    let record: Option<String> = db
        .query_row(
            "SELECT record FROM integration_settings WHERE job=?1 AND revision=?2",
            params![id.to_string(), i64::try_from(revision)?],
            |row| row.get(0),
        )
        .optional()?;
    record
        .map(|record| Ok(serde_json::from_str(&record)?))
        .transpose()
}
pub(crate) fn save_settings(
    db: &mut Connection,
    id: Uuid,
    revision: u64,
    settings: crate::candidate::Settings,
) -> Result<()> {
    let tx = db.transaction()?;
    let job = read(&tx, id)?;
    if job.revision != revision || job.status != Status::Preparing {
        return Err("The Check settings were superseded.".into());
    }
    tx.execute(
        "INSERT INTO integration_settings VALUES(?1,?2,?3)",
        params![
            id.to_string(),
            i64::try_from(revision)?,
            serde_json::to_string(&settings)?
        ],
    )?;
    tx.execute(
        "INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'integration.settings',?3)",
        params![
            crate::journal::timestamp(),
            job.session_id,
            json!({"jobId":id,"revision":revision,"settingsHash":settings.hash()?}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(())
}
pub(crate) fn candidate(
    db: &mut Connection,
    id: Uuid,
    revision: u64,
    result: std::result::Result<crate::candidate::Candidate, String>,
) -> Result<Job> {
    let tx = db.transaction()?;
    let mut job = read(&tx, id)?;
    apply_candidate(&tx, &mut job, revision, result)?;
    tx.commit()?;
    Ok(job)
}

/// The caller owns the transaction, so a repair can close its Run atomically
/// with admission of the rebuilt candidate.
pub(crate) fn apply_candidate(
    db: &Connection,
    job: &mut Job,
    revision: u64,
    result: std::result::Result<crate::candidate::Candidate, String>,
) -> Result<()> {
    let id = job.id;
    if job.revision != revision || !matches!(job.status, Status::Building | Status::Staging) {
        return Err("The candidate result was superseded.".into());
    }
    match result {
        Ok(candidate) => {
            let settings = settings(db, id, revision)?.ok_or("Missing Check settings.")?;
            if candidate.job_id != id
                || candidate.revision != revision
                || candidate.settings_hash != settings.hash()?
            {
                return Err("The candidate does not match its frozen inputs.".into());
            }
            job.status = if candidate.state == crate::candidate::State::Checked {
                Status::Checked
            } else {
                Status::Failed
            };
            job.error = candidate.reason.clone();
            job.contributions = candidate.contributions.clone();
            if !candidate.commits.is_empty()
                && let Some(plan) = &mut job.plan
            {
                plan.expected_staging = Some(candidate.head.clone());
            }
            job.candidate = Some(candidate);
        }
        Err(error) => {
            job.status = Status::Failed;
            job.error = Some(error);
        }
    }
    save(db, job)?;
    db.execute("INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'integration.candidate',?3)",params![crate::journal::timestamp(),job.session_id,json!({"jobId":id,"revision":revision,"state":job.status,"candidate":job.candidate,"error":job.error}).to_string()])?;
    Ok(())
}
pub(crate) fn stage_begin(db: &mut Connection, id: Uuid, revision: u64) -> Result<bool> {
    let tx = db.transaction()?;
    let mut job = read(&tx, id)?;
    if job.revision != revision || job.status != Status::Building {
        return Ok(false);
    }
    job.status = Status::Staging;
    save(&tx, &job)?;
    tx.execute(
        "INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'integration.staging',?3)",
        params![
            crate::journal::timestamp(),
            job.session_id,
            json!({"jobId":id,"revision":revision}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(true)
}
pub(crate) fn interrupt(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    let records = tx
        .prepare("SELECT record FROM integration_jobs WHERE active=1")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for record in records {
        let mut job: Job = serde_json::from_str(&record)?;
        if matches!(job.status, Status::Building | Status::Staging) {
            job.status = Status::Failed;
            job.error = Some("Candidate build was interrupted. Retry to continue.".into());
            save(&tx, &job)?;
            tx.execute("INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'integration.interrupted',?3)",params![crate::journal::timestamp(),job.session_id,json!({"jobId":job.id,"revision":job.revision}).to_string()])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn frozen(db: &mut Connection, id: Uuid, revision: u64, plan: Plan) -> Result<Job> {
    let tx = db.transaction()?;
    let mut job = read(&tx, id)?;
    if job.revision != revision || job.status != Status::Preparing || job.plan.is_some() {
        return Err("Integration inputs were superseded.".into());
    }
    job.plan = Some(plan);
    save(&tx, &job)?;
    tx.execute(
        "INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'integration.frozen',?3)",
        params![
            crate::journal::timestamp(),
            job.session_id,
            json!({"jobId":id,"revision":revision,"plan":job.plan}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(job)
}

pub(crate) fn planned(
    db: &mut Connection,
    id: Uuid,
    revision: u64,
    result: std::result::Result<Plan, String>,
) -> Result<Job> {
    let tx = db.transaction()?;
    let mut job = read(&tx, id)?;
    if job.revision != revision || job.status != Status::Preparing {
        return Err("Integration result was superseded.".into());
    }
    match result {
        Ok(plan) => {
            job.status = if plan.rows.iter().any(|row| row.state == RowState::Conflict) {
                Status::MergeConflict
            } else {
                Status::Open
            };
            job.plan = Some(plan);
            job.error = None;
        }
        Err(message) => {
            job.status = Status::Failed;
            job.error = Some(message);
        }
    }
    save(&tx, &job)?;
    tx.execute("INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'integration.planned',?3)", params![crate::journal::timestamp(),job.session_id,json!({"jobId":id,"revision":revision,"status":job.status,"tree":job.plan.as_ref().and_then(|plan|plan.tree.as_deref()),"error":job.error}).to_string()])?;
    tx.commit()?;
    Ok(job)
}

pub(crate) fn snapshot(db: &Connection) -> Result<Value> {
    let record: Option<String> = db
        .query_row(
            "SELECT record FROM integration_jobs ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(record) = record else {
        return Ok(Value::Null);
    };
    let job: Job = serde_json::from_str(&record)?;
    if job.status == Status::Discarded {
        return Ok(Value::Null);
    }
    let mut finished = 0;
    for track in &job.tracks {
        finished += checkpoint_count(db, &track.id)?.saturating_sub(track.checkpoint_count);
    }
    let mut value = serde_json::to_value(job)?;
    value["finishedSinceOpen"] = finished.into();
    Ok(value)
}
