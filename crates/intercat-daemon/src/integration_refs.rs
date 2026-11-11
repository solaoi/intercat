//! Durable hidden candidate refs. User branches are not changed here.
use crate::{
    Result,
    candidate::Candidate,
    integration::Job,
    journal::{Admission, Journal},
};
use intercat_git::Repository;

async fn finish(journal: &Journal, id: &str, state: Admission) -> Result<()> {
    match state {
        Admission::New => {
            journal.finish(id, "dispatched").await?;
            journal.finish(id, "complete").await?;
        }
        Admission::Replay(state) if state == "prepared" => {
            journal.finish(id, "dispatched").await?;
            journal.finish(id, "complete").await?;
        }
        Admission::Replay(state) if state == "dispatched" => {
            journal.finish(id, "complete").await?;
        }
        Admission::Replay(state) if state == "complete" => {}
        _ => return Err("The staging operation was superseded.".into()),
    }
    Ok(())
}
pub async fn stage(job: &Job, candidate: &Candidate, journal: &Journal) -> Result<()> {
    if (candidate.commits.is_empty() && candidate.undo.is_none()) || candidate.pending_undo() {
        return Ok(());
    }
    let plan = job.plan.as_ref().ok_or("No integration plan.")?;
    let name = format!("refs/intercat/{}/staging", job.slug);
    let id = uuid::Uuid::new_v5(
        &job.id,
        format!("stage:{}:{}", job.revision, candidate.head).as_bytes(),
    )
    .to_string();
    let state=journal.intent(&id,"integration.stage",serde_json::json!({"repository":job.project.root,"ref":name,"expected":plan.expected_staging,"next":candidate.head})).await?;
    let root = job.project.root.clone();
    let expected = plan
        .expected_staging
        .as_ref()
        .map(|id| id.parse())
        .transpose()?;
    let next = candidate.head.parse()?;
    let slug = job.slug.clone();
    let session = job.session_id.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(root)?;
        if !repo.owns_session(&slug, &session)? {
            return Err("The session Git namespace changed.".into());
        }
        repo.sync_objects(&[next])?;
        repo.compare_and_swap(&name, expected, next)?;
        repo.sync_ref(&name)?;
        Ok(())
    })
    .await??;
    finish(journal, &id, state).await
}
pub async fn discard(job: &Job, journal: &Journal) -> Result<()> {
    let Some(expected) = job
        .plan
        .as_ref()
        .and_then(|plan| plan.expected_staging.as_ref())
    else {
        return Ok(());
    };
    let name = format!("refs/intercat/{}/staging", job.slug);
    let id = uuid::Uuid::new_v5(
        &job.id,
        format!("discard-staging:{}", job.revision).as_bytes(),
    )
    .to_string();
    let state = journal
        .intent(
            &id,
            "integration.discard-staging",
            serde_json::json!({"repository":job.project.root,"ref":name,"expected":expected}),
        )
        .await?;
    let root = job.project.root.clone();
    let expected = expected.parse()?;
    let slug = job.slug.clone();
    let session = job.session_id.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(root)?;
        if !repo.owns_session(&slug, &session)? {
            return Err("The session Git namespace changed.".into());
        }
        repo.delete_ref(&name, expected)
    })
    .await??;
    finish(journal, &id, state).await
}
