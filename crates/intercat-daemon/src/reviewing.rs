//! Review admission and publication. Fix scheduling is separate from reading.
use crate::{
    Result,
    journal::Journal,
    reviews::{Draft, Finished, Job, State},
    scheduler::{Capacity, Priority},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{sync::watch, task::JoinSet};
use uuid::Uuid;

pub(crate) async fn publish(journal: &Journal, job: &Job, draft: Draft) -> Result<bool> {
    let screened = journal
        .screen_start(
            serde_json::json!({"input":[{"type":"text","text":serde_json::to_string(&draft)?}]}),
        )
        .await?;
    let draft: Draft = serde_json::from_str(
        screened.params()["input"][0]["text"]
            .as_str()
            .ok_or("Missing screened review.")?,
    )?;
    crate::reviews::validate(&draft, &job.target)?;
    loop {
        journal.review_context(job.clone()).await?;
        let state = journal.snapshot().await?;
        let latest = state["workers"]
            .as_array()
            .and_then(|workers| workers.iter().find(|w| w["trackId"] == job.target.track_id))
            .and_then(|worker| worker["checkpoint"]["sha"].as_str())
            .ok_or("The current checkpoint is unavailable.")?
            .to_owned();
        let copy = job.clone();
        let notes = draft.notes.clone();
        let current = latest.clone();
        let outdated = tokio::task::spawn_blocking(move || -> Result<Vec<bool>> {
            notes
                .iter()
                .map(|note| {
                    crate::reviewer::outdated(
                        &copy.target.workspace.path,
                        &copy.target.checkpoint.sha,
                        &current,
                        note,
                    )
                })
                .collect()
        })
        .await??;
        if journal
            .finish_review(
                job.clone(),
                Finished::Complete {
                    draft: draft.clone(),
                    checked_sha: latest,
                    outdated,
                },
            )
            .await?
        {
            return Ok(true);
        }
        if !journal.reviews().await?.iter().any(|value| {
            value.id == job.id
                && value.generation == job.generation
                && value.revision == job.revision
                && value.state == State::Running
        }) {
            return Ok(false);
        }
    }
}
fn clean_previous(data: &Path) -> Result<()> {
    for (id, _, path) in crate::process_ledger::host_paths(data)? {
        if !id.starts_with("review/") {
            continue;
        }
        let record = crate::process_ledger::read(&path.join(crate::process_ledger::NAME))?;
        crate::process::Tracker::restore_host(&path, None)?.stop_confirmed()?;
        std::fs::remove_dir_all(path)?;
        let parent = data.join("review-workspaces").canonicalize()?;
        if record.workspace.parent() == Some(parent.as_path())
            && record
                .workspace
                .file_name()
                .is_some_and(|s| s.to_string_lossy().starts_with("snapshot-"))
        {
            use std::os::unix::fs::PermissionsExt;
            for entry in walkdir::WalkDir::new(&record.workspace) {
                let entry = entry?;
                if entry.file_type().is_dir() {
                    std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o700))?;
                }
            }
            std::fs::remove_dir_all(record.workspace)?;
        }
    }
    Ok(())
}
struct Running {
    stop: watch::Sender<bool>,
    generation: Arc<AtomicU64>,
}
impl Running {
    fn stop(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.stop.send_replace(true);
    }
}
pub(crate) struct Service {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}
impl Service {
    pub fn start(
        host_memory: Arc<crate::host_memory::Pool>,
        journal: Journal,
        data: PathBuf,
        rpc: PathBuf,
        capacity: Arc<Capacity>,
        paused: Arc<crate::dispatch_control::Gate>,
    ) -> Self {
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let cleanup_data = data.clone();
            let cleanup = tokio::task::spawn_blocking(move || clean_previous(&cleanup_data)).await;
            let cleanup_failed = !matches!(cleanup, Ok(Ok(())));
            let mut changes = journal.subscribe();
            let mut active = HashMap::<Uuid, Running>::new();
            let mut tasks = JoinSet::new();
            loop {
                changes.borrow_and_update();
                let Ok(jobs) = journal.reviews().await else {
                    break;
                };
                for (id, running) in &active {
                    if paused.paused()
                        || !jobs.iter().any(|job| {
                            job.generation == *id
                                && matches!(job.state, State::Queued | State::Running)
                        })
                    {
                        running.stop();
                    }
                }
                let config = journal.worker_config().await.ok().flatten();
                if !paused.paused() {
                    for job in jobs.into_iter().filter(|job| job.state == State::Queued) {
                        if active.contains_key(&job.generation) {
                            continue;
                        }
                        let Some(config) = config.clone() else {
                            continue;
                        };
                        let (halt, mut halted) = watch::channel(false);
                        let generation = Arc::new(AtomicU64::new(0));
                        let guard = crate::msp::StartGuard::new(generation.clone(), paused.clone());
                        let id = job.generation;
                        active.insert(
                            id,
                            Running {
                                stop: halt,
                                generation,
                            },
                        );
                        let journal = journal.clone();
                        let data = data.clone();
                        let rpc = rpc.clone();
                        let capacity = capacity.clone();
                        let host_memory = host_memory.clone();
                        tasks.spawn(async move {
                            let _permit=tokio::select!{biased;_=halted.changed()=>return id,p=capacity.request(job.generation.to_string(),Priority::Plan).acquire()=>p};
                            let Ok(Some(job))=journal.begin_review(job.id,job.generation,job.revision).await else{return id};
                            let result=if cleanup_failed {Err("A previous Reviewer could not be stopped.".into())}
                                else {crate::reviewer::run(host_memory, &journal,&config,&data,&rpc,&job,guard,halted.clone()).await};
                            if *halted.borrow() {let _=journal.finish_review(job,Finished::Held).await;}
                            else {match result {
                                Ok(draft)=>{if publish(&journal,&job,draft).await.is_err(){let _=journal.finish_review(job,Finished::Failed("Review could not be saved. Retry to review the checkpoint.".into())).await;}},
                                Err(_)=>{let _=journal.finish_review(job,Finished::Failed("Review could not finish. Retry to review the checkpoint.".into())).await;}
                            }}
                            id
                        });
                    }
                }
                tokio::select! {biased;
                    _=stopping.changed()=>break,
                    done=tasks.join_next(),if !tasks.is_empty()=>{if let Some(Ok(id))=done{active.remove(&id);}},
                    changed=changes.changed()=>{if changed.is_err(){break;}}
                }
            }
            for running in active.values() {
                running.stop();
            }
            while tasks.join_next().await.is_some() {}
        });
        Self { stop, task }
    }
    pub async fn shutdown(self) {
        self.stop.send_replace(true);
        let _ = self.task.await;
    }
}
