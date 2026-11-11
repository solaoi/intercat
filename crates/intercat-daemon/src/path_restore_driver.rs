//! A Restore request outlives its window and resumes its immutable file intent.
use crate::{
    journal::Journal,
    path_restore::{Job, Phase},
    path_restore_store::Change,
    runtime::Service,
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
            let mut active = HashSet::new();
            let mut attempted = HashMap::<Uuid, Instant>::new();
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                changes.borrow_and_update();
                if *stopping.borrow() || *external.borrow() {
                    break;
                }
                if let Ok(snapshot) = journal.snapshot().await
                    && let Ok(jobs) =
                        serde_json::from_value::<Vec<Job>>(snapshot["pathRestores"].clone())
                {
                    for job in jobs {
                        let id = job.request.op_id;
                        let missing_merge =
                            job.phase == Phase::NeedsMerge && job.merge_input.is_none();
                        if !(missing_merge
                            || matches!(
                                job.phase,
                                Phase::Queued
                                    | Phase::WaitingForWorker
                                    | Phase::WaitingForProcesses
                                    | Phase::Applying
                            ))
                            || active.contains(&id)
                            || attempted
                                .get(&id)
                                .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
                        {
                            continue;
                        }
                        let (service, journal) = (service.clone(), journal.clone());
                        active.insert(id);
                        attempted.insert(id, Instant::now());
                        tasks.spawn(async move {
                            if let Err(error)=service.apply_path_restore(job.request.clone()).await {
                                let (phase,inspection)=if let Some(required)=error.downcast_ref::<crate::workspace_access::ConfirmationRequired>() {
                                    (Phase::WaitingForAccess,Some(required.inspection.clone()))
                                } else if error.is::<crate::path_restore::MergeRequired>() {(Phase::NeedsMerge,None)}
                                else if error.downcast_ref::<crate::inheritance_issue::Blocker>().is_some_and(|blocker|matches!(blocker,crate::inheritance_issue::Blocker::Processes)) {(Phase::WaitingForProcesses,None)}
                                else if error.is::<crate::path_restore::Waiting>() || error.is::<crate::journal::StartSuperseded>() {(Phase::WaitingForWorker,None)}
                                else {(Phase::Failed,None)};
                                let _=journal.change_path_restore(Change::Progress{request:job.request,phase,error:Some(error.to_string()),inspection}).await;
                            }
                            id
                        });
                    }
                }
                tokio::select! {
                    _=stopping.changed()=>{},_=external.changed()=>{},_=changes.changed()=>{},_=timer.tick()=>{},
                    done=tasks.join_next(),if !tasks.is_empty()=>{if let Some(Ok(id))=done {active.remove(&id);}}
                }
            }
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        });
        Self { stop, task }
    }
    pub(crate) async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}
