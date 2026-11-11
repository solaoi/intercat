//! Freeze initial files without inventing a completed Muse turn.
use crate::{
    Result,
    branch_start::Initial,
    journal::{Admission, Journal},
    two_ways::{Change, Job},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Intent {
    pub job: Uuid,
    pub track_id: String,
    pub session_id: String,
    pub snapshot: Initial,
    pub lfs_directory: std::path::PathBuf,
}
pub(crate) async fn capture(journal: Journal, job: Job) -> Result<Job> {
    // Ownership includes publication and the reducer update, even if the caller leaves.
    tokio::spawn(async move {
        let workspace = journal.workspace(job.source_track.clone()).await?;
        let id = crate::branch_start::initial_operation(job.request.op_id);
        let reference = format!("{}-initial/{}", workspace.reference, job.request.op_id);
        let (intent, admission, _permit) = if let Some(operation) = journal.operation(id).await? {
            let intent: Intent = serde_json::from_value(operation["payload"].clone())?;
            if operation["kind"] != "two-ways.initial"
                || intent.job != job.request.op_id
                || intent.track_id != job.source_track
                || intent.session_id != job.source_session
                || intent.snapshot.operation != id
                || intent.snapshot.reference != reference
            {
                return Err("The initial-file intent belongs to another paired source.".into());
            }
            let state = operation["state"]
                .as_str()
                .filter(|s| matches!(*s, "prepared" | "dispatched" | "complete"))
                .ok_or("The initial-file intent cannot resume.")?;
            (intent, Admission::Replay(state.into()), None)
        } else {
            let (capture, permit) = crate::checkpoint::capture_owned(
                &workspace,
                &workspace.base,
                &format!("Intercat initial paired files {}\n", job.request.op_id),
                &journal,
            )
            .await?;
            if !capture.unmerged_paths.is_empty() {
                return Err("Resolve the initial files before trying two ways.".into());
            }
            let intent = Intent {
                job: job.request.op_id,
                track_id: job.source_track.clone(),
                session_id: job.source_session.clone(),
                snapshot: Initial {
                    operation: id,
                    sha: capture.sha,
                    reference,
                },
                lfs_directory: capture.lfs_directory,
            };
            let admission = journal
                .intent(
                    &id.to_string(),
                    "two-ways.initial",
                    serde_json::to_value(&intent)?,
                )
                .await?;
            (intent, admission, Some(permit))
        };
        let snapshot = intent.snapshot.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let repo = intercat_git::Repository::open(workspace.path)?;
            let sha = intent.snapshot.sha.parse()?;
            crate::lfs_store::sync(
                &intent.lfs_directory,
                &repo.lfs_entries(repo.tree_id(sha)?)?,
            )?;
            repo.compare_and_swap(&intent.snapshot.reference, None, sha)?;
            repo.sync_ref(&intent.snapshot.reference)?;
            Ok(())
        })
        .await??;
        match admission {
            Admission::New => {
                journal.finish(&id.to_string(), "dispatched").await?;
                journal.finish(&id.to_string(), "complete").await?;
            }
            Admission::Replay(state) if state == "prepared" => {
                journal.finish(&id.to_string(), "dispatched").await?;
                journal.finish(&id.to_string(), "complete").await?;
            }
            Admission::Replay(state) if state == "dispatched" => {
                journal.finish(&id.to_string(), "complete").await?
            }
            Admission::Replay(state) if state == "complete" => {}
            _ => return Err("The initial-file intent was superseded.".into()),
        }
        journal
            .two_ways(Change::InitialCaptured {
                id: job.request.op_id,
                revision: job.revision,
                snapshot,
            })
            .await
    })
    .await?
}
