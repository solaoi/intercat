//! Capture a closed repair host and admit a rebuilt candidate under one durable
//! reservation. Only the hidden staging ref changes; publication stays explicit.
use crate::{Result, journal::Journal, repair_run::Run, run_workspace::Workspace};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::watch;

pub(crate) async fn apply_source(
    journal: &Journal,
    integration: &crate::integration::Service,
    run: &Run,
    workspace: Arc<Workspace>,
    root: PathBuf,
    cancel: watch::Receiver<bool>,
) -> Result<Run> {
    if matches!(&*workspace, Workspace::Restore(_)) {
        let (journal, run) = (journal.clone(), run.clone());
        return tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                let Workspace::Restore(merge) = &*workspace else {
                    unreachable!()
                };
                crate::restore_merge_apply::apply(&journal, &run, merge, cancel).await
            })
        })
        .await?;
    }
    if matches!(&*workspace, Workspace::Shared(_)) {
        let journal = journal.clone();
        let run = run.clone();
        return tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                let Workspace::Shared(merge) = &*workspace else {
                    unreachable!()
                };
                crate::shared_merge_apply::apply(&journal, &run, merge, cancel).await
            })
        })
        .await?;
    }
    if matches!(&*workspace, Workspace::Undo(_)) {
        let source = workspace.clone();
        let saved_journal = journal.clone();
        let saved_run = run.clone();
        let capture_cancel = cancel.clone();
        let done = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                let Workspace::Undo(merge) = &*source else {
                    unreachable!()
                };
                crate::undo_merge_apply::apply(&saved_journal, &saved_run, merge, capture_cancel)
                    .await
            })
        })
        .await??;
        if !*cancel.borrow() {
            integration
                .finish_undo_merge_result(uuid::Uuid::new_v5(&run.id, b"apply-undo-merge"))
                .await?;
        }
        return Ok(done);
    }
    if matches!(&*workspace, Workspace::Realign(_)) {
        let journal = journal.clone();
        let run = run.clone();
        return tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                let Workspace::Realign(merge) = &*workspace else {
                    unreachable!()
                };
                crate::realign_merge_apply::apply(&journal, &run, merge, cancel).await
            })
        })
        .await?;
    }
    let (_, job, _) = journal.repair_input(run.id).await?;
    let candidate = job
        .candidate
        .as_ref()
        .ok_or("Missing captured candidate.")?
        .workspace
        .clone();
    let parent = root.join(job.id.to_string());
    apply(
        journal,
        run,
        workspace,
        candidate,
        parent.join(format!("captured-{}", run.id)),
        root.join("downloads"),
        cancel,
    )
    .await
}

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
            let plan = match (&*workspace, &applying.manual) {
                (Workspace::Merge(merge), Some(snapshot)) => crate::run_workspace::Saved::Merge(
                    merge
                        .resolve(snapshot, &input, &settings, cancel.clone())
                        .await?,
                ),
                (_, None) => {
                    workspace
                        .finish(&input, &candidate_root, &settings, cancel.clone())
                        .await?
                }
                _ => return Err("The manual result belongs to another Run purpose.".into()),
            };
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
