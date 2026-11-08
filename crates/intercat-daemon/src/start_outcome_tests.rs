//! Real execution-loop admission with a local, deterministic MSP peer.
use super::*;
use crate::{
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    runtime::WorkerState,
};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

struct Fixture {
    _root: tempfile::TempDir,
    journal: Journal,
    track: String,
    input: String,
    port: Port,
    capacity: Arc<crate::scheduler::Capacity>,
    incoming: BufReader<DuplexStream>,
    outgoing: DuplexStream,
    editor: crate::editor::Manager,
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<(Result<()>, Continuity)>,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let journal =
            Journal::open(root.path().join("db"), crate::secrets::Key::new([7; 32])).await?;
        journal
            .stage_utterance(
                Uuid::now_v7(),
                "Dummy request that must not run after rejection.".into(),
                Target::New,
                None,
                vec![],
            )
            .await?;
        let word = journal.claim_input().await?.unwrap();
        let input = word.id.clone();
        journal
            .apply_input(
                word.id,
                word.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Fork,
                    track_id: None,
                    boundary: Some("when-told".into()),
                    command: None,
                    confidence: 1.,
                    inference_ms: 1.,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await?;
        let track = journal.snapshot().await?["tracks"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let workspace = root.path().join("worker");
        std::fs::create_dir(&workspace)?;
        gix::init(&workspace)?;
        let repo = intercat_git::Repository::open(&workspace)?;
        let base = repo.empty_starting_point()?;
        if !std::process::Command::new("/usr/bin/git")
            .current_dir(&workspace)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(["read-tree", &base.to_string()])
            .status()?
            .success()
        {
            return Err("Could not prepare the dummy repository index.".into());
        }
        journal
            .workspace_result(crate::preparation::Workspace {
                attachment_roots: vec![],
                track_id: track.clone(),
                path: workspace.clone(),
                reference: "refs/intercat/test/worker".into(),
                base: base.to_string(),
                state: "ready".into(),
                error: None,
            })
            .await?;
        let editor = crate::runtime::tests::native_editor(root.path()).await?;
        let capacity = Arc::new(crate::scheduler::Capacity::new(1));
        let (read, outgoing) = tokio::io::duplex(65536);
        let (write, incoming) = tokio::io::duplex(65536);
        let mut connection = crate::msp::Connection::new(read, write);
        let (port, commands) = Port::new(Arc::new(crate::dispatch_control::Gate::new(false)));
        port.attach(connection.client.clone(), "session".into());
        let mut state = WorkerState {
            track_id: track.clone(),
            phase: "ready".into(),
            session_id: Some("session".into()),
            secret_revision: None,
            identity: None,
            detail: None,
            active_turn: None,
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        };
        journal.worker_state(state.clone()).await?;
        let mut context = Context {
            project_secrets: crate::project_secrets::Store::default(),
            port: port.clone(),
            commands,
            editor: editor.clone(),
            capacity: capacity.clone(),
        };
        let identity = crate::process::identity(std::process::id())?;
        let tracker = Arc::new(Mutex::new(crate::process::Tracker::new(
            "start-outcome-test".into(),
            identity,
        )?));
        let (stop, mut stopping) = tokio::sync::watch::channel(false);
        let saved = journal.clone();
        let task = tokio::spawn(async move {
            let mut continuity = Continuity::default();
            let outcome = run(
                WorkerIo {
                    connection: &mut connection,
                    tracker,
                    identity,
                    recovery: None,
                    continuity: &mut continuity,
                },
                &saved,
                &workspace,
                &mut state,
                &mut context,
                &mut stopping,
            )
            .await;
            connection.close();
            if let Err(error) = continuity.settle().await {
                return (Err(error), continuity);
            }
            if let Err(error) = &outcome {
                eprintln!("Start fixture ended: {error}");
            }
            (outcome, continuity)
        });
        Ok(Self {
            _root: root,
            journal,
            track,
            input,
            port,
            capacity,
            incoming: BufReader::new(incoming),
            outgoing,
            editor,
            stop,
            task,
        })
    }
    async fn start(&self) -> Result<()> {
        let word = self.journal.read_utterance(self.input.clone()).await?;
        if word["state"] == "held" || word["decision"]["boundary"] == "when-told" {
            self.journal
                .start_held_input(
                    Uuid::now_v7(),
                    self.input.clone(),
                    word["revision"].as_i64().unwrap(),
                )
                .await?;
        }
        self.port
            .start_admitted(
                Uuid::now_v7(),
                self.port.stop_generation.load(Ordering::Acquire),
                true,
            )
            .await
    }
    async fn read(&mut self) -> Result<Value> {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), self.incoming.read_line(&mut line)).await??;
        Ok(serde_json::from_str(&line)?)
    }
    async fn send(&mut self, mut value: Value) -> Result<()> {
        value["jsonrpc"] = json!("2.0");
        self.outgoing
            .write_all(format!("{value}\n").as_bytes())
            .await?;
        Ok(())
    }
    async fn settled(&self, command: Uuid, finished: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let word = self.journal.read_utterance(self.input.clone()).await?;
                if word["state"] == "held"
                    && self.capacity.snapshot().active == 0
                    && self
                        .journal
                        .operation(command)
                        .await?
                        .is_some_and(|operation| operation["state"] == finished)
                {
                    return Ok(());
                }
                if self.task.is_finished() {
                    return Err("The execution loop exited before holding the request.".into());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?
    }
    async fn close(self) -> Result<()> {
        self.stop.send_replace(true);
        let (result, continuity) = self.finished().await?;
        drop(continuity);
        result
    }
    async fn finished(self) -> Result<(Result<()>, Continuity)> {
        let result = tokio::time::timeout(Duration::from_secs(10), self.task).await?;
        self.editor.stop().await;
        Ok(result?)
    }
}

#[tokio::test]
async fn ambiguous_start_errors_keep_the_slot_until_confirmed_cleanup() -> Result<()> {
    for (code, kind) in [
        (-32603, "internal"),
        (-32013, "interrupted"),
        (-32014, "cancelled"),
        (-32030, "commandRejected"),
    ] {
        let mut f = Fixture::new().await?;
        f.start().await?;
        let start = f.read().await?;
        let command: Uuid = start["params"]["commandId"].as_str().unwrap().parse()?;
        f.send(json!({"id":start["id"],"error":{"code":code,"message":"Unknown start outcome","data":{"kind":kind,"commandId":command}}})).await?;
        let capacity = f.capacity.clone();
        let journal = f.journal.clone();
        let input = f.input.clone();
        let (outcome, continuity) = f.finished().await?;
        assert!(outcome.is_err(), "{kind}");
        assert!(continuity.has_slot(), "{kind}");
        assert_eq!(capacity.snapshot().active, 1, "{kind}");
        assert_eq!(journal.read_utterance(input).await?["state"], "starting");
        assert_eq!(
            journal.operation(command).await?.unwrap()["state"],
            "prepared"
        );
        drop(continuity);
        assert_eq!(capacity.snapshot().active, 0, "{kind}");
    }
    Ok(())
}

#[tokio::test]
async fn waiting_requests_on_one_worker_keep_their_original_order() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.capacity.set_limit(1, 0);
    f.start().await?;
    f.journal
        .stage_utterance(
            Uuid::now_v7(),
            "Later dummy request".into(),
            Target::Lock(f.track.clone()),
            None,
            vec![],
        )
        .await?;
    let next = f.journal.claim_input().await?.unwrap();
    let later = next.id.clone();
    f.journal
        .apply_input(
            next.id,
            next.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(f.track.clone()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    f.capacity.set_limit(2, 1);
    let start = match f.read().await {
        Ok(start) => start,
        Err(error) => {
            let snapshot = f.journal.snapshot().await?;
            let evidence = json!({"workers":snapshot["workers"],"words":snapshot["utterances"],"capacity":f.capacity.snapshot()});
            let _ = f.close().await;
            return Err(format!("Waiting start failed: {error}; {evidence}").into());
        }
    };
    assert!(
        start["params"]
            .to_string()
            .contains("Dummy request that must not run after rejection.")
    );
    assert!(!start["params"].to_string().contains("Later dummy request"));
    f.send(json!({"id":start["id"],"error":{"code":-32602,"message":"Dummy rejection","data":{"kind":"invalidParams","commandId":start["params"]["commandId"]}}})).await?;
    let command = start["params"]["commandId"].as_str().unwrap().parse()?;
    if let Err(error) = f.settled(command, "superseded").await {
        let snapshot = f.journal.snapshot().await?;
        let evidence = json!({"workers":snapshot["workers"],"words":snapshot["utterances"],"operation":f.journal.operation(command).await?,"capacity":f.capacity.snapshot()});
        let _ = f.close().await;
        return Err(format!("Waiting start did not settle: {error}; {evidence}").into());
    }
    assert_eq!(f.journal.read_utterance(later).await?["state"], "placed");
    f.close().await
}

#[tokio::test]
async fn ten_rejected_starts_release_slots_and_keep_the_original_request() -> Result<()> {
    let mut f = Fixture::new().await?;
    let original = f.journal.read_utterance(f.input.clone()).await?["text"].clone();
    for _ in 0..10 {
        f.start().await?;
        let request = f.read().await?;
        assert_eq!(request["method"], "turn/start");
        assert_eq!(f.capacity.snapshot().active, 1);
        f.send(json!({"id":request["id"],"error":{"code":-32602,"message":"Dummy start was rejected","data":{"kind":"invalidParams","retryable":false,"commandId":request["params"]["commandId"],"reason":"fixture"}}})).await?;
        let command = request["params"]["commandId"].as_str().unwrap().parse()?;
        f.settled(command, "superseded").await?;
        assert!(f.capacity.snapshot().waiting.is_empty());
        let word = f.journal.read_utterance(f.input.clone()).await?;
        assert_eq!(word["text"], original);
        assert_eq!(
            f.journal.operation(command).await?.unwrap()["state"],
            "superseded"
        );
        let state = f.journal.snapshot().await?;
        assert!(state["workers"][0]["activeTurn"].is_null());
        assert_eq!(state["workers"][0]["checkpointPending"], false);
        assert!(!f.task.is_finished());
    }
    assert_eq!(
        f.track,
        f.journal.read_utterance(f.input.clone()).await?["decision"]["trackId"]
            .as_str()
            .unwrap()
    );
    f.close().await
}

#[tokio::test]
async fn ten_queued_starts_return_their_slots_and_words_in_both_reply_orders() -> Result<()> {
    let mut f = Fixture::new().await?;
    let original = f.journal.read_utterance(f.input.clone()).await?["text"].clone();
    for index in 0..10 {
        f.start().await?;
        let start = f.read().await?;
        assert_eq!(start["method"], "turn/start");
        let command: Uuid = start["params"]["commandId"].as_str().unwrap().parse()?;
        let turn = Uuid::now_v7();
        f.send(json!({"id":start["id"],"result":{"commandId":command,"turnId":turn,"status":"accepted","startedNewTurn":false,"disposition":"queued"}})).await?;
        let cancel = f.read().await?;
        assert_eq!(cancel["method"], "turn/unqueue");
        assert_eq!(cancel["params"]["turnId"], turn.to_string());
        assert_eq!(f.capacity.snapshot().active, 1);
        let event = json!({"method":"turn/unqueued","params":{"sessionId":"session","commandId":command,"turnId":turn}});
        let ack = json!({"id":cancel["id"],"result":{"commandId":cancel["params"]["commandId"],"turnId":turn,"status":"accepted"}});
        if index % 2 == 0 {
            f.send(event.clone()).await?;
            f.settled(command, "complete").await?;
            f.send(ack).await?;
        } else {
            f.send(ack).await?;
            tokio::time::sleep(Duration::from_millis(40)).await;
            assert_eq!(
                f.capacity.snapshot().active,
                1,
                "an acknowledgement alone cannot return the slot"
            );
            f.send(event.clone()).await?;
            f.settled(command, "complete").await?;
        }
        f.send(event).await?;
        assert_eq!(f.capacity.available(), 1);
        assert_eq!(
            f.journal.read_utterance(f.input.clone()).await?["text"],
            original
        );
        let state = f.journal.snapshot().await?;
        assert_eq!(state["workers"][0]["checkpointPending"], false);
        assert!(state["workers"][0]["activeTurn"].is_null());
        assert!(!f.task.is_finished());
    }
    f.close().await
}

#[tokio::test]
async fn a_queued_turn_that_started_before_reclaim_keeps_its_slot() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.start().await?;
    let start = f.read().await?;
    let command: Uuid = start["params"]["commandId"].as_str().unwrap().parse()?;
    let turn = Uuid::now_v7();
    f.send(json!({"id":start["id"],"result":{"commandId":command,"turnId":turn,"status":"accepted","startedNewTurn":false,"disposition":"queued"}})).await?;
    let cancel = f.read().await?;
    assert_eq!(cancel["method"], "turn/unqueue");
    f.send(json!({"method":"turn/started","params":{"sessionId":"session","commandId":command,"turnId":turn}})).await?;
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(f.capacity.snapshot().active, 1);
    f.send(json!({"id":cancel["id"],"error":{"code":-32030,"message":"Already active","data":{"kind":"commandRejected","retryable":false,"commandId":cancel["params"]["commandId"],"reason":"run_active"}}})).await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let state = f.journal.snapshot().await?;
            if state["workers"][0]["activeTurn"] == turn.to_string() {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            if f.task.is_finished() {
                return Err("The queued start was discarded after it had started.".into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(f.capacity.snapshot().active, 1);
    let capacity = f.capacity.clone();
    f.close().await?;
    assert_eq!(capacity.available(), 1);
    Ok(())
}

#[tokio::test]
async fn ten_completed_turns_return_slots_once_including_lost_reclaim_races() -> Result<()> {
    let mut f = Fixture::new().await?;
    for index in 0..10 {
        f.start().await?;
        let start = f.read().await?;
        let command: Uuid = start["params"]["commandId"].as_str().unwrap().parse()?;
        let turn = Uuid::now_v7();
        let disposition = if index % 2 == 0 { "started" } else { "queued" };
        f.send(json!({"id":start["id"],"result":{"commandId":command,"turnId":turn,"disposition":disposition}})).await?;
        if disposition == "queued" {
            let cancel = f.read().await?;
            assert_eq!(cancel["method"], "turn/unqueue");
            f.send(json!({"method":"turn/started","params":{"sessionId":"session","commandId":command,"turnId":turn}})).await?;
            f.send(json!({"id":cancel["id"],"error":{"code":-32030,"message":"Already active","data":{"kind":"commandRejected","commandId":cancel["params"]["commandId"],"reason":"run_active"}}})).await?;
        }
        assert_eq!(f.capacity.snapshot().active, 1);
        let completed = json!({"method":"turn/completed","params":{"sessionId":"session","turnId":turn,"terminal":"cancelled"}});
        f.send(completed.clone()).await?;
        f.settled(command, "complete").await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = f.journal.snapshot().await?;
                let worker = &snapshot["workers"][0];
                if worker["checkpointPending"] == false
                    && worker["checkpoint"]["turnId"] == turn.to_string()
                {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                }
                if f.task.is_finished() {
                    return Err("The completed turn did not save its checkpoint.".into());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        f.send(completed).await?;
        assert_eq!(f.capacity.snapshot().active, 0);
        assert_eq!(f.capacity.available(), 1);
    }
    f.close().await
}

#[tokio::test]
async fn a_steered_reply_keeps_the_sent_request_but_returns_only_the_extra_slot() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.capacity.set_limit(1, 2);
    let turn = Uuid::now_v7();
    let mut running = f
        .capacity
        .request("existing-run".into(), crate::scheduler::Priority::User)
        .acquire()
        .await;
    running.bind_turn(&f.track, turn)?;
    f.start().await?;
    let start = f.read().await?;
    assert_eq!(f.capacity.snapshot().active, 2);
    f.send(json!({"id":start["id"],"result":{"commandId":start["params"]["commandId"],"turnId":turn,"status":"accepted","startedNewTurn":false,"disposition":"steered"}})).await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = f.journal.snapshot().await?;
            if snapshot["workers"][0]["activeTurn"] == turn.to_string()
                && f.journal.read_utterance(f.input.clone()).await?["state"] == "sent"
            {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            if f.task.is_finished() {
                return Err("The steered request unexpectedly stopped its worker.".into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(f.capacity.snapshot().active, 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), f.read())
            .await
            .is_err()
    );
    let capacity = f.capacity.clone();
    f.close().await?;
    assert_eq!(
        capacity.snapshot().active,
        1,
        "the existing turn still owns its slot"
    );
    drop(running);
    assert_eq!(capacity.snapshot().active, 0);
    Ok(())
}
