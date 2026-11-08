//! One responsive execution loop per worker. Blocking effects return as events.
use crate::{Result, msp::Client, policy::Reason};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

fn input_priority(word: &Value) -> crate::scheduler::Priority {
    if word["reviewFix"].is_object() {
        crate::scheduler::Priority::Plan
    } else if word["replyTo"].is_object() {
        crate::scheduler::Priority::Continuation
    } else if word["planItemId"].is_string()
        && !word["planContinuation"].is_object()
        && word["planManualStart"] != true
    {
        crate::scheduler::Priority::Plan
    } else {
        crate::scheduler::Priority::User
    }
}

fn input_order<'a>(
    word: &'a Value,
    secret_resume: Option<&Value>,
) -> (u8, u8, crate::scheduler::Priority, &'a str) {
    (
        u8::from(word["decision"]["kind"] != "trim"),
        u8::from(secret_resume != Some(&word["id"])),
        input_priority(word),
        word["id"].as_str().unwrap_or(""),
    )
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub id: String,
    pub requirement: Value,
    pub subject: String,
    pub reason: Reason,
    pub can_allow: bool,
    pub can_deny: bool,
    pub reply_mode: Option<crate::approval::ReplyMode>,
    #[serde(default)]
    pub deletion: Option<crate::deletion::Summary>,
}
#[derive(Clone, Copy)]
pub enum StopTarget {
    All,
    Turn(Option<Uuid>),
}
impl StopTarget {
    fn matches(self, turn: Option<Uuid>) -> bool {
        match self {
            Self::All => true,
            Self::Turn(expected) => expected.is_some() && expected == turn,
        }
    }
}
pub enum Command {
    StopInheritance {
        command: crate::inheritance_issue::Command,
        generation: u64,
        pause_epoch: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    FinishInheritance {
        id: Uuid,
        application: Uuid,
        generation: u64,
        pause_epoch: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    InstallSecret {
        install: crate::secret_input::Install,
        reply: oneshot::Sender<Result<()>>,
    },
    Retry {
        id: Uuid,
        reply: oneshot::Sender<Result<()>>,
    },
    Restore {
        id: Uuid,
        snapshot: Uuid,
        reply: oneshot::Sender<Result<crate::deletion::Restored>>,
    },
    Respond {
        id: Uuid,
        prompt: String,
        revision: String,
        response: crate::questions::Response,
        sent: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    Start {
        id: Uuid,
        generation: u64,
        admitted_input: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    Hold {
        id: Uuid,
    },
    Stop {
        id: Uuid,
        target: StopTarget,
        generation: u64,
    },
    StopOrigin {
        id: Uuid,
        session: String,
        turn: Uuid,
        reply: oneshot::Sender<Result<()>>,
    },
    Answer {
        id: Uuid,
        approval: String,
        requirement: Value,
        response: crate::approval::Response,
        reply: oneshot::Sender<Result<()>>,
    },
}
#[derive(Clone)]
pub struct Port {
    tx: mpsc::UnboundedSender<Command>,
    pub(crate) pending_secret: Arc<Mutex<Option<crate::secret_input::Install>>>,
    pub(crate) reconfigure: Arc<AtomicBool>,
    pub(crate) stop_generation: Arc<AtomicU64>,
    start_generation: Arc<AtomicU64>,
    boundary_hold: Arc<AtomicBool>,
    resume_after_hold: Arc<Mutex<Option<(u64, Uuid)>>>,
    global_pause: Arc<crate::dispatch_control::Gate>,
    held_operations: Arc<Mutex<std::collections::HashSet<Uuid>>>,
    stop_target: Arc<Mutex<StopTarget>>,
    stop_guard: Arc<Mutex<()>>,
    pub stopped: Arc<AtomicBool>,
    emergency: Arc<Mutex<Option<(Client, String)>>>,
    prompts: Arc<Mutex<Vec<crate::questions::Prompt>>>,
    processes: Arc<Mutex<Option<Arc<Mutex<crate::process::Tracker>>>>>,
}
impl Port {
    pub async fn install_secret(&self, mut install: crate::secret_input::Install) -> Result<()> {
        self.require_connection()?;
        install.stop_generation = self.stop_generation.load(Ordering::Acquire);
        let (reply, result) = oneshot::channel();
        self.tx.send(Command::InstallSecret { install, reply })?;
        result.await?
    }
    fn mark_stop(&self, target: StopTarget, preserve_local_hold: bool) -> u64 {
        let _guard = self.stop_guard.lock().unwrap();
        *self.stop_target.lock().unwrap() = target;
        let previous = self.resume_after_hold.lock().unwrap().take();
        let generation = self.stop_generation.fetch_add(1, Ordering::AcqRel) + 1;
        if preserve_local_hold {
            *self.resume_after_hold.lock().unwrap() = previous.map(|(_, id)| (generation, id));
        }
        self.start_generation.fetch_add(1, Ordering::AcqRel);
        self.stopped.store(true, Ordering::Release);
        generation
    }
    fn retain_boundary_hold(&self, generation: u64) {
        let _guard = self.stop_guard.lock().unwrap();
        if self.stop_generation.load(Ordering::Acquire) == generation {
            self.boundary_hold.store(true, Ordering::Release);
            self.stopped.store(false, Ordering::Release);
        }
    }
    fn hold_starts(&self) {
        let _guard = self.stop_guard.lock().unwrap();
        *self.stop_target.lock().unwrap() = StopTarget::All;
        *self.resume_after_hold.lock().unwrap() = None;
        // Internal holds also cancel queued starts, without invalidating the
        // user's in-progress secret-install or resume operation.
        self.start_generation.fetch_add(1, Ordering::AcqRel);
        self.stopped.store(true, Ordering::Release);
    }
    pub(crate) fn start_guard(&self) -> Option<crate::msp::StartGuard> {
        let _guard = self.stop_guard.lock().unwrap();
        (!self.stopped.load(Ordering::Acquire)
            && !self.boundary_hold.load(Ordering::Acquire)
            && !self.global_pause.paused())
        .then(|| {
            crate::msp::StartGuard::new(self.start_generation.clone(), self.global_pause.clone())
        })
    }
    fn should_interrupt(&self, turn: Uuid) -> bool {
        self.stopped.load(Ordering::Acquire) && self.stop_target.lock().unwrap().matches(Some(turn))
    }
    pub(crate) fn release_stop(&self, generation: u64) -> bool {
        let _guard = self.stop_guard.lock().unwrap();
        if self.stop_generation.load(Ordering::Acquire) != generation {
            return false;
        }
        self.stopped.store(false, Ordering::Release);
        self.boundary_hold.store(false, Ordering::Release);
        true
    }
    pub(crate) fn saved_local_hold(&self, generation: u64, id: Uuid) {
        let _guard = self.stop_guard.lock().unwrap();
        if self.stop_generation.load(Ordering::Acquire) == generation && self.locally_held() {
            *self.resume_after_hold.lock().unwrap() = Some((generation, id));
        }
    }
    pub(crate) fn resume_for_new_input(&self, hold: &Value) -> bool {
        let _guard = self.stop_guard.lock().unwrap();
        if self.global_pause.paused() {
            return false;
        }
        let mut saved = self.resume_after_hold.lock().unwrap();
        let Some((generation, id)) = *saved else {
            return false;
        };
        if self.stop_generation.load(Ordering::Acquire) != generation
            || hold.as_str() != Some(&id.to_string())
        {
            return false;
        }
        self.stopped.store(false, Ordering::Release);
        self.boundary_hold.store(false, Ordering::Release);
        *saved = None;
        true
    }
    pub(crate) fn has_secret(&self) -> bool {
        self.pending_secret.lock().unwrap().is_some()
    }

    pub fn is_closed(&self) -> bool {
        self.tx.is_closed()
    }
    pub fn set_redaction(&self, session: &str, mask: crate::progress::Mask) {
        if let Some((client, current)) = self.emergency.lock().unwrap().as_ref()
            && current == session
        {
            client.set_redaction(mask);
        }
    }
    pub fn progress(&self) -> Option<crate::progress::Preview> {
        self.emergency
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|(client, _)| client.progress())
    }
    pub fn output_flow(&self) -> Option<crate::output_flow::Status> {
        self.emergency
            .lock()
            .unwrap()
            .as_ref()
            .map(|(client, _)| client.output_flow())
    }
    pub async fn retry(&self, id: Uuid) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.tx.send(Command::Retry { id, reply })?;
        result.await?
    }
    fn require_connection(&self) -> Result<()> {
        if self.emergency.lock().unwrap().is_none() {
            return Err("The worker is reconnecting. Wait for its current state.".into());
        }
        Ok(())
    }
    pub async fn restore(&self, id: Uuid, snapshot: Uuid) -> Result<crate::deletion::Restored> {
        let (reply, result) = oneshot::channel();
        self.tx.send(Command::Restore {
            id,
            snapshot,
            reply,
        })?;
        result.await?
    }
    pub async fn finish_inheritance(&self, id: Uuid, application: Uuid) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.tx.send(Command::FinishInheritance {
            id,
            application,
            generation: self.stop_generation.load(Ordering::Acquire),
            pause_epoch: self.global_pause.epoch(),
            reply,
        })?;
        result.await?
    }
    pub async fn stop_inheritance(&self, command: crate::inheritance_issue::Command) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.tx.send(Command::StopInheritance {
            command,
            generation: self.stop_generation.load(Ordering::Acquire),
            pause_epoch: self.global_pause.epoch(),
            reply,
        })?;
        result.await?
    }
    pub fn new(
        global_pause: Arc<crate::dispatch_control::Gate>,
    ) -> (Self, mpsc::UnboundedReceiver<Command>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                tx,
                pending_secret: Arc::new(Mutex::new(None)),
                reconfigure: Arc::new(AtomicBool::new(false)),
                stop_generation: Arc::new(AtomicU64::new(0)),
                start_generation: Arc::new(AtomicU64::new(0)),
                boundary_hold: Arc::new(AtomicBool::new(false)),
                resume_after_hold: Arc::default(),
                global_pause,
                held_operations: Arc::new(Mutex::new(std::collections::HashSet::new())),
                stop_target: Arc::new(Mutex::new(StopTarget::All)),
                stop_guard: Arc::new(Mutex::new(())),
                stopped: Arc::new(AtomicBool::new(false)),
                emergency: Arc::new(Mutex::new(None)),
                prompts: Arc::new(Mutex::new(vec![])),
                processes: Arc::new(Mutex::new(None)),
            },
            rx,
        )
    }
    pub fn attach(&self, client: Client, session: String) {
        *self.emergency.lock().unwrap() = Some((client, session));
    }
    pub fn detach(&self) {
        *self.emergency.lock().unwrap() = None;
    }
    pub(crate) fn track_processes(&self, tracker: Option<Arc<Mutex<crate::process::Tracker>>>) {
        *self.processes.lock().unwrap() = tracker;
    }
    pub fn stop(&self, id: Uuid) -> Result<()> {
        self.stop_target(id, StopTarget::All, true)
    }
    pub(crate) fn stop_all_turns(&self, id: Uuid) -> Result<()> {
        self.stop_target(id, StopTarget::All, false)
    }
    pub(crate) fn locally_held(&self) -> bool {
        self.stopped.load(Ordering::Acquire) || self.boundary_hold.load(Ordering::Acquire)
    }
    pub fn stop_captured(&self, id: Uuid, turn: Option<Uuid>) -> Result<()> {
        self.stop_target(id, StopTarget::Turn(turn), true)
    }
    pub async fn stop_origin(&self, id: Uuid, session: &str, turn: Uuid) -> Result<()> {
        let (client, current) = self
            .emergency
            .lock()
            .unwrap()
            .clone()
            .ok_or("The original worker is not connected.")?;
        if current != session || client.is_closed() {
            return Err("The original worker session is not connected.".into());
        }
        // The MSP request names the original turn even when actor state is delayed.
        client.interrupt_now(session, Some(turn))?;
        let (reply, result) = oneshot::channel();
        self.tx.send(Command::StopOrigin {
            id,
            session: session.into(),
            turn,
            reply,
        })?;
        result.await?
    }
    fn stop_target(&self, id: Uuid, target: StopTarget, stop_processes: bool) -> Result<()> {
        let generation = self.mark_stop(target, !stop_processes);
        let connected = if let Some((client, session)) = self.emergency.lock().unwrap().as_ref()
            && !client.is_closed()
        {
            for prompt in self.prompts.lock().unwrap().iter() {
                if !target.matches(prompt.request.turn_id.parse().ok()) {
                    continue;
                }
                let (_, params) = prompt.command(
                    Uuid::now_v7(),
                    &crate::questions::Response::Stop { answers: vec![] },
                )?;
                let _ = client.control_now("userInput/interrupt", params);
            }
            match target {
                StopTarget::All => {
                    let _ = client.interrupt_now(session, None);
                }
                StopTarget::Turn(Some(turn)) => {
                    let _ = client.interrupt_now(session, Some(turn));
                }
                StopTarget::Turn(None) => {}
            }
            true
        } else {
            false
        };
        if stop_processes
            && !connected
            && let Some(tracker) = self.processes.lock().unwrap().clone()
        {
            // A lost connection has no usable interrupt pipe. Stop here still
            // reaches the known processes while the actor or journal is blocked.
            tokio::task::spawn_blocking(move || -> Result<()> {
                for force in [false, true] {
                    {
                        let mut tracker = tracker.lock().unwrap();
                        for process in tracker.refresh()?.processes {
                            tracker.terminate(process.identity, force)?;
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                Ok(())
            });
        }
        if stop_processes {
            self.tx.send(Command::Stop {
                id,
                target,
                generation,
            })?;
        } else {
            self.tx.send(Command::Hold { id })?;
        }
        Ok(())
    }
    pub fn hold(&self, id: Uuid) -> Result<()> {
        if !self.held_operations.lock().unwrap().insert(id) {
            return Ok(());
        }
        let _guard = self.stop_guard.lock().unwrap();
        self.stop_generation.fetch_add(1, Ordering::AcqRel);
        self.start_generation.fetch_add(1, Ordering::AcqRel);
        self.boundary_hold.store(true, Ordering::Release);
        *self.resume_after_hold.lock().unwrap() = None;
        self.tx.send(Command::Hold { id })?;
        Ok(())
    }
    pub async fn respond(
        &self,
        id: Uuid,
        prompt: String,
        revision: String,
        response: crate::questions::Response,
    ) -> Result<()> {
        self.require_connection()?;
        let sent = matches!(response, crate::questions::Response::Stop { .. });
        if sent {
            let current = self
                .prompts
                .lock()
                .unwrap()
                .iter()
                .find(|p| p.request.user_input_id == prompt && p.revision == revision)
                .cloned()
                .ok_or_else(|| std::io::Error::other("The question changed."))?;
            let (method, params) = current.command(id, &response)?;
            self.mark_stop(StopTarget::All, false);
            let emergency = self.emergency.lock().unwrap();
            let (client, _) = emergency
                .as_ref()
                .ok_or_else(|| std::io::Error::other("Worker disconnected."))?;
            client.control_now(method, params)?;
        }
        let (reply, result) = oneshot::channel();
        self.tx.send(Command::Respond {
            id,
            prompt,
            revision,
            response,
            sent,
            reply,
        })?;
        result.await?
    }
    pub async fn start(&self, id: Uuid) -> Result<()> {
        self.start_at(id, self.stop_generation.load(Ordering::Acquire))
            .await
    }
    pub(crate) async fn start_at(&self, id: Uuid, generation: u64) -> Result<()> {
        self.start_admitted(id, generation, false).await
    }
    pub(crate) async fn start_admitted(
        &self,
        id: Uuid,
        generation: u64,
        admitted_input: bool,
    ) -> Result<()> {
        self.require_connection()?;
        if self.stop_generation.load(Ordering::Acquire) != generation {
            return Err("A newer Stop keeps this worker held.".into());
        }
        if self.has_secret() {
            return Err("A secret is being prepared for this worker.".into());
        }
        let (reply, result) = oneshot::channel();
        self.tx.send(Command::Start {
            id,
            generation,
            admitted_input,
            reply,
        })?;
        result.await?
    }
    pub async fn answer(
        &self,
        id: Uuid,
        approval: String,
        requirement: Value,
        response: crate::approval::Response,
    ) -> Result<()> {
        self.require_connection()?;
        let (reply, result) = oneshot::channel();
        self.tx.send(Command::Answer {
            id,
            approval,
            requirement,
            response,
            reply,
        })?;
        result.await?
    }
}

pub struct Context {
    pub project_secrets: crate::project_secrets::Store,
    pub port: Port,
    pub commands: mpsc::UnboundedReceiver<Command>,
    pub editor: crate::editor::Manager,
    pub capacity: Arc<crate::scheduler::Capacity>,
}
/// Resources outlive one transport connection. Outstanding writes finish before
/// another connection can restore the actor, and its execution slot stays held.
#[derive(Default)]
pub struct Continuity {
    permit: Option<crate::scheduler::Permit>,
    jobs: tokio::task::JoinSet<()>,
    pub(crate) host_memory: Option<crate::host_memory::Lease>,
}
impl Continuity {
    pub async fn settle(&mut self) -> Result<()> {
        while let Some(result) = self.jobs.join_next().await {
            result?;
        }
        Ok(())
    }
    pub fn has_slot(&self) -> bool {
        self.permit.is_some()
    }
    pub fn release_slot(&mut self) {
        self.permit = None;
    }
    pub(crate) fn retain_slot(&mut self, permit: crate::scheduler::Permit) {
        assert!(self.permit.is_none());
        self.permit = Some(permit);
    }
}
pub struct WorkerIo<'a> {
    pub connection: &'a mut crate::msp::Connection,
    pub tracker: Arc<Mutex<crate::process::Tracker>>,
    pub identity: crate::process::Identity,
    pub recovery: Option<crate::reconcile::State>,
    pub continuity: &'a mut Continuity,
}
struct Input {
    id: String,
    revision: i64,
    operation: Uuid,
    params: Value,
    reply_report: Option<String>,
    invalidated: bool,
    sent: bool,
    started_event: Option<Value>,
    unqueued_event: Option<Value>,
    queued: Option<Queued>,
}
struct Queued {
    facts: crate::start_queue::QueuedStart,
    wire: crate::msp::PendingRequest,
    began: std::time::Instant,
}
enum Work {
    StoppedInheritance {
        result: Result<()>,
        reply: oneshot::Sender<Result<()>>,
    },
    RecoveredInheritance {
        application: Uuid,
        result: Result<Option<crate::inheritance_apply::Applied>>,
        reply: oneshot::Sender<Result<()>>,
    },
    Inherited(Result<Option<crate::inheritance_apply::Applied>>),
    ApprovalReplySaved {
        result: Result<()>,
        reply: oneshot::Sender<Result<()>>,
    },
    SecretAccepted {
        install: crate::secret_input::Install,
        result: Result<()>,
        reply: oneshot::Sender<Result<()>>,
    },
    Restored {
        id: Uuid,
        result: Result<crate::deletion::Restored>,
        reply: oneshot::Sender<Result<crate::deletion::Restored>>,
    },
    BeforeDeleteFailed {
        params: Value,
        reply: Option<oneshot::Sender<Result<()>>>,
        message: String,
    },
    Policy {
        request: Value,
        result: Box<Result<(crate::policy::Verdict, Option<crate::deletion::Snapshot>)>>,
    },
    ReportObserved(Result<crate::reports::Observation>),
    PromptSaved {
        result: Result<crate::journal::Admission>,
        method: &'static str,
        params: Value,
        reply: oneshot::Sender<Result<()>>,
    },
    PromptDone {
        result: Result<Value>,
        params: Value,
        reply: oneshot::Sender<Result<()>>,
    },
    Scanned(std::io::Result<crate::process::Scan>),
    Snapshot(Result<Value>),
    Reserved(
        Result<crate::journal::Admission>,
        Option<crate::screening::Command>,
    ),
    Started(Result<Value>),
    QueueIntent {
        start: Uuid,
        result: Result<()>,
    },
    Persisted(Result<()>),
    CheckpointSaved(Result<crate::checkpoint::Checkpoint>),
    Release {
        result: Result<crate::journal::Admission>,
        id: Uuid,
        generation: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    ApprovalSaved {
        result: Result<crate::journal::Admission>,
        params: Value,
        reply: Option<oneshot::Sender<Result<()>>>,
    },
    ApprovalDone {
        result: Result<Value>,
        params: Value,
        reply: Option<oneshot::Sender<Result<()>>>,
    },
}
fn prepare_reserved_input(
    jobs: &mut tokio::task::JoinSet<()>,
    journal: &crate::journal::Journal,
    tx: &mpsc::UnboundedSender<Work>,
    input: &Input,
    track: String,
    session: String,
    path: std::path::PathBuf,
) {
    let journal = journal.clone();
    let tx = tx.clone();
    let id = input.id.clone();
    let revision = input.revision;
    let operation = input.operation;
    let params = input.params.clone();
    jobs.spawn(async move {
        let result = async {
            let prepared = journal
                .prepare_turn(id.clone(), track.clone(), session, path, params)
                .await?;
            journal
                .reserve_turn(operation, id, revision, track, prepared.params().clone())
                .await
                .map(|admission| (admission, prepared))
        }
        .await;
        match result {
            Ok((admission, params)) => {
                let _ = tx.send(Work::Reserved(Ok(admission), Some(params)));
            }
            Err(error) => {
                let _ = tx.send(Work::Reserved(Err(error), None));
            }
        }
    });
}
fn write_state(
    jobs: &mut tokio::task::JoinSet<()>,

    journal: &crate::journal::Journal,
    state: &crate::runtime::WorkerState,
    tx: &mpsc::UnboundedSender<Work>,
) -> Result<()> {
    // Submit synchronously to preserve actor order even when the writer is busy.
    let result = journal.try_worker_state(state.clone())?;
    let tx = tx.clone();
    jobs.spawn(async move {
        let result = match result.await {
            Ok(value) => value,
            Err(error) => Err(error.into()),
        };
        let _ = tx.send(Work::Persisted(result));
    });
    Ok(())
}

fn task_write(
    jobs: &mut tokio::task::JoinSet<()>,

    journal: &crate::journal::Journal,
    tx: &mpsc::UnboundedSender<Work>,
    input: &Input,
    state: &'static str,
    finish: Option<&'static str>,
) {
    let journal = journal.clone();
    let tx = tx.clone();
    let id = input.id.clone();
    let rev = input.revision;
    let op = input.operation.to_string();
    jobs.spawn(async move {
        let result = async {
            journal.input_execution(id, rev, state).await?;
            if let Some(state) = finish {
                journal.finish(&op, state).await?;
            }
            Ok(())
        }
        .await;
        let _ = tx.send(Work::Persisted(result));
    });
}
enum BeforeDelete {
    Saved(Uuid),
    Fresh {
        workspace: Box<crate::preparation::Workspace>,
        request: Value,
        parent: String,
    },
}
fn persist_answer(
    jobs: &mut tokio::task::JoinSet<()>,

    journal: &crate::journal::Journal,
    tx: &mpsc::UnboundedSender<Work>,
    params: Value,
    reply: Option<oneshot::Sender<Result<()>>>,
    before: Option<BeforeDelete>,
) {
    let journal = journal.clone();
    let tx = tx.clone();
    jobs.spawn(async move {
        let snapshot = match before {
            Some(BeforeDelete::Saved(id)) => Some(id),
            Some(BeforeDelete::Fresh {
                workspace,
                request,
                parent,
            }) => match async {
                let snapshot = crate::deletion::prepare(
                    (*workspace).clone(),
                    None,
                    request.clone(),
                    parent,
                    journal.clone(),
                )
                .await?;
                journal
                    .evaluate_approval(
                        workspace.track_id,
                        workspace.path,
                        Vec::new(),
                        request,
                        Some(snapshot.clone()),
                    )
                    .await?;
                Ok::<_, Box<dyn std::error::Error + Send + Sync>>(snapshot)
            }
            .await
            {
                Ok(snapshot) => Some(snapshot.id),
                Err(error) => {
                    let _ = tx.send(Work::BeforeDeleteFailed {
                        params: params.clone(),
                        reply,
                        message: error.to_string(),
                    });
                    return;
                }
            },
            None => None,
        };
        if let Some(snapshot) = snapshot
            && let Err(error) = journal
                .append(
                    "deletion.prepared",
                    serde_json::json!({"commandId":params["commandId"],"snapshotId":snapshot}),
                )
                .await
        {
            let _ = tx.send(Work::ApprovalSaved {
                result: Err(error),
                params,
                reply,
            });
            return;
        }
        let result = journal
            .intent(
                params["commandId"].as_str().unwrap(),
                "approval.decide",
                params.clone(),
            )
            .await;
        let _ = tx.send(Work::ApprovalSaved {
            result,
            params,
            reply,
        });
    });
}
fn short_text(text: &str) -> String {
    text.chars().take(32768).collect()
}
fn observe_report(
    jobs: &mut tokio::task::JoinSet<()>,

    journal: &crate::journal::Journal,
    tx: &mpsc::UnboundedSender<Work>,
    report: crate::reports::Report,
) {
    let journal = journal.clone();
    let tx = tx.clone();
    jobs.spawn(async move {
        let result = journal.observe_report(report).await;
        let _ = tx.send(Work::ReportObserved(result));
    });
}

fn secret_question_current(
    target: &crate::secret_input::Target,
    state: &crate::runtime::WorkerState,
    snapshot: &Value,
) -> bool {
    if target.owner != crate::secret_input::Owner::Work
        || state.session_id.as_deref() != Some(&target.session_id)
        || state.track_id != target.track_id
    {
        return false;
    }
    match &target.question {
        crate::secret_input::Question::Msp { id, revision } => {
            state.active_turn == Some(target.turn_id)
                && state.prompts.iter().any(|prompt| {
                    prompt.request.user_input_id == *id
                        && prompt.revision == *revision
                        && prompt.request.turn_id == target.turn_id.to_string()
                        && prompt.phase == crate::questions::Phase::Open
                })
        }
        crate::secret_input::Question::Report { id, revision } => snapshot["reportQuestions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|question| {
                question["id"] == *id
                    && question["revision"] == *revision
                    && question["trackId"] == target.track_id
                    && question["sessionId"] == target.session_id
                    && question["turnId"] == target.turn_id.to_string()
                    && matches!(question["state"].as_str(), Some("open" | "held"))
                    && question["replyUtteranceId"].is_null()
            }),
    }
}

pub async fn run(
    io: WorkerIo<'_>,
    journal: &crate::journal::Journal,
    path: &std::path::Path,
    state: &mut crate::runtime::WorkerState,
    context: &mut Context,
    stopping: &mut tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    use crate::{
        journal::Admission,
        run::{Effect, Hold, Run},
    };
    let session = state
        .session_id
        .clone()
        .ok_or_else(|| std::io::Error::other("Worker session is missing."))?;
    let client = io.connection.client.clone();
    let delivery_track = state.track_id.clone();
    client.configure_progress(
        session.clone(),
        crate::progress::Privacy::KnownValues,
        state.active_turn.map(|turn| turn.to_string()),
    );
    let mut inbox = crate::delivery::Inbox::new(crate::history::Context {
        journal,
        track: &delivery_track,
        session: &session,
        scanner: None,
    });
    let mut preparing_secret: Option<Uuid> = None;
    let mut input: Option<Input> = None;
    let outcome = async {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut changes = journal.subscribe();
    let mut editor = context.editor.subscribe();
    let mut project_changes = context.project_secrets.subscribe();
    let mut capacity_changes = context.capacity.subscribe();
    let mut ticket: Option<crate::scheduler::Ticket> = None;
    // Commands can arrive as soon as the port attaches. Establish the saved
    // queue before accepting Start; an empty placeholder would reject real work.
    let mut snapshot = journal.snapshot().await?;
    let mut snapshot_busy = false;
    let mut dirty = true;
    let mut restoring = false;
    let mut pending_recovery:Option<Command>=None;
    let mut restore_operation: Option<Uuid> = None;
    let mut completed_restores = std::collections::HashSet::new();
    let mut fetched_outputs = std::collections::HashSet::new();
    let jobs = &mut io.continuity.jobs;
    let permit = &mut io.continuity.permit;
    if state.active_turn.is_some() && permit.is_none() {
        *permit = Some(context.capacity.request(format!("recovery:{}", state.track_id), crate::scheduler::Priority::Continuation).acquire().await);
    }
    if let (Some(turn), Some(permit)) = (state.active_turn, permit.as_mut()) {
        permit.bind_turn(&state.track_id, turn)?;
    }
    let mut run = Run::new();
    run.reconciled(state.active_turn);
    let mut recovered_command = io.recovery.as_ref().and_then(|state| state.command);
    let decided = io
        .recovery
        .as_ref()
        .map(|state| state.decided_approvals.clone())
        .unwrap_or_default();
    for prompt in &state.prompts {
        run.question(prompt.key());
        client.notify_now(
            "userInput/engaged",
            serde_json::json!({"sessionId":session,"userInputId":prompt.request.user_input_id}),
        )?;
    }
    let mut manual = false;
    let mut terminals = std::collections::HashMap::<Uuid, String>::new();
    let mut settled_prompts = std::collections::HashSet::<String>::new();
    let mut approval_stages = crate::approval::Stages::new(decided);
    let mut approval_queue = std::collections::VecDeque::<Value>::new();
    let mut pending_writes = 0_usize;
    let mut pending_approvals = 0_usize;
    let mut saving_approval_reply = false;
    let mut scan_busy = false;
    let mut has_children = true;
    let mut scan_timer = tokio::time::interval(std::time::Duration::from_millis(250));
    let mut report_observations = 0usize;
    let mut report_barrier = 0i64;
    let mut final_reports = std::collections::HashSet::<Uuid>::new();
    let mut redirected = std::collections::HashSet::new();
    loop {
        while let Some(result) = jobs.try_join_next() {
            result?;
        }
        while let Some(request)=approval_queue.pop_front() {
            if !approval_stages.current(&request) {continue;}
            let params=&request;
            let id=params["approvalId"].as_str().ok_or("Missing approval ID")?.to_owned();
            state.approvals.retain(|card|card.id!=id);state.phase="approval".into();state.detail=None;write_state(jobs, journal,state,&tx)?;pending_writes+=1;
            if approval_stages.is_decided(&id,&params["currentRequirementId"]) {
                state.approvals.push(Approval{id,requirement:params["currentRequirementId"].clone(),subject:params["subject"]["command"].as_str().unwrap_or("Recovered action").into(),reason:crate::policy::Reason{kind:"reconnecting".into(),message:"The previous answer is still awaiting host confirmation. Stop or reconnect before answering again.".into()},can_allow:false,can_deny:false,reply_mode:None,deletion:None});
                write_state(jobs, journal,state,&tx)?;pending_writes+=1;continue;
            }
            let request=params.clone();let path=path.to_path_buf();let track=state.track_id.clone();let journal=journal.clone();let tx=tx.clone();

            let parent=state.checkpoint.as_ref().map(|p|p.sha.clone());
            jobs.spawn(async move{
                let result=async{
                    let hosts=if request["subject"]["kind"]=="network" {
                        let source=path.clone();tokio::task::spawn_blocking(move||intercat_git::Repository::open(source)?.remote_hosts()).await??
                    }else{Vec::new()};
                    let deletion=if crate::deletion::requested(&request,&path) {
                        let saved=async {
                            let workspace=journal.workspace(track.clone()).await?;
                            let base=parent.unwrap_or_else(||workspace.base.clone());
                            crate::deletion::prepare(workspace,None,request.clone(),base,journal.clone()).await
                        }.await;
                        match saved {Ok(snapshot)=>Some(snapshot),Err(error)=>{journal.append("deletion.failed",serde_json::json!({"trackId":track,"itemId":request["itemId"],"error":error.to_string()})).await?;None}}
                    }else{None};
                    let verdict=journal.evaluate_approval(track,path,hosts,request.clone(),deletion.clone()).await?;
                    Ok((verdict,deletion))
                }.await;
                let _=tx.send(Work::Policy{request,result:Box::new(result)});
            });pending_writes+=1;
        }
        run.set_hold(
            Hold::Reconnecting,
            inbox.has_pending() || io.connection.has_pending_events(),
        );
        run.set_hold(
            Hold::Approval,
            !approval_stages.is_empty() || pending_approvals > 0,
        );
        let project = &snapshot["projectSecrets"];
        let project_changed = project["projectId"].as_str().is_some_and(|id| {
            project["ready"] != true || project["revision"].as_u64() != state.secret_revision
                || context.project_secrets.revision(id)!=state.secret_revision
        });
        run.set_hold(Hold::Auth, preparing_secret.is_some() || context.port.has_secret() || project_changed);
        let flow = client.output_flow();
        run.set_hold(Hold::Output, flow.lagging || flow.interrupted);
        if flow.interrupted && state.active_turn.is_none() {
            return Err(std::io::Error::other("output too fast to record").into());
        }
        *context.port.prompts.lock().unwrap() = state.prompts.clone();
        if project_changed && project["ready"] == true && !context.port.has_secret()
            && project["projectId"].as_str().and_then(|id|context.project_secrets.revision(id))==project["revision"].as_u64()
            && state.active_turn.is_none() && input.is_none() && !state.checkpoint_pending
            && state.prompts.is_empty() && pending_writes==0 && pending_approvals==0 && !restoring {
            context.port.reconfigure.store(true,Ordering::Release);
            state.phase="reconfiguring".into();journal.worker_state(state.clone()).await?;return Ok(());
        }
        if context.port.has_secret() && state.active_turn.is_none() && input.is_none()
            && !state.checkpoint_pending && state.prompts.is_empty() && pending_writes==0
            && pending_approvals==0 && !restoring {
            state.phase="installing-secret".into();
            journal.worker_state(state.clone()).await?;
            return Ok(());
        }
        if *stopping.borrow() {
            context.port.hold_starts();
            let _ = client.interrupt_now(&session, None);
            if input.is_some() {
                state.checkpoint_pending = true;
            }
            if !restoring {
                return Ok(());
            }
        }
        if let Some(memory) = &io.continuity.host_memory {
            let idle = state.active_turn.is_none() && input.is_none() && !state.checkpoint_pending
                && state.prompts.is_empty() && state.approvals.is_empty() && approval_stages.is_empty() && pending_approvals == 0
                && pending_writes == 0 && !restoring && !has_children && !context.port.has_secret()
                && !snapshot["pendingInheritance"].as_array().into_iter().flatten().any(|pending| pending["trackId"] == state.track_id)
                && !snapshot["inheritanceIssues"].as_array().into_iter().flatten().any(|issue| issue["trackId"] == state.track_id)
                && !snapshot["pendingRestores"].as_array().into_iter().flatten().any(|restore| restore["trackId"] == state.track_id)
                && !snapshot["reportQuestions"].as_array().into_iter().flatten().any(|question| question["trackId"] == state.track_id && matches!(question["state"].as_str(), Some("open" | "held")));
            if idle {
                memory.set_idle();
                if memory.reclaim_requested() {
                    let tracker = io.tracker.clone();
                    let scan = tokio::task::spawn_blocking(move || tracker.lock().unwrap().refresh()).await??;
                    has_children = scan.processes.iter().any(|process| process.identity != io.identity)
                        || scan.groups.iter().any(|group| *group != io.identity) || !scan.unreadable.is_empty();
                    if has_children { memory.defer_reclaim(); }
                    else { return Err(crate::host_memory::Reclaimed.into()); }
                }
            } else if !memory.claim_work() {
                memory.defer_reclaim();
            }
        }
        if input.as_ref().and_then(|input|input.queued.as_ref()).is_some_and(|queued|queued.began.elapsed()>=std::time::Duration::from_secs(30)) {
            return Err(crate::msp::ConnectionLost("The queued request's outcome is still unknown. Reconcile before continuing.".into()).into());
        }
        if !snapshot_busy && dirty {
            dirty = false;
            changes.borrow_and_update();
            snapshot_busy = true;
            let journal = journal.clone();
            let tx = tx.clone();
            jobs.spawn(async move {
                let _ = tx.send(Work::Snapshot(journal.snapshot().await));
            });
        }
        run.set_hold(Hold::Editor, !context.editor.status().model_loaded);
        if !restoring
            && let Some(pending) = snapshot["pendingRestores"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|operation| {
                    operation["trackId"] == state.track_id
                        && operation["repairRunId"].is_null()
                        && operation["operationId"]
                            .as_str()
                            .and_then(|s| s.parse::<Uuid>().ok())
                            .is_some_and(|id| !completed_restores.contains(&id))
                })
        {
            restore_operation = pending["operationId"]
                .as_str()
                .and_then(|id| id.parse().ok());
            context.port.hold_starts();
            run.set_hold(Hold::Reconciling, true);
            if state.phase != "needs-recovery" {
                state.phase = "needs-recovery".into();
                state.detail = Some(
                    "An unfinished file restore must be retried before new work can start.".into(),
                );
                write_state(jobs, journal, state, &tx)?;
                pending_writes += 1;
            }
        }
        if state.active_turn.is_none() && !state.checkpoint_pending && input.is_none() && !restoring {
            context.port.resume_for_new_input(&snapshot["localResumes"][&state.track_id]);
        }
        run.set_hold(Hold::Stopping, context.port.stopped.load(Ordering::Acquire)||context.port.global_pause.paused());
        run.set_hold(Hold::Boundary, context.port.boundary_hold.load(Ordering::Acquire));
        let review_blocked=snapshot["reviewFixes"].as_array().into_iter().flatten().any(|fix|fix["trackId"]==state.track_id && fix["state"]=="blocked");
        let reviewing_fix=snapshot["reviewFixes"].as_array().into_iter().flatten().any(|fix|fix["trackId"]==state.track_id && fix["state"]=="awaiting-review");
        run.set_hold(Hold::Review,review_blocked || reviewing_fix);
        if state.active_turn.is_none() && !state.checkpoint_pending && input.is_none() {
            if review_blocked && state.phase!="review-conflict" {state.phase="review-conflict".into();state.detail=Some("Review conflict needs your decision".into());write_state(jobs,journal,state,&tx)?;pending_writes+=1;}
            else if !review_blocked && reviewing_fix && state.phase!="reviewing-fix" {state.phase="reviewing-fix".into();state.detail=Some("Reviewer is checking the saved fix".into());write_state(jobs,journal,state,&tx)?;pending_writes+=1;}
            else if !review_blocked && !reviewing_fix && matches!(state.phase.as_str(),"review-conflict"|"reviewing-fix") {state.phase="ready".into();state.detail=None;write_state(jobs,journal,state,&tx)?;pending_writes+=1;}
        }
        let redirects = crate::redirect::stopping(&snapshot, &state.track_id);
        let redirecting = !redirects.is_empty() || saving_approval_reply;
        run.set_hold(Hold::Redirect, redirecting);
        for redirect in redirects {
            if context.port.stopped.load(Ordering::Acquire) || redirected.contains(&(redirect.input_id.clone(), redirect.revision)) { continue; }
            if redirect.session_id == session && state.active_turn == Some(redirect.turn_id) {
                let Some(redirect) = journal.claim_redirect(redirect.input_id.clone(), redirect.revision).await? else { continue; };
                redirected.insert((redirect.input_id, redirect.revision));
                for prompt in &state.prompts {
                    let (method, params) = prompt.command(Uuid::now_v7(), &crate::questions::Response::Stop { answers: vec![] })?;
                    client.control_now(method, params)?;
                }
                client.interrupt_now(&session, Some(redirect.turn_id))?;
                state.phase = "stopping".into();
                state.detail = Some("Redirecting after this turn's checkpoint is saved.".into());
            }
            // The same transaction that saves a completed checkpoint releases
            // its correction. A changed session/turn holds it instead.
            write_state(jobs, journal, state, &tx)?;
            pending_writes += 1;
        }
        let report_waiting = report_observations > 0
            || snapshot["sequence"].as_i64().unwrap_or(0) < report_barrier
            || ["reports", "reportQuestions"].iter().any(|key| {
                snapshot[key]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|r| r["trackId"] == state.track_id)
            });
        run.set_hold(Hold::Report, report_waiting);
        if state.active_turn.is_none()
            && input.is_none()
            && !state.checkpoint_pending
            && !context.port.stopped.load(Ordering::Acquire)
            && ["ready", "question"].contains(&state.phase.as_str())
        {
            let phase = if report_waiting || !state.prompts.is_empty() {
                "question"
            } else {
                "ready"
            };
            if state.phase != phase {
                state.phase = phase.into();
                write_state(jobs, journal, state, &tx)?;
                pending_writes += 1;
            }
        }
        if input.is_none()
            && pending_recovery.is_none()
            && run.can_reply()
        {
            let reply_candidate = snapshot["reportQuestions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|q| q["trackId"] == state.track_id && q["state"] == "queued")
                .find_map(|q| {
                    snapshot["utterances"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|word| {
                            word["id"] == q["replyUtteranceId"] && word["state"] == "placed" && word["editHold"].is_null() && word["reviewFixRunnable"]!=false && word["workspaceAvailable"] != false && word["dependencyRetryRunnable"] != false && (word["planRunnable"] != false || word["planPreparable"] == true)
                        })
                });
            let candidate = reply_candidate.or_else(|| {
                if run.can_start() {
                    let secret_resume = snapshot["secretInputs"].as_array().into_iter().flatten()
                        .find(|entry|entry["owner"]["kind"]=="work" && entry["trackId"]==state.track_id && entry["state"]=="ready")
                        .map(|entry|&entry["inputId"]);
                    snapshot["utterances"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|word| {
                            word["state"] == "placed" && word["editHold"].is_null() && word["reviewFixRunnable"]!=false && word["workspaceAvailable"] != false && word["dependencyRetryRunnable"] != false && (word["planRunnable"] != false || word["planPreparable"] == true)
                                && word["decision"]["trackId"] == state.track_id
                                && ["fork", "extend", "schedule", "trim"]
                                    .contains(&word["decision"]["kind"].as_str().unwrap_or(""))
                                && (word["decision"]["boundary"] != "when-told" || manual)
                                && (word["startCondition"].is_null() || word["startCondition"]["ready"] == true)
                        })
                        .min_by_key(|word| input_order(word,secret_resume))
                } else {
                    None
                }
            });
            if let Some(word) = candidate {
                if let Some(memory) = &io.continuity.host_memory
                    && !memory.claim_work() {
                    // A newly arrived request makes this host busy. Only the
                    // idle branch above may close it, after a fresh process scan.
                    memory.defer_reclaim();
                }
                let id = word["id"].as_str().unwrap().to_owned();
                if ticket.as_ref().is_none_or(|waiting| waiting.id() != id || waiting.priority()!=input_priority(word)) {
                    ticket = Some(context.capacity.request(id.clone(), input_priority(word)));
                }
                if let Some(slot) = ticket.as_mut().and_then(crate::scheduler::Ticket::try_acquire) {
                ticket = None;
                let revision = word["revision"].as_i64().unwrap();
                let is_reply = word["replyTo"].is_object();
                let mut text = word["text"].as_str().unwrap().to_owned();
                let mut reply_report = None;
                if is_reply {
                    let question = snapshot["reportQuestions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|q| q["id"] == word["replyTo"]["id"])
                        .ok_or_else(|| std::io::Error::other("Reply question is unavailable."))?;
                    text = format!(
                        "Reply to your question from turn {}:\n{}\n\nUser's answer:\n{}",
                        question["turnId"].as_str().unwrap_or(""),
                        question["text"].as_str().unwrap_or(""),
                        text
                    );
                    reply_report = question["reportId"].as_str().map(str::to_owned);
                }
                let operation = Uuid::now_v7();
                let params = serde_json::json!({"commandId":operation,"sessionId":session,"reasoningEffort":"minimal","input":[{"type":"skill","source":"user","selector":"intercat-report","arguments":text}]});
                if (if is_reply {
                    run.request_reply_start(operation)
                } else {
                    run.request_start(operation)
                }) == Some(Effect::PersistStart(operation))
                {
                    input = Some(Input {
                        id: id.clone(),
                        revision,
                        operation,
                        params: params.clone(),
                        reply_report,
                        invalidated: false,
                        sent: false,
                        started_event: None,
                        unqueued_event: None,
                        queued: None,
                    });
                    *permit = Some(slot);
                    manual = false;
                    state.phase = "starting".into();
                    state.output = None;
                    write_state(jobs, journal, state, &tx)?;
                    pending_writes += 1;
                    let journal = journal.clone();
                    let tx = tx.clone();
                    let track = state.track_id.clone();
                    let attachment_session = session.clone();
                    let previous=state.checkpoint.clone();
                    let guard=context.port.start_guard();
                    let tracker=io.tracker.clone();let identity=io.identity;
                    jobs.spawn(async move {
                        let result = async {
                            let guard=guard.ok_or(crate::journal::StartSuperseded)?;
                            let workspace=journal.workspace(track).await?;
                            crate::inheritance_apply::prepare(&journal,id,revision,attachment_session,workspace,previous,crate::inheritance_apply::Boundary{guard:crate::inheritance_apply::Guard::Work(guard),tracker,identity}).await
                        }
                        .await;
                        let _=tx.send(Work::Inherited(result));
                    });
                }
                }
            } else {
                ticket = None;
            }
        } else {
            ticket = None;
        }
        tokio::select! {
            _=stopping.changed()=>{},
            _=capacity_changes.changed()=>{},
            _=scan_timer.tick(),if !scan_busy=>{scan_busy=true;let tracker=io.tracker.clone();let tx=tx.clone();tokio::task::spawn_blocking(move||{let result=tracker.lock().unwrap().refresh();let _=tx.send(Work::Scanned(result));});},
            changed=changes.changed()=>{dirty=true;if changed.is_err(){return Err(std::io::Error::other("Journal closed.").into());}},
            changed=editor.changed()=>{if changed.is_err(){return Err(std::io::Error::other("Editor state closed.").into());}},
            changed=project_changes.changed()=>{if changed.is_err(){return Err("Project secret state closed.".into());}},
            command=async {
                if pending_writes==0 && let Some(command)=pending_recovery.take() {return Some(command)}
                context.commands.recv().await
            }=>match command {
                Some(Command::InstallSecret{install,reply})=>{
                    let target=&install.target;
                    if context.port.pending_secret.lock().unwrap().as_ref().is_some_and(|pending|pending.target.op_id==target.op_id)
                        || preparing_secret==Some(target.op_id) {let _=reply.send(Ok(()));continue;}
                    let current=secret_question_current(target,state,&snapshot);
                    if restoring || redirecting || context.port.stopped.load(Ordering::Acquire) || preparing_secret.is_some() || context.port.has_secret() || !current || install.stop_generation!=context.port.stop_generation.load(Ordering::Acquire) {
                        let _=reply.send(Err("The secret question changed. Check the worker and try again.".into()));continue;
                    }
                    preparing_secret=Some(target.op_id);
                    let journal=journal.clone();let tx=tx.clone();
                    jobs.spawn(async move{let result=journal.secret_accepted(install.target.op_id).await;let _=tx.send(Work::SecretAccepted{install,result,reply});});
                    pending_writes+=1;
                },
                Some(Command::StopInheritance{command,generation,pause_epoch,reply})=>{
                    match journal.operation(command.op_id).await {
                        Ok(Some(_))=>{let _=reply.send(journal.inheritance_issue_command(command,false).await);continue;},
                        Err(error)=>{let _=reply.send(Err(error));continue;},
                        Ok(None)=>{}
                    }
                    let issue=match journal.inheritance_issue(command.issue_id).await {Ok(issue)=>issue,Err(error)=>{let _=reply.send(Err(error));continue;}};
                    if command.action!=crate::inheritance_issue::Action::Stop || !matches!(issue.blocker,crate::inheritance_issue::Blocker::Processes) || issue.track_id!=state.track_id || issue.session_id.as_deref()!=Some(&session) {let _=reply.send(Err("This process request no longer belongs to this worker.".into()));continue;}
                    if generation!=context.port.stop_generation.load(Ordering::Acquire) || pause_epoch!=context.port.global_pause.epoch() {let _=reply.send(Err(crate::journal::StartSuperseded.into()));continue;}
                    if restoring || restore_operation.is_some() || state.active_turn.is_some() || state.checkpoint_pending || input.is_some() || !state.prompts.is_empty() || !state.approvals.is_empty() {let _=reply.send(Err("Wait for the worker's current turn before stopping its remaining processes.".into()));continue;}
                    if pending_writes!=0 {
                        if pending_recovery.is_some(){let _=reply.send(Err("A file operation is already waiting.".into()));continue;}
                        context.port.hold_starts();run.set_hold(Hold::Stopping,true);pending_recovery=Some(Command::StopInheritance{command,generation,pause_epoch,reply});continue;
                    }
                    restoring=true;context.port.hold_starts();run.set_hold(Hold::Reconciling,true);run.set_hold(Hold::Stopping,true);
                    state.phase="stopping-processes".into();state.detail=Some("Stopping the worker's remaining processes…".into());
                    if let Err(error)=journal.worker_state(state.clone()).await {restoring=false;state.phase="held".into();let _=reply.send(Err(error));continue;}
                    let guard=crate::inheritance_apply::Guard::Recovery{generation:context.port.stop_generation.clone(),expected:generation,gate:context.port.global_pause.clone(),epoch:pause_epoch};
                    let journal=journal.clone();let tx=tx.clone();let tracker=io.tracker.clone();let identity=io.identity;
                    jobs.spawn(async move {let result=async {crate::inheritance_apply::stop_children(tracker,identity,guard).await?;journal.inheritance_issue_command(command,true).await}.await;let _=tx.send(Work::StoppedInheritance{result,reply});});pending_writes+=1;
                },
                Some(Command::FinishInheritance{id,application,generation,pause_epoch,reply})=>{
                    match journal.inheritance_recovery(application,state.track_id.clone(),session.clone()).await {
                        Ok(None)=>{let _=reply.send(Ok(()));continue;},
                        Err(error)=>{let _=reply.send(Err(error));continue;},
                        Ok(Some(_))=>{}
                    }
                    if generation!=context.port.stop_generation.load(Ordering::Acquire) || pause_epoch!=context.port.global_pause.epoch() {let _=reply.send(Err(crate::journal::StartSuperseded.into()));continue;}
                    if restoring || restore_operation.is_some() || state.active_turn.is_some() || state.checkpoint_pending || input.is_some() || !state.prompts.is_empty() || !state.approvals.is_empty() {
                        let _=reply.send(Err("Wait for this worker to stop and save before finishing the file update.".into()));continue;
                    }
                    if pending_writes!=0 {
                        if pending_recovery.is_some() {let _=reply.send(Err("This file update is already waiting to finish.".into()));continue;}
                        context.port.hold_starts();run.set_hold(Hold::Stopping,true);
                        pending_recovery=Some(Command::FinishInheritance{id,application,generation,pause_epoch,reply});continue;
                    }
                    let workspace=journal.workspace(state.track_id.clone()).await?;
                    restoring=true;context.port.hold_starts();run.set_hold(Hold::Reconciling,true);run.set_hold(Hold::Stopping,true);
                    state.phase="finishing-inheritance".into();state.detail=Some("Finishing the saved file update…".into());
                    let guard=crate::inheritance_apply::Guard::Recovery{generation:context.port.stop_generation.clone(),expected:generation,gate:context.port.global_pause.clone(),epoch:pause_epoch};
                    if let Err(error)=journal.worker_state(state.clone()).await {restoring=false;state.phase="held".into();let _=reply.send(Err(error));continue;}
                    let journal=journal.clone();let tx=tx.clone();let tracker=io.tracker.clone();let identity=io.identity;let session=session.clone();
                    jobs.spawn(async move {
                        let result=crate::inheritance_apply::recover(&journal,id,application,workspace,session,crate::inheritance_apply::Boundary{guard,tracker,identity}).await;
                        let _=tx.send(Work::RecoveredInheritance{application,result,reply});
                    });pending_writes+=1;
                },
                Some(Command::Restore{id,snapshot:snapshot_id,reply})=>{
                    if restore_operation.is_some_and(|operation|operation!=id){let _=reply.send(Err("Retry the unfinished restore before starting another.".into()));continue;}
                    if restoring || pending_recovery.is_some() || state.active_turn.is_some() || state.checkpoint_pending || input.is_some() || !state.prompts.is_empty() || !state.approvals.is_empty() {let _=reply.send(Err(std::io::Error::other("Stop the worker and wait for its checkpoint before restoring files.").into()));continue;}
                    let Some(workspace)=snapshot["workspaces"].as_array().into_iter().flatten().find(|w|w["trackId"]==state.track_id).cloned() else {let _=reply.send(Err("Worker workspace is unavailable.".into()));continue;};
                    let workspace:crate::preparation::Workspace=serde_json::from_value(workspace)?;
                    restoring=true;restore_operation=Some(id);context.port.hold_starts();run.set_hold(Hold::Reconciling,true);run.set_hold(Hold::Stopping,true);
                    state.phase="restoring".into();state.detail=Some("Restoring saved files…".into());write_state(jobs, journal,state,&tx)?;pending_writes+=1;
                    let journal=journal.clone();let tx=tx.clone();let tracker=io.tracker.clone();let identity=io.identity;
                    jobs.spawn(async move{let result=async{
                        let scan=tokio::task::spawn_blocking(move||tracker.lock().unwrap().refresh()).await??;
                        if scan.processes.iter().any(|p|p.identity!=identity){return Err("A worker process is still running. Stop it before restoring files.".into());}
                        let source=journal.deletion(snapshot_id).await?;crate::deletion::restore(workspace,None,source,id,journal).await
                    }.await;let _=tx.send(Work::Restored{id,result,reply});});pending_writes+=1;
                },
                Some(Command::Respond{id,prompt,revision,response,sent,reply})=>{
                    if sent {
                        let cancelled=context.port.pending_secret.lock().unwrap().take();
                        if let Some(cancelled)=cancelled {
                            let journal=journal.clone();let tx=tx.clone();
                            jobs.spawn(async move{let _=tx.send(Work::Persisted(journal.secret_failed(cancelled.target.op_id).await));});pending_writes+=1;
                        }
                    }
                    if sent && !state.prompts.iter().any(|p|p.request.user_input_id==prompt && p.revision==revision) {
                        // An eager Stop can settle before this control is consumed.
                        // The priority write already carried the validated answers.
                        let journal=journal.clone();let tx=tx.clone();
                        let payload=serde_json::json!({"commandId":id,"sessionId":session,"userInputId":prompt,"response":response});
                        jobs.spawn(async move{let result=journal.append("prompt.interrupted",payload).await.map(|_|());let _=tx.send(Work::Persisted(result));let _=reply.send(Ok(()));});pending_writes+=1;
                        continue;
                    }
                    let Some(current)=state.prompts.iter_mut().find(|p|p.request.user_input_id==prompt && p.revision==revision) else {let _=reply.send(Err(std::io::Error::other("The question changed or already settled.").into()));continue;};
                    if current.operation==Some(id) {let _=reply.send(Ok(()));continue;}
                    if !sent && (current.phase!=crate::questions::Phase::Open || redirecting || context.port.stopped.load(Ordering::Acquire)) {let _=reply.send(Err(std::io::Error::other("This question is no longer accepting answers.").into()));continue;}
                    let (method,params)=match current.command(id,&response){Ok(value)=>value,Err(error)=>{let _=reply.send(Err(error));continue;}};
                    current.phase=crate::questions::Phase::Sending;current.operation=Some(id);
                    if sent {let _=run.stop();state.phase="stopping".into();}
                    write_state(jobs, journal,state,&tx)?;pending_writes+=1;
                    let journal=journal.clone();let tx=tx.clone();
                    if sent {
                        jobs.spawn(async move{let result=journal.append("prompt.interrupted",params).await.map(|_|());let _=tx.send(Work::Persisted(result));let _=reply.send(Ok(()));});pending_writes+=1;
                    } else {
                        pending_approvals+=1;
                        jobs.spawn(async move{let result=journal.intent(&id.to_string(),method,params.clone()).await;let _=tx.send(Work::PromptSaved{result,method,params,reply});});
                    }
                },
                Some(Command::Retry{reply,..})=>{let _=reply.send(Err("The worker is already connected.".into()));},
                Some(Command::StopOrigin{id,session:expected,turn,reply})=>{
                    if expected!=session || state.active_turn!=Some(turn) {
                        let _=reply.send(Err("The worker has moved on from this request's turn.".into()));
                        continue;
                    }
                    context.port.mark_stop(StopTarget::Turn(Some(turn)),false);
                    let cancelled={let mut pending=context.port.pending_secret.lock().unwrap();
                        if pending.as_ref().is_some_and(|pending|pending.target.session_id==session && pending.target.turn_id==turn){pending.take()}else{None}};
                    if let Some(cancelled)=cancelled {
                        let journal=journal.clone();let tx=tx.clone();
                        jobs.spawn(async move{let _=tx.send(Work::Persisted(journal.secret_failed(cancelled.target.op_id).await));});pending_writes+=1;
                    }
                    let _=run.stop();state.phase="stopping".into();
                    write_state(jobs,journal,state,&tx)?;pending_writes+=1;
                    let journal=journal.clone();let tx=tx.clone();let track=state.track_id.clone();
                    jobs.spawn(async move{let _=tx.send(Work::Persisted(journal.append("worker.stop",serde_json::json!({"opId":id,"trackId":track,"sessionId":expected,"turnId":turn})).await.map(|_|())));});pending_writes+=1;
                    let _=reply.send(Ok(()));
                },
                Some(Command::Stop{id,target,generation})=>{
                    let cancelled=context.port.pending_secret.lock().unwrap().take();
                    if let Some(cancelled)=cancelled {
                        let journal=journal.clone();let tx=tx.clone();
                        jobs.spawn(async move{let _=tx.send(Work::Persisted(journal.secret_failed(cancelled.target.op_id).await));});pending_writes+=1;
                    }
                    if target.matches(state.active_turn) {
                        let _=run.stop();let _=client.interrupt_now(&session,state.active_turn);
                        state.phase=if state.active_turn.is_some(){"stopping"}else{"held"}.into();
                    } else {
                        context.port.retain_boundary_hold(generation);
                        if state.active_turn.is_none() {state.phase="held".into();}
                    }
                    write_state(jobs, journal,state,&tx)?;pending_writes+=1;
                    let journal=journal.clone();let tx=tx.clone();let track=state.track_id.clone();jobs.spawn(async move{let _=tx.send(Work::Persisted(journal.append("worker.stop",serde_json::json!({"opId":id,"trackId":track})).await.map(|_|())));});pending_writes+=1;
                },
                Some(Command::Hold{..})=>{
                    if state.active_turn.is_none() && !state.checkpoint_pending {state.phase="held".into();}
                    state.detail=Some("Finishing this turn · next requests held".into());
                    write_state(jobs,journal,state,&tx)?;pending_writes+=1;
                },
                Some(Command::Start{id,generation,admitted_input,reply})=>{
                    if generation!=context.port.stop_generation.load(Ordering::Acquire) {let _=reply.send(Err("A newer Stop keeps this worker held.".into()));continue;}
                    if restoring||restore_operation.is_some()||pending_recovery.is_some() {let _=reply.send(Err("Finish the pending file update before starting work.".into()));continue;}
                    if state.active_turn.is_some()||state.checkpoint_pending||input.is_some()||!state.prompts.is_empty(){let _=reply.send(Err(std::io::Error::other("Worker is not at a start boundary.").into()));continue;}
                    let queued=snapshot["utterances"].as_array().into_iter().flatten().any(|word|word["state"]=="placed"&&word["decision"]["trackId"]==state.track_id);
                    let awaiting_report=snapshot["reportQuestions"].as_array().into_iter().flatten().any(|question|question["trackId"]==state.track_id && matches!(question["state"].as_str(),Some("open"|"held")));
                    if !admitted_input && !queued && !awaiting_report {let _=reply.send(Err(std::io::Error::other("No request is waiting on this worker.").into()));continue;}
                    let journal=journal.clone();let tx=tx.clone();let track=state.track_id.clone();
                    jobs.spawn(async move{let result=journal.intent(&id.to_string(),"worker.start",serde_json::json!({"trackId":track})).await;let _=tx.send(Work::Release{result,id,generation,reply});});
                },
                Some(Command::Answer{id,approval,requirement,response,reply})=>{
                    if approval_stages.is_decided(&approval,&requirement) { let _=reply.send(Err("The previous answer is still awaiting host confirmation.".into()));continue; }
                    let request=approval_stages.get(&approval).filter(|request|request["currentRequirementId"]==requirement);
                    if redirecting || context.port.stopped.load(Ordering::Acquire)||request.is_none(){let _=reply.send(Err(std::io::Error::other("Approval is no longer actionable.").into()));continue;}
                    let request=request.unwrap();
                    let allow=matches!(response,crate::approval::Response::Allow);
                    if allow && !state.approvals.iter().any(|card|card.id==approval && card.requirement==requirement && card.can_allow) {
                        let _=reply.send(Err(std::io::Error::other("This action cannot be allowed from this card.").into()));continue;
                    }
                    let prepared=match response.prepare(request) {Ok(prepared)=>prepared,Err(error)=>{let _=reply.send(Err(error));continue;}};
                    let (choice,feedback)=match prepared {
                        crate::approval::Prepared::Decision{choice,feedback}=>(choice,feedback),
                        crate::approval::Prepared::Restart{text}=>{
                            let Some(turn_id)=state.active_turn else {let _=reply.send(Err("The approval turn has already ended.".into()));continue;};
                            if let Err(error)=client.interrupt_now(&session,Some(turn_id)) {let _=reply.send(Err(error));continue;}
                            redirected.insert((Uuid::new_v5(&id,b"approval-reply").to_string(),1));
                            saving_approval_reply=true;pending_approvals+=1;
                            run.set_hold(Hold::Redirect,true);
                            state.phase="stopping".into();state.detail=Some("Stopping before sending your reply.".into());
                            write_state(jobs,journal,state,&tx)?;pending_writes+=1;
                            let request=crate::approval_reply::Request{op_id:id,owner:crate::secret_input::Owner::Work,track_id:state.track_id.clone(),session_id:session.clone(),turn_id,approval_id:approval,requirement,text};
                            let journal=journal.clone();let tx=tx.clone();
                            jobs.spawn(async move {let result=journal.queue_approval_reply(request).await;let _=tx.send(Work::ApprovalReplySaved{result,reply});});
                            continue;
                        }
                    };
                    let before=if allow&&crate::deletion::requested(request,path) {
                        let Some(workspace)=snapshot["workspaces"].as_array().into_iter().flatten().find(|w|w["trackId"]==state.track_id).cloned() else {let _=reply.send(Err("Worker workspace is unavailable.".into()));continue;};
                        let workspace:crate::preparation::Workspace=serde_json::from_value(workspace)?;
                        let parent=state.checkpoint.as_ref().map(|p|p.sha.clone()).unwrap_or_else(||workspace.base.clone());
                        Some(BeforeDelete::Fresh{workspace:Box::new(workspace),request:request.clone(),parent})
                    }else{None};
                    if !approval_stages.begin(&approval,&requirement) {let _=reply.send(Err("The approval was already answered.".into()));continue;}
                    pending_approvals+=1;persist_answer(jobs, journal,&tx,serde_json::json!({"commandId":id,"sessionId":session,"approvalId":approval,"requirementId":requirement,"choiceId":choice,"feedback":feedback,"response":response,"scope":crate::secret_input::Owner::Work.key(&state.track_id)}),Some(reply),before);
                },
                None=>return Err(std::io::Error::other("Worker controls closed.").into()),
            },
            work=rx.recv()=>match work.unwrap() {
                Work::StoppedInheritance{result,reply}=>{
                    pending_writes=pending_writes.saturating_sub(1);restoring=false;run.set_hold(Hold::Reconciling,false);
                    state.phase="held".into();state.detail=Some(match &result {Ok(())=>"The remaining processes stopped.".into(),Err(error)=>format!("The remaining processes could not be stopped: {error}")});
                    write_state(jobs,journal,state,&tx)?;pending_writes+=1;dirty=true;let _=reply.send(result);
                },
                Work::RecoveredInheritance{application,result,reply}=>{
                    pending_writes=pending_writes.saturating_sub(1);restoring=false;run.set_hold(Hold::Reconciling,false);
                    let result=async {
                        if let Some(applied)=result? {
                            state.checkpoint=Some(applied.checkpoint);state.checkpoint_pending=false;
                            journal.worker_state(state.clone()).await?;
                            journal.change_inheritance(crate::inheritance_apply_store::Change::Complete(applied.operation)).await?;
                        }
                        Ok::<(),Box<dyn std::error::Error+Send+Sync>>(())
                    }.await;
                    if let Err(error)=&result {journal.change_inheritance(crate::inheritance_apply_store::Change::RecoveryFailure{operation:application,blocker:crate::inheritance_issue::Blocker::from_error(error.as_ref())}).await?;}
                    state.phase="held".into();state.detail=Some(match &result {Ok(())=>"File update finished. No request was sent.".into(),Err(error)=>format!("File update did not finish: {error}")});
                    write_state(jobs,journal,state,&tx)?;pending_writes+=1;dirty=true;let _=reply.send(result);
                },
                Work::Inherited(result)=>{
                    match result {
                        Ok(applied)=>{
                            if let Some(applied)=applied {
                                state.checkpoint=Some(applied.checkpoint);state.checkpoint_pending=false;
                                journal.worker_state(state.clone()).await?;
                                journal.change_inheritance(crate::inheritance_apply_store::Change::Complete(applied.operation)).await?;
                            }
                            let current=input.as_ref().ok_or("Inherited files have no reserved input.")?;
                            if current.invalidated || context.port.stopped.load(Ordering::Acquire) || context.port.global_pause.paused() {
                                run.start_failed(current.operation);input=None;*permit=None;state.phase="held".into();
                                journal.worker_state(state.clone()).await?;dirty=true;
                            } else {
                                prepare_reserved_input(jobs,journal,&tx,current,state.track_id.clone(),session.clone(),path.to_path_buf());
                            }
                        },
                        Err(error)=>{
                            let current=input.take().ok_or("Inherited file failure has no reserved input.")?;
                            run.start_failed(current.operation);*permit=None;dirty=true;
                            if error.is::<crate::journal::StartSuperseded>() {state.phase="ready".into();}
                            else {
                                run.set_hold(Hold::Log,true);state.phase="held".into();state.detail=Some(error.to_string());
                                journal.change_inheritance(crate::inheritance_apply_store::Change::Failure{input:current.id,revision:current.revision,blocker:crate::inheritance_issue::Blocker::from_error(error.as_ref())}).await?;
                            }
                            journal.worker_state(state.clone()).await?;
                        }
                    }
                },
                Work::ApprovalReplySaved{result,reply}=>{
                    saving_approval_reply=false;pending_approvals=pending_approvals.saturating_sub(1);dirty=true;
                    if result.is_err() {context.port.hold_starts();}
                    let _=reply.send(result);
                },
                Work::SecretAccepted{install,result,reply}=>{
                    preparing_secret=None;pending_writes=pending_writes.saturating_sub(1);
                    if let Err(error)=result {
                        let journal=journal.clone();let tx=tx.clone();let id=install.target.op_id;
                        jobs.spawn(async move{let _=tx.send(Work::Persisted(journal.secret_failed(id).await));});pending_writes+=1;
                        let _=reply.send(Err(error));continue;
                    }
                    let target=&install.target;
                    let current=secret_question_current(target,state,&snapshot);
                    if redirecting || context.port.stopped.load(Ordering::Acquire) || !current || install.stop_generation!=context.port.stop_generation.load(Ordering::Acquire) {
                        let journal=journal.clone();let tx=tx.clone();let id=target.op_id;
                        jobs.spawn(async move{let _=tx.send(Work::Persisted(journal.secret_failed(id).await));});pending_writes+=1;
                        let _=reply.send(Err("Secret preparation was stopped or the question changed.".into()));continue;
                    }
                    *context.port.pending_secret.lock().unwrap()=Some(install);
                    context.port.hold_starts();let _=run.stop();
                    for prompt in &state.prompts {let(method,params)=prompt.command(Uuid::now_v7(),&crate::questions::Response::Stop{answers:vec![]})?;client.control_now(method,params)?;}
                    if state.active_turn.is_some() { client.interrupt_now(&session,state.active_turn)?; }
                    state.phase="stopping".into();write_state(jobs,journal,state,&tx)?;pending_writes+=1;
                    let _=reply.send(Ok(()));
                },
                Work::Restored{id,result,reply}=>{
                    pending_writes=pending_writes.saturating_sub(1);restoring=false;
                    match &result {Ok(saved)=>{completed_restores.insert(id);restore_operation=None;run.set_hold(Hold::Reconciling,false);state.phase="held".into();state.detail=Some(format!("Restored {} saved files. Worker is held.",saved.files));},Err(error)=>{state.phase="needs-recovery".into();state.detail=Some(format!("File restore did not finish: {error}"));}}
                    write_state(jobs, journal,state,&tx)?;pending_writes+=1;let _=reply.send(result);
                },
                Work::BeforeDeleteFailed{params,reply,message}=>{
                    if let Some(request)=approval_stages.cancel(params["approvalId"].as_str().unwrap())? {approval_queue.push_back(request);}
                    pending_approvals=pending_approvals.saturating_sub(1);state.detail=Some(format!("The action was not allowed because its snapshot failed: {message}"));write_state(jobs, journal,state,&tx)?;pending_writes+=1;if let Some(reply)=reply{let _=reply.send(Err(message.into()));}
                },
                Work::Policy{request,result}=>{
                    pending_writes=pending_writes.saturating_sub(1);
                    let (verdict,deletion)=(*result)?;
                    let id=request["approvalId"].as_str().unwrap().to_owned();
                    if redirecting || context.port.stopped.load(Ordering::Acquire) || !approval_stages.current(&request) {continue;}
                    let review=verdict.review.or_else(||crate::approval::offered(&request,true).is_none().then(||crate::policy::Reason{kind:"unknown".into(),message:"The worker did not offer a one-time approval. Review or stop this action.".into()}));
                    if let Some(reason)=review {
                        let subject=request["subject"]["command"].as_str().map(str::to_owned).unwrap_or_else(||request["subject"].to_string());
                        state.approvals.retain(|card|card.id!=id);
                        state.approvals.push(Approval{id,requirement:request["currentRequirementId"].clone(),subject,reason,can_allow:verdict.allow_once&&crate::approval::offered(&request,true).is_some(),can_deny:crate::approval::offered(&request,false).is_some(),reply_mode:Some(crate::approval::reply_mode(&request)),deletion:deletion.as_ref().map(|s|s.summary())});
                        state.phase="approval".into();write_state(jobs, journal,state,&tx)?;pending_writes+=1;
                    }else if let Some(choice)=crate::approval::offered(&request,true) && approval_stages.begin(&id,&request["currentRequirementId"]){pending_approvals+=1;persist_answer(jobs, journal,&tx,serde_json::json!({"commandId":Uuid::now_v7(),"sessionId":session,"approvalId":id,"requirementId":request["currentRequirementId"],"choiceId":choice}),None,deletion.as_ref().map(|s|BeforeDelete::Saved(s.id)));}
                },
                Work::ReportObserved(result)=>{report_observations=report_observations.saturating_sub(1);report_barrier=report_barrier.max(result?.sequence);dirty=true;},
                Work::PromptSaved{result,method,params,reply}=>{
                    match result {
                        Err(error)=>{let _=reply.send(Err(error));return Err(std::io::Error::other("The question answer could not be saved.").into());},
                        Ok(Admission::Replay(_))=>{pending_approvals=pending_approvals.saturating_sub(1);let _=reply.send(Ok(()));},
                        Ok(Admission::New)=>{
                            if redirecting || context.port.stopped.load(Ordering::Acquire) || !state.prompts.iter().any(|p|p.request.user_input_id==params["userInputId"] && p.operation.map(|id|id.to_string())==params["commandId"].as_str().map(str::to_owned)){pending_approvals=pending_approvals.saturating_sub(1);let _=reply.send(Err(std::io::Error::other("Worker is stopping.").into()));let journal=journal.clone();let tx=tx.clone();jobs.spawn(async move{let _=tx.send(Work::Persisted(journal.finish(params["commandId"].as_str().unwrap(),"superseded").await));});pending_writes+=1;continue;}
                            let client=client.clone();let tx=tx.clone();jobs.spawn(async move{let result=client.request(method,params.clone()).await;let _=tx.send(Work::PromptDone{result,params,reply});});
                        }
                    }
                },
                Work::PromptDone{result,params,reply}=>{
                    pending_approvals=pending_approvals.saturating_sub(1);
                    let result=result.and_then(|value|if value["commandId"]==params["commandId"]&&value["userInputId"]==params["userInputId"]&&value["status"]=="accepted"{Ok(())}else{Err(std::io::Error::other("The host did not confirm this question answer.").into())});
                    if let Err(error)=result{let _=reply.send(Err(error));return Err(crate::msp::ConnectionLost("Question answer outcome is unknown. Recovery is required.".into()).into());}
                    if let Some(prompt)=state.prompts.iter_mut().find(|p|p.request.user_input_id==params["userInputId"] && p.operation.map(|id|id.to_string())==params["commandId"].as_str().map(str::to_owned)){prompt.phase=crate::questions::Phase::AwaitingSettlement;}
                    write_state(jobs, journal,state,&tx)?;pending_writes+=1;
                    let journal=journal.clone();let tx=tx.clone();jobs.spawn(async move{let result=async{let id=params["commandId"].as_str().unwrap();journal.finish(id,"dispatched").await?;journal.finish(id,"complete").await}.await;let _=tx.send(Work::Persisted(result));let _=reply.send(Ok(()));});pending_writes+=1;
                },
                Work::Scanned(result)=>{scan_busy=false;let scan=result?;has_children=scan.processes.iter().any(|process|process.identity!=io.identity)||scan.groups.iter().any(|group|*group!=io.identity)||!scan.unreadable.is_empty();},
                Work::CheckpointSaved(result)=>{
                    let checkpoint=result?;
                    // The reducer releases the next turn only after the UI state
                    // and the checkpoint's completed intent are durable.
                    state.checkpoint_pending=false;
                    state.checkpoint=Some(checkpoint.clone());
                    state.phase=if context.port.stopped.load(Ordering::Acquire)||context.port.boundary_hold.load(Ordering::Acquire){"held"}else if report_waiting || !state.prompts.is_empty(){"question"}else{"ready"}.into();
                    state.detail=if report_waiting {None}else{Some(format!("Turn {} · checkpoint {}",terminals.get(&checkpoint.turn_id).map(String::as_str).unwrap_or("finished"),&checkpoint.sha[..8]))};
                    journal.worker_state(state.clone()).await?;
                    run.checkpoint_saved(checkpoint.turn_id);
                    dirty=true;
                },
                Work::Snapshot(result)=>{
                    let value=result?;
                    snapshot=value;snapshot_busy=false;
                },
                Work::Reserved(result,params)=>{
                    if let Some(prepared)=&params{input.as_mut().unwrap().params=prepared.params().clone();}
                    let current=input.as_ref().unwrap();
                    match result {
                        Ok(Admission::New) if !current.invalidated&&!context.port.stopped.load(Ordering::Acquire)&&context.editor.status().model_loaded&&run.start_persisted(current.operation)==Some(Effect::SendStart(current.operation))=>{
                            let client = client.clone();
                            let tx = tx.clone();
                            let prepared = params.ok_or("The start has no screened request.")?;
                            let guard = context.port.start_guard();
                            jobs.spawn(async move {
                                let result = match guard {
                                    Some(guard) => client.start(prepared, guard).await,
                                    None => Err(crate::msp::StartCancelled.into()),
                                };
                                let _ = tx.send(Work::Started(result));
                            });
                            input.as_mut().unwrap().sent=true;
                        },
                        Ok(Admission::New)=>{
                            run.start_failed(current.operation);task_write(jobs, journal,&tx,current,"held",Some("superseded"));pending_writes+=1;input=None;*permit=None;state.phase="held".into();write_state(jobs, journal,state,&tx)?;pending_writes+=1;
                        },
                        Ok(_)=>return Err(std::io::Error::other("A turn start was already admitted; recovery is required.").into()),
                        Err(error) if error.is::<crate::journal::StartSuperseded>()=>{run.start_failed(current.operation);input=None;*permit=None;dirty=true;state.phase="ready".into();write_state(jobs, journal,state,&tx)?;pending_writes+=1;},
                        Err(error) => {
                            run.start_failed(current.operation);
                            input = None;
                            *permit = None;
                            run.set_hold(Hold::Log, true);
                            dirty = true;
                            state.phase = "held".into();
                            state.detail = Some(if error.is::<crate::attachment_screening::ReviewRequired>() {
                                "Pasted references need screening. Open Reference screening, then Start.".into()
                            } else {
                                "Request changed or could not be recorded. Review it before starting again.".into()
                            });
                            write_state(jobs, journal, state, &tx)?;
                            pending_writes += 1;
                        },
                    }
                },
                Work::Started(result)=>{
                    if result.as_ref().is_err_and(|error|error.is::<crate::msp::StartCancelled>()) {
                        if let Some(current)=input.as_ref(){run.start_failed(current.operation);task_write(jobs,journal,&tx,current,"held",Some("superseded"));pending_writes+=1;}
                        input=None;*permit=None;state.phase="held".into();write_state(jobs,journal,state,&tx)?;pending_writes+=1;continue;
                    }
                    if let Err(error) = &result
                        && let Some(code) = crate::msp::rejected_start_code(error.as_ref()) {
                        let current=input.take().ok_or("A rejected start has no reserved request.")?;
                        run.start_failed(current.operation);
                        context.port.hold_starts();
                        task_write(jobs,journal,&tx,&current,"held",Some("superseded"));pending_writes+=1;
                        *permit=None;state.phase="held".into();
                        state.detail=Some("The request could not start. Review it and try again.".into());
                        let operation=current.operation;let track=state.track_id.clone();
                        let saved_journal=journal.clone();let saved_tx=tx.clone();
                        jobs.spawn(async move{let _=saved_tx.send(Work::Persisted(saved_journal.append("worker.startRejected",serde_json::json!({"commandId":operation,"trackId":track,"code":code})).await.map(|_|())));});pending_writes+=1;
                        write_state(jobs,journal,state,&tx)?;pending_writes+=1;continue;
                    }
                    let result=result?;let turn:Uuid=result["turnId"].as_str().ok_or_else(||std::io::Error::other("Missing turn ID."))?.parse()?;
                    let current=input.as_mut().ok_or("A start reply has no reserved request.")?;
                    if result["disposition"]=="queued" {
                        if result["commandId"]!=current.operation.to_string() {return Err(crate::msp::ConnectionLost("A queued start reply names another request.".into()).into());}
                        let command=Uuid::new_v5(&current.operation,b"unqueue-before-launch");
                        let params=serde_json::json!({"commandId":command,"sessionId":session,"turnId":turn});
                        let wire=client.query("turn/unqueue",params.clone()).await?;
                        let mut facts=crate::start_queue::QueuedStart::new(session.clone(),current.operation,turn,command);
                        if let Some(event)=&current.started_event {facts.observe("turn/started",event).map_err(|e|crate::msp::ConnectionLost(e.to_string()))?;}
                        if let Some(event)=&current.unqueued_event {facts.observe("turn/unqueued",event).map_err(|e|crate::msp::ConnectionLost(e.to_string()))?;}
                        if terminals.contains_key(&turn) {facts.observe("turn/completed",&serde_json::json!({"sessionId":session,"turnId":turn})).map_err(|e|crate::msp::ConnectionLost(e.to_string()))?;}
                        let start=current.operation;
                        current.queued=Some(Queued {facts,wire,began:std::time::Instant::now()});
                        state.detail=Some("Holding this request before it starts…".into());
                        write_state(jobs,journal,state,&tx)?;pending_writes+=1;
                        let saved=journal.clone();let sent=tx.clone();
                        jobs.spawn(async move {
                            let result=async {
                                saved.finish(&start.to_string(),"dispatched").await?;
                                let mut intent=params;intent["reclaimedStart"]=serde_json::json!(start);
                                saved.intent(&command.to_string(),"turn.unqueue",intent).await?;
                                Ok(())
                            }.await;
                            let _=sent.send(Work::QueueIntent{start,result});
                        });pending_writes+=1;
                        continue;
                    }
                    if !matches!(result["disposition"].as_str(),Some("started"|"steered")) {return Err(crate::msp::ConnectionLost("The start did not create the expected turn. Reconcile before continuing.".into()).into());}
                    if result["commandId"]!=current.operation.to_string() {return Err(crate::msp::ConnectionLost("A start reply names another request.".into()).into());}
                    if let Some(permit)=permit.as_mut() {permit.bind_turn(&state.track_id,turn)?;}
                    if result["disposition"]=="steered" {
                        let saved=journal.clone();let sent=tx.clone();let operation=current.operation;let track=state.track_id.clone();
                        jobs.spawn(async move{let _=sent.send(Work::Persisted(saved.append("worker.startSteered",serde_json::json!({"commandId":operation,"trackId":track,"turnId":turn})).await.map(|_|())));});pending_writes+=1;
                    }
                    state.active_turn=Some(turn);state.phase="working".into();
                    if run.start_acknowledged(current.operation,turn).is_some()||context.port.should_interrupt(turn){let _=client.interrupt_now(&session,Some(turn));}
                    task_write(jobs, journal,&tx,current,"sent",Some("dispatched"));pending_writes+=1;write_state(jobs, journal,state,&tx)?;pending_writes+=1;
                },
                Work::QueueIntent{start,result}=>{
                    pending_writes=pending_writes.saturating_sub(1);
                    result?;
                    if let Some(queued)=input.as_mut().filter(|input|input.operation==start).and_then(|input|input.queued.as_mut()) {queued.facts.logged();}
                },
                Work::Persisted(result)=>{
                    pending_writes=pending_writes.saturating_sub(1);
                    if let Err(error)=result {
                        if !journal.write_unavailable(error.as_ref()) {
                            let _=client.interrupt_now(&session,None);
                            return Err(error);
                        }
                        // A log failure blocks the next turn. It must not turn
                        // a boundary hold into an unsolicited immediate Stop.
                        run.set_hold(Hold::Log,true);
                        state.detail=Some("The log could not be saved. Next requests are held; Stop still works.".into());
                        dirty=true;
                    }
                },
                Work::Release{result,id,generation,reply}=>match result {
                    Ok(Admission::New) if !context.port.release_stop(generation)=>{let journal=journal.clone();let tx=tx.clone();jobs.spawn(async move{let _=tx.send(Work::Persisted(journal.finish(&id.to_string(),"superseded").await));});pending_writes+=1;let _=reply.send(Err("A newer Stop keeps this worker held.".into()));},
                    Ok(Admission::New)=>{run.set_hold(Hold::Stopping,false);run.set_hold(Hold::Log,false);manual=true;state.phase="ready".into();write_state(jobs, journal,state,&tx)?;pending_writes+=1;let journal=journal.clone();let tx=tx.clone();jobs.spawn(async move{let result=async{journal.finish(&id.to_string(),"dispatched").await?;journal.finish(&id.to_string(),"complete").await}.await;let _=tx.send(Work::Persisted(result));});pending_writes+=1;let _=reply.send(Ok(()));},
                    Ok(_)=>{let _=reply.send(Ok(()));},Err(error)=>{let _=reply.send(Err(error));},
                },
                Work::ApprovalSaved{result,params,reply}=>{
                    if let Err(error)=result{if let Some(reply)=reply{let _=reply.send(Err(error));}let _=client.interrupt_now(&session,None);return Err(std::io::Error::other("Approval could not be recorded.").into());}
                    if redirecting || context.port.stopped.load(Ordering::Acquire) {
                        pending_approvals=pending_approvals.saturating_sub(1);
                        if let Some(request)=approval_stages.cancel(params["approvalId"].as_str().unwrap())? {
                            approval_queue.push_back(request);
                        }
                        if let Some(reply)=reply {
                            let _=reply.send(Err(std::io::Error::other("Worker is stopping.").into()));
                        }
                        let journal=journal.clone();let tx=tx.clone();
                        jobs.spawn(async move {
                            let _=tx.send(Work::Persisted(journal.finish(params["commandId"].as_str().unwrap(),"superseded").await));
                        });
                        pending_writes+=1;
                        continue;
                    }
                    let client=client.clone();let tx=tx.clone();let journal=journal.clone();jobs.spawn(async move{let result=async {let screened=journal.screen_start(crate::approval::wire_params(&params)).await?;client.decide(screened).await}.await;let _=tx.send(Work::ApprovalDone{result,params,reply});});
                },
                Work::ApprovalDone{result,params,reply}=>{
                    pending_approvals=pending_approvals.saturating_sub(1);
                    let confirmed=match result {
                        Ok(value) if value["commandId"]==params["commandId"] && value["approvalId"]==params["approvalId"] && value["status"]=="accepted" && value["terminal"].is_boolean()=>value,
                        _=>{if let Some(reply)=reply{let _=reply.send(Err("The approval outcome is unknown. Reconnect before answering again.".into()));}return Err(crate::msp::ConnectionLost("The host did not confirm the exact approval decision.".into()).into());}
                    };
                    if let Some(reply)=reply{let _=reply.send(Ok(()));}
                    {
                        let id=params["approvalId"].as_str().unwrap();
                        if let Some(request)=approval_stages.acknowledged(id,confirmed["terminal"]==true)? {approval_queue.push_back(request);}
                        state.approvals.retain(|card|card.id!=id || card.requirement!=params["requirementId"] && !approval_stages.is_resolved(id));
                        state.phase=if context.port.stopped.load(Ordering::Acquire){if state.active_turn.is_some(){"stopping"}else{"held"}}else if state.active_turn.is_some(){"working"}else if context.port.boundary_hold.load(Ordering::Acquire){"held"}else{"ready"}.into();write_state(jobs, journal,state,&tx)?;pending_writes+=1;
                        let journal=journal.clone();let tx=tx.clone();jobs.spawn(async move{let result=async{let id=params["commandId"].as_str().unwrap();journal.finish(id,"dispatched").await?;journal.finish(id,"complete").await}.await;let _=tx.send(Work::Persisted(result));});pending_writes+=1;
                    }
                },
            },
            event=inbox.next(io.connection)=>{
                let frame=match event {Ok(frame)=>frame,Err(error)=>{let _=client.interrupt_now(&session,None);return Err(error);}};
                let input_engaged=frame.input_engaged;
                let display=frame.recorded_display.as_deref().cloned().unwrap_or_else(||crate::recording::sanitize(frame.frame.clone(),&[]));
                let value=frame.frame;let record=journal.try_msp(state.track_id.clone(),session.clone(),frame.stamp,display,Some(frame.lease), frame.source)?;
                let write_tx=tx.clone();jobs.spawn(async move{let result=match record.await{Ok(result)=>result.map(|_|()),Err(error)=>Err(error.into())};let _=write_tx.send(Work::Persisted(result));});pending_writes+=1;
                let params=&value["params"];
                if let Some(current)=input.as_mut() {
                    if params["sessionId"]==session && params["commandId"]==current.operation.to_string() {
                        match value["method"].as_str() {
                            Some("turn/started")=>current.started_event=Some(params.clone()),
                            Some("turn/unqueued")=>current.unqueued_event=Some(params.clone()),
                            _=>{},
                        }
                    }
                    if let Some(queued)=current.queued.as_mut() {
                        if value.get("method").is_none() && value["id"].as_u64()==Some(queued.wire.id) {
                            queued.facts.reply(&value).map_err(|e|crate::msp::ConnectionLost(e.to_string()))?;
                        } else if let Some(method)=value["method"].as_str() {
                            queued.facts.observe(method,params).map_err(|e|crate::msp::ConnectionLost(e.to_string()))?;
                        }
                    }
                }
                if params["sessionId"] == session && value["method"] == "view/gap" {
                    let head=params["next"].as_str().ok_or("Delivery gap has no next cursor.")?;
                    inbox.flush(io.connection).await?;
                    crate::history::recover(io.connection, crate::history::Context{journal,track:&state.track_id,session:&session,scanner:None},head,state.active_turn).await.map_err(|error|crate::msp::ConnectionLost(error.to_string()))?;
                    continue;
                }
                if params["sessionId"] == session && value["method"] == "session/viewHealthChanged" {
                    return Err(crate::msp::ConnectionLost("The worker history is unavailable. Reconnect before sending work.".into()).into());
                }
                if params["sessionId"]==session && matches!(value["method"].as_str(),Some("item/updated"|"item/completed")) {
                    for field in ["patchRef","outputRef"] {
                        let reference=&params["item"][field];
                        if reference["availability"]!="available" {continue;}
                        if let (Some(item),Some(reference))=(params["item"]["itemId"].as_str(),reference["id"].as_str())
                            && fetched_outputs.insert((item.to_owned(),reference.to_owned())) {
                                let (item,reference,session)=(item.to_owned(),reference.to_owned(),session.clone());
                                let client=client.clone();let journal=journal.clone();let tx=tx.clone();
                                jobs.spawn(async move {
                                    let output=match client.request("item/readOutput",serde_json::json!({"sessionId":session,"itemId":item,"outputRef":reference,"offsetBytes":0,"lengthBytes":1_048_576})).await {
                                        Ok(value)=>crate::recording::sanitize(value,&[]),
                                        Err(_)=>serde_json::json!({"unavailable":true}),
                                    };
                                    let result=journal.cache_output(session,item,reference,output).await;
                                    let _=tx.send(Work::Persisted(result));
                                });pending_writes+=1;
                        }
                    }
                }
                match value["method"].as_str(){
                    Some("turn/completed") if params["sessionId"]==session=>{if let Some(turn)=params["turnId"].as_str().and_then(|s|s.parse::<Uuid>().ok()){terminals.insert(turn,params["terminal"].as_str().unwrap_or("unknown").into());}},
                    Some("item/completed") if params["sessionId"]==session && params["item"]["kind"]=="agentMessage"=>{if let Some(text)=params["item"]["text"].as_str(){
                        state.output=Some(short_text(text));write_state(jobs, journal,state,&tx)?;pending_writes+=1;
                        if params["item"]["messagePhase"]=="finalAnswer" || !crate::reports::explicit_questions(text).is_empty() {
                            let turn=params["item"]["turnId"].as_str().ok_or_else(||std::io::Error::other("Report turn is missing."))?.parse()?;
                            let item=params["item"]["itemId"].as_str().ok_or_else(||std::io::Error::other("Report item is missing."))?.to_owned();
                            final_reports.insert(turn);report_observations+=1;run.set_hold(Hold::Report,true);
                            let report=crate::reports::Report::new(state.track_id.clone(),session.clone(),turn,item,text.to_owned());
                            if let Some(pending)=input.as_mut().filter(|input|input.reply_report.as_deref()==Some(&report.id)) {
                                pending.invalidated=true;
                                if pending.sent {run.stop();context.port.hold_starts();let _=client.interrupt_now(&session,None);}
                            }
                            observe_report(jobs, journal,&tx,report);
                        }
                    }},
                    Some("approval/request"|"approval/requested"|"approval/updated"|"approval/resolved") if params["sessionId"]==session=>{
                        if let Some(request)=approval_stages.observe(value["method"].as_str().unwrap(),params)? {approval_queue.push_back(request);}
                        if let Some(id)=params["approvalId"].as_str() && approval_stages.is_resolved(id) {
                            state.approvals.retain(|card|card.id!=id);write_state(jobs,journal,state,&tx)?;pending_writes+=1;
                        }
                    },
                    Some("userInput/request" | "userInput/requested") if params["sessionId"]==session=>{
                        let prompt=crate::questions::Prompt::parse(params.clone())?;
                        if settled_prompts.contains(&prompt.request.user_input_id){continue;}
                        if !input_engaged {client.engage_input(&value)?;}
                        if state.prompts.iter().any(|old|old.request.user_input_id==prompt.request.user_input_id&&old.revision==prompt.revision){continue;}
                        run.question(prompt.key());
                        state.prompts.retain(|old|old.request.user_input_id!=prompt.request.user_input_id);
                        state.prompts.push(prompt);state.phase="question".into();state.detail=None;
                        write_state(jobs, journal,state,&tx)?;pending_writes+=1;
                    },
                    Some("userInput/settled") if params["sessionId"]==session=>{
                        if let Some(id)=params["userInputId"].as_str(){settled_prompts.insert(id.to_owned());}
                        if let Some(index)=state.prompts.iter().position(|p|p.request.user_input_id==params["userInputId"]){let prompt=state.prompts.remove(index);run.answer(prompt.key());}
                        if !state.checkpoint_pending {state.phase=if context.port.stopped.load(Ordering::Acquire)||context.port.boundary_hold.load(Ordering::Acquire){"held"}else if !state.prompts.is_empty(){"question"}else if state.active_turn.is_some(){"working"}else{"ready"}.into();}
                        state.detail=None;write_state(jobs, journal,state,&tx)?;pending_writes+=1;
                    },
                    _=>{},
                }
            },
        }
        let queued_outcome=input.as_ref().and_then(|input|input.queued.as_ref()).map(|queued|queued.facts.outcome()).transpose()?.flatten();
        if let Some(outcome)=queued_outcome {
            match outcome {
                crate::start_queue::Outcome::Reclaimed=>{
                    let current=input.take().ok_or("The reclaimed start has no request.")?;
                    let command=current.queued.as_ref().unwrap().facts.command;
                    run.start_failed(current.operation);context.port.hold_starts();
                    task_write(jobs,journal,&tx,&current,"held",Some("complete"));pending_writes+=1;
                    *permit=None;state.phase="held".into();state.detail=Some("Request held before it started.".into());
                    write_state(jobs,journal,state,&tx)?;pending_writes+=1;
                    let saved=journal.clone();let sent=tx.clone();jobs.spawn(async move {
                        let result=async {saved.finish(&command.to_string(),"dispatched").await?;saved.finish(&command.to_string(),"complete").await}.await;
                        let _=sent.send(Work::Persisted(result));
                    });pending_writes+=1;
                },
                crate::start_queue::Outcome::Running=>{
                    let current=input.as_mut().ok_or("The running queued start has no request.")?;
                    let queued=current.queued.take().unwrap();let turn=queued.facts.turn;let command=queued.facts.command;
                    if let Some(permit)=permit.as_mut() {permit.bind_turn(&state.track_id,turn)?;}
                    state.active_turn=Some(turn);state.phase="working".into();state.detail=None;
                    if run.start_acknowledged(current.operation,turn).is_some()||context.port.should_interrupt(turn){let _=client.interrupt_now(&session,Some(turn));}
                    task_write(jobs,journal,&tx,current,"sent",Some("dispatched"));pending_writes+=1;
                    write_state(jobs,journal,state,&tx)?;pending_writes+=1;
                    let saved=journal.clone();let sent=tx.clone();jobs.spawn(async move {let _=sent.send(Work::Persisted(saved.finish(&command.to_string(),"superseded").await));});pending_writes+=1;
                },
            }
        }
        if let Some(turn) = state.active_turn
            && terminals.contains_key(&turn)
            && pending_writes == 0
            && pending_approvals == 0
        {
            if terminals.get(&turn).map(String::as_str) == Some("completed")
                && !final_reports.contains(&turn)
            {
                final_reports.insert(turn);
                report_observations += 1;
                run.set_hold(Hold::Report, true);
                observe_report(
                    jobs,
                    journal,
                    &tx,
                    crate::reports::Report::new(
                        state.track_id.clone(),
                        session.clone(),
                        turn,
                        format!("missing:{turn}"),
                        String::new(),
                    ),
                );
            }
            run.completed(turn);
            state.active_turn = None;
            state.checkpoint_pending = true;
            state.phase = "checkpoint".into();
            state.detail = Some(format!(
                "Turn {} · saving checkpoint",
                terminals.get(&turn).unwrap()
            ));
            state.approvals.clear();
            approval_stages.clear_pending();
            approval_queue.clear();
            if let Some(current) = input.take() {
                // Receipt is authoritative. If no userMessage arrived, retain the
                // request for review; never infer receipt from turn completion.
                task_write(jobs, journal, &tx, &current, "held", Some("complete"));
                pending_writes += 1;
            } else if let Some(command) = recovered_command.take() {
                let journal = journal.clone();
                let tx = tx.clone();
                jobs.spawn(async move {
                    let _ = tx.send(Work::Persisted(
                        journal.finish(&command.to_string(), "complete").await,
                    ));
                });
                pending_writes += 1;
            }
            *permit = None;
            write_state(jobs, journal, state, &tx)?;
            pending_writes += 1;
            let workspace = snapshot["workspaces"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|workspace| workspace["trackId"] == state.track_id)
                .cloned()
                .ok_or_else(|| std::io::Error::other("Worker workspace is missing."))?;
            let workspace = serde_json::from_value(workspace)?;
            let previous = state.checkpoint.clone();
            let journal = journal.clone();
            let tx = tx.clone();
            let tracker = io.tracker.clone();
            let identity = io.identity;
            jobs.spawn(async move {
                let result = async {
                    let scan =
                        tokio::task::spawn_blocking(move || tracker.lock().unwrap().refresh())
                            .await??;
                    let live_processes = scan
                        .processes
                        .iter()
                        .any(|process| process.identity != identity);
                    crate::checkpoint::save(workspace, turn, previous, live_processes, journal)
                        .await
                }
                .await;
                let _ = tx.send(Work::CheckpointSaved(result));
            });
        }
    }
    }.await;
    // A queued acceptance may outlive this receiver. Failing either before or
    // after that write prevents it becoming a stranded, accepted operation.
    if let Some(id) = preparing_secret {
        journal.secret_failed(id).await?;
    }
    let flushed = inbox.flush(io.connection).await;
    // Only a turn that may have reached the host needs a checkpoint. A report
    // question can remain open after its completed turn is already saved.
    if (outcome.is_err() || flushed.is_err())
        && (state.active_turn.is_some() || input.as_ref().is_some_and(|input| input.sent))
    {
        state.checkpoint_pending = true;
    }
    flushed?;
    outcome
}

#[cfg(test)]
#[path = "memory_execution_tests.rs"]
mod memory_tests;

#[cfg(test)]
#[path = "start_outcome_tests.rs"]
mod start_outcome_tests;

#[cfg(test)]
mod continuity_tests {
    use super::*;
    #[test]
    fn same_priority_uses_saved_order_even_when_snapshot_lists_newest_first() {
        let words = [
            serde_json::json!({"id":"003","text":"Later dummy request"}),
            serde_json::json!({"id":"001","text":"Earlier dummy request"}),
            serde_json::json!({"id":"002","text":"Middle dummy request"}),
        ];
        let first = words
            .iter()
            .min_by_key(|word| input_order(word, None))
            .unwrap();
        assert_eq!(first["id"], "001");
    }

    #[test]
    fn explicit_item_continuations_keep_user_priority_ahead_of_automatic_plan_work() {
        use crate::scheduler::Priority;
        use serde_json::json;
        let capacity = Arc::new(crate::scheduler::Capacity::new(1));
        let active = capacity
            .request("000".into(), Priority::User)
            .try_acquire()
            .unwrap();
        let mut plan =
            capacity.request("001".into(), input_priority(&json!({"planItemId":"item"})));
        let mut ordinary = capacity.request(
            "002".into(),
            input_priority(&json!({"text":"Dummy user work"})),
        );
        let mut continuation = capacity.request(
            "003".into(),
            input_priority(&json!({"planItemId":"item","planContinuation":{"title":"Dummy"}})),
        );
        let mut answer = capacity.request("004".into(), input_priority(&json!({"planItemId":"item","planContinuation":{"title":"Dummy"},"replyTo":{"id":"question"}})));
        drop(active);
        assert!(plan.try_acquire().is_none());
        assert!(continuation.try_acquire().is_none());
        let replying = answer.try_acquire().unwrap();
        assert!(ordinary.try_acquire().is_none());
        drop(replying);
        let speaking = ordinary.try_acquire().unwrap();
        assert!(continuation.try_acquire().is_none());
        drop(speaking);
        let continuing = continuation.try_acquire().unwrap();
        assert!(plan.try_acquire().is_none());
        drop(continuing);
        assert!(plan.try_acquire().is_some());
    }

    #[tokio::test]
    async fn an_internal_hold_cancels_old_starts_without_invalidating_its_resume() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (port, _commands) = Port::new(std::sync::Arc::new(crate::dispatch_control::Gate::new(
            false,
        )));
        let root = tempfile::tempdir().unwrap();
        let journal = crate::journal::Journal::open(
            root.path().join("db"),
            crate::secrets::Key::new([7; 32]),
        )
        .await
        .unwrap();
        let generation = port.stop_generation.load(Ordering::Acquire);
        let old = port.start_guard().unwrap();
        port.hold_starts();
        assert!(port.start_guard().is_none());
        assert!(port.release_stop(generation));
        let fresh = port.start_guard().unwrap();
        let (read, mut responses) = tokio::io::duplex(4096);
        let (write, requests) = tokio::io::duplex(4096);
        let mut connection = crate::msp::Connection::new(read, write);
        assert!(
            connection
                .client
                .start(
                    journal
                        .screen_start(serde_json::json!({"commandId":"old"}))
                        .await
                        .unwrap(),
                    old
                )
                .await
                .unwrap_err()
                .is::<crate::msp::StartCancelled>()
        );
        let client = connection.client.clone();
        let submitted = tokio::spawn(async move {
            client
                .start(
                    journal
                        .screen_start(serde_json::json!({"commandId":"new"}))
                        .await
                        .unwrap(),
                    fresh,
                )
                .await
        });
        let mut requests = BufReader::new(requests);
        let mut line = String::new();
        requests.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["params"]["commandId"], "new");
        responses
            .write_all(
                format!(
                    "{}\n",
                    serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        assert!(submitted.await.unwrap().is_ok());
        connection.close();
    }
    #[tokio::test]
    async fn lost_connection_keeps_its_slot_across_another_recovery_attempt() -> Result<()> {
        let root = tempfile::tempdir()?;
        let journal = crate::journal::Journal::open(
            root.path().join("db"),
            crate::secrets::Key::new([7; 32]),
        )
        .await?;
        let (port, commands) = Port::new(std::sync::Arc::new(crate::dispatch_control::Gate::new(
            false,
        )));
        let capacity = Arc::new(crate::scheduler::Capacity::new(1));
        let mut context = Context {
            project_secrets: crate::project_secrets::Store::default(),
            port,
            commands,
            editor: crate::editor::Manager::new(root.path(), &root.path().join("editor.log"))?,
            capacity: capacity.clone(),
        };
        let mut state = crate::runtime::WorkerState {
            secret_revision: None,
            track_id: "track".into(),
            phase: "working".into(),
            session_id: Some("session".into()),
            identity: None,
            detail: None,
            active_turn: Some(Uuid::now_v7()),
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        };
        let identity = crate::process::identity(std::process::id())?;
        let tracker = Arc::new(Mutex::new(crate::process::Tracker::new(
            "continuity-test".into(),
            identity,
        )?));
        let (_stop, mut stopping) = tokio::sync::watch::channel(false);
        let mut continuity = Continuity::default();
        for _ in 0..2 {
            let mut connection = crate::msp::Connection::new(tokio::io::empty(), tokio::io::sink());
            let error = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                run(
                    WorkerIo {
                        connection: &mut connection,
                        tracker: tracker.clone(),
                        identity,
                        recovery: None,
                        continuity: &mut continuity,
                    },
                    &journal,
                    root.path(),
                    &mut state,
                    &mut context,
                    &mut stopping,
                ),
            )
            .await?
            .unwrap_err();
            assert!(error.is::<crate::msp::ConnectionLost>());
            assert!(state.checkpoint_pending);
            assert!(continuity.has_slot());
            assert_eq!(capacity.available(), 0);
            continuity.settle().await?;
            assert_eq!(capacity.available(), 0);
        }
        drop(continuity);
        assert_eq!(capacity.available(), 1);
        Ok(())
    }
    #[tokio::test]
    async fn a_saved_report_question_does_not_become_an_unfinished_checkpoint() -> Result<()> {
        let root = tempfile::tempdir()?;
        let journal = crate::journal::Journal::open(
            root.path().join("db"),
            crate::secrets::Key::new([7; 32]),
        )
        .await?;
        journal
            .observe_report(crate::reports::Report::new(
                "track".into(),
                "session".into(),
                Uuid::now_v7(),
                "report".into(),
                "Waiting for you: A token is needed.".into(),
            ))
            .await?;
        let (port, commands) = Port::new(std::sync::Arc::new(crate::dispatch_control::Gate::new(
            false,
        )));
        let mut context = Context {
            project_secrets: crate::project_secrets::Store::default(),
            port,
            commands,
            editor: crate::editor::Manager::new(root.path(), &root.path().join("editor.log"))?,
            capacity: Arc::new(crate::scheduler::Capacity::new(1)),
        };
        let mut state = crate::runtime::WorkerState {
            secret_revision: None,
            track_id: "track".into(),
            phase: "question".into(),
            session_id: Some("session".into()),
            identity: None,
            detail: None,
            active_turn: None,
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        };
        let identity = crate::process::identity(std::process::id())?;
        let tracker = Arc::new(Mutex::new(crate::process::Tracker::new(
            "saved-report-test".into(),
            identity,
        )?));
        let (_stop, mut stopping) = tokio::sync::watch::channel(false);
        let mut continuity = Continuity::default();
        let mut connection = crate::msp::Connection::new(tokio::io::empty(), tokio::io::sink());
        let result = run(
            WorkerIo {
                connection: &mut connection,
                tracker,
                identity,
                recovery: None,
                continuity: &mut continuity,
            },
            &journal,
            root.path(),
            &mut state,
            &mut context,
            &mut stopping,
        )
        .await;
        assert!(result.unwrap_err().is::<crate::msp::ConnectionLost>());
        assert_eq!(state.phase, "question");
        assert!(!state.checkpoint_pending);
        assert!(!continuity.has_slot());
        continuity.settle().await?;
        Ok(())
    }
    #[tokio::test]
    async fn origin_stop_checks_actor_identity_before_holding_and_targets_only_the_original_turn()
    -> Result<()> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        for current_matches in [false, true] {
            let root = tempfile::tempdir()?;
            let journal = crate::journal::Journal::open(
                root.path().join("db"),
                crate::secrets::Key::new([7; 32]),
            )
            .await?;
            let (read, responses) = tokio::io::duplex(4096);
            let (write, requests) = tokio::io::duplex(4096);
            let mut connection = crate::msp::Connection::new(read, write);
            let (port, commands) = Port::new(Arc::new(crate::dispatch_control::Gate::new(false)));
            port.attach(connection.client.clone(), "session".into());
            let original = Uuid::now_v7();
            let active = if current_matches {
                original
            } else {
                Uuid::now_v7()
            };
            let mut context = Context {
                project_secrets: crate::project_secrets::Store::default(),
                port: port.clone(),
                commands,
                editor: crate::editor::Manager::new(root.path(), &root.path().join("editor.log"))?,
                capacity: Arc::new(crate::scheduler::Capacity::new(1)),
            };
            let mut state = crate::runtime::WorkerState {
                track_id: "track".into(),
                phase: "working".into(),
                session_id: Some("session".into()),
                secret_revision: None,
                identity: None,
                detail: None,
                active_turn: Some(active),
                approvals: vec![],
                output: None,
                checkpoint_pending: false,
                checkpoint: None,
                prompts: vec![],
            };
            let identity = crate::process::identity(std::process::id())?;
            let tracker = Arc::new(Mutex::new(crate::process::Tracker::new(
                "origin-stop-test".into(),
                identity,
            )?));
            let (_stop, mut stopping) = tokio::sync::watch::channel(false);
            let mut continuity = Continuity::default();
            let controller = async {
                let result = port.stop_origin(Uuid::now_v7(), "session", original).await;
                let held = port.locally_held();
                let mut requests = BufReader::new(requests);
                let mut line = String::new();
                requests.read_line(&mut line).await?;
                let packet: Value = serde_json::from_str(&line)?;
                assert_eq!(packet["method"], "turn/interrupt");
                assert_eq!(packet["params"]["sessionId"], "session");
                assert_eq!(packet["params"]["turnId"], original.to_string());
                assert_eq!(result.is_ok(), current_matches);
                assert_eq!(held, current_matches);
                drop(responses);
                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            };
            let runner = run(
                WorkerIo {
                    connection: &mut connection,
                    tracker,
                    identity,
                    recovery: None,
                    continuity: &mut continuity,
                },
                &journal,
                root.path(),
                &mut state,
                &mut context,
                &mut stopping,
            );
            let (_, controlled) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(runner, controller)
            })
            .await?;
            controlled?;
            continuity.settle().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn origin_stop_reaches_msp_before_the_actor_can_consume_its_control() -> Result<()> {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let (read, _responses) = tokio::io::duplex(4096);
        let (write, requests) = tokio::io::duplex(4096);
        let connection = crate::msp::Connection::new(read, write);
        let (port, mut commands) = Port::new(Arc::new(crate::dispatch_control::Gate::new(false)));
        port.attach(connection.client.clone(), "original-session".into());
        let original = Uuid::now_v7();
        let caller = port.clone();
        let pending = tokio::spawn(async move {
            caller
                .stop_origin(Uuid::now_v7(), "original-session", original)
                .await
        });
        let mut requests = BufReader::new(requests);
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            requests.read_line(&mut line),
        )
        .await??;
        let packet: Value = serde_json::from_str(&line)?;
        assert_eq!(packet["params"]["sessionId"], "original-session");
        assert_eq!(packet["params"]["turnId"], original.to_string());
        assert!(!port.locally_held());
        match commands.recv().await.unwrap() {
            Command::StopOrigin { reply, .. } => {
                let _ = reply.send(Err("The worker moved on.".into()));
            }
            _ => panic!("unexpected control"),
        }
        assert!(pending.await?.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn origin_stop_rejects_a_replaced_session_without_local_side_effects() -> Result<()> {
        let (read, _responses) = tokio::io::duplex(4096);
        let (write, _requests) = tokio::io::duplex(4096);
        let connection = crate::msp::Connection::new(read, write);
        let (port, _commands) = Port::new(Arc::new(crate::dispatch_control::Gate::new(false)));
        port.attach(connection.client.clone(), "replacement-session".into());
        assert!(
            port.stop_origin(Uuid::now_v7(), "original-session", Uuid::now_v7())
                .await
                .is_err()
        );
        assert!(!port.locally_held());
        assert_eq!(port.stop_generation.load(Ordering::Acquire), 0);
        Ok(())
    }

    #[test]
    fn new_input_resumes_only_its_saved_local_hold_and_never_a_newer_stop() {
        let gate = Arc::new(crate::dispatch_control::Gate::new(false));
        let (port, _controls) = Port::new(gate.clone());
        let first = Uuid::now_v7();
        port.hold(first).unwrap();
        let generation = port.stop_generation.load(Ordering::Acquire);
        assert!(!port.resume_for_new_input(&serde_json::json!(first)));
        port.saved_local_hold(generation, first);
        assert!(!port.resume_for_new_input(&serde_json::json!(Uuid::now_v7())));
        let epoch = gate.begin(true);
        port.stop_all_turns(Uuid::now_v7()).unwrap();
        assert!(!port.resume_for_new_input(&serde_json::json!(first)));
        assert!(gate.resume(epoch));
        assert!(port.resume_for_new_input(&serde_json::json!(first)));
        assert!(!port.locally_held());
        port.hold(first).unwrap();
        let second = Uuid::now_v7();
        port.hold(second).unwrap();
        port.saved_local_hold(generation, first);
        assert!(!port.resume_for_new_input(&serde_json::json!(first)));
        assert!(port.locally_held());
    }

    #[tokio::test]
    async fn disconnected_stop_reaches_processes_before_the_actor_consumes_control() -> Result<()> {
        use std::os::unix::process::CommandExt;
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()?;
        let identity = crate::process::identity(child.id())?;
        let tracker = Arc::new(Mutex::new(crate::process::Tracker::new_process_group(
            "stop-test".into(),
            identity,
        )?));
        let (port, mut controls) = Port::new(std::sync::Arc::new(
            crate::dispatch_control::Gate::new(false),
        ));
        port.track_processes(Some(tracker));
        let result = async {
            port.stop(Uuid::now_v7())?;
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while child.try_wait()?.is_none() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Ok::<_, std::io::Error>(())
            })
            .await??;
            assert!(matches!(controls.recv().await, Some(Command::Stop { .. })));
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }
        .await;
        let _ = child.kill();
        let _ = child.wait();
        result
    }
}
