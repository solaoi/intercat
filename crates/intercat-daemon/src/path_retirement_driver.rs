//! Stop confirmed retirement targets independently, protect their last saved
//! points, then release the waiting publication. No window owns this work.
use crate::{
    Result,
    journal::Journal,
    path_retirement::{Change, Entry, Phase},
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
            let mut attempted = HashMap::<Uuid, Instant>::new();
            loop {
                changes.borrow_and_update();
                if *stopping.borrow() || *external.borrow() {
                    break;
                }
                if let Ok(snapshot) = journal.snapshot().await
                    && let Ok(entries) =
                        serde_json::from_value::<Vec<Entry>>(snapshot["pathRetirements"].clone())
                {
                    if let Ok(stops) = serde_json::from_value::<Vec<crate::aside_processes::Status>>(
                        snapshot["asideProcessStops"].clone(),
                    ) {
                        for status in stops {
                            let id = status.record.request.op_id;
                            if active.contains(&id)
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
                                if let Err(error) =
                                    service.finish_aside_processes(status.record.request).await
                                {
                                    let _ = journal
                                        .change_aside_processes(
                                            crate::aside_processes::Change::Failed {
                                                operation: id,
                                                error: error.to_string(),
                                            },
                                        )
                                        .await;
                                }
                                (id, Ok(()))
                            });
                        }
                    }
                    for entry in entries {
                        if !matches!(entry.phase, Phase::Stopping | Phase::Saving)
                            || active.contains(&entry.id)
                            || attempted
                                .get(&entry.id)
                                .is_some_and(|at| at.elapsed() < Duration::from_secs(1))
                        {
                            continue;
                        }
                        let service = service.clone();
                        let journal = journal.clone();
                        active.insert(entry.id);
                        attempted.insert(entry.id, Instant::now());
                        tasks.spawn(async move {
                            let id = entry.id;
                            let result = execute(&service, &journal, entry).await;
                            (id, result)
                        });
                    }
                }
                tokio::select! {
                    _=stopping.changed()=>{},_=external.changed()=>{},_=changes.changed()=>{},_=timer.tick()=>{},
                    done=tasks.join_next(),if !tasks.is_empty()=>{
                        if let Some(Ok((id,result)))=done {
                            active.remove(&id);
                            if let Err(error)=result {let _=journal.change_path_retirement(Change::Failed{id,error:error.to_string()}).await;}
                        }
                    }
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
async fn execute(service: &Service, journal: &Journal, mut entry: Entry) -> Result<()> {
    if entry.phase == Phase::Stopping {
        let (session, checkpoint) = service.stop_path_boundary(entry.id, &entry.track).await?;
        journal
            .change_path_retirement(Change::Freeze {
                id: entry.id,
                session,
                checkpoint,
            })
            .await?;
        let snapshot = journal.snapshot().await?;
        entry = serde_json::from_value::<Vec<Entry>>(snapshot["pathRetirements"].clone())?
            .into_iter()
            .find(|saved| saved.id == entry.id)
            .ok_or("The path retirement is no longer active.")?;
    }
    if entry.phase != Phase::Saving {
        return Ok(());
    }
    let saved = entry
        .saved
        .clone()
        .ok_or("Missing frozen path retirement.")?;
    let id = entry.id;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = intercat_git::Repository::open(&entry.project_root)?;
        if !repo.owns_session(&entry.slug, &entry.session)? {
            return Err("The retired path's repository reservation changed.".into());
        }
        if let Some(checkpoint) = &saved.checkpoint
            && repo.reference_target(&checkpoint.reference)? != Some(checkpoint.sha.parse()?)
        {
            return Err("The retired path's saved checkpoint changed.".into());
        }
        intercat_git::validate_reference(&saved.reference)?;
        let sha = saved.sha.parse()?;
        if repo.reference_target(&saved.reference)? != Some(sha) {
            repo.compare_and_swap(&saved.reference, None, sha)?;
        }
        repo.sync_ref(&saved.reference)?;
        Ok(())
    })
    .await??;
    journal.change_path_retirement(Change::Ready(id)).await
}
