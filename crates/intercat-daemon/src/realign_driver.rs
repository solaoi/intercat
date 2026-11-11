//! Resume explicit requests at worker boundaries. Keep and Wait retain the
//! request while ordinary work continues; no window needs to stay open.
use crate::{
    Result,
    journal::Journal,
    realign::{Job, Phase},
    realign_queue::Change,
    runtime::{Service, WorkerState},
};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};
use tokio::sync::watch;
use uuid::Uuid;

pub(crate) struct Driver {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}
impl Driver {
    pub(crate) fn start(
        service: Service,
        journal: Journal,
        mut external: watch::Receiver<bool>,
    ) -> Self {
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut changes = journal.subscribe();
            let mut timer = tokio::time::interval(Duration::from_millis(250));
            let mut jobs = tokio::task::JoinSet::new();
            let mut active = HashMap::new();
            let mut ids = HashSet::new();
            let mut attempted = HashMap::<Uuid, Instant>::new();
            let mut unsaved = HashMap::<Uuid, (Job, Outcome)>::new();
            loop {
                changes.borrow_and_update();
                if *stopping.borrow() || *external.borrow() {
                    break;
                }
                for (id, (job, outcome)) in unsaved.clone() {
                    if attempted
                        .get(&id)
                        .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
                    {
                        continue;
                    }
                    attempted.insert(id, Instant::now());
                    if finish(&journal, &job, &outcome).await.is_ok() {
                        unsaved.remove(&id);
                        ids.remove(&id);
                    }
                }
                if let Ok(snapshot) = journal.snapshot().await {
                    let Ok(requests) =
                        serde_json::from_value::<Vec<Job>>(snapshot["realignments"].clone())
                    else {
                        break;
                    };
                    for job in requests {
                        let id = job.request.op_id;
                        if ids.contains(&id)
                            || attempted
                                .get(&id)
                                .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
                            || !matches!(
                                job.phase,
                                Phase::Queued
                                    | Phase::WaitingForWorker
                                    | Phase::WaitingProcesses
                                    | Phase::WaitingAccess
                                    | Phase::StoppingProcesses
                            )
                        {
                            continue;
                        }
                        let worker = snapshot["workers"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|w| w["trackId"] == job.request.track_id)
                            .cloned()
                            .and_then(|w| serde_json::from_value::<WorkerState>(w).ok());
                        let Some(worker) = worker else {
                            continue;
                        };
                        if worker.active_turn.is_some()
                            || worker.checkpoint_pending
                            || !worker.approvals.is_empty()
                            || !worker.prompts.is_empty()
                        {
                            continue;
                        }
                        let host = worker.identity.filter(|id| crate::process::is_alive(*id));
                        let released = matches!(worker.phase.as_str(), "sleeping" | "stopped");
                        if host.is_none() && !released {
                            continue;
                        }
                        attempted.insert(id, Instant::now());
                        if job.phase == Phase::WaitingProcesses
                            && let Some(host) = host
                            && service
                                .remaining_realign_processes(&job.request.track_id, host)
                                .await
                                .unwrap_or(true)
                        {
                            continue;
                        }
                        if job.phase == Phase::WaitingAccess {
                            let root = snapshot["workspaces"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .find(|w| w["trackId"] == job.request.track_id)
                                .and_then(|w| w["path"].as_str())
                                .map(std::path::PathBuf::from);
                            let Some(root) = root else {
                                continue;
                            };
                            let inspection = tokio::task::spawn_blocking(move || {
                                let mut excluded =
                                    vec![crate::process::identity(std::process::id())?];
                                excluded.extend(host);
                                crate::workspace_access::inspect(&root, &excluded)
                            })
                            .await;
                            let Ok(Ok(inspection)) = inspection else {
                                continue;
                            };
                            if !inspection.clear() {
                                let _ = journal
                                    .change_realign_queue(Change::Inspection {
                                        id,
                                        revision: job.revision,
                                        value: inspection,
                                    })
                                    .await;
                                continue;
                            }
                        }
                        if host.is_none() {
                            // Waiting itself must not restart an idle SDK host.
                            // Reopen its saved session only after its blocker ends.
                            if matches!(job.phase, Phase::WaitingAccess | Phase::WaitingProcesses) {
                                let _ = journal
                                    .change_realign_queue(Change::Wake {
                                        id,
                                        revision: job.revision,
                                    })
                                    .await;
                            }
                            continue;
                        }
                        let claimed = journal
                            .change_realign_queue(Change::Claim {
                                id,
                                revision: job.revision,
                            })
                            .await;
                        let Ok(Some(claimed)) = claimed else {
                            continue;
                        };
                        let service = service.clone();
                        let session = worker.session_id;
                        let work = claimed.clone();
                        let task = jobs.spawn(async move {
                            execute(&service, &work, session.as_deref()).await
                        });
                        ids.insert(id);
                        active.insert(task.id(), claimed);
                    }
                }
                tokio::select! {
                    _=stopping.changed()=>{},_=external.changed()=>{},_=changes.changed()=>{},_=timer.tick()=>{},
                    done=jobs.join_next_with_id(),if !jobs.is_empty()=>{
                        if let Some(done)=done {
                            let task=match &done {Ok((id,_))=>*id,Err(error)=>error.id()};
                            if let Some(job)=active.remove(&task) {
                                let id=job.request.op_id;
                                let outcome=match done {Ok((_,outcome))=>outcome,Err(_)=>(Phase::Held,Some("The realignment task stopped · Continue when ready".into()),None,true)};
                                if finish(&journal,&job,&outcome).await.is_err() {unsaved.insert(id,(job,outcome));}
                                else {ids.remove(&id);}
                            }
                        }
                    }
                }
            }
            while let Some(done) = jobs.join_next_with_id().await {
                let task = match &done {
                    Ok((id, _)) => *id,
                    Err(error) => error.id(),
                };
                if let Some(job) = active.remove(&task) {
                    let outcome = match done {
                        Ok((_, outcome)) => outcome,
                        Err(_) => (
                            Phase::Held,
                            Some("Realignment was interrupted · Continue when ready".into()),
                            None,
                            true,
                        ),
                    };
                    let _ = finish(&journal, &job, &outcome).await;
                }
            }
            for (_, (job, outcome)) in unsaved {
                let _ = finish(&journal, &job, &outcome).await;
            }
        });
        Self { stop, task }
    }
    pub(crate) async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}
type Outcome = (
    Phase,
    Option<String>,
    Option<crate::workspace_access::Inspection>,
    bool,
);
async fn finish(journal: &Journal, job: &Job, outcome: &Outcome) -> Result<()> {
    let (phase, error, inspection, consume_stop) = outcome.clone();
    journal
        .change_realign_queue(Change::Finish {
            id: job.request.op_id,
            revision: job.revision,
            phase,
            error,
            inspection,
            consume_stop,
        })
        .await?;
    Ok(())
}
async fn execute(service: &Service, job: &Job, session: Option<&str>) -> Outcome {
    if session != Some(&job.session_id) {
        return (
            Phase::Failed,
            Some("The worker has a new session. Review realignment again.".into()),
            None,
            true,
        );
    }
    let consume_stop = job.stop_command.is_some();
    if let Some(command) = job.stop_command
        && let Err(error) = service
            .stop_realign_processes(job.request.clone(), command)
            .await
    {
        return (Phase::NeedsProcesses, Some(error.to_string()), None, true);
    }
    let result: Result<()> = service.realign(job.request.clone()).await;
    match result {
        Ok(()) => (Phase::Done, None, None, consume_stop),
        Err(error) => {
            let phase = if error.is::<crate::realign_merge::Required>()
                || error.is::<crate::shared_context_service::Required>()
            {
                Phase::NeedsMerge
            } else if error.is::<crate::realign::Waiting>() {
                Phase::WaitingForWorker
            } else if error.is::<crate::journal::StartSuperseded>() {
                Phase::Held
            } else if matches!(
                error.downcast_ref::<crate::inheritance_issue::Blocker>(),
                Some(crate::inheritance_issue::Blocker::Processes)
            ) {
                if job.keep_processes {
                    Phase::WaitingProcesses
                } else {
                    Phase::NeedsProcesses
                }
            } else if error.is::<crate::workspace_access::ConfirmationRequired>() {
                if job.wait_for_access {
                    Phase::WaitingAccess
                } else {
                    Phase::NeedsAccess
                }
            } else {
                Phase::Failed
            };
            let inspection = error
                .downcast_ref::<crate::workspace_access::ConfirmationRequired>()
                .map(|required| required.inspection.clone());
            let error = (!matches!(
                phase,
                Phase::NeedsProcesses
                    | Phase::WaitingProcesses
                    | Phase::NeedsAccess
                    | Phase::WaitingAccess
            ))
            .then(|| error.to_string());
            (phase, error, inspection, consume_stop)
        }
    }
}
