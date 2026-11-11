//! Resume each Start fresh source independently. Only one callback owns a
//! source revision; final file intents and refs survive process restarts.
use crate::{
    fresh::{Change, Phase},
    journal::Journal,
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
                    for status in snapshot["freshTracks"].as_array().into_iter().flatten() {
                        let Some(id) = status["request"]["opId"]
                            .as_str()
                            .and_then(|value| value.parse::<Uuid>().ok())
                        else {
                            continue;
                        };
                        let Ok(phase) = serde_json::from_value::<Phase>(status["phase"].clone())
                        else {
                            continue;
                        };
                        if active.contains(&id)
                            || attempted
                                .get(&id)
                                .is_some_and(|time| time.elapsed() < Duration::from_secs(1))
                            || matches!(
                                phase,
                                Phase::Cancelled
                                    | Phase::OwnerHeld
                                    | Phase::Transferred
                                    | Phase::FilesHeld
                                    | Phase::SummaryHeld
                            )
                        {
                            continue;
                        }
                        if phase == Phase::WaitingForTurn
                            && snapshot["workers"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .any(|worker| {
                                    worker["trackId"] == status["request"]["trackId"]
                                        && (!worker["activeTurn"].is_null()
                                            || worker["checkpointPending"] == true
                                            || worker["approvals"]
                                                .as_array()
                                                .is_some_and(|value| !value.is_empty())
                                            || worker["prompts"]
                                                .as_array()
                                                .is_some_and(|value| !value.is_empty()))
                                })
                        {
                            continue;
                        }
                        let (journal, service) = (journal.clone(), service.clone());
                        active.insert(id);
                        attempted.insert(id, Instant::now());
                        let spawned = tasks.spawn(async move {
                            if let Ok(Some(job)) = journal.fresh_job(id).await {
                                let revision = job.revision;
                                if let Err(error) = service.advance_fresh(job).await
                                    && !error.is::<crate::fresh::Waiting>()
                                {
                                    let _ = journal
                                        .fresh(Change::Failed {
                                            id,
                                            revision,
                                            error: error.to_string(),
                                        })
                                        .await;
                                }
                            }
                            id
                        });
                        owners.insert(spawned.id(), id);
                    }
                }
                tokio::select! {
                    _=stopping.changed()=>{},_=external.changed()=>{},_=changes.changed()=>{},_=timer.tick()=>{},
                    done=tasks.join_next(),if !tasks.is_empty()=>{match done {Some(Ok(id))=>{active.remove(&id);owners.retain(|_,owner|*owner!=id);},Some(Err(error))=>{if let Some(id)=owners.remove(&error.id()){active.remove(&id);}},None=>{}}}
                }
            }
            while tasks.join_next().await.is_some() {}
        });
        Self { stop, task }
    }
    pub(crate) async fn shutdown(self) {
        self.stop.send_replace(true);
        let _ = self.task.await;
    }
}
