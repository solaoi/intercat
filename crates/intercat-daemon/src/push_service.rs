//! A confirmation starts one driver; retries retain finished destinations.
use crate::{
    Result,
    git_tool::GitTool,
    journal::Journal,
    push_driver::Driver,
    push_inspect::Inspector,
    push_job::{Action, Command, Job},
    push_lfs::Tool,
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::{Mutex, watch};
use uuid::Uuid;
#[derive(Clone)]
pub struct Configuration {
    pub git: GitTool,
    pub inspector: Arc<Inspector>,
    pub lfs: Arc<Tool>,
    pub root: PathBuf,
    pub path: OsString,
    pub agent: Option<PathBuf>,
}
pub struct Service {
    driver: Driver,
    running: Mutex<BTreeMap<Uuid, Running>>,
}
struct Running {
    cancel: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        self.task.abort();
    }
}
impl Service {
    pub async fn new(journal: Journal, configuration: Configuration) -> Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&configuration.root)?;
        let root = configuration.root.canonicalize()?;
        let metadata = fs::symlink_metadata(&root)?;
        if metadata.uid() != unsafe { libc::geteuid() }
            || !metadata.is_dir()
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err("Push requires a private transport directory.".into());
        }
        journal.recover_push_jobs().await?;
        Ok(Self {
            driver: Driver {
                journal,
                git: configuration.git,
                inspector: configuration.inspector,
                lfs: configuration.lfs,
                root,
                path: configuration.path,
                agent: configuration.agent,
            },
            running: Mutex::new(BTreeMap::new()),
        })
    }
    pub async fn command(&self, command: Command) -> Result<Job> {
        let mut running = self.running.lock().await;
        running.retain(|_, task| !task.task.is_finished());
        let cancellation = matches!(command.action, Action::Cancel { .. });
        let job = self.driver.journal.push_command(command).await?;
        if cancellation {
            if let Some(task) = running.get(&job.id) {
                let _ = task.cancel.send(true);
            }
        } else if job.active() && !job.stopping && !running.contains_key(&job.id) {
            let driver = self.driver.clone();
            let id = job.id;
            let (cancel, stopping) = watch::channel(false);
            let task = tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current().block_on(async move {
                    let _ = driver.run(id, stopping).await;
                });
            });
            running.insert(job.id, Running { cancel, task });
        }
        Ok(job)
    }
    pub async fn job(&self, id: Uuid) -> Result<Job> {
        self.driver.journal.push_job(id).await
    }
    pub async fn shutdown(&self) {
        let mut running = self.running.lock().await;
        for task in running.values() {
            let _ = task.cancel.send(true);
        }
        for (_, mut task) in std::mem::take(&mut *running) {
            let _ = (&mut task.task).await;
        }
    }
}
