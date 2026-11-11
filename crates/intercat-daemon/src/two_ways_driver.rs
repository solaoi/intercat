//! Preparation resumes from saved boundaries; model turns use ordinary workers.
use crate::{
    journal::Journal,
    runtime::Service,
    two_ways::{Change, Job, Phase, Waiting},
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
            let mut tasks = tokio::task::JoinSet::new();
            let mut active = HashSet::new();
            let mut owners = HashMap::new();
            let mut attempted = HashMap::<Uuid, Instant>::new();
            loop {
                changes.borrow_and_update();
                if *stopping.borrow() || *external.borrow() {
                    break;
                }
                if let Ok(snapshot) = journal.snapshot().await {
                    for value in snapshot["twoWays"].as_array().into_iter().flatten() {
                        let Ok(job) = serde_json::from_value::<Job>(value.clone()) else {
                            continue;
                        };
                        let id = job.request.op_id;
                        if !matches!(
                            job.phase,
                            Phase::CapturingInitial
                                | Phase::Inspecting
                                | Phase::InspectingConfirmedUsage
                                | Phase::Forking
                                | Phase::AwaitingReceipt
                                | Phase::Cancelling
                        ) || active.contains(&id)
                            || attempted
                                .get(&id)
                                .is_some_and(|time| time.elapsed() < Duration::from_secs(1))
                        {
                            continue;
                        }
                        let journal = journal.clone();
                        let service = service.clone();
                        active.insert(id);
                        attempted.insert(id, Instant::now());
                        let spawned = tasks.spawn(async move {
                            let revision = job.revision;
                            if let Err(error) = service.advance_two_ways(job).await
                                && !error.is::<Waiting>()
                            {
                                let _ = journal
                                    .two_ways(Change::Hold {
                                        id,
                                        revision,
                                        error: error.to_string(),
                                    })
                                    .await;
                            }
                            id
                        });
                        owners.insert(spawned.id(), id);
                    }
                }
                tokio::select! {
                    _=stopping.changed()=>{},_=external.changed()=>{},_=changes.changed()=>{},_=timer.tick()=>{},
                    done=tasks.join_next(),if !tasks.is_empty()=>{match done{Some(Ok(id))=>{active.remove(&id);owners.retain(|_,owner|*owner!=id);},Some(Err(error))=>{if let Some(id)=owners.remove(&error.id()){active.remove(&id);}},None=>{}}}
                }
            }
            while tasks.join_next().await.is_some() {}
        });
        Self { stop, task }
    }
    pub(crate) async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}
