//! PR text and intent are journaled before any GitHub write.
use crate::{
    Result,
    github_repository::{Repository, Target},
    pull_request::{Job, Member, Open, Phase},
};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

fn require_available(db: &Connection, id: Uuid, targets: &[Target]) -> Result<()> {
    let occupied:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM pull_jobs j,json_each(j.record,'$.members') m,json_each(?2) t
        WHERE j.id!=?1
        AND json_extract(m.value,'$.target.repository.owner')=json_extract(t.value,'$.repository.owner') COLLATE NOCASE
        AND json_extract(m.value,'$.target.repository.name')=json_extract(t.value,'$.repository.name') COLLATE NOCASE
        AND json_extract(m.value,'$.target.head')=json_extract(t.value,'$.head')
        AND json_extract(m.value,'$.target.base')=json_extract(t.value,'$.base')
        AND (json_extract(m.value,'$.phase') IN ('creating','uncertain','checking','linking')
            OR (json_type(j.record,'$.confirmed')='text' AND json_extract(j.record,'$.held')=0 AND json_extract(m.value,'$.phase') IN ('ready','created'))))",params![id.to_string(),serde_json::to_string(targets)?],|row|row.get(0))?;
    if occupied {
        return Err("Another PR request is using this branch or has an unknown result. Reopen its saved review before continuing.".into());
    }
    Ok(())
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE pull_jobs(id TEXT PRIMARY KEY,push_review TEXT UNIQUE NOT NULL,session TEXT NOT NULL,record TEXT NOT NULL);")?;
    Ok(())
}
pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Job> {
    let raw: String = db.query_row(
        "SELECT record FROM pull_jobs WHERE id=?1",
        [id.to_string()],
        |row| row.get(0),
    )?;
    let job: Job = serde_json::from_str(&raw)?;
    job.validate()?;
    if job.id != id {
        return Err("The PR record belongs to another job.".into());
    }
    Ok(job)
}
pub(crate) fn save(db: &Connection, job: &mut Job) -> Result<()> {
    job.revision = job.revision.checked_add(1).ok_or("PR revision overflow.")?;
    job.validate()?;
    db.execute("INSERT INTO pull_jobs VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![job.id.to_string(),job.request.push_review.to_string(),job.push.session_id,serde_json::to_string(job)?])?;
    db.execute(
        "INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'pull.changed',?3)",
        params![
            crate::journal::timestamp(),
            job.push.session_id,
            serde_json::json!({"id":job.id,"revision":job.revision}).to_string()
        ],
    )?;
    Ok(())
}
pub(crate) fn open(db: &mut Connection, request: Open) -> Result<Job> {
    request.validate()?;
    let tx = db.transaction()?;
    let saved: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, payload)) = saved {
        if kind != "pull.open" || serde_json::from_str::<Open>(&payload)? != request {
            return Err("The PR operation ID was used for another request.".into());
        }
        return read(&tx, request.op_id);
    }
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pull_jobs WHERE push_review=?1)",
        [request.push_review.to_string()],
        |row| row.get::<_, bool>(0),
    )? {
        return Err("This Push review already has a PR review. Reopen its saved result.".into());
    }
    let push = crate::push_review_store::read(&tx, request.push_review)?
        .ok_or("Open a Push review before preparing PR.")?;
    let current = crate::push_review_store::context(&tx, &push.request)?;
    if current.project_id != push.project_id || current.session_id != push.session_id {
        return Err("The project or session changed. Review its current work.".into());
    }
    let sent = crate::push_job_store::list(&tx)?;
    let mut members = Vec::new();
    for choice in &request.choices {
        let source = push
            .members
            .iter()
            .find(|member| member.folder.id == choice.folder_id)
            .ok_or("The PR folder was not in the Push review.")?;
        if source.folder.base_branch.is_none() {
            return Err("no base branch yet".into());
        }
        let destination = source
            .target
            .destinations
            .iter()
            .find(|destination| destination.fingerprint == choice.destination)
            .ok_or("The PR destination was not in the Push review.")?;
        let complete = sent.iter().any(|job| {
            job.recipients.iter().any(|recipient| {
                recipient.folder_id == source.folder.id
                    && recipient.destination == *destination
                    && recipient.receipt.as_ref().is_some_and(|receipt| {
                        receipt.sha == source.target.sha
                            && receipt.reference == source.target.target_ref
                    })
            })
        });
        if !complete {
            return Err("Push this reviewed destination before preparing PR.".into());
        }
        let target = Target {
            repository: Repository::from_git(&destination.display)?,
            base: choice.base.clone(),
            head: source
                .target
                .target_ref
                .strip_prefix("refs/heads/")
                .ok_or("PR needs a branch head.")?
                .into(),
            sha: source.target.sha.clone(),
        };
        target.validate()?;
        require_available(&tx, request.op_id, std::slice::from_ref(&target))?;
        let sources = crate::pull_sources::capture(&tx, &push.session_id, source)?;
        members.push(Member {
            folder_id: source.folder.id.clone(),
            name: source.folder.name.clone(),
            destination: destination.fingerprint.clone(),
            target,
            sources,
            draft: None,
            existing: None,
            result: None,
            phase: Phase::Drafting,
            error: None,
        });
    }
    let mut job = Job {
        id: request.op_id,
        revision: 0,
        request,
        push,
        account: None,
        confirmed: None,
        held: false,
        members,
    };
    save(&tx, &mut job)?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'pull.open',?2,'complete')",
        params![job.id.to_string(), serde_json::to_string(&job.request)?],
    )?;
    tx.commit()?;
    Ok(job)
}

pub(crate) fn snapshot(db: &Connection) -> Result<Vec<serde_json::Value>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key='session'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let session = raw
        .map(|raw| serde_json::from_str::<serde_json::Value>(&raw))
        .transpose()?;
    let Some(session) = session.as_ref().and_then(|session| session["id"].as_str()) else {
        return Ok(Vec::new());
    };
    db.prepare("SELECT id FROM pull_jobs WHERE session=?1 ORDER BY rowid")?
        .query_map([session], |row| row.get::<_, String>(0))?
        .map(|id| read(db, id?.parse()?)?.project())
        .collect()
}

pub(crate) fn command(db: &mut Connection, request: crate::pull_request::Command) -> Result<Job> {
    if request.op_id.get_version_num() != 7 {
        return Err("Invalid PR operation.".into());
    }
    let tx = db.transaction()?;
    let payload = serde_json::to_value(&request)?;
    let saved: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, raw)) = saved {
        let old: serde_json::Value = serde_json::from_str(&raw)?;
        if kind != "pull.command" || old["request"] != payload {
            return Err("The operation ID was used for another PR action.".into());
        }
        return read(&tx, serde_json::from_value(old["job"].clone())?);
    }
    let id = match request.action {
        crate::pull_request::Action::Edit { id, .. }
        | crate::pull_request::Action::Confirm { id, .. }
        | crate::pull_request::Action::Retry { id, .. }
        | crate::pull_request::Action::ReviewAgain { id, .. }
        | crate::pull_request::Action::Discard { id, .. }
        | crate::pull_request::Action::Stop { id, .. } => id,
    };
    let mut job = read(&tx, id)?;
    if matches!(request.action, crate::pull_request::Action::Confirm { .. })
        || matches!(request.action, crate::pull_request::Action::Retry { .. })
            && job.confirmed.is_some()
    {
        let targets = job
            .members
            .iter()
            .filter(|member| member.phase != Phase::Done)
            .map(|member| member.target.clone())
            .collect::<Vec<_>>();
        require_available(&tx, id, &targets)?;
    }
    crate::pull_changes::command(&mut job, request.op_id, &request.action)?;
    save(&tx, &mut job)?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'pull.command',?2,'complete')",
        params![
            request.op_id.to_string(),
            serde_json::json!({"request":payload,"job":id}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(job)
}
pub(crate) fn change(
    db: &mut Connection,
    id: Uuid,
    change: crate::pull_changes::Change,
) -> Result<Job> {
    let tx = db.transaction()?;
    let mut job = read(&tx, id)?;
    crate::pull_changes::apply(&mut job, change)?;
    save(&tx, &mut job)?;
    tx.commit()?;
    Ok(job)
}
pub(crate) fn recover(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    let ids = tx
        .prepare("SELECT id FROM pull_jobs")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let mut job = read(&tx, id.parse()?)?;
        if crate::pull_changes::recover(&mut job)? {
            save(&tx, &mut job)?;
        }
    }
    tx.commit()?;
    Ok(())
}
