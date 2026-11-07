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
    fn cleanly_stopped(&self) -> bool {
        self.phase == "stopped" && self.active_turn.is_none() && !self.checkpoint_pending
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
    capacity: Arc<tokio::sync::Semaphore>,
    data: PathBuf,
    rpc: PathBuf,
    branches: Arc<tokio::sync::Mutex<()>>,
    recoveries: crate::pickup_run::Runs,
    stopping: watch::Receiver<bool>,
    journal: Journal,
    retry: mpsc::UnboundedSender<String>,
    ports: Arc<Mutex<HashMap<String, crate::execution::Port>>>,
    project_secrets: crate::project_secrets::Store,
    paused_ports: Arc<Mutex<HashMap<String, u64>>>,
}
impl Service {
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
                &service.journal,
                config,
                &service.data,
                &service.rpc,
                recovery,
                service.capacity.clone(),
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
            if operation["kind"] == "worker.stopLost" && operation["payload"]["trackId"] == track {
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
    pub async fn start_turn(&self, id: Uuid, track: &str) -> Result<()> {
        self.port(track)?.start(id).await
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
                    .is_ok_and(|state| state.cleanly_stopped())
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
                        .is_ok_and(|state| state.cleanly_stopped())
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
                    port.start_admitted(Uuid::new_v5(&id, b"start-held"), generation, true)
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
        data: PathBuf,
        rpc: PathBuf,
        journal: Journal,
        editor: crate::editor::Manager,
        capacity: Arc<tokio::sync::Semaphore>,
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
            data: data.clone(),
            rpc: rpc.clone(),
            branches: branches.clone(),
            recoveries: recoveries.clone(),
            stopping: stopping.clone(),
            journal: journal.clone(),
            retry,
            ports: ports.clone(),
            project_secrets: project_secrets.clone(),
            paused_ports: Arc::default(),
        };
        let shutdown = stop.clone();
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
                        if workspace["state"] != "ready" || started.contains(track) {
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
                        if previous.as_ref().is_some_and(WorkerState::cleanly_stopped)
                            && !snapshot["utterances"].as_array().into_iter().flatten().any(
                                |word| {
                                    word["state"] == "placed"
                                        && word["decision"]["trackId"] == track
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
                        if previous.as_ref().is_some_and(|state| {
                            state.phase != "retry"
                                && (state.phase != "stopped"
                                    || state.active_turn.is_some()
                                    || state.checkpoint_pending)
                        }) {
                            let mut state = previous.unwrap();
                            state.phase = "needs-recovery".into();
                            state.detail=Some("The previous worker did not close cleanly. Recovery is required before sending more work.".into());
                            let _ = journal.worker_state(state).await;
                            started.insert(track.to_string());
                            continue;
                        }
                        if paused.paused() {
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
                        tasks.spawn(async move {
                            run_host(Launch {
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
                        });
                    }
                }
                tokio::select! {
                    Some(track)=retrying.recv()=>{started.remove(&track);},
                    _=stopping.changed()=>{},
                    changed=changes.changed()=>{if changed.is_err(){break;}},
                    changed=secret_changes.changed()=>{if changed.is_err(){break;}},
                    _=tasks.join_next(),if !tasks.is_empty()=>{},
                }
            }
            let _ = shutdown.send(true);
            recoveries.shutdown();
            // The stop watch is shared by every host; await their bounded shutdown.
            while tasks.join_next().await.is_some() {}
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
    let reserved = if previous.is_none() {
        journal.backup_resume(track).await?
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
            .filter(|(_, opened)| *opened)
            .map(|(resume, _)| resume.session_id.as_str())
    });
    if let Some((resume, _)) = &reserved {
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
        .filter(|(_, opened)| !*opened)
        .map(|(r, _)| r.start_command.to_string())
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let (method, params) = if let Some(session) = previous {
        (
            "session/resume",
            json!({"commandId":id,"sessionId":session,"excludeItems":false,"history":"snapshot"}),
        )
    } else {
        ("session/start", {
            let mut params = json!({"commandId":id,"workspaceRoot":workspace,"approvalMode":"promptUnmatched","providerId":"meta","modelId":"muse-spark-1.3-contributor","recoveryMode":"on"});
            if let Some((resume, _)) = &reserved {
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
        .is_some_and(|(resume, _)| resume.session_id != session)
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
        journal.backup_resume_opened(track, &session).await?;
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
    runner.state.phase = if cleanup.is_err() {
        "needs-recovery"
    } else if result.is_ok() && runner.state.phase == "after-connection-loss" {
        "after-connection-loss"
    } else if result.is_ok() {
        "stopped"
    } else {
        "failed"
    }
    .into();
    runner.state.detail = result
        .err()
        .or_else(|| cleanup.err())
        .map(|error| error.to_string())
        .or(runner.state.detail);
    let _ = runner.journal.worker_state(runner.state).await;
}

struct WorkerRunner {
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
        let capacity = self.execution.capacity.clone();
        loop {
            tokio::select! { biased;
                _ = self.stopping.changed() => if *self.stopping.borrow() { return Ok(false); },
                command = self.execution.commands.recv() => match command {
                    Some(crate::execution::Command::Stop { id, .. }) => {self.stop_here(id).await?;return Ok(false);},
                    Some(command) => Self::reject(command),
                    None => return Ok(false),
                },
                permit = capacity.clone().acquire_owned() => {
                    self.continuity.retain_slot(permit?);
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
        let mut host = match self.open_host().await {
            Ok(host) => host,
            Err(error) if self.state.session_id.is_some() => {
                return Err(crate::msp::ConnectionLost(error.to_string()).into());
            }
            Err(error) => return Err(error),
        };
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
            self.execution.port.attach(host.connection.client.clone(),self.state.session_id.clone().unwrap());
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
        self.continuity.release_slot();
        if let Some(session) = &self.state.session_id {
            self.journal.interrupt_effects(session.clone()).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
