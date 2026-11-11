//! One task per durable PR request. Startup never resumes an external write.
use crate::{
    Result,
    editor::Manager,
    github_auth::Credentials,
    github_pull::Client,
    journal::Journal,
    pull_driver::Driver,
    pull_request::{Action, Command, Job, Open},
    push_inspect::Inspector,
};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::{Mutex, watch};
use uuid::Uuid;
pub struct Configuration {
    pub editor: Manager,
    pub client: Arc<Client>,
    pub credentials: Arc<Credentials>,
    pub inspector: Arc<Inspector>,
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
        journal.recover_pulls().await?;
        Ok(Self {
            driver: Driver {
                journal,
                editor: configuration.editor,
                client: configuration.client,
                credentials: configuration.credentials,
                inspector: configuration.inspector,
            },
            running: Mutex::new(BTreeMap::new()),
        })
    }
    fn start(&self, running: &mut BTreeMap<Uuid, Running>, job: &Job) {
        if !job.running() || running.contains_key(&job.id) {
            return;
        }
        let driver = self.driver.clone();
        let id = job.id;
        let (cancel, stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            if driver.run(id, stopping).await.is_err() {
                let _ = driver
                    .journal
                    .change_pull(id, crate::pull_changes::Change::Hold)
                    .await;
            }
        });
        running.insert(id, Running { cancel, task });
    }
    pub async fn open(&self, request: Open) -> Result<Job> {
        let mut running = self.running.lock().await;
        running.retain(|_, task| !task.task.is_finished());
        let job = self.driver.journal.open_pull(request).await?;
        self.start(&mut running, &job);
        Ok(job)
    }
    pub async fn command(&self, mut command: Command) -> Result<Job> {
        let mut running = self.running.lock().await;
        running.retain(|_, task| !task.task.is_finished());
        let stopped = matches!(command.action, Action::Stop { .. } | Action::Discard { .. });
        let id = match &command.action {
            Action::Edit { id, .. }
            | Action::Confirm { id, .. }
            | Action::Retry { id, .. }
            | Action::ReviewAgain { id, .. }
            | Action::Stop { id, .. }
            | Action::Discard { id, .. } => *id,
        };
        if !stopped && running.contains_key(&id) {
            if self.driver.journal.pull_job(id).await?.running() {
                return Err(
                    "The current PR step is still running. Wait for its result or stop it.".into(),
                );
            }
            // The review can reach its resting state just before its task
            // returns. Join that task before starting the confirmed next step.
            if let Some(mut task) = running.remove(&id) {
                let _ = (&mut task.task).await;
            }
        }
        if let Action::Edit { drafts, .. } = &mut command.action {
            for edit in drafts {
                edit.draft = crate::pull_text::screen(&self.driver.journal, &edit.draft)
                    .await?
                    .0;
            }
        }
        let job = self.driver.journal.pull_command(command).await?;
        if stopped {
            if let Some(mut task) = running.remove(&job.id) {
                let _ = task.cancel.send(true);
                let _ = (&mut task.task).await;
            }
            return self.driver.journal.pull_job(job.id).await;
        }
        self.start(&mut running, &job);
        Ok(job)
    }
    pub async fn job(&self, id: Uuid) -> Result<Job> {
        self.driver.journal.pull_job(id).await
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
