//! Freeze local handoff evidence; reuse the tool-free summary engine. Job and
//! application Stop changes cancel the host as well as future turn admission.
use crate::{
    Result,
    fresh::{Job, Phase},
    journal::Journal,
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};
use tokio::sync::watch;

fn changed(repo: &intercat_git::Repository, before: &str, after: &str) -> Result<Vec<Value>> {
    let before = repo.saved_files(before.parse()?)?;
    let after = repo.saved_files(after.parse()?)?;
    let paths = before.keys().chain(after.keys()).collect::<BTreeSet<_>>();
    let mut changes = vec![];
    for path in paths {
        let kind = match (before.get(path), after.get(path)) {
            (None, Some(_)) => "added",
            (Some(_), None) => "deleted",
            (Some(a), Some(b)) if a.object != b.object || a.mode != b.mode => "modified",
            _ => continue,
        };
        changes.push(json!({"path":path,"kind":kind}));
    }
    Ok(changes)
}
pub(crate) async fn changes(job: &Job) -> Result<Value> {
    let frozen = job.clone();
    tokio::task::spawn_blocking(move||->Result<Value> {
        let files=frozen.files.as_ref().ok_or("Missing final handoff files.")?;
        let boundary=frozen.boundary.as_ref().ok_or("Missing saved handoff boundary.")?;
        let repo=intercat_git::Repository::open(&frozen.workspace.path)?;
        if repo.reference_target(&files.checkpoint.reference)?!=Some(files.checkpoint.sha.parse()?) {return Err("The final handoff files changed.".into());}
        Ok(json!({"address":files.checkpoint.reference,"sinceSavedTurn":changed(&repo,&boundary.checkpoint.sha,&files.checkpoint.sha)?,"currentFilesVsWorkspaceBase":{"authorship":"These are current file differences, including inherited work; not proof of who authored them.","changes":changed(&repo,&frozen.workspace.base,&files.checkpoint.sha)?}}))
    }).await?
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    journal: &Journal,
    job: &Job,
    config: &crate::runtime::Config,
    data: &Path,
    rpc: &Path,
    memory: Arc<crate::host_memory::Pool>,
    capacity: Arc<crate::scheduler::Capacity>,
    paused: Arc<crate::dispatch_control::Gate>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<crate::handoff::Document> {
    if paused.paused() || *shutdown.borrow() {
        return Err(crate::fresh::Waiting("Resume before making the handoff summary.").into());
    }
    let epoch = paused.epoch();
    let source = crate::handoff::Source {
        id: job.summary_id.ok_or("Missing frozen summary identity.")?,
        identity: crate::handoff::Identity {
            track_id: job.request.track_id.clone(),
            session_id: job.request.session_id.clone(),
        },
        context: job
            .summary_input
            .clone()
            .ok_or("Missing frozen summary input.")?,
    };
    let (cancel, stopping) = watch::channel(false);
    let (finish, mut finished) = watch::channel(false);
    let saved = journal.clone();
    let id = job.request.op_id;
    let revision = job.revision;
    let gate = paused.clone();
    let monitor_cancel = cancel.clone();
    let monitor = tokio::spawn(async move {
        let mut changes = saved.subscribe();
        let mut timer = tokio::time::interval(Duration::from_millis(25));
        loop {
            if *finished.borrow() {
                return;
            }
            let current = saved.fresh_job(id).await;
            if *shutdown.borrow()
                || gate.paused()
                || gate.epoch() != epoch
                || !current.is_ok_and(|job| {
                    job.is_some_and(|job| {
                        job.phase == Phase::Summarizing && job.revision == revision
                    })
                })
            {
                monitor_cancel.send_replace(true);
                return;
            }
            tokio::select! {_=shutdown.changed()=>{},_=finished.changed()=>{},_=changes.changed()=>{},_=timer.tick()=>{}}
        }
    });
    let guard = crate::msp::StartGuard::Background {
        stopping: stopping.clone(),
        paused: paused.clone(),
        pause_epoch: epoch,
    };
    let result = crate::handoff_run::run(
        memory,
        journal,
        config,
        data,
        rpc,
        &source,
        capacity,
        guard.clone(),
        stopping,
    )
    .await;
    finish.send_replace(true);
    let _ = monitor.await;
    let admitted = guard.current();
    drop(cancel);
    if !admitted {
        return Err(crate::msp::StartCancelled.into());
    }
    result
}
