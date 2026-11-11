//! Effects for retired integration hosts. A durable Cleaning state precedes
//! deleting private resources, and each process owner has one maintenance task.
use crate::{
    Result,
    journal::Journal,
    repair_run::{Phase, Run},
    run_processes::{Action, Command, Remaining, State},
};
use std::{
    collections::{HashMap, HashSet},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::{
    sync::watch,
    task::{JoinHandle, JoinSet},
};
use uuid::Uuid;

#[derive(Clone)]
struct Paths {
    integration: PathBuf,
    data: PathBuf,
}
impl Paths {
    fn scope(&self, run: &Run, remaining: &Remaining) -> Result<(PathBuf, PathBuf)> {
        let host = self
            .data
            .canonicalize()?
            .join("repair-hosts")
            .join(run.id.to_string());
        let workspace = self
            .integration
            .canonicalize()?
            .join(run.owner.id().to_string())
            .join(format!("repair-{}", run.id));
        if remaining.directory != host || remaining.workspace != workspace.join("repo") {
            return Err("The remaining processes do not own these private directories.".into());
        }
        Ok((host, workspace))
    }
}
fn tracker(remaining: &Remaining) -> Result<crate::process::Tracker> {
    let host = remaining.host.ok_or("This cleanup has no process host.")?;
    let record =
        crate::process_ledger::read(&remaining.directory.join(crate::process_ledger::NAME))?;
    if record.workspace != remaining.workspace
        || record.root != Some(host)
        || !record.nonces.contains(&remaining.nonce.to_string())
    {
        return Err("The remaining process records changed owner.".into());
    }
    Ok(crate::process::Tracker::restore_host(
        &remaining.directory,
        Some(host),
    )?)
}
fn remove_private(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(info)
            if info.is_dir()
                && !info.file_type().is_symlink()
                && info.uid() == unsafe { libc::geteuid() }
                && info.mode() & 0o077 == 0 =>
        {
            std::fs::remove_dir_all(path)?
        }
        Ok(_) => return Err("A Run cleanup path is no longer a private owned directory.".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
async fn maintain_inner(
    journal: Journal,
    paths: Paths,
    mut run: Run,
    force_stop: bool,
) -> Result<()> {
    let Some(mut remaining) = run.remaining.clone() else {
        return Ok(());
    };
    let (host_directory, workspace_root) = paths.scope(&run, &remaining)?;
    if force_stop && !matches!(remaining.state, State::Stopping | State::Cleaning) {
        run = journal
            .decide_repair_processes(Command {
                op_id: Uuid::now_v7(),
                id: run.id,
                nonce: remaining.nonce,
                revision: remaining.revision,
                action: Action::Stop,
            })
            .await?;
        remaining = run
            .remaining
            .clone()
            .ok_or("Run processes were already cleaned up.")?;
    }
    if remaining.state != State::Cleaning {
        let scope = remaining.clone();
        let empty = tokio::task::spawn_blocking(move || -> Result<bool> {
            let mut tracker = tracker(&scope)?;
            if scope.state == State::Stopping {
                tracker.stop_confirmed()?;
            }
            Ok(tracker.refresh()?.is_empty())
        })
        .await??;
        if !empty {
            return Ok(());
        }
        run = journal
            .repair_processes_cleaning(run.id, remaining.nonce)
            .await?;
    }
    // These effects are idempotent. Keep the Cleaning record until all of
    // them succeed so a restart repeats them instead of forgetting ownership.
    journal
        .project_use_closed(remaining.nonce.to_string())
        .await?;
    journal.secret_expired(remaining.nonce.to_string()).await?;
    if matches!(
        run.phase,
        Phase::Captured | Phase::Applying | Phase::NeedsRecovery
    ) {
        return Ok(());
    }
    if matches!(run.phase, Phase::Done | Phase::Discarded) {
        tokio::task::spawn_blocking(move || -> Result<()> {
            remove_private(&host_directory)?;
            remove_private(&workspace_root)
        })
        .await??;
    }
    journal
        .repair_processes_clear(run.id, remaining.nonce)
        .await
}
async fn maintain(journal: Journal, paths: Paths, run: Run, force_stop: bool) -> Result<()> {
    let id = run.id;
    let nonce = run.remaining.as_ref().map(|remaining| remaining.nonce);
    let result = maintain_inner(journal.clone(), paths, run, force_stop).await;
    if result.is_err()
        && let Some(nonce) = nonce
    {
        let _ = journal.repair_processes_failed(id, nonce).await;
    }
    result
}
fn remaining(snapshot: &serde_json::Value) -> Result<Vec<Run>> {
    snapshot["repairRuns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|value| value["remaining"].is_object())
        .map(|value| Ok(serde_json::from_value(value.clone())?))
        .collect()
}

pub(crate) struct Monitor {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}
impl Monitor {
    pub(crate) fn start(journal: Journal, integration: PathBuf, data: PathBuf) -> Self {
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let paths = Paths { integration, data };
            let mut changes = journal.subscribe();
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            let mut active = HashMap::new();
            let mut scanned: HashMap<Uuid, (Uuid, u64, Instant)> = HashMap::new();
            let mut tasks = JoinSet::new();
            loop {
                changes.borrow_and_update();
                if *stopping.borrow() {
                    break;
                }
                let Ok(snapshot) = journal.snapshot().await else {
                    break;
                };
                let Ok(runs) = remaining(&snapshot) else {
                    break;
                };
                let current: HashSet<_> = runs.iter().map(|run| run.id).collect();
                scanned.retain(|id, _| current.contains(id));
                for run in runs {
                    let remaining = run.remaining.as_ref().unwrap();
                    let due = scanned.get(&run.id).is_none_or(|(nonce, revision, time)| {
                        *nonce != remaining.nonce
                            || *revision != remaining.revision
                            || time.elapsed() >= Duration::from_secs(1)
                    });
                    if !due || active.contains_key(&run.id) {
                        continue;
                    }
                    scanned.insert(
                        run.id,
                        (remaining.nonce, remaining.revision, Instant::now()),
                    );
                    let journal = journal.clone();
                    let paths = paths.clone();
                    let id = run.id;
                    let task = tasks.spawn(async move {
                        let result = maintain(journal, paths, run, false).await;
                        (id, result)
                    });
                    active.insert(id, task.id());
                }
                tokio::select! {
                    _ = stopping.changed() => {},
                    changed = changes.changed() => { if changed.is_err() { break; } },
                    _ = tick.tick(), if !current.is_empty() => {},
                    Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                        match result {
                            Ok((id,_)) => { active.remove(&id); }
                            Err(error) => {
                                if let Some(id) = active.iter().find_map(|(id,task)| (*task == error.id()).then_some(*id)) {
                                    active.remove(&id);
                                    if let Some((nonce,_,_)) = scanned.get(&id) { let _ = journal.repair_processes_failed(id,*nonce).await; }
                                }
                            },
                        }
                    },
                }
            }
            while tasks.join_next().await.is_some() {}
            // The model-task coordinator joins its hosts before requesting
            // this flush, so no new retired owner can appear afterwards.
            if let Ok(snapshot) = journal.snapshot().await
                && let Ok(runs) = remaining(&snapshot)
            {
                for run in runs {
                    let _ = maintain(journal.clone(), paths.clone(), run, true).await;
                }
            }
        });
        Self { stop, task }
    }
    pub(crate) async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, process::Stdio};

    fn private(path: &Path) -> Result<()> {
        std::fs::create_dir_all(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        Ok(())
    }
    fn sleeper(nonce: Option<Uuid>) -> Result<tokio::process::Child> {
        let mut command = if nonce.is_some() {
            let mut command = tokio::process::Command::new(std::env::current_exe()?);
            command.args([
                "--exact",
                "repair_process_service::tests::owned_scope_sleeper",
                "--ignored",
                "--nocapture",
            ]);
            command
        } else {
            let mut command = tokio::process::Command::new("/bin/sleep");
            command.arg("30");
            command
        };
        command
            .env_clear()
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(nonce) = nonce {
            command.env("INTERCAT_HOST", nonce.to_string());
        }
        Ok(command.spawn()?)
    }
    #[test]
    #[ignore = "test-created child process; launched only by the ownership test"]
    fn owned_scope_sleeper() {
        std::thread::sleep(Duration::from_secs(120));
    }

    #[tokio::test]
    async fn keeps_retired_processes_across_reopen_and_stops_only_their_owner_before_cleanup()
    -> Result<()> {
        let root = tempfile::tempdir_in("/private/tmp")?;
        let data = root.path().join("data");
        let integration = root.path().join("integration");
        private(&data)?;
        private(&integration)?;
        let id = Uuid::now_v7();
        let job = Uuid::now_v7();
        let nonce = Uuid::now_v7();
        let directory = data.join("repair-hosts").join(id.to_string());
        let workspace_root = integration
            .join(job.to_string())
            .join(format!("repair-{id}"));
        let workspace = workspace_root.join("repo");
        private(&directory)?;
        private(&workspace_root)?;
        private(&workspace)?;
        std::fs::write(workspace.join("dummy.txt"), "owned disposable repair\n")?;
        let mut tracker =
            crate::process::Tracker::prepare(&directory, &workspace, nonce.to_string())?;
        let mut host = sleeper(Some(nonce))?;
        let identity = crate::process::identity(host.id().unwrap())?;
        tracker.attach_process_group(identity)?;
        // An escaped/reparented child can retain the nonce while owning a
        // different process group. No user's process is used by this test.
        let mut child = sleeper(Some(nonce))?;
        let child_identity = crate::process::identity(child.id().unwrap())?;
        let mut unrelated = sleeper(None)?;
        let unrelated_identity = crate::process::identity(unrelated.id().unwrap())?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if tracker
                    .refresh()?
                    .processes
                    .iter()
                    .any(|process| process.identity == child_identity)
                {
                    break Ok::<_, std::io::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        host.kill().await?;
        assert!(crate::process::is_alive(child_identity));
        let journal = Journal::open(
            data.join("events.sqlite"),
            crate::secrets::Key::new([7; 32]),
        )
        .await?;
        let run = Run {
            id,
            revision: 1,
            owner: crate::repair_run::Owner::Commit {
                id: job,
                revision: 1,
            },
            track: Uuid::now_v7().to_string(),
            purpose: crate::repair_run::Purpose::Check,
            phase: Phase::Running,
            created_at: 1,
            host: Some(identity),
            host_nonce: Some(nonce),
            secret_revision: None,
            secret_input: None,
            checkpoint: None,
            session: Some("dummy-retired-session".into()),
            session_ready: true,
            workspace_ready: true,
            start_command: Some(Uuid::now_v7()),
            turn: Some(Uuid::now_v7()),
            failure: None,
            approvals: vec![],
            prompts: vec![],
            output: None,
            instruction: None,
            selection: None,
            refresh: None,
            restoring: None,
            remaining: None,
            manual: None,
            external_editor: false,
        };
        let db = rusqlite::Connection::open(data.join("events.sqlite"))?;
        crate::repair_run_store::save(&db, &run)?;
        let remaining = Remaining {
            nonce,
            host: Some(identity),
            directory: directory.clone(),
            workspace: workspace.clone(),
            label: "A".into(),
            session_name: "Dummy session".into(),
            created_at: 1,
            revision: 0,
            state: State::Waiting,
            error: None,
        };
        // Completion may arrive after Stop. Its receipt must preserve that
        // choice instead of exposing a new Keep decision for the same owner.
        let mut late = run.clone();
        late.phase = Phase::Stopping;
        late.retired(remaining.clone())?;
        assert_eq!(late.phase, Phase::Stopping);
        assert_eq!(late.remaining.as_ref().unwrap().state, State::Stopping);
        assert!(late.host.is_none());
        let mut empty = run.clone();
        empty.phase = Phase::Stopping;
        let mut receipt = remaining.clone();
        receipt.state = State::Cleaning;
        empty.retired(receipt)?;
        assert_eq!(empty.remaining.as_ref().unwrap().state, State::Cleaning);
        let mut run = journal.repair_retired(id, remaining).await?;
        run.phase = Phase::Done;
        crate::repair_run_store::save(&db, &run)?;
        let paths = Paths {
            integration,
            data: data.clone(),
        };
        let mut wrong = run.clone();
        wrong.remaining.as_mut().unwrap().workspace = root.path().join("another-dummy-root");
        assert!(
            maintain(journal.clone(), paths.clone(), wrong, false)
                .await
                .is_err()
        );
        maintain(journal.clone(), paths.clone(), run, false).await?;
        assert!(crate::process::is_alive(child_identity));
        let keep = Command {
            op_id: Uuid::now_v7(),
            id,
            nonce,
            revision: 0,
            action: Action::Keep,
        };
        let kept = journal.decide_repair_processes(keep).await?;
        maintain(journal.clone(), paths.clone(), kept.clone(), false).await?;
        assert!(directory.exists());
        assert!(workspace_root.exists());
        assert!(crate::process::is_alive(child_identity));
        assert!(crate::process::is_alive(unrelated_identity));
        let reopened = Journal::open(
            data.join("events.sqlite"),
            crate::secrets::Key::new([7; 32]),
        )
        .await?;
        let stop = Command {
            op_id: Uuid::now_v7(),
            id,
            nonce,
            revision: 1,
            action: Action::Stop,
        };
        let monitor = Monitor::start(
            reopened.clone(),
            paths.integration.clone(),
            paths.data.clone(),
        );
        // Stop succeeds, but cleanup must not delete a directory that no
        // longer has the app's private permissions. Its durable marker also
        // survives the host directory having already been removed.
        std::fs::set_permissions(&workspace_root, std::fs::Permissions::from_mode(0o755))?;
        reopened.decide_repair_processes(stop.clone()).await?;
        let failed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = reopened.snapshot().await?;
                if let Some(value) = snapshot["repairRuns"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|run| {
                        run["id"] == id.to_string() && run["remaining"]["error"].is_string()
                    })
                {
                    break Ok::<Run, Box<dyn std::error::Error + Send + Sync>>(
                        serde_json::from_value(value.clone())?,
                    );
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        assert!(!crate::process::is_alive(child_identity));
        assert!(crate::process::is_alive(unrelated_identity));
        assert!(!directory.exists());
        assert!(workspace_root.exists());
        assert_eq!(failed.remaining.as_ref().unwrap().state, State::Cleaning);
        assert_eq!(failed.revision, kept.revision);
        std::fs::set_permissions(&workspace_root, std::fs::Permissions::from_mode(0o700))?;
        reopened
            .decide_repair_processes(Command {
                op_id: Uuid::now_v7(),
                id,
                nonce,
                revision: failed.remaining.as_ref().unwrap().revision,
                action: Action::RetryCleanup,
            })
            .await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = reopened.snapshot().await?;
                if snapshot["repairRuns"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|run| run["id"] == id.to_string() && run["remaining"].is_null())
                {
                    break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        monitor.shutdown().await;
        assert!(!crate::process::is_alive(child_identity));
        child.wait().await?;
        assert!(crate::process::is_alive(unrelated_identity));
        assert!(!directory.exists());
        assert!(!workspace_root.exists());
        assert!(
            reopened
                .decide_repair_processes(stop)
                .await?
                .remaining
                .is_none()
        );
        unrelated.kill().await?;
        Ok(())
    }
}
