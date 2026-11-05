//! Capture a closed repair host and admit a rebuilt candidate under one durable
//! reservation. Only the hidden staging ref changes; publication stays explicit.
use crate::{Result, journal::Journal, repair::Workspace, repair_run::Run};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::watch;

pub async fn apply(
    journal: &Journal,
    run: &Run,
    workspace: Arc<Workspace>,
    candidate_root: PathBuf,
    rebuilt_root: PathBuf,
    cache_root: PathBuf,
    cancel: watch::Receiver<bool>,
) -> Result<Run> {
    let (applying, input, settings) = journal.repair_begin_apply(run.id, run.revision).await?;
    let staging = input.clone();
    let result = tokio::task::spawn_blocking(move || {
        tokio::runtime::Handle::current().block_on(async move {
            let plan = workspace
                .finish(&input, &candidate_root, &settings, cancel.clone())
                .await?;
            workspace
                .rebuild(&input, &plan, &rebuilt_root, &cache_root, &settings, cancel)
                .await
        })
    })
    .await;
    let result = match result {
        Ok(Ok(candidate)) => {
            match crate::integration_refs::stage(&staging, &candidate, journal).await {
                Ok(()) => Ok(candidate),
                Err(_) => Err("The rebuilt candidate could not be staged. Review the integration before retrying.".into()),
            }
        }
        // Capture/checkout failures may contain file content. Only the safe
        // candidate pipeline returns detailed redacted diagnostics.
        _ => Err(
            "The repair could not be captured or rebuilt. Its private edits were preserved.".into(),
        ),
    };
    journal
        .repair_applied(applying.id, applying.revision, result)
        .await
}
