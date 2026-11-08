//! JSON-RPC over stdio. A dedicated reader resolves replies even while consumers log.

use crate::Result;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Notify, mpsc, oneshot},
    task::JoinHandle,
};

pub const FINGERPRINT: &str =
    "sha256:c94c3d1f2f51e6424074b9ec43eac3eedcf532d3dcd77351ec1383a87d3049c6";
const MAX_FRAME: u64 = crate::output_flow::HISTORY_LIMIT as u64;
/// The transport no longer establishes an outcome; reconcile before dispatch.
#[derive(Debug)]
pub struct ConnectionLost(pub String);
impl std::fmt::Display for ConnectionLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for ConnectionLost {}

/// Only errors that reject the start before admission can release its reservation.
/// Internal failures, interruption and command-ID conflicts need reconciliation.
pub(crate) fn rejected_start_code(error: &(dyn std::error::Error + 'static)) -> Option<i64> {
    use muse_code_sdk::errors::ErrorCode;
    let error = error.downcast_ref::<muse_code_sdk::errors::MspError>()?;
    [
        ErrorCode::InvalidRequest,
        ErrorCode::MethodNotFound,
        ErrorCode::InvalidParams,
        ErrorCode::Overloaded,
        ErrorCode::InputTooLarge,
        ErrorCode::CapabilityRequired,
        ErrorCode::SessionNotFound,
        ErrorCode::SessionNotLoaded,
        ErrorCode::SessionInUse,
        ErrorCode::SessionAmbiguous,
        ErrorCode::SessionStreamMismatch,
        ErrorCode::Backpressured,
        ErrorCode::SkillNotFound,
        ErrorCode::DaemonDraining,
    ]
    .iter()
    .any(|code| error.matches(*code))
    .then(|| error.code())
}
/// A queued start lost its authorization before reaching the transport.
#[derive(Debug)]
pub struct StartCancelled;
impl std::fmt::Display for StartCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("A newer Stop cancelled this start before it was sent.")
    }
}
impl std::error::Error for StartCancelled {}

/// Stop generations are checked again by the writer, after queueing delays.
#[derive(Clone, Debug)]
pub enum StartGuard {
    Worker {
        generation: Arc<AtomicU64>,
        expected: u64,
        paused: Arc<crate::dispatch_control::Gate>,
        pause_epoch: u64,
    },
    Stopped(Arc<AtomicBool>),
    Shutdown(tokio::sync::watch::Receiver<bool>),
}
impl StartGuard {
    pub fn new(generation: Arc<AtomicU64>, paused: Arc<crate::dispatch_control::Gate>) -> Self {
        Self::Worker {
            expected: generation.load(Ordering::Acquire),
            pause_epoch: paused.epoch(),
            generation,
            paused,
        }
    }
    pub(crate) fn current(&self) -> bool {
        match self {
            Self::Worker {
                generation,
                expected,
                paused,
                pause_epoch,
            } => {
                generation.load(Ordering::Acquire) == *expected
                    && !paused.paused()
                    && paused.epoch() == *pause_epoch
            }
            Self::Stopped(stopped) => !stopped.load(Ordering::Acquire),
            Self::Shutdown(stopping) => !*stopping.borrow() && stopping.has_changed().is_ok(),
        }
    }
}
type Reply = oneshot::Sender<Result<Value>>;
struct PendingReply {
    screening: Option<Arc<tokio::sync::OwnedRwLockReadGuard<()>>>,
    sender: Option<Reply>,
    history: bool,
    cancelled: Option<oneshot::Sender<()>>,
}
type Pending = Arc<Mutex<HashMap<u64, PendingReply>>>;
#[derive(Debug)]
enum Outgoing {
    Frame(Value),
    Start { frame: Value, guard: StartGuard },
    Flushed(oneshot::Sender<std::io::Result<()>>),
}

pub struct Received {
    pub frame: Value,
    pub stamp: crate::clock::Stamp,
    pub lease: crate::output_flow::Lease,
    pub source: Option<i64>,
    pub input_engaged: bool,
    pub recorded_display: Option<Arc<Value>>,
}
pub enum Event {
    Frame(Received),
    Closed(String),
}

#[derive(Clone)]
pub struct Client {
    normal: mpsc::Sender<Outgoing>,
    priority: mpsc::Sender<Outgoing>,
    pending: Pending,
    next: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
    handshake_started: Arc<AtomicBool>,
    flow: crate::output_flow::Flow,
    display: Arc<Mutex<Display>>,
    preview_changed: Arc<Notify>,
}

#[derive(Default)]
struct Display {
    mask: crate::progress::Mask,
    progress: crate::progress::Progress,
}

pub struct Connection {
    pub client: Client,
    events: mpsc::UnboundedReceiver<Event>,
    buffered: VecDeque<Event>,
    tasks: Vec<JoinHandle<()>>,
    #[cfg(test)]
    pub(crate) hide_approval_feedback: bool,
}

pub(crate) struct PendingRequest {
    pub id: u64,
    pending: Pending,
    cancelled: Option<oneshot::Receiver<()>>,
}
impl PendingRequest {
    pub(crate) async fn cancelled(&mut self) {
        if let Some(cancelled) = &mut self.cancelled {
            if cancelled.await.is_ok() {
                return;
            }
            self.cancelled = None;
        }
        std::future::pending::<()>().await;
    }
}
impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

fn fail(
    pending: &Pending,
    closed: &AtomicBool,
    events: &mpsc::UnboundedSender<Event>,
    reason: &str,
) {
    if !closed.swap(true, Ordering::SeqCst) {
        let _ = events.send(Event::Closed(reason.into()));
        for (_, reply) in pending.lock().unwrap().drain() {
            if let Some(sender) = reply.sender {
                let _ = sender.send(Err(ConnectionLost(reason.to_owned()).into()));
            }
        }
    }
}

impl Connection {
    pub fn new<R, W>(read: R, mut write: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (normal, mut normal_rx) = mpsc::channel::<Outgoing>(64);
        let (priority, mut priority_rx) = mpsc::channel::<Outgoing>(16);
        let (event_tx, events) = mpsc::unbounded_channel();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let client = Client {
            normal,
            priority,
            pending: pending.clone(),
            next: Arc::new(AtomicU64::new(1)),
            closed: closed.clone(),
            handshake_started: Arc::new(AtomicBool::new(false)),
            flow: crate::output_flow::Flow::default(),
            display: Arc::new(Mutex::new(Display::default())),
            preview_changed: Arc::new(Notify::new()),
        };
        let writer_pending = pending.clone();
        let writer_closed = closed.clone();
        let writer_events = event_tx.clone();
        let writer = tokio::spawn(async move {
            loop {
                let frame = tokio::select! { biased;
                    frame = priority_rx.recv() => frame,
                    frame = normal_rx.recv() => frame,
                };
                let Some(frame) = frame else {
                    break;
                };
                let (frame, _screening) = match frame {
                    Outgoing::Frame(frame) => (frame, None),
                    Outgoing::Start { frame, guard } => {
                        let screening = frame["id"].as_u64().and_then(|id| {
                            writer_pending
                                .lock()
                                .unwrap()
                                .get_mut(&id)
                                .and_then(|reply| reply.screening.take())
                        });
                        if !guard.current() || screening.is_none() {
                            let reply = frame["id"]
                                .as_u64()
                                .and_then(|id| writer_pending.lock().unwrap().remove(&id));
                            if let Some(reply) = reply {
                                if let Some(sender) = reply.sender {
                                    let _ = sender.send(Err(StartCancelled.into()));
                                }
                                if let Some(cancelled) = reply.cancelled {
                                    let _ = cancelled.send(());
                                }
                            }
                            continue;
                        }
                        (frame, screening)
                    }
                    Outgoing::Flushed(reply) => {
                        let result = write.flush().await;
                        let error = result.as_ref().err().map(ToString::to_string);
                        let _ = reply.send(result);
                        if let Some(error) = error {
                            fail(&writer_pending, &writer_closed, &writer_events, &error);
                            break;
                        }
                        continue;
                    }
                };
                let mut bytes = serde_json::to_vec(&frame).expect("JSON value serializes");
                bytes.push(b'\n');
                if let Err(error) = write.write_all(&bytes).await {
                    fail(
                        &writer_pending,
                        &writer_closed,
                        &writer_events,
                        &error.to_string(),
                    );
                    break;
                }
                if let Err(error) = write.flush().await {
                    fail(
                        &writer_pending,
                        &writer_closed,
                        &writer_events,
                        &error.to_string(),
                    );
                    break;
                }
            }
        });
        let reader_client = client.clone();
        let reader = tokio::spawn(async move {
            let flow = reader_client.flow.clone();
            let mut active = HashMap::<String, Option<uuid::Uuid>>::new();
            let mut reader = BufReader::new(read);
            loop {
                let mut bytes = Vec::new();
                let count = (&mut reader)
                    .take(MAX_FRAME + 1)
                    .read_until(b'\n', &mut bytes)
                    .await;
                let result = match count {
                    Ok(0) => Err("host closed stdout".to_owned()),
                    Ok(n) if n as u64 > MAX_FRAME => Err("MSP frame exceeds 64 MiB".to_owned()),
                    Ok(_) => serde_json::from_slice::<Value>(&bytes).map_err(|e| e.to_string()),
                    Err(error) => Err(error.to_string()),
                };
                let mut frame = match result {
                    Ok(frame) if frame.get("jsonrpc") == Some(&json!("2.0")) => frame,
                    Ok(_) => {
                        fail(&pending, &closed, &event_tx, "invalid JSON-RPC version");
                        break;
                    }
                    Err(error) => {
                        if error == "MSP frame exceeds 64 MiB" && flow.interrupt() {
                            for (session, turn) in &active {
                                let _ = reader_client.interrupt_now(session, *turn);
                            }
                        }
                        fail(&pending, &closed, &event_tx, &error);
                        break;
                    }
                };
                // Server request IDs are positive integers, not the wider
                // JSON-RPC ID set. Reject before automatic input engagement.
                if frame["method"].is_string()
                    && let Some(id) = frame.get("id")
                    && !id.as_f64().is_some_and(|id| id > 0.0 && id.fract() == 0.0)
                {
                    fail(
                        &pending,
                        &closed,
                        &event_tx,
                        "server request id must be a positive integer",
                    );
                    break;
                }
                // A reply is never delayed behind the persistence queue.
                if let Some(object) = frame.as_object_mut() {
                    object.remove("_intercatRecovered");
                }
                let delta = frame["method"]
                    .as_str()
                    .is_some_and(|name| name.ends_with("/delta"));
                if delta && flow.status().lagging {
                    reader_client.clear_progress();
                    flow.drop_deltas(1);
                    continue;
                }
                {
                    let mut display = reader_client.display.lock().unwrap();
                    display.progress.receive(&frame);
                    reader_client.preview_changed.notify_one();
                    if delta {
                        frame = crate::recording::sanitize(frame, &[]);
                    }
                    if display.mask.redact_value(&mut frame).is_err() {
                        fail(
                            &pending,
                            &closed,
                            &event_tx,
                            "The host frame could not be masked safely.",
                        );
                        break;
                    }
                }
                let reply = if frame.get("method").is_none() {
                    frame
                        .get("id")
                        .and_then(Value::as_u64)
                        .and_then(|id| pending.lock().unwrap().remove(&id))
                } else {
                    None
                };
                let history_reply = reply.as_ref().is_some_and(|reply| reply.history);
                if let Some(sender) = reply.and_then(|reply| reply.sender) {
                    let result = if let Some(error) = frame.get("error") {
                        match serde_json::from_value::<muse_code_sdk::msp::ErrorObject>(
                            error.clone(),
                        ) {
                            Ok(error) => match muse_code_sdk::errors::MspError::try_from(error) {
                                Ok(error) => Err(error.into()),
                                Err(error) => Err(error.into()),
                            },
                            Err(error) => Err(error.into()),
                        }
                    } else {
                        Ok(frame.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = sender.send(result);
                }
                if frame["method"] == "approval/request"
                    && let Some(id) = frame.get("id")
                    && let Err(error) = reader_client.presented(id.clone())
                {
                    fail(&pending, &closed, &event_tx, &error.to_string());
                    break;
                }
                let input_engaged = if frame["method"] == "userInput/request" {
                    match reader_client.engage_input(&frame) {
                        Ok(engaged) => engaged,
                        Err(error) => {
                            fail(&pending, &closed, &event_tx, &error.to_string());
                            break;
                        }
                    }
                } else {
                    false
                };
                if frame["method"] == "turn/started"
                    && let Some(session) = frame["params"]["sessionId"].as_str()
                {
                    active.insert(
                        session.into(),
                        frame["params"]["turnId"]
                            .as_str()
                            .and_then(|id| id.parse().ok()),
                    );
                }
                let bytes = serde_json::to_vec(&frame).map_or(bytes.len(), |s| s.len());
                let mut permit = if history_reply {
                    flow.reserve_history(bytes)
                } else {
                    flow.reserve(bytes)
                };
                if permit.is_none() && delta {
                    reader_client.clear_progress();
                    flow.drop_deltas(1);
                    continue;
                }
                if permit.is_none() {
                    if flow.interrupt() {
                        for (session, turn) in &active {
                            let _ = reader_client.interrupt_now(session, *turn);
                        }
                    }
                    permit = if history_reply {
                        flow.wait_history(bytes).await
                    } else {
                        flow.wait(bytes).await
                    };
                }
                let completed = (frame["method"] == "turn/completed")
                    .then(|| {
                        frame["params"]["sessionId"]
                            .as_str()
                            .zip(frame["params"]["turnId"].as_str())
                            .map(|(session, turn)| {
                                (session.to_owned(), turn.parse::<uuid::Uuid>().ok())
                            })
                    })
                    .flatten();
                match permit {
                    Some(lease) => {
                        if event_tx
                            .send(Event::Frame(Received {
                                frame,
                                stamp: crate::clock::Stamp::now(),
                                lease,
                                source: None,
                                input_engaged,
                                recorded_display: None,
                            }))
                            .is_err()
                        {
                            break;
                        }
                    }
                    None => {
                        fail(&pending, &closed, &event_tx, "output too fast to record");
                        break;
                    }
                }
                if let Some((session, turn)) = completed
                    && turn.is_some()
                    && active.get(&session) == Some(&turn)
                {
                    active.remove(&session);
                    if active.is_empty() && flow.status().interrupted {
                        flow.settle_interrupt();
                    }
                }
            }
        });
        let preview_client = client.clone();
        let preview = tokio::spawn(async move {
            loop {
                preview_client.preview_changed.notified().await;
                if preview_client.is_closed() {
                    break;
                }
                // Coalesce deltas without putting scanning or queueing on the
                // transport path. At most one bounded scan is in flight.
                let pending = preview_client.display.lock().unwrap().progress.pending();
                let Some(pending) = pending else {
                    continue;
                };
                let result = tokio::task::spawn_blocking(move || {
                    let safe = pending.scan();
                    (pending, safe)
                })
                .await;
                if preview_client.is_closed() {
                    break;
                }
                if let Ok((pending, Ok(safe))) = result {
                    preview_client
                        .display
                        .lock()
                        .unwrap()
                        .progress
                        .apply(pending, safe);
                }
                tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            }
        });
        Self {
            client,
            events,
            buffered: VecDeque::new(),
            tasks: vec![reader, writer, preview],
            #[cfg(test)]
            hide_approval_feedback: false,
        }
    }

    pub fn close(&mut self) {
        self.client.closed.store(true, Ordering::SeqCst);
        for task in &self.tasks {
            task.abort();
        }
        for (_, reply) in self.client.pending.lock().unwrap().drain() {
            if let Some(sender) = reply.sender {
                let _ = sender.send(Err(ConnectionLost("MSP connection closed".into()).into()));
            }
        }
    }

    pub async fn next_event(&mut self) -> Option<Event> {
        let event = match self.buffered.pop_front() {
            Some(event) => event,
            None => self.events.recv().await?,
        };
        #[cfg(test)]
        let event = self.test_capabilities(event);
        Some(event)
    }
    pub fn try_next_event(&mut self) -> std::result::Result<Event, mpsc::error::TryRecvError> {
        let event = self
            .buffered
            .pop_front()
            .map(Ok)
            .unwrap_or_else(|| self.events.try_recv());
        #[cfg(test)]
        let event = event.map(|event| self.test_capabilities(event));
        event
    }
    #[cfg(test)]
    fn test_capabilities(&self, mut event: Event) -> Event {
        if self.hide_approval_feedback
            && let Event::Frame(received) = &mut event
            && let Some(choices) = received.frame["params"]["availableChoices"].as_array_mut()
        {
            for choice in choices {
                choice.as_object_mut().unwrap().remove("acceptsFeedback");
            }
            received.recorded_display = None;
        }
        event
    }
    pub(crate) fn restore_events(&mut self, events: Vec<Event>) {
        for event in events.into_iter().rev() {
            self.buffered.push_front(event);
        }
    }
    pub(crate) fn take_buffered(&mut self) -> Vec<Event> {
        self.buffered.drain(..).collect()
    }
    pub fn has_pending_events(&self) -> bool {
        !self.buffered.is_empty() || !self.events.is_empty()
    }

    pub fn close_input(&mut self) {
        self.tasks[1].abort();
    }

    /// Drain the reader after closing stdin; preserve final host events during shutdown.
    pub async fn finish(&mut self) -> Vec<Value> {
        self.finish_received()
            .await
            .into_iter()
            .map(|frame| frame.frame)
            .collect()
    }
    pub(crate) async fn finish_received(&mut self) -> Vec<Received> {
        if tokio::time::timeout(std::time::Duration::from_secs(1), &mut self.tasks[0])
            .await
            .is_err()
        {
            self.tasks[0].abort();
        }
        let mut frames = Vec::new();
        while let Ok(event) = self.try_next_event() {
            if let Event::Frame(received) = event {
                frames.push(received);
            }
        }
        self.close();
        frames
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
    }
}

impl Client {
    pub fn set_redaction(&self, mask: crate::progress::Mask) {
        let mut display = self.display.lock().unwrap();
        display.progress.replace_mask(mask.clone());
        display.mask = mask;
    }
    pub fn configure_progress(
        &self,
        session: String,
        privacy: crate::progress::Privacy,
        active_turn: Option<String>,
    ) {
        let mut display = self.display.lock().unwrap();
        let mask = display.mask.clone();
        display
            .progress
            .configure(session, mask, privacy, active_turn);
    }
    pub fn progress(&self) -> Option<crate::progress::Preview> {
        if self.is_closed() {
            None
        } else {
            self.display.lock().unwrap().progress.snapshot()
        }
    }
    pub fn scanned_progress(&self, frame: &Value) {
        self.display.lock().unwrap().progress.scanned(frame);
    }
    pub fn clear_progress(&self) {
        self.display.lock().unwrap().progress.clear();
    }
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
    /// The receive loop consumes and records this reply in physical order.
    pub(crate) async fn query(&self, method: &str, params: Value) -> Result<PendingRequest> {
        if method == "turn/start" {
            return Err("Turn starts require a screened request.".into());
        }
        self.query_guarded(method, params, None).await
    }
    pub(crate) async fn query_start(
        &self,
        prepared: crate::screening::Command,
        guard: StartGuard,
    ) -> Result<PendingRequest> {
        self.query_guarded(
            "turn/start",
            prepared.params,
            Some((guard, prepared.permit)),
        )
        .await
    }
    async fn query_guarded(
        &self,
        method: &str,
        params: Value,
        guard: Option<(StartGuard, Arc<tokio::sync::OwnedRwLockReadGuard<()>>)>,
    ) -> Result<PendingRequest> {
        if self.is_closed() {
            return Err(ConnectionLost("MSP connection closed".into()).into());
        }
        self.expect_progress(method, &params);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (cancelled, receive_cancelled) = oneshot::channel();
        self.pending.lock().unwrap().insert(
            id,
            PendingReply {
                screening: guard.as_ref().map(|(_, permit)| permit.clone()),
                sender: None,
                history: matches!(method, "session/read" | "session/resume" | "session/fork"),
                cancelled: Some(cancelled),
            },
        );
        let request = PendingRequest {
            id,
            pending: self.pending.clone(),
            cancelled: Some(receive_cancelled),
        };
        let sender = if method == "turn/unqueue" {
            &self.priority
        } else {
            &self.normal
        };
        let frame = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        let outgoing = match guard {
            Some((guard, _permit)) => Outgoing::Start { frame, guard },
            None => Outgoing::Frame(frame),
        };
        sender.send(outgoing).await?;
        Ok(request)
    }
    pub(crate) fn output_budget(&self) -> crate::output_flow::Flow {
        self.flow.clone()
    }
    pub fn output_flow(&self) -> crate::output_flow::Status {
        self.flow.status()
    }
    /// Emergency stop on the still-writable pipe when the receive/log path failed.
    /// This deliberately does not wait for an acknowledgement or journal write.
    pub fn interrupt_now(&self, session: &str, turn: Option<uuid::Uuid>) -> Result<()> {
        let mut params = json!({"commandId":uuid::Uuid::now_v7(),"sessionId":session});
        if let Some(turn) = turn {
            params["turnId"] = json!(turn);
        }
        self.control_now("turn/interrupt", params)
    }
    pub fn control_now(&self, method: &str, params: Value) -> Result<()> {
        if method == "turn/start" {
            return Err("Turn starts require a screened request.".into());
        }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.priority.try_send(Outgoing::Frame(
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        ))?;
        Ok(())
    }
    pub fn notify_now(&self, method: &str, params: Value) -> Result<()> {
        if method == "turn/start" {
            return Err("Turn starts require a screened request.".into());
        }
        self.priority.try_send(Outgoing::Frame(
            json!({"jsonrpc":"2.0","method":method,"params":params}),
        ))?;
        Ok(())
    }
    pub(crate) fn engage_input(&self, frame: &Value) -> Result<bool> {
        let params = &frame["params"];
        let (Some(session), Some(input)) =
            (params["sessionId"].as_str(), params["userInputId"].as_str())
        else {
            return Ok(false);
        };
        if session.is_empty() || input.is_empty() {
            return Ok(false);
        }
        self.notify_now(
            "userInput/engaged",
            json!({"sessionId":session,"userInputId":input}),
        )?;
        if let Some(id) = frame.get("id") {
            self.presented(id.clone())?;
        }
        Ok(true)
    }
    pub fn presented(&self, id: Value) -> Result<()> {
        self.priority.try_send(Outgoing::Frame(
            json!({"jsonrpc":"2.0","id":id,"result":{}}),
        ))?;
        Ok(())
    }
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if method == "turn/start" {
            return Err("Turn starts require a screened request.".into());
        }
        self.request_guarded(method, params, None).await
    }
    pub async fn decide(&self, prepared: crate::screening::Command) -> Result<Value> {
        let _permit = prepared.permit;
        self.request("approval/decide", prepared.params).await
    }
    pub async fn start(
        &self,
        prepared: crate::screening::Command,
        guard: StartGuard,
    ) -> Result<Value> {
        self.request_guarded(
            "turn/start",
            prepared.params,
            Some((guard, prepared.permit)),
        )
        .await
    }
    async fn request_guarded(
        &self,
        method: &str,
        params: Value,
        guard: Option<(StartGuard, Arc<tokio::sync::OwnedRwLockReadGuard<()>>)>,
    ) -> Result<Value> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(ConnectionLost("MSP connection closed".into()).into());
        }
        self.expect_progress(method, &params);
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (send, receive) = oneshot::channel();
        self.pending.lock().unwrap().insert(
            id,
            PendingReply {
                screening: guard.as_ref().map(|(_, permit)| permit.clone()),
                sender: Some(send),
                history: matches!(method, "session/read" | "session/resume" | "session/fork"),
                cancelled: None,
            },
        );
        let _request = PendingRequest {
            id,
            pending: self.pending.clone(),
            cancelled: None,
        };
        let frame = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
        let sender = if matches!(
            method,
            "approval/decide"
                | "turn/interrupt"
                | "turn/unqueue"
                | "userInput/interrupt"
                | "userInput/answer"
                | "userInput/clarify"
                | "userInput/cancel"
        ) {
            &self.priority
        } else {
            &self.normal
        };
        let outgoing = match guard {
            Some((guard, _permit)) => Outgoing::Start { frame, guard },
            None => Outgoing::Frame(frame),
        };
        if let Err(error) = sender.send(outgoing).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(ConnectionLost(error.to_string()).into());
        }
        match tokio::time::timeout(std::time::Duration::from_secs(30), receive).await {
            Ok(result) => result.map_err(|error| ConnectionLost(error.to_string()))?,
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(ConnectionLost("MSP response timed out; outcome unknown".into()).into())
            }
        }
    }
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        if method == "turn/start" {
            return Err("Turn starts require a screened request.".into());
        }
        if self.closed.load(Ordering::SeqCst) {
            return Err(ConnectionLost("MSP connection closed".into()).into());
        }
        self.normal
            .send(Outgoing::Frame(
                json!({"jsonrpc":"2.0","method":method,"params":params}),
            ))
            .await?;
        Ok(())
    }
    fn expect_progress(&self, method: &str, params: &Value) {
        if method == "turn/start"
            && let (Some(session), Some(turn)) =
                (params["sessionId"].as_str(), params["commandId"].as_str())
        {
            self.display
                .lock()
                .unwrap()
                .progress
                .expect_turn(session, turn);
        }
    }
    pub async fn initialize(&self) -> Result<Value> {
        if self.handshake_started.swap(true, Ordering::AcqRel) {
            return Err(
                std::io::Error::other("initialize may be sent only once per connection").into(),
            );
        }
        let result = self.request("initialize", json!({"clientInfo":{"name":"intercat","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true,"userInputDialogs":true}})).await?;
        if result
            .pointer("/schema/fingerprint")
            .and_then(Value::as_str)
            != Some(FINGERPRINT)
        {
            return Err(std::io::Error::other("Muse MSP fingerprint is not verified").into());
        }
        self.notify("initialized", json!({})).await?;
        self.flush().await?;
        Ok(result)
    }
    async fn flush(&self) -> Result<()> {
        if self.is_closed() {
            return Err(ConnectionLost("MSP connection closed".into()).into());
        }
        let (reply, receive) = oneshot::channel();
        self.normal
            .send(Outgoing::Flushed(reply))
            .await
            .map_err(|error| ConnectionLost(error.to_string()))?;
        tokio::time::timeout(std::time::Duration::from_secs(30), receive)
            .await
            .map_err(|_| ConnectionLost("MSP write timed out".into()))?
            .map_err(|error| ConnectionLost(error.to_string()))?
            .map_err(|error| ConnectionLost(error.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn screened(params: Value) -> crate::screening::Command {
        crate::screening::Command {
            params,
            permit: Arc::new(Arc::new(tokio::sync::RwLock::new(())).read_owned().await),
        }
    }

    #[tokio::test]
    async fn server_rejections_keep_the_sdk_error_kind_and_malformed_errors_do_not() {
        for structured in [true, false] {
            let (read, mut responses) = tokio::io::duplex(4096);
            let (write, requests) = tokio::io::duplex(4096);
            let mut connection = Connection::new(read, write);
            let client = connection.client.clone();
            let task = tokio::spawn(async move { client.request("fixture", json!({})).await });
            let mut requests = BufReader::new(requests);
            let mut line = String::new();
            requests.read_line(&mut line).await.unwrap();
            let frame: Value = serde_json::from_str(&line).unwrap();
            let mut error = json!({"code":-32030,"message":"Dummy rejected command"});
            if structured {
                error["data"] = json!({"kind":"commandRejected","retryable":false});
            }
            responses
                .write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":frame["id"],"error":error})
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let error = task.await.unwrap().unwrap_err();
            assert_eq!(rejected_start_code(error.as_ref()), None);
            if structured {
                let error = error
                    .downcast_ref::<muse_code_sdk::errors::MspError>()
                    .unwrap();
                assert_eq!(error.code(), -32030);
                assert_eq!(error.retryable(), Some(false));
            } else {
                assert!(error.is::<muse_code_sdk::errors::ProtocolError>());
            }
            connection.close();
        }
    }

    #[tokio::test]
    async fn a_stop_cancels_queued_starts_but_keeps_the_connection_usable() {
        queued_starts_cancel(false).await;
    }
    #[tokio::test]
    async fn global_stop_then_resume_cannot_revive_a_queued_start() {
        queued_starts_cancel(true).await;
    }
    async fn queued_starts_cancel(global: bool) {
        let (read, mut responses) = tokio::io::duplex(4096);
        let (write, mut requests) = tokio::io::duplex(1);
        let mut connection = Connection::new(read, write);
        connection
            .client
            .notify("fixture/block", json!({"text":"x".repeat(100)}))
            .await
            .unwrap();
        let mut first_byte = [0];
        requests.read_exact(&mut first_byte).await.unwrap();
        let generation = Arc::new(AtomicU64::new(0));
        let paused = Arc::new(crate::dispatch_control::Gate::new(false));
        let guard = StartGuard::new(generation.clone(), paused.clone());
        let client = connection.client.clone();
        let old = tokio::spawn(async move {
            client
                .start(
                    screened(json!({"sessionId":"session","commandId":"old"})).await,
                    guard,
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while connection.client.pending.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if global {
            paused.begin(true);
            let epoch = paused.begin(false);
            assert!(paused.resume(epoch));
        } else {
            generation.fetch_add(1, Ordering::AcqRel);
        }
        connection.client.interrupt_now("session", None).unwrap();
        let client = connection.client.clone();
        let fresh = tokio::spawn(async move {
            client
                .start(
                    screened(json!({"sessionId":"session","commandId":"new"})).await,
                    StartGuard::new(generation, paused),
                )
                .await
        });
        let mut requests = BufReader::new(requests);
        let mut line = String::new();
        requests.read_line(&mut line).await.unwrap();
        line.clear();
        requests.read_line(&mut line).await.unwrap();
        let interrupt: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(interrupt["method"], "turn/interrupt");
        line.clear();
        requests.read_line(&mut line).await.unwrap();
        let next: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(next["params"]["commandId"], "new");
        responses
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":next["id"],"result":{"ok":true}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        assert!(old.await.unwrap().unwrap_err().is::<StartCancelled>());
        assert_eq!(fresh.await.unwrap().unwrap(), json!({"ok":true}));
        assert!(!connection.client.is_closed());
        connection.close();
    }

    #[tokio::test]
    async fn a_cancelled_recovery_query_wakes_without_a_fabricated_host_frame() {
        let (read, _responses) = tokio::io::duplex(4096);
        let (write, requests) = tokio::io::duplex(4096);
        let mut connection = Connection::new(read, write);
        let generation = Arc::new(AtomicU64::new(0));
        let guard = StartGuard::new(
            generation.clone(),
            std::sync::Arc::new(crate::dispatch_control::Gate::new(false)),
        );
        generation.fetch_add(1, Ordering::AcqRel);
        let mut request = connection
            .client
            .query_start(
                screened(json!({"sessionId":"session","commandId":"old"})).await,
                guard,
            )
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), request.cancelled())
            .await
            .unwrap();
        assert!(connection.client.pending.lock().unwrap().is_empty());
        assert!(connection.try_next_event().is_err());
        let abandoned = connection
            .client
            .query_start(
                screened(json!({"sessionId":"session","commandId":"abandoned"})).await,
                StartGuard::new(
                    generation,
                    std::sync::Arc::new(crate::dispatch_control::Gate::new(false)),
                ),
            )
            .await
            .unwrap();
        drop(abandoned);
        connection.client.flush().await.unwrap();
        let mut requests = BufReader::new(requests);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), requests.fill_buf())
                .await
                .is_err()
        );
        connection.close();
    }

    #[tokio::test]
    async fn initialize_cannot_be_repeated_on_one_connection() {
        let (read, mut responses) = tokio::io::duplex(4096);
        let (write, requests) = tokio::io::duplex(4096);
        let mut connection = Connection::new(read, write);
        let peer = tokio::spawn(async move {
            let mut requests = BufReader::new(requests);
            loop {
                let mut line = String::new();
                if requests.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                if request["method"] == "initialize" {
                    responses.write_all(format!("{}\n",json!({"jsonrpc":"2.0","id":request["id"],"result":{"schema":{"fingerprint":FINGERPRINT}}})).as_bytes()).await.unwrap();
                }
            }
        });
        connection.client.initialize().await.unwrap();
        let repeated = connection.client.initialize().await;
        connection.close();
        peer.abort();
        assert!(
            repeated.is_err(),
            "A second initialize was sent and accepted"
        );
    }

    #[tokio::test]
    async fn initialize_waits_until_its_notification_is_written() {
        struct RefuseInitialized(mpsc::UnboundedSender<Value>);
        impl AsyncWrite for RefuseInitialized {
            fn poll_write(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                bytes: &[u8],
            ) -> std::task::Poll<std::io::Result<usize>> {
                let frame: Value = serde_json::from_slice(bytes).unwrap();
                if frame["method"] == "initialized" {
                    return std::task::Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "fixture rejected initialized",
                    )));
                }
                self.0.send(frame).unwrap();
                std::task::Poll::Ready(Ok(bytes.len()))
            }
            fn poll_flush(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }
        let (read, mut responses) = tokio::io::duplex(4096);
        let (sent, mut requests) = mpsc::unbounded_channel();
        let mut connection = Connection::new(read, RefuseInitialized(sent));
        let client = connection.client.clone();
        let initialization = tokio::spawn(async move { client.initialize().await });
        let request = requests.recv().await.unwrap();
        responses.write_all(format!("{}\n",json!({"jsonrpc":"2.0","id":request["id"],"result":{"schema":{"fingerprint":FINGERPRINT}}})).as_bytes()).await.unwrap();
        let result = initialization.await.unwrap();
        connection.close();
        assert!(
            result.is_err(),
            "Handshake succeeded although initialized could not be written"
        );
    }

    #[tokio::test]
    async fn requested_history_can_exceed_the_notification_budget() {
        let (read, mut host_write) = tokio::io::duplex(65536);
        let (host_read, write) = tokio::io::duplex(4096);
        let mut connection = Connection::new(read, write);
        let query = connection
            .client
            .query(
                "session/read",
                json!({"sessionId":"session","excludeItems":false}),
            )
            .await
            .unwrap();
        let id = query.id;
        let mut host_read = BufReader::new(host_read);
        let mut line = String::new();
        host_read.read_line(&mut line).await.unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["id"], id);
        let items:Vec<_>=(0..144).map(|n|json!({"itemId":format!("item-{n}"),"kind":"agentMessage","revision":1,"status":"completed","text":"x".repeat(65536)})).collect();
        let frame = json!({"jsonrpc":"2.0","id":id,"result":{"history":{"mode":"inline","items":items,"snapshot":null}}});
        let producer = tokio::spawn(async move {
            host_write
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .unwrap();
        });
        let received =
            tokio::time::timeout(std::time::Duration::from_secs(3), connection.next_event())
                .await
                .unwrap();
        assert!(
            matches!(received, Some(Event::Frame(_))),
            "Requested history was rejected by the live notification budget"
        );
        assert!(!connection.client.output_flow().interrupted);
        producer.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_unexpected_or_cancelled_replies_keep_the_notification_limit() {
        for (method, cancel, foreign_id) in [
            ("turn/start", false, false),
            ("session/read", true, false),
            ("session/read", false, true),
        ] {
            let (read, mut host_write) = tokio::io::duplex(65536);
            let (host_read, write) = tokio::io::duplex(4096);
            let mut connection = Connection::new(read, write);
            let request = if method == "turn/start" {
                connection
                    .client
                    .query_start(
                        screened(json!({})).await,
                        StartGuard::Stopped(Arc::new(AtomicBool::new(false))),
                    )
                    .await
                    .unwrap()
            } else {
                connection.client.query(method, json!({})).await.unwrap()
            };
            let id = request.id;
            let mut request = Some(request);
            let mut host_read = BufReader::new(host_read);
            let mut line = String::new();
            host_read.read_line(&mut line).await.unwrap();
            if cancel {
                drop(request.take());
            }
            let frame = json!({"jsonrpc":"2.0","id":if foreign_id{id+1}else{id},"result":{"history":{"mode":"inline","items":[],"text":"x".repeat(crate::output_flow::LIMIT+1)}}});
            let producer = tokio::spawn(async move {
                host_write
                    .write_all(format!("{frame}\n").as_bytes())
                    .await
                    .unwrap();
            });
            let event = connection.next_event().await;
            assert!(
                matches!(event,Some(Event::Closed(reason)) if reason=="output too fast to record")
            );
            assert!(connection.client.output_flow().interrupted);
            producer.await.unwrap();
        }
    }

    #[tokio::test]
    async fn approval_receipt_precedes_consumption_and_never_decides_the_request() {
        let (read, mut host_write) = tokio::io::duplex(4096);
        let (host_read, write) = tokio::io::duplex(4096);
        let mut connection = Connection::new(read, write);
        let request = json!({"jsonrpc":"2.0","id":7,"method":"approval/request",
            "params":{"sessionId":"session","approvalId":"approval"}});
        host_write
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        let mut host_read = BufReader::new(host_read);
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            host_read.read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap(),
            json!({"jsonrpc":"2.0","id":7,"result":{}})
        );
        assert!(
            matches!(connection.next_event().await, Some(Event::Frame(frame)) if frame.frame == request)
        );
        connection.close();
        let mut tail = Vec::new();
        host_read.read_to_end(&mut tail).await.unwrap();
        assert!(tail.is_empty());
    }

    #[tokio::test]
    async fn invalid_server_request_ids_never_engage_or_present_a_question() {
        for id in [
            json!("one"),
            json!(0),
            json!(-1),
            json!(1.5),
            Value::Null,
            json!(true),
        ] {
            let (read, mut host_write) = tokio::io::duplex(4096);
            let (mut host_read, write) = tokio::io::duplex(4096);
            let mut connection = Connection::new(read, write);
            let request = json!({"jsonrpc":"2.0","id":id,"method":"userInput/request","params":{"sessionId":"session","userInputId":"question"}});
            host_write
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            assert!(matches!(connection.next_event().await,
                Some(Event::Closed(reason)) if reason == "server request id must be a positive integer"));
            connection.close();
            let mut output = Vec::new();
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                host_read.read_to_end(&mut output),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                output.is_empty(),
                "Invalid request received an engagement or receipt"
            );
        }
    }

    #[tokio::test]
    async fn pending_rpc_reports_transport_loss_without_replaying_it() {
        let (read, output) = tokio::io::duplex(4096);
        let (write, input) = tokio::io::duplex(4096);
        let mut connection = Connection::new(read, write);
        let client = connection.client.clone();
        let request = tokio::spawn(async move {
            client
                .start(
                    screened(json!({"commandId":"one"})).await,
                    StartGuard::Stopped(Arc::new(AtomicBool::new(false))),
                )
                .await
        });
        let mut input = BufReader::new(input);
        let mut line = String::new();
        input.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["method"],
            "turn/start"
        );
        drop(output);
        let error = request.await.unwrap().unwrap_err();
        assert!(error.is::<ConnectionLost>());
        assert!(matches!(
            connection.next_event().await,
            Some(Event::Closed(_))
        ));
        assert!(
            connection
                .client
                .request("session/read", json!({}))
                .await
                .unwrap_err()
                .is::<ConnectionLost>()
        );
    }

    #[tokio::test]
    async fn oversized_output_requires_host_cleanup_even_without_an_active_turn() {
        let input = tokio::io::repeat(b'x').take(MAX_FRAME + 1);
        let mut connection = Connection::new(input, tokio::io::sink());
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(2), connection.next_event())
                .await
                .unwrap();
        assert!(
            matches!(event, Some(Event::Closed(reason)) if reason == "MSP frame exceeds 64 MiB")
        );
        assert!(connection.client.output_budget().needs_force());
    }

    #[tokio::test]
    async fn only_the_current_turn_completion_acknowledges_an_output_interrupt() {
        let (incoming, mut output) = tokio::io::duplex(4096);
        let (outgoing, _input) = tokio::io::duplex(4096);
        let mut connection = Connection::new(incoming, outgoing);
        let current = uuid::Uuid::now_v7();
        let old = uuid::Uuid::now_v7();
        output.write_all(format!("{}\n",json!({"jsonrpc":"2.0","method":"turn/started","params":{"sessionId":"s","turnId":current}})).as_bytes()).await.unwrap();
        let Some(Event::Frame(_)) = connection.next_event().await else {
            panic!("Missing start");
        };
        let flow = connection.client.output_budget();
        flow.interrupt();
        output.write_all(format!("{}\n",json!({"jsonrpc":"2.0","method":"turn/completed","params":{"sessionId":"s","turnId":old}})).as_bytes()).await.unwrap();
        let Some(Event::Frame(_)) = connection.next_event().await else {
            panic!("Missing old completion");
        };
        tokio::task::yield_now().await;
        assert!(flow.needs_force());
        output.write_all(format!("{}\n",json!({"jsonrpc":"2.0","method":"turn/completed","params":{"sessionId":"s","turnId":current}})).as_bytes()).await.unwrap();
        let Some(Event::Frame(_)) = connection.next_event().await else {
            panic!("Missing completion");
        };
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while flow.needs_force() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn committed_approval_and_question_responses_use_the_priority_writer() {
        for (method, choice) in [
            ("approval/decide", "fixture-yes"),
            ("approval/decide", "fixture-no"),
            ("userInput/clarify", "clarification"),
        ] {
            let (incoming, mut replies) = tokio::io::duplex(4096);
            let (outgoing, mut receiver) = tokio::io::duplex(64);
            let connection = Connection::new(incoming, outgoing);
            connection.client.normal.send(Outgoing::Frame(json!({"jsonrpc":"2.0","method":"blocking-fixture","params":{"text":"x".repeat(2048)}}))).await.unwrap();
            let mut prefix = [0u8; 1];
            receiver.read_exact(&mut prefix).await.unwrap();
            connection
                .client
                .normal
                .send(Outgoing::Frame(
                    json!({"jsonrpc":"2.0","method":"ordinary-query","params":{}}),
                ))
                .await
                .unwrap();
            let client = connection.client.clone();
            let choice = choice.to_owned();
            let action =
                tokio::spawn(
                    async move { client.request(method, json!({"choiceId":choice})).await },
                );
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while connection.client.priority.capacity() == 16 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let mut receiver = BufReader::new(receiver);
            let mut line = String::new();
            receiver.read_line(&mut line).await.unwrap();
            line.clear();
            receiver.read_line(&mut line).await.unwrap();
            let decision: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(decision["method"], method);
            line.clear();
            receiver.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["method"],
                "ordinary-query"
            );
            replies
                .write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":decision["id"],"result":{}})
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            action.await.unwrap().unwrap();
        }
    }
}
