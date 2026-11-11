//! Journal transactions for confirmed Push jobs. Sending is never replayed here.
use crate::{
    Result,
    push_job::{Action, Command, Job, Phase, Progress, Recipient},
};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE push_jobs(id TEXT PRIMARY KEY,review TEXT UNIQUE NOT NULL,session TEXT NOT NULL,record TEXT NOT NULL);")?;
    Ok(())
}
pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Job> {
    let raw: String = db.query_row(
        "SELECT record FROM push_jobs WHERE id=?1",
        [id.to_string()],
        |row| row.get(0),
    )?;
    let job: Job = serde_json::from_str(&raw)?;
    job.validate()?;
    if job.id != id {
        return Err("The Push record belongs to another job.".into());
    }
    Ok(job)
}
pub(crate) fn list(db: &Connection) -> Result<Vec<Job>> {
    db.prepare("SELECT id FROM push_jobs ORDER BY rowid")?
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|id| read(db, id?.parse()?))
        .collect()
}
fn save(db: &Connection, job: &mut Job) -> Result<()> {
    job.revision = job
        .revision
        .checked_add(1)
        .ok_or("Push revision overflow.")?;
    job.validate()?;
    db.execute("INSERT INTO push_jobs VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![job.id.to_string(),job.review.request.op_id.to_string(),job.review.session_id,serde_json::to_string(job)?])?;
    db.execute(
        "INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'push.changed',?3)",
        params![
            crate::journal::timestamp(),
            job.review.session_id,
            serde_json::json!({"id":job.id,"revision":job.revision}).to_string()
        ],
    )?;
    Ok(())
}
pub(crate) fn command(db: &mut Connection, command: Command) -> Result<Job> {
    if command.op_id.get_version_num() != 7 {
        return Err("Invalid Push operation.".into());
    }
    let tx = db.transaction()?;
    let request = serde_json::to_value(&command)?;
    let saved: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [command.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, payload)) = saved {
        let payload: serde_json::Value = serde_json::from_str(&payload)?;
        if kind != "push.command" || payload["request"] != request {
            return Err("The operation ID was used for another Push action.".into());
        }
        return read(&tx, serde_json::from_value(payload["job"].clone())?);
    }
    let mut job = match command.action {
        Action::Confirm { review } => {
            if let Some(id) = tx
                .query_row(
                    "SELECT id FROM push_jobs WHERE review=?1",
                    [review.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            {
                read(&tx, id.parse()?)?
            } else {
                let review = crate::push_review_store::read(&tx, review)?
                    .ok_or("Open a Push review before confirming.")?;
                let complete: bool = tx.query_row(
                    "SELECT state='complete' FROM operations WHERE id=?1",
                    [review.request.op_id.to_string()],
                    |row| row.get(0),
                )?;
                if !complete {
                    return Err("The Push review is not ready.".into());
                }
                let current = crate::push_review_store::context(&tx, &review.request)?;
                if current.project_id != review.project_id
                    || current.session_id != review.session_id
                    || current
                        .sources
                        .iter()
                        .zip(&review.members)
                        .any(|(source, member)| {
                            source.reference != member.target.source_ref
                                || serde_json::to_value(&source.folder).ok()
                                    != serde_json::to_value(&member.folder).ok()
                        })
                {
                    return Err("The project or session changed. Open its Push review.".into());
                }
                let recipients = review
                    .members
                    .iter()
                    .flat_map(|member| {
                        member
                            .target
                            .destinations
                            .iter()
                            .map(move |destination| Recipient {
                                folder_id: member.folder.id.clone(),
                                destination: destination.clone(),
                                phase: Phase::Queued,
                                attempt: 0,
                                scans: Vec::new(),
                                receipt: None,
                                reason: None,
                            })
                    })
                    .collect();
                Job {
                    id: command.op_id,
                    revision: 0,
                    review,
                    stopping: false,
                    recipients,
                }
            }
        }
        Action::Retry { id, revision } => {
            let mut job = read(&tx, id)?;
            if job.revision != revision || job.active() {
                return Err("The Push job changed or is still running.".into());
            }
            if job
                .recipients
                .iter()
                .any(|recipient| recipient.phase == Phase::NeedsReview)
            {
                return Err("The destinations changed. Open a new Push review.".into());
            }
            for recipient in &mut job.recipients {
                if recipient.phase != Phase::Done {
                    recipient.phase = Phase::Queued;
                    recipient.attempt = recipient
                        .attempt
                        .checked_add(1)
                        .ok_or("Push attempt overflow.")?;
                    recipient.reason = None;
                    recipient.scans.clear();
                }
            }
            job.stopping = false;
            job
        }
        Action::Cancel { id, revision } => {
            let mut job = read(&tx, id)?;
            if job.revision != revision {
                return Err("The Push job changed. Read its current status.".into());
            }
            job.stopping = true;
            for recipient in &mut job.recipients {
                if recipient.phase == Phase::Queued {
                    recipient.phase = Phase::Cancelled;
                    recipient.reason =
                        Some("Push cancelled before this destination was started.".into());
                }
            }
            job
        }
    };
    save(&tx, &mut job)?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'push.command',?2,'complete')",
        params![
            command.op_id.to_string(),
            serde_json::json!({"request":request,"job":job.id}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(job)
}
pub(crate) fn progress(db: &mut Connection, progress: Progress) -> Result<Job> {
    let tx = db.transaction()?;
    let mut job = read(&tx, progress.id)?;
    let recipient = job
        .recipients
        .get_mut(progress.recipient)
        .ok_or("Missing Push recipient.")?;
    if recipient.attempt != progress.attempt || !recipient.phase.active() {
        return Err("This Push attempt has already ended.".into());
    }
    let next = progress.phase;
    let ordinary = matches!(
        (recipient.phase, next),
        (Phase::Queued, Phase::Preparing)
            | (Phase::Preparing, Phase::Scanning)
            | (Phase::Scanning, Phase::Hooks)
            | (Phase::Hooks, Phase::Lfs)
            | (Phase::Lfs, Phase::Sending)
            | (Phase::Sending, Phase::Done)
            | (Phase::Preparing, Phase::Done)
    );
    let stopped = matches!(
        next,
        Phase::Failed | Phase::Cancelled | Phase::NeedsReview | Phase::Uncertain
    );
    if !ordinary && !stopped
        || recipient.phase == Phase::Sending && matches!(next, Phase::Failed | Phase::Cancelled)
    {
        return Err("The Push attempt cannot enter this phase.".into());
    }
    if job.stopping && next.active() {
        return Err("The Push job is stopping.".into());
    }
    recipient.phase = next;
    recipient.reason = progress.reason.map(str::to_owned);
    if let Some(scans) = progress.scans {
        recipient.scans = scans;
    }
    recipient.receipt = progress.receipt;
    let folder = recipient.folder_id.clone();
    save(&tx, &mut job)?;
    if next == Phase::Done {
        crate::publication_timeline::refresh_pushed(&tx, &folder)?;
    }
    tx.commit()?;
    Ok(job)
}
pub(crate) fn recover(db: &mut Connection) -> Result<Vec<Job>> {
    let tx = db.transaction()?;
    let mut jobs = list(&tx)?;
    for job in &mut jobs {
        if job.active() {
            for recipient in &mut job.recipients {
                if recipient.phase.active() {
                    let old = recipient.phase;
                    recipient.phase = old.interrupted();
                    recipient.reason=Some(if old==Phase::Sending {"Push was interrupted while sending. Retry checks the destination before sending again."} else {"Push was interrupted. Its confirmed inputs are retained; Retry continues unfinished destinations."}.into());
                }
            }
            job.stopping = false;
            save(&tx, job)?;
        }
    }
    tx.commit()?;
    Ok(jobs)
}

pub(crate) fn approvals(
    db: &Connection,
    session: &str,
    folder: &str,
) -> Result<Vec<crate::push_job::Approval>> {
    let mut approvals = Vec::new();
    let mut query=db.prepare("SELECT record FROM integration_jobs WHERE session=?1 AND json_extract(record,'$.project.id')=?2 AND json_extract(record,'$.status')='done'")?;
    for row in query.query_map(params![session, folder], |row| row.get::<_, String>(0))? {
        let job: crate::integration::Job = serde_json::from_str(&row?)?;
        if let Some(candidate) = job.candidate {
            for scan in candidate.scans() {
                approvals.push(crate::push_job::Approval {
                    scan: scan.clone(),
                    kept: job.kept_secrets.clone(),
                });
            }
        }
    }
    Ok(approvals)
}

pub(crate) fn snapshot(db: &Connection) -> Result<Vec<Job>> {
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
    Ok(list(db)?
        .into_iter()
        .filter(|job| job.review.session_id == session)
        .collect())
}
