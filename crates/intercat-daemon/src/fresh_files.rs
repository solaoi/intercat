//! Stop evidence, access checks and immutable final files for Start fresh.
//! Reading Git objects and publishing a private ref never edits worker files.
use crate::{
    Result,
    fresh::{Change, Files, Job, Phase},
    journal::Journal,
};
use intercat_git::Repository;
use std::path::Path;
use uuid::Uuid;

pub(crate) fn turn(id: Uuid, revision: u64) -> Uuid {
    Uuid::new_v5(&id, format!("fresh-files:{revision}").as_bytes())
}
pub(crate) fn reference(job: &Job, sha: &str) -> String {
    format!(
        "{}-fresh/{}/{sha}",
        job.workspace.reference, job.request.op_id
    )
}
pub(crate) async fn stopped(data: &Path, journal: &Journal, job: &Job) -> Result<()> {
    let snapshot = journal.snapshot().await?;
    let worker = snapshot["workers"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|worker| worker["trackId"] == job.request.track_id)
        .ok_or("Missing Start fresh source worker.")?;
    let worker: crate::runtime::WorkerState = serde_json::from_value(worker.clone())?;
    if worker.session_id.as_deref() != Some(&job.request.session_id) {
        return Err("The Start fresh source session changed.".into());
    }
    if !matches!(worker.phase.as_str(), "stopped" | "sleeping")
        || worker.active_turn.is_some()
        || worker.checkpoint_pending
    {
        return Err(crate::fresh::Waiting(
            "Waiting for the original host and its processes to close.",
        )
        .into());
    }
    let directory = data.join("hosts").join(&job.request.track_id);
    let workspace = job.workspace.path.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        if worker.identity.is_some_and(crate::process::is_alive) {
            return Err("The original host is still alive.".into());
        }
        if let Some(mut tracker) =
            crate::process::Tracker::restore(&directory, &workspace, worker.identity)?
            && !tracker.refresh()?.is_empty()
        {
            return Err("The original worker still has processes that must be stopped.".into());
        }
        Ok(())
    })
    .await?
}
pub(crate) async fn inspect(job: &Job) -> Result<crate::workspace_access::Inspection> {
    let path = job.workspace.path.clone();
    tokio::task::spawn_blocking(move || {
        crate::workspace_access::inspect(&path, &[crate::process::identity(std::process::id())?])
    })
    .await?
}
fn check_repository(job: &Job) -> Result<()> {
    let repo = Repository::open(&job.workspace.path)?;
    let point = &job
        .boundary
        .as_ref()
        .ok_or("Missing Start fresh turn boundary.")?
        .checkpoint;
    if repo.worktree_root()? != job.workspace.path.canonicalize()?
        || repo.common_directory()? != job.folder.common_dir
        || !repo.owns_session(&job.slug, &job.session)?
        || repo.reference_target(&point.reference)? != Some(point.sha.parse()?)
    {
        return Err("The Start fresh repository or saved point changed.".into());
    }
    Ok(())
}
pub(crate) async fn capture(journal: &Journal, job: &Job) -> Result<Job> {
    let before = inspect(job).await?;
    if !before.clear() && job.access_consent.as_deref() != Some(&before.fingerprint) {
        return journal
            .fresh(Change::FilesChanged {
                id: job.request.op_id,
                revision: job.revision,
                inspection: before,
            })
            .await;
    }
    let frozen = job.clone();
    tokio::task::spawn_blocking(move || check_repository(&frozen)).await??;
    let point = &job
        .boundary
        .as_ref()
        .ok_or("Missing Start fresh turn boundary.")?
        .checkpoint;
    let captured = crate::checkpoint::capture(
        &job.workspace,
        &point.sha,
        "Intercat Start fresh final files",
        journal,
    )
    .await?;
    if !captured.unmerged_paths.is_empty() {
        return Err("Resolve the worker's conflicting files before starting fresh.".into());
    }
    let after = inspect(job).await?;
    if before.fingerprint != after.fingerprint
        || (!after.clear() && job.access_consent.as_deref() != Some(&after.fingerprint))
    {
        return journal
            .fresh(Change::FilesChanged {
                id: job.request.op_id,
                revision: job.revision,
                inspection: after,
            })
            .await;
    }
    let checkpoint = crate::checkpoint::Checkpoint {
        turn_id: turn(job.request.op_id, job.revision),
        reference: reference(job, &captured.sha),
        sha: captured.sha,
        unmerged_paths: captured.unmerged_paths,
        live_processes: false,
    };
    let files = Files {
        summary_id: Uuid::new_v5(
            &job.request.op_id,
            format!("summary:{}", checkpoint.sha).as_bytes(),
        ),
        checkpoint,
        lfs_directory: captured.lfs_directory,
    };
    journal
        .fresh(Change::Files {
            id: job.request.op_id,
            revision: job.revision,
            files,
        })
        .await
}
pub(crate) async fn publish(journal: &Journal, job: &Job) -> Result<Job> {
    if job.phase != Phase::PublishingFiles {
        return Err("This Start fresh snapshot is not ready to publish.".into());
    }
    let frozen = job.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        check_repository(&frozen)?;
        let files = frozen
            .files
            .as_ref()
            .ok_or("Missing Start fresh file snapshot.")?;
        let repo = Repository::open(&frozen.workspace.path)?;
        let sha = files.checkpoint.sha.parse()?;
        match repo.reference_target(&files.checkpoint.reference)? {
            None => repo.compare_and_swap(&files.checkpoint.reference, None, sha)?,
            Some(current) if current == sha => {}
            Some(_) => return Err("The Start fresh file snapshot reference changed.".into()),
        }
        repo.sync_ref(&files.checkpoint.reference)?;
        Ok(())
    })
    .await??;
    journal
        .fresh(Change::Published {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await
}

/// Recheck current bytes after summarizing. Late external edits require a new
/// snapshot and a new summary; this path never resets the shared worktree.
pub(crate) async fn transfer(journal: &Journal, job: &Job) -> Result<Job> {
    let before = inspect(job).await?;
    if !before.clear() && job.access_consent.as_deref() != Some(&before.fingerprint) {
        return journal
            .fresh(Change::RefreshFiles {
                id: job.request.op_id,
                revision: job.revision,
                inspection: before,
            })
            .await;
    }
    let frozen = job.clone();
    tokio::task::spawn_blocking(move || check_repository(&frozen)).await??;
    let files = job.files.as_ref().ok_or("Missing final handoff files.")?;
    let current = crate::checkpoint::capture(
        &job.workspace,
        &files.checkpoint.sha,
        "Intercat handoff ownership verification",
        journal,
    )
    .await?;
    let path = job.workspace.path.clone();
    let expected = files.checkpoint.sha.parse()?;
    let actual = current.sha.parse()?;
    let equal = tokio::task::spawn_blocking(move || -> Result<bool> {
        let repo = Repository::open(&path)?;
        Ok(repo.tree_id(expected)? == repo.tree_id(actual)?)
    })
    .await??;
    let after = inspect(job).await?;
    if !equal || !current.unmerged_paths.is_empty() || before.fingerprint != after.fingerprint {
        return journal
            .fresh(Change::RefreshFiles {
                id: job.request.op_id,
                revision: job.revision,
                inspection: after,
            })
            .await;
    }
    journal
        .fresh(Change::Transfer {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await
}
pub(crate) async fn publish_owner(journal: &Journal, job: &Job) -> Result<Job> {
    let workspace = journal.workspace(job.successor_track.clone()).await?;
    if workspace.state != "fresh-publishing" || workspace.path != job.workspace.path {
        return Err("The reserved successor workspace changed.".into());
    }
    let frozen = job.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        check_repository(&frozen)?;
        let repo = Repository::open(&frozen.workspace.path)?;
        let files = frozen
            .files
            .as_ref()
            .ok_or("Missing final handoff files.")?;
        let sha = files.checkpoint.sha.parse()?;
        if repo.reference_target(&files.checkpoint.reference)? != Some(sha) {
            return Err("The final handoff snapshot changed.".into());
        }
        match repo.reference_target(&workspace.reference)? {
            None => repo.compare_and_swap(&workspace.reference, None, sha)?,
            Some(current) if current == sha => {}
            Some(_) => return Err("The successor workspace reference changed.".into()),
        };
        repo.sync_ref(&workspace.reference)?;
        Ok(())
    })
    .await??;
    journal
        .fresh(Change::OwnerPublished {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await
}
