//! Planner admission, cancellation and publication. Host I/O lives in planner.
use crate::{
    Result,
    journal::Journal,
    plan_store::Generated,
    plans::{GenerationState, Plan},
    scheduler::{Capacity, Priority},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{sync::watch, task::JoinSet};
use uuid::Uuid;

#[cfg(test)]
#[path = "planning_deadline_tests.rs"]
mod deadline_tests;

pub(crate) struct Service {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

#[allow(clippy::too_many_arguments)]
async fn generate(
    journal: Journal,
    data: PathBuf,
    rpc: PathBuf,
    capacity: Arc<Capacity>,
    host_memory: Arc<crate::host_memory::Pool>,
    paused: Arc<crate::dispatch_control::Gate>,
    config: crate::runtime::Config,
    plan: Plan,
    mut stopping: watch::Receiver<bool>,
) -> Result<()> {
    let _permit = tokio::select! { biased;
        _=stopping.changed()=>return Ok(()),
        permit=capacity.request(plan.source_id.clone(),Priority::Plan).acquire()=>permit,
    };
    if *stopping.borrow()
        || !journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?
    {
        return Ok(());
    }
    let Some(guard) = journal.plan_guard(&plan, paused).await? else {
        return Ok(());
    };
    let result = crate::planner::run(
        host_memory,
        &journal,
        &config,
        &data,
        &rpc,
        &plan,
        guard,
        stopping.clone(),
    )
    .await;
    if *stopping.borrow() {
        return Ok(());
    }
    let (value, based_on) = match result {
        Ok((draft, based_on)) => {
            // Protect the durable model result using the same registered-value
            // and Full-privacy inspection as other model-visible text.
            let inspected=journal.screen_start(serde_json::json!({"input":[{"type":"text","text":serde_json::to_string(&draft)?}]})).await?;
            let draft = serde_json::from_str(
                inspected.params()["input"][0]["text"]
                    .as_str()
                    .ok_or("Missing screened plan.")?,
            )?;
            (Generated::Draft(draft), based_on)
        }
        Err(error) if error.to_string() == "The Planner did not finish within 60 seconds." => {
            (Generated::TimedOut, vec![])
        }
        Err(error) if error.to_string() == "The Planner returned invalid JSON." => {
            (Generated::Invalid(error.to_string()), vec![])
        }
        Err(error) => (Generated::Failed(error.to_string()), vec![]),
    };
    journal
        .generated_plan(plan.id, plan.generation.id, plan.rev, value, based_on)
        .await?;
    Ok(())
}
fn clean_previous(data: &Path) -> Result<()> {
    for (id, _, path) in crate::process_ledger::host_paths(data)? {
        if !id.starts_with("planner/") {
            continue;
        }
        let record = crate::process_ledger::read(&path.join(crate::process_ledger::NAME))?;
        crate::process::Tracker::restore_host(&path, None)?.stop_confirmed()?;
        std::fs::remove_dir_all(path)?;
        let parent = data.join("planner-workspaces").canonicalize()?;
        if record.workspace.parent() == Some(parent.as_path())
            && record
                .workspace
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("snapshot-"))
        {
            for entry in walkdir::WalkDir::new(&record.workspace) {
                let entry = entry?;
                if entry.file_type().is_dir() {
                    std::fs::set_permissions(
                        entry.path(),
                        std::os::unix::fs::PermissionsExt::from_mode(0o700),
                    )?;
                }
            }
            std::fs::remove_dir_all(record.workspace)?;
        }
    }
    Ok(())
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
            let cleanup_error = match cleanup {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            let mut changes = journal.subscribe();
            let mut active: HashMap<Uuid, watch::Sender<bool>> = HashMap::new();
            let mut tasks = JoinSet::new();
            loop {
                changes.borrow_and_update();
                if journal.advance_plans().await.is_err() {
                    break;
                }
                let plans = match journal.plans().await {
                    Ok(plans) => plans,
                    Err(_) => break,
                };
                for (generation, stop) in &active {
                    if paused.paused()
                        || !plans.iter().any(|plan| {
                            plan.generation.id == *generation
                                && plan.can_plan()
                                && matches!(
                                    plan.generation.state,
                                    GenerationState::Queued | GenerationState::Running
                                )
                        })
                    {
                        stop.send_replace(true);
                    }
                }
                let config = journal.worker_config().await.ok().flatten();
                if !paused.paused() {
                    for plan in plans.into_iter().filter(|plan| {
                        plan.can_plan() && plan.generation.state == GenerationState::Queued
                    }) {
                        if active.contains_key(&plan.generation.id) {
                            continue;
                        }
                        if let Some(error) = &cleanup_error {
                            if journal
                                .begin_plan(plan.id, plan.generation.id, plan.rev)
                                .await
                                .unwrap_or(false)
                            {
                                let _ = journal
                                    .generated_plan(
                                        plan.id,
                                        plan.generation.id,
                                        plan.rev,
                                        Generated::Failed(format!(
                                            "A previous Planner host could not be stopped: {error}"
                                        )),
                                        vec![],
                                    )
                                    .await;
                            }
                            continue;
                        }
                        let Some(config) = config.clone() else {
                            continue;
                        };
                        let (halt, receiver) = watch::channel(false);
                        let generation = plan.generation.id;
                        active.insert(generation, halt);
                        let journal = journal.clone();
                        let data = data.clone();
                        let rpc = rpc.clone();
                        let capacity = capacity.clone();
                        let host_memory = host_memory.clone();
                        let paused = paused.clone();
                        tasks.spawn(async move {
                            let result=generate(journal.clone(),data,rpc,capacity,host_memory,paused,config,plan.clone(),receiver).await;
                            if result.is_err() {
                                let _=journal.generated_plan(plan.id,plan.generation.id,plan.rev,Generated::Failed("Planning could not finish. Retry to prepare a new plan.".into()),vec![]).await;
                            }
                            generation
                        });
                    }
                }
                tokio::select! { biased;
                    _=stopping.changed()=>break,
                    done=tasks.join_next(),if !tasks.is_empty()=>{if let Some(Ok(generation))=done {active.remove(&generation);}},
                    result=changes.changed()=>{if result.is_err(){break;}},
                }
            }
            for stop in active.values() {
                stop.send_replace(true);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::State;
    use crate::{
        decision::Target,
        plan_store::{Action, Command},
        request_sources::Submission,
        secrets::Key,
    };
    async fn input(journal: &Journal) -> Result<Plan> {
        journal
            .stage_document(Submission {
                op_id: Uuid::now_v7(),
                text: "Plan a dummy task.".into(),
                target: Target::New,
                selected_track_id: None,
                attachment_ids: vec![],
                sources: None,
                execute_allowed: false,
                plan_item: None,
            })
            .await?;
        Ok(journal.plans().await?.pop().unwrap())
    }
    #[tokio::test]
    async fn hold_invalidates_the_transport_guard_before_the_command_returns() -> Result<()> {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        let plan = input(&journal).await?;
        journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?;
        let gate = Arc::new(crate::dispatch_control::Gate::new(false));
        let guard = journal.plan_guard(&plan, gate).await?.unwrap();
        let crate::msp::StartGuard::Worker {
            generation,
            expected,
            ..
        } = guard
        else {
            panic!("Wrong guard");
        };
        assert_eq!(
            generation.load(std::sync::atomic::Ordering::Acquire),
            expected
        );
        journal
            .plan_command(Command {
                op_id: Uuid::now_v7(),
                plan_id: plan.id,
                revision: plan.rev,
                action: Action::Hold,
            })
            .await?;
        assert_ne!(
            generation.load(std::sync::atomic::Ordering::Acquire),
            expected
        );
        Ok(())
    }
    #[tokio::test]
    async fn snapshot_sources_are_saved_before_dispatch_and_old_generations_cannot_replace_them()
    -> Result<()> {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        let plan = input(&journal).await?;
        let sources = vec![crate::plans::BasedOn {
            folder_id: "dummy-folder".into(),
            track_id: None,
            sha: "a".repeat(40),
        }];
        assert!(!journal.plan_based_on(&plan, sources.clone()).await?);
        journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?;
        assert!(journal.plan_based_on(&plan, sources.clone()).await?);
        assert_eq!(journal.plans().await?[0].based_on, sources);
        journal
            .plan_command(Command {
                op_id: Uuid::now_v7(),
                plan_id: plan.id,
                revision: plan.rev,
                action: Action::Hold,
            })
            .await?;
        assert!(
            !journal
                .plan_based_on(
                    &plan,
                    vec![crate::plans::BasedOn {
                        folder_id: "dummy-folder".into(),
                        track_id: None,
                        sha: "b".repeat(40)
                    }]
                )
                .await?
        );
        assert_eq!(journal.plans().await?[0].based_on, sources);
        Ok(())
    }
    #[tokio::test]
    async fn stop_everything_cancels_a_queued_planner_without_spawning_a_host() -> Result<()> {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        journal
            .set_worker_config(
                Uuid::now_v7(),
                crate::runtime::Config {
                    source: root.path().join("never-run"),
                    binary: root.path().join("never-run"),
                    credentials: root.path().join("no-credentials"),
                    path: "/usr/bin:/bin".into(),
                },
            )
            .await?;
        let pool = Arc::new(Capacity::new(1));
        let occupied = pool
            .request("occupied".into(), Priority::User)
            .acquire()
            .await;
        let gate = Arc::new(crate::dispatch_control::Gate::new(false));
        let service = Service::start(
            crate::host_memory::Pool::new(),
            journal.clone(),
            root.path().into(),
            root.path().join("rpc"),
            pool.clone(),
            gate,
        );
        let plan = input(&journal).await?;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while pool.snapshot().waiting.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert_eq!(
            journal.plans().await?[0].generation.state,
            GenerationState::Queued
        );
        journal.pause_dispatch(Uuid::now_v7(), true).await?;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !pool.snapshot().waiting.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await?;
        assert_eq!(journal.plans().await?[0].state, State::Held);
        journal.pause_dispatch(Uuid::now_v7(), false).await?;
        service.shutdown().await;
        assert_eq!(pool.snapshot().active, 1);
        drop(occupied);
        assert_eq!(pool.snapshot().active, 0);
        assert!(!root.path().join("planner-hosts").exists());
        assert_eq!(journal.plans().await?[0].id, plan.id);
        Ok(())
    }
}
