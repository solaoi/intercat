//! Owned worker hosts. Configuration never contains credential contents.
use crate::{
    Result,
    host::{self, Host},
    journal::Journal,
    process::Identity,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::{
    process::Command,
    sync::{mpsc, watch},
    task::JoinSet,
};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub source: PathBuf,
    pub binary: PathBuf,
    pub credentials: PathBuf,
    pub path: String,
}
impl Config {
    pub async fn prepare(source: PathBuf, credentials: PathBuf, data: PathBuf) -> Result<Self> {
        let canonical_credentials = credentials
            .canonicalize()
            .map_err(|_| std::io::Error::other("Sign in to Muse before enabling workers."))?;
        let info = std::fs::metadata(&canonical_credentials)?;
        if !info.is_file() || info.uid() != unsafe { libc::geteuid() } {
            return Err(std::io::Error::other("Muse credentials must belong to this user.").into());
        }
        let original_source = source.clone();
        let binary =
            tokio::task::spawn_blocking(move || host::prepare_binary(&source, &data)).await??;
        let shell = std::env::var_os("SHELL")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/bin/zsh"));
        if !shell.is_absolute() {
            return Err(std::io::Error::other("Login shell path is invalid.").into());
        }
        let mut command = Command::new(shell);
        command
            .args(["-lic", r#"printf '\036%s\037' "$PATH""#])
            .env_clear()
            .env(
                "HOME",
                std::env::var_os("HOME").ok_or_else(|| std::io::Error::other("HOME missing"))?,
            )
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .kill_on_drop(true);
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(10), command.output()).await??;
        let start = output
            .stdout
            .iter()
            .rposition(|b| *b == 30)
            .ok_or_else(|| std::io::Error::other("Login shell did not return PATH."))?;
        let end = output.stdout[start + 1..]
            .iter()
            .position(|b| *b == 31)
            .map(|i| start + 1 + i)
            .ok_or_else(|| std::io::Error::other("Login shell PATH is incomplete."))?;
        let path = std::str::from_utf8(&output.stdout[start + 1..end])?.to_owned();
        if !output.status.success() || path.is_empty() || path.len() > 65536 || path.contains('\n')
        {
            return Err(std::io::Error::other("Login shell PATH is invalid.").into());
        }
        Ok(Self {
            source: original_source,
            binary,
            credentials,
            path,
        })
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkerState {
    pub track_id: String,
    pub phase: String,
    pub session_id: Option<String>,
    pub secret_revision: Option<u64>,
    pub identity: Option<Identity>,
    pub detail: Option<String>,
    pub active_turn: Option<Uuid>,
    pub approvals: Vec<crate::execution::Approval>,
    pub output: Option<String>,
    pub checkpoint_pending: bool,
    pub checkpoint: Option<crate::checkpoint::Checkpoint>,
    pub prompts: Vec<crate::questions::Prompt>,
}
impl WorkerState {
    pub(crate) fn released(&self) -> bool {
        matches!(self.phase.as_str(), "stopped" | "sleeping")
            && self.active_turn.is_none()
            && !self.checkpoint_pending
    }
    fn starting(track_id: String, previous: Option<&Self>) -> Self {
        Self {
            track_id,
            phase: "starting".into(),
            session_id: previous.and_then(|state| state.session_id.clone()),
            secret_revision: None,
            identity: None,
            detail: None,
            active_turn: previous.and_then(|state| state.active_turn),
            approvals: vec![],
            output: previous.and_then(|state| state.output.clone()),
            checkpoint_pending: previous.is_some_and(|state| state.checkpoint_pending),
            checkpoint: previous.and_then(|state| state.checkpoint.clone()),
            prompts: previous
                .map(|state| state.prompts.clone())
                .unwrap_or_default(),
        }
    }
}

pub struct Runtime {
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<()>>,
    pub service: Service,
}
#[derive(Clone)]
pub struct Service {
    capacity: Arc<crate::scheduler::Capacity>,
    assistant_capacity: Arc<crate::scheduler::Capacity>,
    host_memory: Arc<crate::host_memory::Pool>,
    data: PathBuf,
    rpc: PathBuf,
    branches: Arc<tokio::sync::Mutex<()>>,
    global_pause: Arc<crate::dispatch_control::Gate>,
    recoveries: crate::pickup_run::Runs,
    stopping: watch::Receiver<bool>,
    journal: Journal,
    retry: mpsc::UnboundedSender<String>,
    ports: Arc<Mutex<HashMap<String, crate::execution::Port>>>,
    project_secrets: crate::project_secrets::Store,
    paused_ports: Arc<Mutex<HashMap<String, u64>>>,
}
impl Service {
    pub fn host_memory(&self) -> Arc<crate::host_memory::Pool> {
        self.host_memory.clone()
    }
    pub fn set_capacity(&self, setting: &crate::execution_limits::Setting) {
        self.capacity
            .set_limit(setting.revision, setting.parallel_workers.into());
        self.assistant_capacity
            .set_limit(setting.revision, setting.planner_reviewer_turns.into());
    }
    pub fn assistant_capacity(&self) -> crate::scheduler::Snapshot {
        self.assistant_capacity.snapshot()
    }
    pub(crate) fn assistant_pool(&self) -> Arc<crate::scheduler::Capacity> {
        self.assistant_capacity.clone()
    }
    pub fn capacity(&self) -> crate::scheduler::Snapshot {
        self.capacity.snapshot()
    }

    pub async fn process_audit(&self) -> Result<crate::process_audit::Audit> {
        let data = self.data.clone();
        tokio::task::spawn_blocking(move || crate::process_audit::inspect(&data)).await?
    }
    pub fn interrupt_all(&self, id: Uuid) -> Result<()> {
        let ports: Vec<_> = self
            .ports
            .lock()
            .unwrap()
            .iter()
            .map(|(track, port)| (track.clone(), port.clone()))
            .collect();
        for (track, port) in ports {
            if port.is_closed() {
                continue;
            }
            let before = port
                .stop_generation
                .load(std::sync::atomic::Ordering::Acquire);
            let resume = !port.locally_held()
                || self.paused_ports.lock().unwrap().get(&track) == Some(&before);
            port.stop_all_turns(Uuid::new_v5(&id, track.as_bytes()))?;
            if resume {
                self.paused_ports.lock().unwrap().insert(
                    track,
                    port.stop_generation
                        .load(std::sync::atomic::Ordering::Acquire),
                );
            }
        }
        Ok(())
    }
    pub async fn ready_to_resume(&self) -> Result<()> {
        let snapshot = self.journal.snapshot().await?;
        if snapshot["workers"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|worker| !worker["activeTurn"].is_null() || worker["checkpointPending"] == true)
        {
            return Err("Wait for the interrupted turns to finish saving.".into());
        }
        Ok(())
    }
    pub fn resume_all(&self) {
        let stopped = std::mem::take(&mut *self.paused_ports.lock().unwrap());
        for (track, generation) in stopped {
            if let Ok(port) = self.port(&track) {
                port.release_stop(generation);
            }
        }
    }
    pub async fn prepare_fresh(&self, request: crate::fresh::Request) -> Result<crate::fresh::Job> {
        if *self.stopping.borrow() {
            return Err("Shutdown has started.".into());
        }
        let job = self
            .journal
            .fresh(crate::fresh::Change::Request(request))
            .await?;
        if job.phase != crate::fresh::Phase::Cancelled
            && let Ok(port) = self.port(&job.request.track_id)
            && !port.is_closed()
        {
            port.hold(job.request.op_id)?;
        }
        Ok(job)
    }
    pub async fn cancel_fresh(
        &self,
        op_id: Uuid,
        id: Uuid,
        revision: u64,
    ) -> Result<crate::fresh::Job> {
        let replay = self.journal.operation(op_id).await?.is_some();
        let old = self
            .journal
            .fresh_job(id)
            .await?
            .ok_or("Missing Start fresh request.")?;
        let port = self
            .port(&old.request.track_id)
            .ok()
            .filter(|port| !port.is_closed());
        let generation = port.as_ref().map(|port| {
            port.stop_generation
                .load(std::sync::atomic::Ordering::Acquire)
        });
        let job = self
            .journal
            .fresh(crate::fresh::Change::Cancel {
                op_id,
                id,
                revision,
            })
            .await?;
        if !replay && let (Some(port), Some(generation)) = (port, generation) {
            port.saved_local_hold(generation, Uuid::new_v5(&op_id, b"fresh-cancel-hold"));
        }
        Ok(job)
    }
    pub async fn fresh_command(&self, command: crate::fresh::Command) -> Result<crate::fresh::Job> {
        use crate::fresh::{Action, Change};
        let crate::fresh::Command {
            op_id,
            id,
            revision,
            action,
        } = command;
        match action {
            Action::Cancel => self.cancel_fresh(op_id, id, revision).await,
            Action::Retry => {
                self.journal
                    .fresh(Change::Retry {
                        op_id,
                        id,
                        revision,
                    })
                    .await
            }
            Action::AllowAccess { fingerprint } => {
                self.journal
                    .fresh(Change::ApproveAccess {
                        op_id,
                        id,
                        revision,
                        fingerprint,
                    })
                    .await
            }
        }
    }
    pub(crate) async fn advance_fresh(&self, job: crate::fresh::Job) -> Result<crate::fresh::Job> {
        use crate::fresh::{Change, Phase};
        if *self.stopping.borrow() {
            return Err(crate::fresh::Waiting("Shutdown has started.").into());
        }
        let id = job.request.op_id;
        let revision = job.revision;
        match job.phase {
            Phase::WaitingForTurn => self.journal.fresh(Change::Capture { id, revision }).await,
            Phase::Captured => self.journal.fresh(Change::Release { id, revision }).await,
            Phase::Releasing => {
                self.release_fresh_source(&job).await?;
                self.journal.fresh(Change::Released { id, revision }).await
            }
            Phase::Cancelling => {
                self.release_fresh_source(&job).await?;
                crate::fresh_cancel::complete(&self.data, &self.journal, &job).await
            }
            Phase::Inspecting | Phase::NeedsAccess => {
                crate::fresh_files::stopped(&self.data, &self.journal, &job).await?;
                let inspection = crate::fresh_files::inspect(&job).await?;
                self.journal
                    .fresh(Change::Inspect {
                        id,
                        revision,
                        inspection,
                    })
                    .await
            }
            Phase::CapturingFiles => {
                crate::fresh_files::stopped(&self.data, &self.journal, &job).await?;
                crate::fresh_files::capture(&self.journal, &job).await
            }
            Phase::PublishingFiles => {
                crate::fresh_files::stopped(&self.data, &self.journal, &job).await?;
                crate::fresh_files::publish(&self.journal, &job).await
            }
            Phase::SnapshotReady => {
                if self.journal.worker_config().await?.is_none() {
                    return Err(crate::fresh::Waiting(
                        "Connect workers before making the handoff summary.",
                    )
                    .into());
                }
                if self.global_pause.paused() {
                    return Err(
                        crate::fresh::Waiting("Resume before making the handoff summary.").into(),
                    );
                }
                let changes = crate::fresh_summary::changes(&job).await?;
                self.journal
                    .fresh(Change::PrepareSummary {
                        id,
                        revision,
                        changes,
                    })
                    .await
            }
            Phase::Summarizing => {
                let config = self
                    .journal
                    .worker_config()
                    .await?
                    .ok_or("Connect workers before making the handoff summary.")?;
                let document = crate::fresh_summary::run(
                    &self.journal,
                    &job,
                    &config,
                    &self.data,
                    &self.rpc,
                    self.host_memory.clone(),
                    self.assistant_capacity.clone(),
                    self.global_pause.clone(),
                    self.stopping.clone(),
                )
                .await?;
                self.journal
                    .fresh(Change::Summarized {
                        id,
                        revision,
                        document,
                    })
                    .await
            }
            Phase::SummaryReady => {
                crate::fresh_files::stopped(&self.data, &self.journal, &job).await?;
                crate::fresh_files::transfer(&self.journal, &job).await
            }
            Phase::PublishingOwner => crate::fresh_files::publish_owner(&self.journal, &job).await,
            Phase::FilesHeld
            | Phase::SummaryHeld
            | Phase::OwnerHeld
            | Phase::Transferred
            | Phase::Cancelled => Ok(job),
        }
    }
    async fn release_fresh_source(&self, job: &crate::fresh::Job) -> Result<()> {
        if let Ok(port) = self.port(&job.request.track_id)
            && !port.is_closed()
        {
            let snapshot = self.journal.snapshot().await?;
            if snapshot["workers"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|worker| {
                    worker["trackId"] == job.request.track_id
                        && worker["sessionId"] == job.request.session_id
                        && worker["activeTurn"].is_null()
                        && worker["checkpointPending"] == false
                        && matches!(
                            worker["phase"].as_str(),
                            Some("lost" | "failed" | "needs-recovery")
                        )
                })
            {
                port.stop(Uuid::new_v5(&job.request.op_id, b"release-source"))?;
            }
            return Err(
                crate::fresh::Waiting("Waiting for the source actor to close its host.").into(),
            );
        }
        let _guard = self.branches.lock().await;
        let snapshot = self.journal.snapshot().await?;
        let mut worker: WorkerState = serde_json::from_value(
            snapshot["workers"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|worker| worker["trackId"] == job.request.track_id)
                .cloned()
                .ok_or("Missing Start fresh source worker.")?,
        )?;
        if worker.session_id.as_deref() != Some(&job.request.session_id)
            || worker.active_turn.is_some()
            || worker.checkpoint_pending
            || serde_json::to_value(&worker.checkpoint)?
                != serde_json::to_value(job.boundary.as_ref().map(|value| &value.checkpoint))?
        {
            return Err("The source changed before its host could be released.".into());
        }
        // After a daemon restart there is no actor. The durable Fresh request
        // already authorizes stopping this exact source's owned processes.
        if !matches!(worker.phase.as_str(), "stopped" | "sleeping") {
            let directory = self.data.join("hosts").join(&job.request.track_id);
            let workspace = job.workspace.path.clone();
            let expected = worker.identity;
            tokio::task::spawn_blocking(move || -> Result<()> {
                if let Some(mut tracker) =
                    crate::process::Tracker::restore(&directory, &workspace, expected)?
                {
                    tracker.stop_confirmed()?;
                }
                Ok(())
            })
            .await??;
            self.journal
                .unmount_dependency_references(&job.request.track_id)
                .await?;
            worker.phase = "stopped".into();
            worker.detail = Some("Source closed for Start fresh".into());
            self.journal.worker_state(worker).await?;
        }
        crate::fresh_files::stopped(&self.data, &self.journal, job).await
    }
    pub async fn prepare_pickup(&self, request: crate::pickup::Request) -> Result<Value> {
        let service = self.clone();
        tokio::spawn(async move {
            let active = service.recoveries.begin(request.op_id)?;
            let config = service
                .journal
                .worker_config()
                .await?
                .ok_or("Connect workers before file recovery.")?;
            let mut recovery = {
                let _guard = service.branches.lock().await;
                if *service.stopping.borrow() {
                    return Err("Shutdown has started.".into());
                }
                service
                    .journal
                    .pickup(crate::pickup::Change::Prepare(request.clone()))
                    .await?
            };
            if recovery.phase == crate::pickup::Phase::Stopping {
                service
                    .stop(recovery.stop_command, &request.track_id)
                    .await?;
                service.wait_pickup_stop(&request.track_id).await?;
            }
            let _guard = service.branches.lock().await;
            recovery = service
                .journal
                .pickup(crate::pickup::Change::Prepare(request))
                .await?;
            if recovery.phase == crate::pickup::Phase::Stopping {
                recovery = service
                    .journal
                    .pickup(crate::pickup::Change::Stopped(recovery.request.op_id))
                    .await?;
            }
            if *service.stopping.borrow() {
                return Err("Shutdown interrupted file recovery.".into());
            }
            crate::pickup_run::prepare(
                service.host_memory.clone(),
                &service.journal,
                config,
                &service.data,
                &service.rpc,
                recovery,
                service.assistant_capacity.clone(),
                active.stopping.clone(),
            )
            .await
        })
        .await?
    }
    pub async fn cancel_pickup(&self, request: crate::pickup::CancelRequest) -> Result<Value> {
        let service = self.clone();
        tokio::spawn(async move {
            // This must not wait for branches: a Planner can hold it while
            // awaiting model output. Persist first, then interrupt that host.
            let recovery = service.journal.cancel_pickup(request).await?;
            if recovery.phase != crate::pickup::Phase::Cancelled {
                service.recoveries.cancel(recovery.request.op_id);
                let old_worker = async {
                    service.stop(recovery.stop_command, &recovery.request.track_id).await?;
                    service.wait_pickup_stop(&recovery.request.track_id).await
                }.await;
                let _guard = service.branches.lock().await;
                // The active preparation has now closed its hosts. Also stop
                // durable ownership records left by a previous daemon.
                let tasks = service.journal.summary_tasks(recovery.request.op_id).await?;
                let data = service.data.clone();
                let stopped = recovery.clone();
                let hosts = tokio::task::spawn_blocking(move || -> Result<()> {
                    let stop = |directory: &Path, workspace: &Path, expected| -> Result<()> {
                        if let Some(mut tracker) = crate::process::Tracker::restore(directory, workspace, expected)? {
                            tracker.stop_confirmed()?;
                        }
                        Ok(())
                    };
                    let mut results = vec![stop(
                        &data.join("hosts").join(&stopped.request.track_id),
                        &stopped.workspace.path, stopped.previous.identity,
                    )];
                    for task in tasks {
                        results.push(stop(
                            &data.join("summary-hosts").join(task.to_string()),
                            &data.join("summary-workspaces").join(stopped.request.op_id.to_string()), None,
                        ));
                    }
                    // A damaged ledger must not prevent attempts to stop the
                    // other known hosts, but any failure still blocks completion.
                    results.into_iter().collect::<Result<Vec<_>>>()?;
                    Ok(())
                }).await?;
                old_worker?;
                hosts?;
                let cancelled = service.journal.pickup(crate::pickup::Change::Cancelled(recovery.request.op_id)).await?;
                return Ok(json!({"recovery":cancelled.status(),"catalog":service.journal.fingerprint_catalog().await?}));
            }
            Ok(json!({"recovery":recovery.status(),"catalog":service.journal.fingerprint_catalog().await?}))
        }).await?
    }
    async fn wait_pickup_stop(&self, track: &str) -> Result<()> {
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                let snapshot = self.journal.snapshot().await?;
                if snapshot["workers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|w| w["trackId"] == track && w["phase"] == "after-connection-loss")
                    && self.port(track).map_or(true, |port| port.is_closed())
                {
                    return Ok(());
                }
                if *self.stopping.borrow() {
                    return Err("Shutdown interrupted file recovery.".into());
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        })
        .await
        .map_err(|_| "The old worker has not finished stopping.")?
    }
    pub async fn activate_pickup(&self, id: Uuid) -> Result<crate::pickup::Recovery> {
        let _guard = self.branches.lock().await;
        if *self.stopping.borrow() {
            return Err("Shutdown has started.".into());
        }
        let operation = self
            .journal
            .operation(id)
            .await?
            .ok_or("Unknown file recovery.")?;
        if operation["kind"] != "worker.pickup" {
            return Err("Operation ID reused.".into());
        }
        let recovery: crate::pickup::Recovery =
            serde_json::from_value(operation["payload"].clone())?;
        if recovery.phase == crate::pickup::Phase::Ready {
            return Ok(recovery);
        }
        if self
            .port(&recovery.request.track_id)
            .is_ok_and(|port| !port.is_closed())
        {
            return Err("The old worker is still closing.".into());
        }
        crate::pickup_run::verify_snapshot(&recovery).await?;
        let recovery = self
            .journal
            .pickup(crate::pickup::Change::Activate(id))
            .await?;
        self.retry.send(recovery.request.track_id.clone())?;
        Ok(recovery)
    }
    pub(crate) async fn two_ways_options(
        &self,
        query: crate::two_ways::OptionsQuery,
    ) -> Result<crate::two_ways::Options> {
        let service = self.clone();
        tokio::spawn(async move {
            let source = service.journal.two_ways_source(query.clone()).await?;
            let models = if let Ok(port) = service.port(&source.track)
                && !port.is_closed()
            {
                port.two_ways_models(&source.session).await?
            } else {
                if !source.released {
                    return Err("Reconnect this worker before reading its model choices.".into());
                }
                let config = service
                    .journal
                    .worker_config()
                    .await?
                    .ok_or("Connect workers to read their model choices.")?;
                let workspace = service.journal.workspace(source.track.clone()).await?;
                crate::two_ways_options::read(
                    service.host_memory.clone(),
                    &service.journal,
                    &config,
                    &service.data,
                    &service.rpc,
                    service.stopping.clone(),
                    &workspace,
                    &source.session,
                )
                .await?
            };
            let variation = crate::two_ways::Variation::from_catalog(&models)?;
            if service.journal.two_ways_source(query).await? != source {
                return Err(
                    "The instruction or source session changed while reading its options.".into(),
                );
            }
            Ok(crate::two_ways::Options {
                input_id: source.query.input_id,
                revision: source.query.revision,
                selection: crate::two_ways::Selection {
                    track_id: source.track,
                    session_id: source.session,
                    variation,
                },
            })
        })
        .await?
    }
    pub(crate) async fn advance_two_ways(
        &self,
        job: crate::two_ways::Job,
    ) -> Result<crate::two_ways::Job> {
        use crate::two_ways::{Change, Phase, Waiting};
        if *self.stopping.borrow() || self.global_pause.paused() && job.phase != Phase::Cancelling {
            return Err(Waiting("Paired preparation waits while turns are stopped.").into());
        }
        let id = job.request.op_id;
        let revision = job.revision;
        match job.phase {
            Phase::Cancelling => {
                let _branches = self.branches.lock().await;
                let snapshot = self.journal.snapshot().await?;
                if let Some(raw) = snapshot["branches"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|branch| branch["request"]["opId"] == job.branch_id.to_string())
                {
                    let branch: crate::branch::Branch = serde_json::from_value(raw.clone())?;
                    crate::branch_run::release_hosts(&self.data, &branch).await?;
                    if branch.session_id.is_some() {
                        let catalog = self.journal.fingerprint_catalog().await?;
                        if catalog.sealed.as_ref() != Some(&catalog.current) {
                            return Err(Waiting(
                                "Confirm the saved fingerprints before completing cancellation.",
                            )
                            .into());
                        }
                    }
                } else if self
                    .data
                    .join("branch-hosts")
                    .join(job.branch_id.to_string())
                    .try_exists()?
                {
                    return Err("The branch host's saved owner is missing. Cancellation still needs review.".into());
                }
                self.journal
                    .two_ways(Change::Cancelled { id, revision })
                    .await
            }
            Phase::CapturingInitial => {
                let port = self
                    .port(&job.source_track)
                    .map_err(|_| Waiting("Reconnect the empty paired source worker."))?;
                port.verify_empty_session(&job.source_session).await?;
                crate::two_ways_initial::capture(self.journal.clone(), job).await
            }
            Phase::Inspecting | Phase::InspectingConfirmedUsage => {
                let port = self
                    .port(&job.source_track)
                    .map_err(|_| Waiting("Reconnect the paired source worker."))?;
                let (usage, models) = port.two_ways_observation(&job.source_session).await?;
                self.journal
                    .two_ways(Change::Inspect {
                        id,
                        revision,
                        usage,
                        models,
                    })
                    .await
            }
            Phase::Forking => {
                self.prepare_branch(job.branch_request()?).await?;
                self.journal
                    .two_ways(Change::BranchReady { id, revision })
                    .await
            }
            Phase::AwaitingReceipt => {
                let catalog = self.journal.fingerprint_catalog().await?;
                if catalog.sealed.as_ref() != Some(&catalog.current) {
                    return Err(Waiting(
                        "Save the inherited fingerprints before starting both paths.",
                    )
                    .into());
                }
                let branch = self.activate_branch(job.branch_id).await?;
                crate::two_ways_files::before_publish(&self.journal, &job, &branch.track_id)
                    .await?;
                self.journal
                    .two_ways(Change::Publish { id, revision })
                    .await
            }
            _ => Err(Waiting("This paired attempt needs no automatic preparation.").into()),
        }
    }
    pub async fn prepare_branch(&self, request: crate::branch::Request) -> Result<Value> {
        let service = self.clone();
        tokio::spawn(async move {
            let _guard = service.branches.lock().await;
            if *service.stopping.borrow() {
                return Err("Shutdown has started.".into());
            }
            let config = service
                .journal
                .worker_config()
                .await?
                .ok_or("Connect workers before branching.")?;
            crate::branch_run::prepare(
                service.host_memory.clone(),
                &service.journal,
                config,
                &service.data,
                &service.rpc,
                service.stopping.clone(),
                request,
            )
            .await
        })
        .await?
    }
    pub async fn activate_branch(&self, operation: Uuid) -> Result<crate::branch::Branch> {
        let _guard = self.branches.lock().await;
        if *self.stopping.borrow() {
            return Err("Shutdown has started.".into());
        }
        crate::branch_run::activate(&self.journal, operation).await
    }
    pub fn project_secrets(&self) -> crate::project_secrets::Store {
        self.project_secrets.clone()
    }
    pub async fn project_usage(&self) -> Result<Vec<crate::project_secret_usage::Usage>> {
        let records = self.journal.project_uses().await?;
        let check = records.clone();
        let mut usage =
            tokio::task::spawn_blocking(move || crate::project_secret_usage::scan(&check))
                .await??;
        for item in &mut usage {
            if let Some(record) = records.iter().find(|r| r.host.nonce == item.nonce)
                && let Some(current) = self.project_secrets.get(&record.host.project_id)
            {
                item.names.retain(|name| {
                    !current.manifest.fingerprints.iter().any(|new| {
                        new.name == *name
                            && record
                                .host
                                .fingerprints
                                .iter()
                                .any(|old| old.name == *name && old.tag == new.tag)
                    })
                });
            }
        }
        usage.retain(|item| !item.names.is_empty() && !item.processes.is_empty());
        Ok(usage)
    }
    pub async fn project_process(
        &self,
        decision: crate::project_secret_usage::Decision,
    ) -> Result<()> {
        let records = self.journal.project_uses().await?;
        let record = records
            .into_iter()
            .find(|record| record.host.nonce == decision.nonce)
            .ok_or("The host record changed.")?;
        let check = record.clone();
        let expected = decision.identity;
        let usage =
            tokio::task::spawn_blocking(move || crate::project_secret_usage::scan(&[check]))
                .await??;
        if !usage
            .iter()
            .flat_map(|usage| &usage.processes)
            .any(|p| p.identity == expected && !p.host)
        {
            return Err("The process is no longer recorded for this host.".into());
        }
        if decision.keep {
            self.journal.project_use_kept(decision).await
        } else {
            crate::project_secret_usage::stop(&record, decision.identity).await
        }
    }
    pub async fn apply_project_secrets(&self, expected_revision: u64) -> Result<()> {
        let snapshot = self.journal.snapshot().await?;
        let revision = snapshot["projectSecrets"]["revision"]
            .as_u64()
            .ok_or("No project is configured.")?;
        if revision != expected_revision {
            return Err("Project secrets changed. Reload before applying.".into());
        }
        for worker in snapshot["workers"].as_array().into_iter().flatten() {
            if worker["secretRevision"].as_u64() != Some(revision)
                && let Some(track) = worker["trackId"].as_str()
                && let Ok(port) = self.port(track)
            {
                port.stop(Uuid::now_v7())?;
            }
        }
        Ok(())
    }
    pub async fn activate_project_secrets(
        &self,
        request: crate::project_secrets::Load,
    ) -> Result<()> {
        let journal = self.journal.clone();
        let store = self.project_secrets.clone();
        tokio::spawn(async move {
            let config = journal.activate_project_secrets(request).await?;
            store.publish(config);
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await?
    }
    pub async fn install_secret(&self, request: crate::secret_input::Request) -> Result<()> {
        if request.owner != crate::secret_input::Owner::Work {
            return Err("This secret is addressed to a repair Run.".into());
        }
        let port = self.port(&request.track_id)?;
        let journal = self.journal.clone();
        // Losing the RPC acknowledgement must not strand a prepared operation
        // after its value disappears. The owned operation outlives its caller.
        tokio::spawn(async move {
            let id = request.op_id;
            let Some(install) = journal.prepare_secret(request).await? else {
                return Ok(());
            };
            let result = port.install_secret(install).await;
            if result.is_err() {
                journal.secret_failed(id).await?;
            }
            result
        })
        .await?
    }
    pub async fn reload_fingerprints(&self, track: &str, session: &str) -> Result<()> {
        let mask = self.journal.session_mask(track, session).await?;
        if let Some(port) = self.ports.lock().unwrap().get(track) {
            port.set_redaction(session, mask);
        }
        Ok(())
    }
    pub fn output_flows(&self) -> Vec<Value> {
        self.ports
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(track, port)| {
                port.output_flow().map(|flow| {
                    let mut value = serde_json::to_value(flow).unwrap();
                    value["trackId"] = json!(track);
                    value["repairRunId"] = Value::Null;
                    value["preview"] = serde_json::to_value(port.progress()).unwrap();
                    value
                })
            })
            .collect()
    }
    pub async fn restore(
        &self,
        id: Uuid,
        track: &str,
        snapshot: Uuid,
    ) -> Result<crate::deletion::Restored> {
        self.port(track)?.restore(id, snapshot).await
    }
    pub async fn exclude_checkpoint(
        &self,
        command: crate::checkpoint_scope::Exclude,
    ) -> Result<()> {
        self.journal
            .checkpoint_scope(crate::checkpoint_scope::Change::Exclude(command.clone()))
            .await?;
        let operation = self
            .journal
            .operation(command.op_id)
            .await?
            .ok_or("The checkpoint decision was not saved.")?;
        if operation["state"] == "complete" {
            return Ok(());
        }
        let issue: crate::checkpoint_scope::Issue =
            serde_json::from_value(operation["payload"]["issue"].clone())?;
        let checkpoint_id = Uuid::new_v5(&issue.turn_id, b"checkpoint");
        let saved = self
            .journal
            .operation(checkpoint_id)
            .await?
            .is_some_and(|operation| {
                operation["kind"] == "checkpoint.save" && operation["state"] == "complete"
            });
        if !saved {
            if let Ok(port) = self.port(&issue.track_id)
                && !port.is_closed()
            {
                let workspace = self.journal.workspace(issue.track_id.clone()).await?;
                port.retry_checkpoint(issue.turn_id, workspace).await?;
            } else {
                let retry = serde_json::from_value(operation["payload"]["retryId"].clone())?;
                self.retry(retry, issue.track_id.clone()).await?;
            }
        }
        let id = command.op_id.to_string();
        if operation["state"] == "prepared" {
            self.journal.finish(&id, "dispatched").await?;
        }
        self.journal.finish(&id, "complete").await
    }
    pub(crate) async fn remaining_realign_processes(
        &self,
        track: &str,
        host: crate::process::Identity,
    ) -> Result<bool> {
        self.port(track)?.remaining_processes(host).await
    }
    pub(crate) async fn stop_realign_processes(
        &self,
        request: crate::realign::Request,
        command: Uuid,
    ) -> Result<()> {
        self.port(&request.track_id)?
            .stop_realign_processes(request, command)
            .await
    }
    pub async fn request_realign(
        &self,
        request: crate::realign::Request,
    ) -> Result<crate::realign::Job> {
        self.journal
            .change_realign_queue(crate::realign_queue::Change::Request(request))
            .await?
            .ok_or_else(|| "The realignment request was not saved.".into())
    }
    pub async fn control_realign(
        &self,
        control: crate::realign::Control,
    ) -> Result<crate::realign::Job> {
        self.journal
            .change_realign_queue(crate::realign_queue::Change::Control(control))
            .await?
            .ok_or_else(|| "The realignment request changed.".into())
    }
    pub async fn approve_realign_access(
        &self,
        consent: crate::workspace_access::Consent,
    ) -> Result<()> {
        self.journal.approve_realign_access(consent).await
    }
    pub async fn realign(&self, request: crate::realign::Request) -> Result<()> {
        self.port(&request.track_id)
            .map_err(|_| {
                Box::new(crate::realign::Waiting) as Box<dyn std::error::Error + Send + Sync>
            })?
            .realign(request)
            .await
    }
    pub async fn finish_inheritance(&self, id: Uuid, track: &str, application: Uuid) -> Result<()> {
        self.port(track)?.finish_inheritance(id, application).await
    }
    pub async fn stop_inheritance(
        &self,
        track: &str,
        command: crate::inheritance_issue::Command,
    ) -> Result<()> {
        self.port(track)?.stop_inheritance(command).await
    }
    fn port(&self, track: &str) -> Result<crate::execution::Port> {
        self.ports
            .lock()
            .unwrap()
            .get(track)
            .cloned()
            .ok_or_else(|| std::io::Error::other("Worker is unavailable.").into())
    }
    pub async fn stop(&self, id: Uuid, track: &str) -> Result<()> {
        if let Ok(port) = self.port(track)
            && !port.is_closed()
        {
            return port.stop(id);
        }
        // Serialize a recovered stop with Retry and branch preparation. No
        // worker actor exists for a dirty startup, so use its durable records.
        let _guard = self.branches.lock().await;
        if let Some(operation) = self.journal.operation(id).await? {
            if matches!(
                operation["kind"].as_str(),
                Some("worker.stopLost" | "worker.hold")
            ) && operation["payload"]["trackId"] == track
            {
                return Ok(());
            }
            return Err("Operation ID reused.".into());
        }
        let snapshot = self.journal.snapshot().await?;
        let mut state: WorkerState = serde_json::from_value(
            snapshot["workers"]
                .as_array()
                .and_then(|workers| workers.iter().find(|worker| worker["trackId"] == track))
                .cloned()
                .ok_or("Worker is unavailable.")?,
        )?;
        if state.phase == "sleeping" {
            self.journal.hold_inputs(id, track.to_owned()).await?;
            state.phase = "stopped".into();
            self.journal.worker_state(state).await?;
            return Ok(());
        }
        if !["needs-recovery", "lost", "failed", "after-connection-loss"]
            .contains(&state.phase.as_str())
        {
            return Err("This worker cannot be stopped without its running host.".into());
        }
        let workspace = self.journal.workspace(track.to_owned()).await?;
        state.phase = "needs-recovery".into();
        state.detail = Some("Stopping recorded worker processes…".into());
        self.journal.worker_state(state.clone()).await?;
        let directory = self.data.join("hosts").join(track);
        let expected = state.identity;
        let stopped = tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(mut tracker) =
                crate::process::Tracker::restore(&directory, &workspace.path, expected)?
            {
                tracker.stop_confirmed()?;
            }
            Ok(())
        })
        .await?;
        if let Err(error) = stopped {
            state.detail = Some(error.to_string());
            self.journal.worker_state(state).await?;
            return Err(error);
        }
        let turn = state.active_turn.take();
        state.checkpoint_pending = false;
        state.approvals.clear();
        state.prompts.clear();
        state.phase = "after-connection-loss".into();
        state.detail = Some("stopped · connection lost".into());
        if let Some(session) = &state.session_id {
            self.journal.interrupt_effects(session.clone()).await?;
        }
        self.journal.stop_lost_worker(id, state, turn).await
    }
    pub async fn restore_path(
        &self,
        request: crate::path_restore::Request,
    ) -> Result<crate::path_restore::Job> {
        self.journal.restore_path(request).await
    }
    pub async fn control_path_restore(
        &self,
        control: crate::path_restore::Control,
    ) -> Result<crate::path_restore::Job> {
        let _guard = self.branches.lock().await;
        self.journal
            .change_path_restore(crate::path_restore_store::Change::Control(control))
            .await
    }
    pub async fn aside_processes(&self, id: Uuid) -> Result<crate::aside_processes::Info> {
        let (entry, worker) = self.journal.aside_process_scope(id).await?;
        let stops: Vec<crate::aside_processes::Status> =
            serde_json::from_value(self.journal.snapshot().await?["asideProcessStops"].clone())?;
        let stop = stops
            .into_iter()
            .find(|status| status.record.request.retirement_id == id);
        let host = worker.as_ref().and_then(|state| state.identity);
        let tracker = self
            .port(&entry.track)
            .ok()
            .and_then(|port| port.process_tracker());
        let directory = self.data.join("hosts").join(&entry.track);
        let result = tokio::task::spawn_blocking(move || -> Result<usize> {
            let scan = if let Some(tracker) = tracker {
                Some(tracker.lock().unwrap().refresh()?)
            } else {
                crate::process::Tracker::restore(&directory, &entry.workspace.path, host)?
                    .map(|mut tracker| tracker.refresh())
                    .transpose()?
            };
            Ok(scan
                .map(|scan| crate::aside_processes::count(scan, host))
                .unwrap_or(0))
        })
        .await?;
        Ok(match result {
            Ok(groups) => crate::aside_processes::Info {
                retirement_id: id,
                groups: Some(groups),
                error: None,
                stop,
            },
            Err(error) => crate::aside_processes::Info {
                retirement_id: id,
                groups: None,
                error: Some(error.to_string()),
                stop,
            },
        })
    }
    pub async fn stop_aside_processes(
        &self,
        request: crate::aside_processes::Request,
    ) -> Result<crate::aside_processes::Status> {
        let _guard = self.branches.lock().await;
        self.journal
            .change_aside_processes(crate::aside_processes::Change::Begin(request))
            .await
    }
    pub(crate) async fn finish_aside_processes(
        &self,
        request: crate::aside_processes::Request,
    ) -> Result<()> {
        let _guard = self.branches.lock().await;
        let status = self
            .journal
            .change_aside_processes(crate::aside_processes::Change::Begin(request.clone()))
            .await?;
        if status.state == "complete" {
            return Ok(());
        }
        let (entry, worker) = self
            .journal
            .aside_process_scope(request.retirement_id)
            .await?;
        if entry.track != status.record.track
            || entry.saved.as_ref().unwrap().session_id != status.record.session
        {
            return Err("The process Stop belongs to another worker session.".into());
        }
        self.journal
            .finish(&request.op_id.to_string(), "dispatched")
            .await?;
        if let Some((port, tracker, identity)) = self
            .port(&entry.track)
            .ok()
            .filter(|port| !port.is_closed())
            .and_then(|port| {
                Some((
                    port.clone(),
                    port.process_tracker()?,
                    worker.as_ref()?.identity?,
                ))
            })
        {
            let generation = port
                .stop_generation
                .load(std::sync::atomic::Ordering::Acquire);
            let guard = crate::inheritance_apply::Guard::Recovery {
                generation: port.stop_generation.clone(),
                expected: generation,
                gate: self.global_pause.clone(),
                epoch: self.global_pause.epoch(),
            };
            crate::inheritance_apply::stop_children(tracker, identity, guard).await?;
        } else {
            let directory = self.data.join("hosts").join(&entry.track);
            let host = worker.as_ref().and_then(|worker| worker.identity);
            tokio::task::spawn_blocking(move || -> Result<()> {
                if let Some(mut tracker) =
                    crate::process::Tracker::restore(&directory, &entry.workspace.path, host)?
                {
                    tracker.stop_confirmed()?;
                }
                Ok(())
            })
            .await??;
        }
        self.journal
            .change_aside_processes(crate::aside_processes::Change::Complete(request.op_id))
            .await?;
        Ok(())
    }
    pub(crate) async fn apply_path_restore(
        &self,
        request: crate::path_restore::Request,
    ) -> Result<()> {
        let _guard = self.branches.lock().await;
        let job = self.journal.restore_path(request.clone()).await?;
        if job.phase == crate::path_restore::Phase::Done {
            return Ok(());
        }
        if job.phase == crate::path_restore::Phase::Held {
            return Err(crate::path_restore::Waiting("This Restore is held.").into());
        }
        if self.journal.snapshot().await?["asideProcessStops"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|stop| {
                stop["record"]["request"]["retirementId"] == request.retirement_id.to_string()
            })
        {
            return Err(
                crate::path_restore::Waiting("Waiting for this path's processes to stop.").into(),
            );
        }
        if let Ok(port) = self.port(&job.track_id)
            && !port.is_closed()
        {
            return port.restore_path(request).await;
        }
        let workspace = self.journal.workspace(job.track_id.clone()).await?;
        let snapshot = self.journal.snapshot().await?;
        let mut worker = snapshot["workers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|worker| worker["trackId"] == job.track_id)
            .cloned()
            .map(serde_json::from_value::<WorkerState>)
            .transpose()?;
        let guard = crate::inheritance_apply::Guard::Recovery {
            generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            expected: 0,
            gate: self.global_pause.clone(),
            epoch: self.global_pause.epoch(),
        };
        let applied = crate::path_restore_apply::apply(
            &self.journal,
            request,
            crate::path_restore_apply::Boundary::Offline {
                guard,
                directory: self.data.join("hosts").join(&job.track_id),
                workspace: workspace.path,
                expected: worker.as_ref().and_then(|worker| worker.identity),
            },
        )
        .await?;
        if let Some(applied) = applied {
            if let Some(worker) = &mut worker {
                worker.checkpoint = Some(applied.checkpoint);
                worker.checkpoint_pending = false;
                self.journal.worker_state(worker.clone()).await?;
            }
            self.journal
                .change_path_restore(crate::path_restore_store::Change::Complete(
                    applied.operation,
                ))
                .await?;
        }
        Ok(())
    }
    pub(crate) async fn stop_path_boundary(
        &self,
        id: Uuid,
        track: &str,
    ) -> Result<(Option<String>, Option<crate::checkpoint::Checkpoint>)> {
        if let Ok(port) = self.port(track)
            && !port.is_closed()
        {
            let stopped = port.stop_and_wait(id).await?;
            let generation = port
                .stop_generation
                .load(std::sync::atomic::Ordering::Acquire);
            let hold = Uuid::new_v5(&id, b"hold-after-stop");
            self.journal.hold_inputs(hold, track.to_owned()).await?;
            port.saved_local_hold(generation, hold);
            return Ok((Some(stopped.session_id), stopped.checkpoint));
        }
        let snapshot = self.journal.snapshot().await?;
        let worker = snapshot["workers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|state| state["trackId"] == track);
        if worker.is_none() {
            return Ok((None, None));
        }
        if worker.is_some_and(|state| {
            state["checkpointPending"] == true || !state["activeTurn"].is_null()
        }) {
            return Err(
                "Recover the worker's final checkpoint before setting this path aside.".into(),
            );
        }
        self.stop(id, track).await?;
        let snapshot = self.journal.snapshot().await?;
        let worker = snapshot["workers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|state| state["trackId"] == track)
            .ok_or("The stopped worker is unavailable.")?;
        let worker: WorkerState = serde_json::from_value(worker.clone())?;
        if worker.active_turn.is_some()
            || worker.checkpoint_pending
            || !worker.approvals.is_empty()
            || !worker.prompts.is_empty()
        {
            return Err("Wait for the stopped worker's saved boundary.".into());
        }
        Ok((worker.session_id, worker.checkpoint))
    }
    pub async fn start_turn(&self, id: Uuid, track: &str) -> Result<()> {
        let snapshot = self.journal.snapshot().await?;
        let has_placed = snapshot["utterances"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|word| word["decision"]["trackId"] == track && word["state"] == "placed");
        if has_placed
            && let Ok(port) = self.port(track)
            && !port.is_closed()
        {
            return port.start(id).await;
        }
        if let Some(operation) = self.journal.operation(id).await? {
            if operation["kind"] == "utterance.start"
                && snapshot["utterances"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|word| {
                        word["id"] == operation["payload"]["utteranceId"]
                            && word["decision"]["trackId"] == track
                    })
            {
                return Ok(());
            }
            return Err("Operation ID reused.".into());
        }
        let word = snapshot["utterances"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|word| {
                word["decision"]["trackId"] == track
                    && (word["state"] == "held"
                        || word["state"] == "placed" && word["decision"]["boundary"] == "when-told")
            })
            .min_by_key(|word| {
                (
                    word["createdAt"].as_i64().unwrap_or_default(),
                    word["id"].as_str().unwrap_or_default(),
                )
            })
            .ok_or("No request is waiting on this worker.")?;
        self.start_held_input(
            id,
            word["id"].as_str().ok_or("Missing request ID.")?.to_owned(),
            word["revision"]
                .as_i64()
                .ok_or("Missing request revision.")?,
        )
        .await?;
        Ok(())
    }
    pub async fn stop_captured(&self, id: Uuid, track: &str, turn: Option<Uuid>) -> Result<()> {
        if let Ok(port) = self.port(track)
            && !port.is_closed()
        {
            port.stop_captured(id, turn)?;
            let generation = port
                .stop_generation
                .load(std::sync::atomic::Ordering::Acquire);
            let hold = Uuid::new_v5(&id, b"hold-after-stop");
            self.journal.hold_inputs(hold, track.to_owned()).await?;
            port.saved_local_hold(generation, hold);
            return Ok(());
        }
        let snapshot = self.journal.snapshot().await?;
        if snapshot["workers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|state| state["trackId"] == track)
            .any(|state| {
                serde_json::from_value::<WorkerState>(state.clone())
                    .is_ok_and(|state| state.released())
            })
        {
            return self
                .journal
                .hold_inputs(Uuid::new_v5(&id, b"hold-after-stop"), track.to_owned())
                .await;
        }
        self.stop(id, track).await
    }
    pub async fn stop_input_origin(
        &self,
        id: Uuid,
        track: String,
        identity: crate::input_origin::Identity,
    ) -> Result<()> {
        let service = self.clone();
        tokio::spawn(async move {
            let port = service.port(&track)?;
            port.stop_origin(id, &identity.session_id, identity.turn_id)
                .await?;
            let generation = port
                .stop_generation
                .load(std::sync::atomic::Ordering::Acquire);
            let hold = Uuid::new_v5(&id, b"hold-after-stop");
            service.journal.hold_inputs(hold, track).await?;
            port.saved_local_hold(generation, hold);
            Ok(())
        })
        .await?
    }
    pub async fn hold(&self, id: Uuid, track: &str) -> Result<()> {
        let port = self.port(track).ok().filter(|port| !port.is_closed());
        let generation = if let Some(port) = &port {
            port.hold(id)?;
            Some(
                port.stop_generation
                    .load(std::sync::atomic::Ordering::Acquire),
            )
        } else {
            None
        };
        self.journal.hold_inputs(id, track.to_owned()).await?;
        if let Some((port, generation)) = port.zip(generation) {
            port.saved_local_hold(generation, id);
        }
        Ok(())
    }
    pub async fn start_held_input(&self, id: Uuid, input: String, revision: i64) -> Result<Value> {
        if *self.stopping.borrow() {
            return Err("Shutdown has started.".into());
        }
        let snapshot = self.journal.snapshot().await?;
        let word = snapshot["utterances"]
            .as_array()
            .and_then(|words| words.iter().find(|word| word["id"] == input))
            .ok_or("The request is unavailable.")?;
        if word["decision"]["kind"] == "command" {
            return self.journal.start_held_input(id, input, revision).await;
        }
        let track = word["decision"]["trackId"]
            .as_str()
            .ok_or("The request needs a destination.")?;
        let port = self.port(track).ok().filter(|port| !port.is_closed());
        if port.is_none()
            && !snapshot["workers"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|state| state["trackId"] == track)
                .any(|state| {
                    serde_json::from_value::<WorkerState>(state.clone())
                        .is_ok_and(|state| state.released())
                })
        {
            return Err("Recover the worker before starting this request.".into());
        }
        let generation = port.as_ref().map(|port| {
            port.stop_generation
                .load(std::sync::atomic::Ordering::Acquire)
        });
        let result = self
            .journal
            .start_held_input(id, input.clone(), revision)
            .await?;
        if result["state"] == "placed" {
            let started = match (port, generation) {
                (Some(port), Some(generation)) => {
                    port.start_admitted(
                        Uuid::new_v5(&id, b"start-held"),
                        generation,
                        Some(input.clone()),
                    )
                    .await
                }
                _ => self.retry.send(track.to_owned()).map_err(Into::into),
            };
            if let Err(error) = started {
                let _ = self
                    .journal
                    .resolve_input(
                        Uuid::new_v5(&id, b"keep-held"),
                        input,
                        result["revision"].as_i64().unwrap(),
                        crate::decision::Resolution::Hold,
                    )
                    .await;
                return Err(error);
            }
        }
        Ok(result)
    }
    pub fn start_generation(&self, track: &str) -> Result<u64> {
        Ok(self
            .port(track)?
            .stop_generation
            .load(std::sync::atomic::Ordering::Acquire))
    }
    pub async fn start_correction(&self, id: Uuid, track: &str, generation: u64) -> Result<()> {
        let port = self.port(track)?;
        if port
            .stop_generation
            .load(std::sync::atomic::Ordering::Acquire)
            != generation
        {
            return Err("A newer Stop keeps this correction held.".into());
        }
        if port.stopped.load(std::sync::atomic::Ordering::Acquire) {
            port.start_at(id, generation).await?;
        }
        Ok(())
    }
    pub async fn answer(
        &self,
        id: Uuid,
        track: &str,
        approval: String,
        requirement: Value,
        response: crate::approval::Response,
    ) -> Result<()> {
        if crate::approval::replay(
            &self.journal,
            id,
            &crate::secret_input::Owner::Work.key(track),
            &approval,
            &requirement,
            &response,
        )
        .await?
        {
            return Ok(());
        }
        response.screen(&self.journal).await?;
        self.port(track)?
            .answer(id, approval, requirement, response)
            .await
    }
    pub async fn respond(
        &self,
        id: Uuid,
        track: &str,
        prompt: String,
        revision: String,
        response: crate::questions::Response,
    ) -> Result<()> {
        self.port(track)?
            .respond(id, prompt, revision, response)
            .await
    }

    pub async fn retry(&self, id: Uuid, track: String) -> Result<()> {
        let _guard = self.branches.lock().await;
        if self.journal.snapshot().await?["pickups"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|r| r["trackId"] == track && r["phase"] != "ready" && r["phase"] != "cancelled")
        {
            return Err("Finish this track's file recovery before retrying its connection.".into());
        }
        if let Some(previous) = self.journal.operation(id).await? {
            if previous["kind"] != "worker.retry" || previous["payload"]["trackId"] != track {
                return Err(std::io::Error::other("Operation ID reused.").into());
            }
            return Ok(());
        }
        let state = self.journal.snapshot().await?["workers"]
            .as_array()
            .and_then(|workers| workers.iter().find(|worker| worker["trackId"] == track))
            .cloned()
            .ok_or_else(|| std::io::Error::other("Worker is unavailable."))?;
        let state: WorkerState = serde_json::from_value(state)?;
        if state.identity.is_some_and(crate::process::is_alive) {
            return Err(std::io::Error::other("The previous worker is still alive.").into());
        }
        if self.journal.retry_worker(id, track.clone()).await? == crate::journal::Admission::New {
            if state.phase == "lost"
                && let Ok(port) = self.port(&track)
                && !port.is_closed()
            {
                port.retry(id).await?;
            } else {
                self.retry.send(track)?;
            }
        }
        Ok(())
    }
    pub async fn configure(&self, id: Uuid, source: PathBuf, credentials: PathBuf) -> Result<()> {
        if let Some(previous) = self.journal.operation(id).await? {
            if previous["kind"] != "worker.configure"
                || previous["payload"]["source"] != json!(source)
                || previous["payload"]["credentials"] != json!(credentials)
            {
                return Err(std::io::Error::other("Operation ID reused.").into());
            }
            return Ok(());
        }
        let config = Config::prepare(source, credentials, self.data.clone()).await?;
        self.journal.set_worker_config(id, config).await?;
        Ok(())
    }
}
impl Runtime {
    pub fn start(
        host_memory: Arc<crate::host_memory::Pool>,
        data: PathBuf,
        rpc: PathBuf,
        journal: Journal,
        editor: crate::editor::Manager,
        capacity: Arc<crate::scheduler::Capacity>,
        paused: Arc<crate::dispatch_control::Gate>,
    ) -> Self {
        let (stop, mut stopping) = watch::channel(false);
        let (retry, mut retrying) = mpsc::unbounded_channel();
        let ports = Arc::new(Mutex::new(HashMap::new()));
        let project_secrets = crate::project_secrets::Store::default();
        let branches = Arc::new(tokio::sync::Mutex::new(()));
        let recoveries = crate::pickup_run::Runs::default();
        let service = Service {
            capacity: capacity.clone(),
            assistant_capacity: Arc::new(crate::scheduler::Capacity::new(2)),
            host_memory,
            data: data.clone(),
            rpc: rpc.clone(),
            branches: branches.clone(),
            global_pause: paused.clone(),
            recoveries: recoveries.clone(),
            stopping: stopping.clone(),
            journal: journal.clone(),
            retry,
            ports: ports.clone(),
            project_secrets: project_secrets.clone(),
            paused_ports: Arc::default(),
        };
        let host_memory = service.host_memory.clone();
        let shutdown = stop.clone();
        let two_ways_driver = crate::two_ways_driver::Driver::start(
            service.clone(),
            journal.clone(),
            stopping.clone(),
        );
        let fresh_driver =
            crate::fresh_driver::Driver::start(service.clone(), journal.clone(), stopping.clone());
        let realign_driver = crate::realign_driver::Driver::start(
            service.clone(),
            journal.clone(),
            stopping.clone(),
        );
        let retirement_driver = crate::path_retirement_driver::Driver::start(
            service.clone(),
            journal.clone(),
            stopping.clone(),
        );
        let restore_driver = crate::path_restore_driver::Driver::start(
            service.clone(),
            journal.clone(),
            stopping.clone(),
        );
        let task = tokio::spawn(async move {
            let mut changes = journal.subscribe();
            let mut secret_changes = project_secrets.subscribe();
            let mut started = HashSet::new();
            let mut tasks = JoinSet::new();
            loop {
                changes.borrow_and_update();
                if *stopping.borrow() {
                    break;
                }
                if journal.advance_approval_replies().await.is_err() {
                    break;
                }
                let snapshot = match journal.snapshot().await {
                    Ok(value) => value,
                    Err(_) => break,
                };
                let config = match journal.worker_config().await {
                    Ok(value) => value,
                    Err(_) => break,
                };
                if let Some(config) = config {
                    for workspace in snapshot["workspaces"].as_array().into_iter().flatten() {
                        let Some(track) = workspace["trackId"].as_str() else {
                            continue;
                        };
                        if workspace["state"] != "ready"
                            || workspace["writerClosed"] == true
                            || started.contains(track)
                        {
                            continue;
                        }
                        let previous = snapshot["workers"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|state| state["trackId"] == track);
                        let previous = previous
                            .cloned()
                            .and_then(|value| serde_json::from_value::<WorkerState>(value).ok());
                        if previous.as_ref().is_some_and(WorkerState::released)
                            && snapshot["freshTracks"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .any(|job| {
                                    job["request"]["trackId"] == track
                                        && job["phase"] != "cancelled"
                                })
                        {
                            continue;
                        }
                        // A persisted Fresh boundary already owns source closure.
                        // Do not relabel it as generic dirty startup or hold its
                        // future words before the Fresh driver can release it.
                        if let Some(fresh) = snapshot["freshTracks"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|job| {
                                job["request"]["trackId"] == track && job["phase"] != "cancelled"
                            })
                            && (fresh["phase"] != "waiting-for-turn"
                                || previous.as_ref().is_some_and(|worker| {
                                    worker.active_turn.is_none()
                                        && !worker.checkpoint_pending
                                        && worker.checkpoint.is_some()
                                        && worker.approvals.is_empty()
                                        && worker.prompts.is_empty()
                                }))
                        {
                            continue;
                        }
                        let retiring = snapshot["pathRetirements"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .find(|entry| entry["track"] == track);
                        if retiring.is_some_and(|entry| {
                            entry["phase"] != "stopping"
                                || previous.as_ref().is_none_or(WorkerState::released)
                        }) {
                            continue;
                        }
                        let realign_requested = snapshot["realignments"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|job| {
                                job["request"]["trackId"] == track
                                    && matches!(
                                        job["phase"].as_str(),
                                        Some(
                                            "queued" | "waiting-for-worker" | "stopping-processes"
                                        )
                                    )
                            });
                        if previous.as_ref().is_some_and(WorkerState::released)
                            && !realign_requested
                            && !snapshot["utterances"].as_array().into_iter().flatten().any(
                                |word| {
                                    word["state"] == "placed"
                                        && word["dispatchRunnable"] != false
                                        && word["decision"]["trackId"] == track
                                        && word["decision"]["boundary"] != "when-told"
                                        && (word["startCondition"].is_null()
                                            || word["startCondition"]["ready"] == true)
                                },
                            )
                        {
                            // A cleanly closed completed track stays in SQLite. Only
                            // a newly released input needs another worker process.
                            continue;
                        }
                        if previous.as_ref().is_some_and(|state| {
                            ["failed", "needs-recovery", "lost", "after-connection-loss"]
                                .contains(&state.phase.as_str())
                        }) {
                            started.insert(track.to_string());
                            continue;
                        }
                        if previous
                            .as_ref()
                            .is_some_and(|state| state.phase != "retry" && !state.released())
                        {
                            let mut state = previous.unwrap();
                            state.phase = "needs-recovery".into();
                            state.detail=Some("The previous worker did not close cleanly. Recovery is required before sending more work.".into());
                            let _ = journal.worker_state(state).await;
                            started.insert(track.to_string());
                            continue;
                        }
                        // Explicit file-only work may reopen a saved session while
                        // turns remain paused. New sessions still need work admission.
                        let reconnect_for_realign = realign_requested
                            && previous
                                .as_ref()
                                .is_some_and(|state| state.session_id.is_some());
                        if paused.paused() && !reconnect_for_realign {
                            continue;
                        }
                        started.insert(track.to_string());
                        let data = data.clone();
                        let rpc = rpc.clone();
                        let journal = journal.clone();
                        let config = config.clone();
                        let workspace = workspace.clone();
                        let stopping = stopping.clone();
                        let (port, commands) = crate::execution::Port::new(paused.clone());
                        ports.lock().unwrap().insert(track.to_owned(), port.clone());
                        let execution = crate::execution::Context {
                            project_secrets: project_secrets.clone(),
                            port,
                            commands,
                            editor: editor.clone(),
                            capacity: capacity.clone(),
                        };
                        let host_memory = host_memory.clone();
                        let completed_track = track.to_owned();
                        tasks.spawn(async move {
                            run_host(Launch {
                                host_memory,
                                config,
                                data,
                                rpc,
                                journal,
                                workspace,
                                previous,
                                stopping,
                                execution,
                            })
                            .await;
                            completed_track
                        });
                    }
                }
                tokio::select! {
                    Some(track)=retrying.recv()=>{started.remove(&track);},
                    _=stopping.changed()=>{},
                    changed=changes.changed()=>{if changed.is_err(){break;}},
                    changed=secret_changes.changed()=>{if changed.is_err(){break;}},
                    completed=tasks.join_next(),if !tasks.is_empty()=>{if let Some(Ok(track))=completed{started.remove(&track);}},
                }
            }
            let _ = shutdown.send(true);
            recoveries.shutdown();
            // The stop watch is shared by every host; await their bounded shutdown.
            while tasks.join_next().await.is_some() {}
            two_ways_driver.shutdown().await;
            fresh_driver.shutdown().await;
            realign_driver.shutdown().await;
            retirement_driver.shutdown().await;
            restore_driver.shutdown().await;
            let _branches_closed = branches.lock().await;
        });
        Self {
            stop,
            task: Some(task),
            service,
        }
    }
    pub async fn shutdown(mut self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

pub(crate) struct Opened {
    pub id: String,
    pub recovery: Option<crate::reconcile::State>,
}
pub(crate) async fn open_session(
    connection: &mut crate::msp::Connection,
    journal: &Journal,
    workspace: &Path,
    previous: Option<&str>,
    track: &str,
    scanner: Option<&crate::secret_scan::Scanner>,
    expected: crate::reconcile::Expected,
) -> Result<Opened> {
    let reservation = journal.session_start(track).await?;
    let reserved = if previous.is_none() {
        reservation
    } else {
        None
    };
    let expected = if reserved.is_some() {
        crate::reconcile::Expected {
            turn: None,
            checkpoint: None,
            checkpoint_pending: false,
            replay: None,
        }
    } else {
        expected
    };
    let previous = previous.or_else(|| {
        reserved
            .as_ref()
            .filter(|resume| resume.opened)
            .map(|resume| resume.session_id.as_str())
    });
    if let Some(resume) = &reserved {
        connection
            .client
            .set_redaction(journal.session_mask(track, &resume.session_id).await?);
    }
    if let Some(session) = previous {
        connection
            .client
            .set_redaction(journal.session_mask(track, session).await?);
    }
    let initialized = connection.client.initialize().await?;
    if initialized["sessionDurability"] != "durable" {
        return Err("The worker host does not provide durable sessions.".into());
    }
    let id = reserved
        .as_ref()
        .filter(|resume| !resume.opened)
        .map(|resume| resume.start_command.to_string())
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let (method, params) = if let Some(session) = previous {
        (
            "session/resume",
            json!({"commandId":id,"sessionId":session,"excludeItems":false,"history":"snapshot"}),
        )
    } else {
        ("session/start", {
            let mut params = json!({"commandId":id,"workspaceRoot":workspace,"approvalMode":"promptUnmatched","providerId":"meta","modelId":"muse-spark-1.3-contributor","recoveryMode":"on"});
            #[cfg(test)]
            if host::OFFLINE_ECHO.try_with(|_| ()).is_ok() {
                params["providerId"] = json!("echo");
                params.as_object_mut().unwrap().remove("modelId");
            }
            if let Some(resume) = &reserved {
                params["sessionId"] = json!(resume.session_id);
            }
            params
        })
    };
    journal.intent(&id, method, params.clone()).await?;
    let result = if let Some(session) = previous {
        crate::history::resume(
            connection,
            crate::history::Context {
                journal,
                track,
                session,
                scanner,
            },
            id.parse()?,
        )
        .await?
    } else {
        connection.client.request(method, params).await?
    };
    journal.finish(&id, "dispatched").await?;
    if result
        .pointer("/session/permissionState/profileId")
        .and_then(Value::as_str)
        != Some("intercat-worker")
    {
        return Err(std::io::Error::other("Worker permission profile did not match.").into());
    }
    let session = result
        .pointer("/session/sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| std::io::Error::other("Worker did not return a session ID."))?
        .to_owned();
    if previous.is_some_and(|previous| previous != session) {
        return Err("The resumed host returned a different session.".into());
    }
    if reserved
        .as_ref()
        .is_some_and(|resume| resume.session_id != session)
    {
        return Err("The new recovery session has a different identity.".into());
    }
    connection
        .client
        .set_redaction(journal.session_mask(track, &session).await?);
    let recovery = if previous.is_some() {
        let head = result["viewCursor"]
            .as_str()
            .ok_or("The resumed session has no view cursor.")?;
        crate::history::recover(
            connection,
            crate::history::Context {
                journal,
                track,
                session: &session,
                scanner,
            },
            head,
            None,
        )
        .await?;
        Some(
            crate::reconcile::prepare(
                connection,
                crate::history::Context {
                    journal,
                    track,
                    session: &session,
                    scanner,
                },
                expected,
            )
            .await?,
        )
    } else {
        None
    };
    let skills = connection
        .client
        .request("skill/list", json!({"sessionId":session}))
        .await?;
    if !skills["skills"].as_array().is_some_and(|skills| {
        skills
            .iter()
            .any(|skill| skill["selector"] == "intercat-report")
    }) {
        return Err(std::io::Error::other("Worker report skill is unavailable.").into());
    }
    journal
        .append(
            "worker.session",
            json!({"sessionId":session,"result":result}),
        )
        .await?;
    journal.finish(&id, "complete").await?;
    if reserved.is_some() {
        journal.session_start_opened(track, &session).await?;
    }
    Ok(Opened {
        id: session,
        recovery,
    })
}

pub(crate) async fn release_session(
    host: &mut Host,
    journal: &Journal,
    session: &str,
    track: &str,
    scanner: Option<&crate::secret_scan::Scanner>,
) -> Result<()> {
    let id = Uuid::now_v7().to_string();
    let params = json!({"commandId":id,"sessionId":session,"stop":true});
    journal.intent(&id, "session/leave", params.clone()).await?;
    let accepted = host
        .connection
        .client
        .request("session/leave", params)
        .await?;
    if accepted["status"] != "accepted" {
        return Err("The worker did not accept session release.".into());
    }
    journal.finish(&id, "dispatched").await?;
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while let Some(event) = host.connection.next_event().await {
            let crate::msp::Event::Frame(received) = event else {
                return Err("The worker disconnected before releasing its session.".into());
            };
            let frame = received.frame;
            let params = &frame["params"];
            let display = match scanner {
                Some(scanner) => crate::recording::redact(scanner, frame.clone()).await?,
                None => crate::recording::sanitize(frame.clone(), &[]),
            };
            journal
                .try_msp(
                    track.into(),
                    session.into(),
                    received.stamp,
                    display,
                    Some(received.lease),
                    received.source,
                )?
                .await??;
            if frame["method"] == "session/leaveCompleted"
                && params["commandId"] == id
                && params["sessionId"] == session
            {
                journal
                    .append("worker.sessionReleased", params.clone())
                    .await?;
                if params["outcome"] != "released" || !params["viewCursor"].is_string() {
                    return Err("The worker did not confirm durable session release.".into());
                }
                journal.finish(&id, "complete").await?;
                return Ok(());
            }
        }
        Err::<(), Box<dyn std::error::Error + Send + Sync>>(
            "The worker closed before releasing its session.".into(),
        )
    })
    .await
    .map_err(|_| "Timed out waiting for durable session release.")?
}

struct Launch {
    host_memory: Arc<crate::host_memory::Pool>,
    config: Config,
    data: PathBuf,
    rpc: PathBuf,
    journal: Journal,
    workspace: Value,
    previous: Option<WorkerState>,
    stopping: watch::Receiver<bool>,
    execution: crate::execution::Context,
}
async fn run_host(launch: Launch) {
    let Launch {
        host_memory,
        config,
        data,
        rpc,
        journal,
        workspace,
        previous,
        stopping,
        execution,
    } = launch;
    let Some(track) = workspace["trackId"].as_str().map(str::to_owned) else {
        return;
    };
    let Some(path) = workspace["path"].as_str().map(PathBuf::from) else {
        return;
    };
    let state = WorkerState::starting(track, previous.as_ref());
    let directory = data.join("hosts").join(&state.track_id);
    let workspace_path = path.clone();
    let expected = previous.as_ref().and_then(|state| state.identity);
    let recovered = tokio::task::spawn_blocking(move || {
        crate::process::Tracker::restore(&directory, &workspace_path, expected)
    })
    .await;
    let tracker = match recovered {
        Ok(Ok(tracker))
            if !tracker
                .as_ref()
                .is_some_and(crate::process::Tracker::has_live_host) =>
        {
            tracker.map(|tracker| Arc::new(Mutex::new(tracker)))
        }
        result => {
            let mut held = state;
            held.identity = expected;
            held.phase = "needs-recovery".into();
            held.detail = Some(match result {
                Ok(Err(error)) => error.to_string(),
                Err(error) => error.to_string(),
                Ok(Ok(_)) => {
                    "The previous host is still alive. Stop it before reconnecting.".into()
                }
            });
            let _ = journal.worker_state(held).await;
            return;
        }
    };
    execution.port.track_processes(tracker.clone());
    let mut runner = WorkerRunner {
        host_memory,
        config,
        data,
        rpc,
        journal,
        path,
        state,
        stopping,
        execution,
        continuity: crate::execution::Continuity::default(),
        tracker,
        stable: false,
        environment: vec![],
        secret: None,
    };
    let result = runner.manage().await;
    let cleanup = loop {
        match runner.stop_owned().await {
            Ok(()) => break Ok(()),
            Err(error) if *runner.stopping.borrow() => break Err(error),
            Err(error) => {
                runner.state.phase = "needs-recovery".into();
                runner.state.detail = Some(error.to_string());
                let _ = runner.journal.worker_state(runner.state.clone()).await;
                // Keep the execution slot until process death is established.
                loop {
                    tokio::select! {
                        _=runner.stopping.changed()=>if *runner.stopping.borrow(){break;},
                        command=runner.execution.commands.recv()=>match command {
                            Some(crate::execution::Command::Stop{..})=>break,
                            Some(command)=>WorkerRunner::reject(command),None=>break,
                        }
                    }
                }
            }
        }
    };
    let pending = runner.execution.port.pending_secret.lock().unwrap().take();
    if let Some(pending) = pending {
        let _ = runner.journal.secret_failed(pending.target.op_id).await;
    }
    if let Some((target, _)) = runner.secret.take() {
        let _ = runner.journal.secret_failed(target.op_id).await;
    }
    let sleeping = result
        .as_ref()
        .is_err_and(|error| error.is::<crate::host_memory::Reclaimed>());
    runner.state.phase = if cleanup.is_err() {
        "needs-recovery"
    } else if sleeping {
        "sleeping"
    } else if result.is_ok() && runner.state.phase == "after-connection-loss" {
        "after-connection-loss"
    } else if result.is_ok() {
        "stopped"
    } else {
        "failed"
    }
    .into();
    if sleeping && cleanup.is_ok() {
        runner.state.identity = None;
        runner.state.detail = None;
    }
    runner.state.detail = result
        .err()
        .filter(|_| !sleeping)
        .or_else(|| cleanup.err())
        .map(|error| error.to_string())
        .or(runner.state.detail);
    let _ = runner.journal.worker_state(runner.state).await;
}

struct WorkerRunner {
    host_memory: Arc<crate::host_memory::Pool>,
    config: Config,
    data: PathBuf,
    rpc: PathBuf,
    journal: Journal,
    path: PathBuf,
    state: WorkerState,
    stopping: watch::Receiver<bool>,
    execution: crate::execution::Context,
    continuity: crate::execution::Continuity,
    tracker: Option<Arc<Mutex<crate::process::Tracker>>>,
    stable: bool,
    environment: Vec<crate::secret_input::Value>,
    secret: Option<(crate::secret_input::Target, u64)>,
}
impl WorkerRunner {
    async fn manage(&mut self) -> Result<()> {
        let mut retries = 0u32;
        let mut reconnecting = self.state.session_id.is_some();
        loop {
            if *self.stopping.borrow() {
                return Ok(());
            }
            if !self.await_project_secrets().await? {
                return Ok(());
            }
            if let Some((target, generation)) = &self.secret
                && *generation
                    != self
                        .execution
                        .port
                        .stop_generation
                        .load(std::sync::atomic::Ordering::Acquire)
            {
                self.journal.secret_failed(target.op_id).await?;
                self.environment.clear();
                self.secret = None;
                return Ok(());
            }
            if reconnecting {
                if retries == 3 {
                    self.state.phase = "lost".into();
                    self.state.detail = Some(format!(
                        "The worker lost its session. Retry its history or stop here. Its processes may still be running. {}",
                        self.state.detail.as_deref().unwrap_or("")
                    ));
                    self.journal.worker_state(self.state.clone()).await?;
                    if !self.wait_lost().await? {
                        return Ok(());
                    }
                    retries = 0;
                }
                retries += 1;
                self.state.phase = "reconnecting".into();
                self.state.detail = Some(format!("Reconnecting · attempt {retries} of 3"));
                self.journal.worker_state(self.state.clone()).await?;
                if !self
                    .wait_retry(std::time::Duration::from_millis(200u64 << (retries - 1)))
                    .await?
                {
                    return Ok(());
                }
            } else {
                self.journal.worker_state(self.state.clone()).await?;
            }
            match self.connection().await {
                Ok(()) => {
                    if self.execution.port.has_secret() {
                        self.stop_owned().await?;
                        let Some(install) =
                            self.execution.port.pending_secret.lock().unwrap().take()
                        else {
                            return Ok(());
                        };
                        if install.stop_generation
                            != self
                                .execution
                                .port
                                .stop_generation
                                .load(std::sync::atomic::Ordering::Acquire)
                        {
                            self.journal.secret_failed(install.target.op_id).await?;
                            return Ok(());
                        }
                        self.journal.secret_restarting(install.target.op_id).await?;
                        self.secret = Some((install.target, install.stop_generation));
                        self.environment = vec![install.value];
                        self.state.identity = None;
                        reconnecting = false;
                        continue;
                    }
                    if self
                        .execution
                        .port
                        .reconfigure
                        .swap(false, std::sync::atomic::Ordering::AcqRel)
                    {
                        self.state.identity = None;
                        reconnecting = false;
                        continue;
                    }
                    return Ok(());
                }
                Err(error) if self.secret.is_some() => {
                    let (target, _) = self.secret.take().unwrap();
                    self.environment.clear();
                    self.journal.secret_failed(target.op_id).await?;
                    return Err(error);
                }
                Err(error) if error.is::<crate::msp::ConnectionLost>() => {
                    if self.stable {
                        retries = 0;
                    }
                    reconnecting = true;
                    self.state.approvals.clear();
                    self.state.detail = Some(error.to_string());
                    self.journal.append("worker.reconnectFailed",json!({"trackId":self.state.track_id,"sessionId":self.state.session_id,"attempt":retries,"reason":error.to_string()})).await?;
                }
                Err(error) => return Err(error),
            }
        }
    }
    async fn await_project_secrets(&mut self) -> Result<bool> {
        let mut changes = self.journal.subscribe();
        let mut values = self.execution.project_secrets.subscribe();
        loop {
            if *self.stopping.borrow() {
                return Ok(false);
            }
            let snapshot = self.journal.snapshot().await?;
            let state = &snapshot["projectSecrets"];
            if state.is_null()
                || (state["ready"] == true
                    && state["projectId"]
                        .as_str()
                        .and_then(|id| self.execution.project_secrets.revision(id))
                        == state["revision"].as_u64())
            {
                return Ok(true);
            }
            if self.state.phase != "waiting-secrets" {
                self.state.phase = "waiting-secrets".into();
                self.state.detail = Some("Waiting for project secrets from Keychain.".into());
                self.journal.worker_state(self.state.clone()).await?;
            }
            tokio::select! {
                _=self.stopping.changed()=>{},
                _=changes.changed()=>{},
                _=values.changed()=>{},
                command=self.execution.commands.recv()=>match command {
                    Some(crate::execution::Command::Stop{id,..})=>{self.journal.append("worker.stop",json!({"opId":id,"trackId":self.state.track_id})).await?;},
                    Some(command)=>Self::reject(command),None=>return Ok(false),
                }
            }
        }
    }
    async fn wait_retry(&mut self, delay: std::time::Duration) -> Result<bool> {
        let timer = tokio::time::sleep(delay);
        tokio::pin!(timer);
        loop {
            tokio::select! {
                _=&mut timer=>return Ok(true),
                _=self.stopping.changed()=>if *self.stopping.borrow(){return Ok(false);},
                command=self.execution.commands.recv()=>match command {
                    Some(crate::execution::Command::Stop{id,..})=>{self.stop_here(id).await?;return Ok(false);},
                    Some(command)=>Self::reject(command),None=>return Ok(false),
                }
            }
        }
    }
    async fn wait_lost(&mut self) -> Result<bool> {
        loop {
            tokio::select! {
                _=self.stopping.changed()=>if *self.stopping.borrow(){return Ok(false);},
                command=self.execution.commands.recv()=>match command {
                    Some(crate::execution::Command::Stop{id,..})=>{self.stop_here(id).await?;return Ok(false);},
                    Some(crate::execution::Command::Retry{id,reply})=>{
                        let valid=self.journal.operation(id).await?.is_some_and(|op|op["kind"]=="worker.retry"&&op["payload"]["trackId"]==self.state.track_id);
                        if valid {let _=reply.send(Ok(()));return Ok(true);}
                        let _=reply.send(Err("Retry was not saved for this worker.".into()));
                    },
                    Some(command)=>Self::reject(command),None=>return Ok(false),
                }
            }
        }
    }
    fn reject(command: crate::execution::Command) {
        use crate::execution::Command;
        let error = "The worker is reconnecting. Wait for its current state.";
        match command {
            Command::Start { reply, .. }
            | Command::RetryCheckpoint { reply, .. }
            | Command::StopInheritance { reply, .. }
            | Command::FinishInheritance { reply, .. }
            | Command::Realign { reply, .. }
            | Command::RestorePath { reply, .. }
            | Command::StopRealignProcesses { reply, .. }
            | Command::Retry { reply, .. }
            | Command::Answer { reply, .. }
            | Command::Respond { reply, .. }
            | Command::StopOrigin { reply, .. }
            | Command::InstallSecret { reply, .. } => {
                let _ = reply.send(Err(error.into()));
            }
            Command::Restore { reply, .. } => {
                let _ = reply.send(Err(error.into()));
            }
            Command::Stop { .. } | Command::Hold { .. } => {}
        }
    }
    async fn stop_here(&mut self, id: Uuid) -> Result<()> {
        self.stop_owned().await?;
        let turn = self.state.active_turn;
        self.state.active_turn = None;
        self.state.checkpoint_pending = false;
        self.state.approvals.clear();
        self.state.prompts.clear();
        self.state.phase = "after-connection-loss".into();
        self.state.detail = Some("stopped · connection lost".into());
        self.journal
            .stop_lost_worker(id, self.state.clone(), turn)
            .await?;
        Ok(())
    }
    async fn record_final(&self, frames: Vec<crate::msp::Received>) -> Result<()> {
        let Some(session) = &self.state.session_id else {
            return Ok(());
        };
        for frame in frames {
            self.journal
                .try_msp(
                    self.state.track_id.clone(),
                    session.clone(),
                    frame.stamp,
                    crate::recording::sanitize(frame.frame, &[]),
                    Some(frame.lease),
                    frame.source,
                )?
                .await??;
        }
        Ok(())
    }
    async fn open_host(&mut self) -> Result<Host> {
        let workspace = self.path.canonicalize()?;
        let binary = self.config.binary.clone();
        if tokio::task::spawn_blocking(move || host::hash(&binary)).await?? != host::BINARY_SHA256 {
            return Err("The saved Muse binary changed. Configure workers again.".into());
        }
        if self.state.identity.is_some_and(crate::process::is_alive) {
            return Err(
                "The previous worker host is still alive. Stop it before reconnecting.".into(),
            );
        }
        if let Some(session) = &self.state.session_id {
            self.journal
                .restore_session_attachments(
                    session.clone(),
                    self.state.track_id.clone(),
                    self.path.clone(),
                )
                .await?;
        }
        let project = self.journal.snapshot().await?["projectSecrets"].clone();
        let mut environment = Vec::new();
        let mut usage = None;
        if let Some(id) = project["projectId"].as_str() {
            let config = self
                .execution
                .project_secrets
                .get(id)
                .ok_or("Project secrets are not loaded.")?;
            if project["ready"] != true
                || Some(config.manifest.revision) != project["revision"].as_u64()
            {
                return Err("Project secrets changed before host start.".into());
            }
            self.state.secret_revision = Some(config.manifest.revision);
            if !config.manifest.fingerprints.is_empty() {
                usage = Some((
                    id.to_owned(),
                    config.manifest.revision,
                    config.manifest.fingerprints.clone(),
                ));
            }
            environment = config.values;
        } else {
            self.state.secret_revision = None;
        }
        environment.append(&mut self.environment);
        let host = Host::spawn(
            &self.config.binary,
            &self
                .data
                .join("hosts")
                .join(&self.state.track_id)
                .join(Uuid::now_v7().to_string()),
            &self.path,
            std::ffi::OsStr::new(&self.config.path),
            Some(&self.config.credentials),
            host::Boundary {
                steps: Some(crate::step_capture::Service::start(
                    self.journal.clone(),
                    self.state.track_id.clone(),
                    &self.path,
                )?),
                profile: crate::host::Profile::Worker,
                secrets: &environment,
                rpc: &self.rpc,
                denied: &[],
            },
        )
        .await?;
        if let Some((project_id, revision, fingerprints)) = usage
            && let Err(error) = self
                .journal
                .project_host_use(crate::project_secret_usage::HostUse {
                    project_id,
                    revision,
                    fingerprints,
                    nonce: host.nonce.clone(),
                    track_id: self.state.track_id.clone(),
                    repair_run_id: None,
                    workspace,
                    identity: host.identity,
                })
                .await
        {
            let _ = host.shutdown().await;
            return Err(error);
        }
        if let Some(previous) = &self.tracker {
            host.tracker
                .lock()
                .unwrap()
                .inherit(&previous.lock().unwrap())?;
        }
        Ok(host)
    }
    async fn reserve_recovery_slot(&mut self) -> Result<bool> {
        if self.continuity.has_slot()
            || self
                .execution
                .port
                .stopped
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(true);
        }
        let Some(session) = &self.state.session_id else {
            return Ok(true);
        };
        let local = self.journal.reconcile_local(session.clone()).await?;
        if !local.operations.values().any(|op| {
            op.kind == "turn.start" && matches!(op.state.as_str(), "prepared" | "dispatched")
        }) {
            return Ok(true);
        }
        let ticket = self.execution.capacity.request(
            format!("recovery:{}", self.state.track_id),
            crate::scheduler::Priority::Continuation,
        );
        let reservation = ticket.acquire();
        tokio::pin!(reservation);
        loop {
            tokio::select! { biased;
                _ = self.stopping.changed() => if *self.stopping.borrow() { return Ok(false); },
                command = self.execution.commands.recv() => match command {
                    Some(crate::execution::Command::Stop { id, .. }) => {self.stop_here(id).await?;return Ok(false);},
                    Some(command) => Self::reject(command),
                    None => return Ok(false),
                },
                permit = &mut reservation => {
                    self.continuity.retain_slot(permit);
                    return Ok(true);
                },
            }
        }
    }
    async fn connection(&mut self) -> Result<()> {
        self.stable = false;
        if !self.reserve_recovery_slot().await? {
            return Ok(());
        }
        // Reopening durable history must remain possible while work is held:
        // the connected actor also handles file recovery and process controls.
        // open_session below still receives the work guard separately, so a
        // held request cannot be replayed by this host-memory admission.
        let guard = if self.state.session_id.is_some() {
            Some(crate::msp::StartGuard::Shutdown(self.stopping.clone()))
        } else {
            self.execution.port.start_guard()
        };
        let Some(guard) = guard else {
            return Ok(());
        };
        let mut ticket = self.host_memory.request(self.state.track_id.clone());
        let memory = if let Some(memory) = ticket.try_acquire() {
            memory
        } else {
            self.state.phase = "waiting-memory".into();
            self.state.detail = Some("queued · waiting for memory".into());
            self.journal.worker_state(self.state.clone()).await?;
            let admission = ticket.acquire(guard);
            tokio::pin!(admission);
            let admitted = loop {
                tokio::select! { biased;
                    _=self.stopping.changed()=>return Ok(()),
                    command=self.execution.commands.recv()=>match command {
                        Some(crate::execution::Command::Stop{id,..} | crate::execution::Command::Hold{id}) => {
                            if self.state.active_turn.is_some() || self.state.checkpoint_pending {
                                self.stop_here(id).await?;
                            } else {
                                self.journal.hold_inputs(id,self.state.track_id.clone()).await?;
                                self.state.phase="stopped".into();
                                self.state.detail=None;
                                self.journal.worker_state(self.state.clone()).await?;
                            }
                            return Ok(());
                        },
                        Some(command)=>Self::reject(command),
                        None=>return Ok(()),
                    },
                    result=&mut admission=>break result,
                }
            };
            match admitted {
                Ok(memory) => memory,
                Err(error) if error.is::<crate::msp::StartCancelled>() => {
                    return Ok(());
                }
                Err(error) => return Err(error),
            }
        };
        self.continuity.host_memory = Some(memory);
        self.journal
            .mount_dependency_references(&self.state.track_id)
            .await?;
        let mut host = match self.open_host().await {
            Ok(host) => host,
            Err(error) if self.state.session_id.is_some() => {
                return Err(crate::msp::ConnectionLost(error.to_string()).into());
            }
            Err(error) => return Err(error),
        };
        self.continuity
            .host_memory
            .as_ref()
            .unwrap()
            .attach(host.identity);
        self.tracker = Some(host.tracker.clone());
        self.execution.port.track_processes(self.tracker.clone());
        self.state.identity = Some(host.identity);
        let previous = self.state.session_id.clone();
        let mut checkpoint = self.state.checkpoint.as_ref().map(|c| c.turn_id);
        if let (Some(session), Some(turn)) = (&previous, checkpoint)
            && self
                .journal
                .pickup_checkpoint(self.state.track_id.clone(), session.clone(), turn)
                .await?
        {
            checkpoint = None;
        }
        let expected = crate::reconcile::Expected {
            turn: self.state.active_turn,
            checkpoint,
            checkpoint_pending: self.state.checkpoint_pending,
            replay: self.execution.port.start_guard(),
        };
        let mut opened = false;
        let result=async {
            let session=tokio::select!{
                _=self.stopping.changed()=>return Ok(()),
                result=open_session(&mut host.connection,&self.journal,&self.path,previous.as_deref(),&self.state.track_id,None,expected)=>result?,
            };
            opened=true;self.state.session_id=Some(session.id);
            if let Some(recovery)=&session.recovery {
                self.state.active_turn=recovery.resume_turn;self.state.checkpoint_pending=false;
                self.state.prompts=recovery.restore_prompts(&self.state.prompts);
                if recovery.resume_turn.is_none() {self.continuity.release_slot();}
            }
            if let Some((target,generation))=self.secret.as_ref() {
                if self.state.active_turn.is_some() || !self.state.prompts.is_empty() {return Err("The secret turn did not finish cleanly. Reconnect before continuing.".into());}
                self.journal.secret_ready(target.clone(),host.nonce.clone()).await?;
                if !self.execution.port.release_stop(*generation) {
                    self.journal.secret_failed(target.op_id).await?;self.secret=None;return Ok(());
                }
                self.secret=None;
            }
            self.state.phase=if self.state.active_turn.is_some(){"working"}else if self.state.prompts.is_empty(){"ready"}else{"question"}.into();
            self.state.detail=None;
            self.journal.worker_state(self.state.clone()).await?;
            self.execution.port.attach(host.connection.client.clone(),self.state.session_id.clone().unwrap(),self.state.active_turn);
            let began=std::time::Instant::now();let checkpoint=self.state.checkpoint.as_ref().map(|c|c.turn_id);
            let result=crate::execution::run(crate::execution::WorkerIo{connection:&mut host.connection,tracker:host.tracker.clone(),identity:host.identity,recovery:session.recovery,continuity:&mut self.continuity},&self.journal,&self.path,&mut self.state,&mut self.execution,&mut self.stopping).await;
            self.stable=began.elapsed()>=std::time::Duration::from_secs(20)||checkpoint!=self.state.checkpoint.as_ref().map(|c|c.turn_id);
            result
        }.await;
        self.execution.port.detach();
        let recoverable = result
            .as_ref()
            .is_err_and(|error| error.is::<crate::msp::ConnectionLost>())
            || (!opened && previous.is_some() && result.is_err());
        let host_nonce = host.nonce.clone();
        let reconfiguring = self
            .execution
            .port
            .reconfigure
            .load(std::sync::atomic::Ordering::Acquire);
        let frames = if recoverable || reconfiguring {
            if reconfiguring && let Some(session) = &self.state.session_id {
                let _ = release_session(
                    &mut host,
                    &self.journal,
                    session,
                    &self.state.track_id,
                    None,
                )
                .await;
            }
            host.retire().await?
        } else {
            if let Some(session) = &self.state.session_id {
                let _ = release_session(
                    &mut host,
                    &self.journal,
                    session,
                    &self.state.track_id,
                    None,
                )
                .await;
            }
            host.shutdown_received().await?
        };
        self.journal.secret_expired(host_nonce.clone()).await?;
        if !recoverable && !reconfiguring {
            self.journal.project_use_closed(host_nonce).await?;
        }
        self.record_final(frames).await?;
        self.continuity.settle().await?;
        self.continuity.host_memory = None;
        if recoverable {
            return Err(crate::msp::ConnectionLost(result.unwrap_err().to_string()).into());
        }
        result
    }
    async fn stop_owned(&mut self) -> Result<()> {
        self.execution.port.detach();
        self.continuity.settle().await?;
        if let Some(tracker) = &self.tracker {
            for force in [false, true] {
                let source = tracker.clone();
                tokio::task::spawn_blocking(move || -> Result<()> {
                    let mut tracker = source.lock().unwrap();
                    for process in tracker.refresh()?.processes {
                        tracker.terminate(process.identity, force)?;
                    }
                    Ok(())
                })
                .await??;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            if !tracker.lock().unwrap().refresh()?.is_empty() {
                return Err("Worker processes could not be confirmed stopped.".into());
            }
        }
        self.execution.port.track_processes(None);
        self.journal
            .unmount_dependency_references(&self.state.track_id)
            .await?;
        self.continuity.release_slot();
        self.continuity.host_memory = None;
        if let Some(session) = &self.state.session_id {
            self.journal.interrupt_effects(session.clone()).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
pub(crate) mod tests;
