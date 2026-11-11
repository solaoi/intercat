//! Commit points are recorded in the same transaction as publication.
use crate::{Result, integration::Job};
use rusqlite::{Connection, params};
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE timeline_commits(session TEXT NOT NULL,item TEXT NOT NULL,folder TEXT NOT NULL,sha TEXT NOT NULL,job TEXT NOT NULL,PRIMARY KEY(session,item));")?;
    Ok(())
}
pub(crate) fn record(db: &Connection, job: &Job) -> Result<()> {
    let at = crate::clock::Stamp::now().timeline_ms;
    if let Some(undo) = &job
        .candidate
        .as_ref()
        .ok_or("Missing committed candidate.")?
        .undo
    {
        let track = job
            .undo
            .as_ref()
            .and_then(|plan| plan.records.first())
            .ok_or("Missing undo timeline owner.")?
            .track_id
            .clone();
        let item = format!("commit/{}/{}", job.id, undo.sha);
        db.execute("INSERT INTO timeline_commits VALUES(?1,?2,?3,?4,?5) ON CONFLICT(session,item) DO NOTHING", params![job.session_id,item,job.project.id,undo.sha,job.id.to_string()])?;
        db.execute("INSERT INTO timeline_marks VALUES(?1,?2,?3,?4,NULL,NULL) ON CONFLICT(session,item) DO NOTHING", params![job.session_id,item,track,at])?;
    }
    for commit in &job
        .candidate
        .as_ref()
        .ok_or("Missing committed candidate.")?
        .commits
    {
        let item = format!("commit/{}/{}", job.id, commit.sha);
        db.execute("INSERT INTO timeline_commits VALUES(?1,?2,?3,?4,?5) ON CONFLICT(session,item) DO NOTHING",params![job.session_id,item,job.project.id,commit.sha,job.id.to_string()])?;
        db.execute("INSERT INTO timeline_marks VALUES(?1,?2,?3,?4,NULL,NULL) ON CONFLICT(session,item) DO NOTHING",params![job.session_id,item,commit.track,at])?;
    }
    Ok(())
}

pub(crate) fn pushed(db: &Connection) -> Result<std::collections::BTreeSet<(String, String)>> {
    let mut heads = std::collections::BTreeSet::new();
    let mut covered = std::collections::BTreeSet::new();
    for job in crate::push_job_store::list(db)? {
        for recipient in &job.recipients {
            if job.recipients.iter().any(|other| {
                other.folder_id == recipient.folder_id
                    && other.phase != crate::push_job::Phase::Done
            }) {
                continue;
            }
            let Some(receipt) = &recipient.receipt else {
                continue;
            };
            if !heads.insert((recipient.folder_id.clone(), receipt.sha.clone())) {
                continue;
            }
            let member = job
                .review
                .members
                .iter()
                .find(|member| member.folder.id == recipient.folder_id)
                .ok_or("Missing pushed folder.")?;
            let repo = crate::push_review::repository(member)?;
            for sha in repo.push_commits(
                receipt.sha.parse()?,
                &[],
                &std::sync::atomic::AtomicBool::new(false),
            )? {
                covered.insert((member.folder.id.clone(), sha.to_string()));
            }
        }
    }
    Ok(covered)
}
pub(crate) fn refresh_pushed(db: &Connection, folder: &str) -> Result<()> {
    db.execute("UPDATE timeline_marks SET input=input WHERE EXISTS(SELECT 1 FROM timeline_commits c WHERE c.session=timeline_marks.session AND c.item=timeline_marks.item AND c.folder=?1)",[folder])?;
    Ok(())
}
