use super::*;

#[test]
fn reopening_keeps_unresolved_progress_until_reconciliation_succeeds() {
    let mut previous = WorkerState::starting("track".into(), None);
    previous.phase = "failed".into();
    previous.session_id = Some("retained-session".into());
    previous.active_turn = Some(Uuid::now_v7());
    previous.checkpoint_pending = true;
    let reopened = WorkerState::starting("track".into(), Some(&previous));
    assert_eq!(reopened.session_id, previous.session_id);
    assert_eq!(reopened.active_turn, previous.active_turn);
    assert!(reopened.checkpoint_pending);
    assert_eq!(reopened.phase, "starting");
    assert!(reopened.identity.is_none());
}
async fn reconnect_fixture(
    text: &str,
) -> Result<(
    tempfile::TempDir,
    WorkerRunner,
    crate::execution::Port,
    Service,
    mpsc::UnboundedReceiver<String>,
    watch::Sender<bool>,
)> {
    use crate::decision::{Decision, Kind, Outcome, Target};
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), crate::secrets::Key::new([7; 32])).await?;
    journal
        .stage_utterance(Uuid::now_v7(), text.into(), Target::New, None, Vec::new())
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id,
            input.revision,
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
    let mut state = WorkerState::starting(track.clone(), None);
    state.session_id = Some("retained".into());
    state.active_turn = Some(Uuid::now_v7());
    state.checkpoint_pending = true;
    let (port, commands) = crate::execution::Port::new(std::sync::Arc::new(
        crate::dispatch_control::Gate::new(false),
    ));
    let (stop, stopping) = watch::channel(false);
    let (retry, retrying) = mpsc::unbounded_channel();
    let project_secrets = crate::project_secrets::Store::default();
    let service = Service {
        paused_ports: std::sync::Arc::default(),
        capacity: Arc::new(tokio::sync::Semaphore::new(1)),
        project_secrets: project_secrets.clone(),
        data: root.path().into(),
        rpc: root.path().join("rpc"),
        branches: Arc::new(tokio::sync::Mutex::new(())),
        recoveries: crate::pickup_run::Runs::default(),
        stopping: stopping.clone(),
        journal: journal.clone(),
        retry,
        ports: Arc::new(Mutex::new(HashMap::from([(track, port.clone())]))),
    };
    let runner = WorkerRunner {
        config: Config {
            source: root.path().join("absent"),
            binary: root.path().join("absent"),
            credentials: root.path().join("absent-auth"),
            path: "/usr/bin:/bin".into(),
        },
        data: root.path().into(),
        rpc: root.path().join("rpc"),
        journal,
        path: root.path().into(),
        state,
        stopping,
        execution: crate::execution::Context {
            project_secrets,
            port: port.clone(),
            commands,
            editor: crate::editor::Manager::new(root.path(), &root.path().join("editor.log"))?,
            capacity: Arc::new(tokio::sync::Semaphore::new(1)),
        },
        continuity: crate::execution::Continuity::default(),
        tracker: None,
        stable: false,
        environment: vec![],
        secret: None,
    };
    Ok((root, runner, port, service, retrying, stop))
}

#[tokio::test]
async fn global_resume_preserves_local_stops_before_or_during_the_pause() -> Result<()> {
    for local_stop in ["none", "before", "during"] {
        let (_root, _runner, port, service, _scheduled, _stop) =
            reconnect_fixture("Held replay").await?;
        if local_stop == "before" {
            port.hold(Uuid::now_v7())?;
        }
        service.interrupt_all(Uuid::now_v7())?;
        service.interrupt_all(Uuid::now_v7())?;
        if local_stop == "during" {
            port.hold(Uuid::now_v7())?;
        }
        service.resume_all();
        assert_eq!(port.locally_held(), local_stop != "none", "{local_stop}");
    }
    Ok(())
}

#[tokio::test]
async fn recovery_reserves_its_execution_slot_and_stop_interrupts_the_wait() -> Result<()> {
    for occupied in [false, true] {
        let (_root, mut runner, port, _service, _scheduled, _stop) =
            reconnect_fixture("Held replay").await?;
        runner.state.active_turn = None;
        runner.state.checkpoint_pending = false;
        let command = Uuid::now_v7();
        runner
            .journal
            .intent(
                &command.to_string(),
                "turn.start",
                json!({"commandId":command,"sessionId":"retained","input":[]}),
            )
            .await?;
        let capacity = runner.execution.capacity.clone();
        let occupied_slot = if occupied {
            Some(capacity.clone().acquire_owned().await?)
        } else {
            None
        };
        let task = tokio::spawn(async move {
            let result = runner.reserve_recovery_slot().await;
            (result, runner)
        });
        if occupied {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            assert!(!task.is_finished());
            port.stop(Uuid::now_v7())?;
        }
        let (result, mut runner) =
            tokio::time::timeout(std::time::Duration::from_secs(3), task).await??;
        assert_eq!(result?, !occupied);
        assert_eq!(runner.continuity.has_slot(), !occupied);
        assert_eq!(capacity.available_permits(), 0);
        assert!(runner.tracker.is_none());
        assert_eq!(
            runner.journal.operation(command).await?.unwrap()["state"],
            "prepared"
        );
        runner.continuity.release_slot();
        drop(occupied_slot);
        assert_eq!(capacity.available_permits(), 1);
    }
    Ok(())
}

#[tokio::test]
#[ignore = "offline SDK handshake oracle; set INTERCAT_HANDSHAKE_ORACLE to a prepared directory"]
async fn sdk_handshake_and_worker_opening_match() -> Result<()> {
    use std::os::unix::process::ExitStatusExt;
    let directory = PathBuf::from(std::env::var("INTERCAT_HANDSHAKE_ORACLE")?);
    let path = directory.join("fixture.json");
    let fixture: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let scripts = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tools/fake_msp_handshake.mjs")
        .canonicalize()?;
    std::fs::create_dir_all(directory.join("rust"))?;
    let mut results = Vec::new();
    for case in fixture["cases"].as_array().unwrap() {
        let transcript = directory
            .join("rust")
            .join(format!("{}.jsonl", case["id"].as_str().unwrap()));
        std::fs::write(&transcript, "")?;
        let mut child = tokio::process::Command::new("node")
            .arg(&scripts)
            .arg(&path)
            .arg(case["id"].as_str().unwrap())
            .arg(&transcript)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut connection =
            crate::msp::Connection::new(child.stdout.take().unwrap(), child.stdin.take().unwrap());
        let (_root, runner, _port, _service, _scheduled, _stop) =
            reconnect_fixture("Held handshake fixture").await?;
        let mut phase = "initialize";
        let attempt: Result<Value> = async {
            if case["kind"] == "handshake" {
                let initialized = connection.client.initialize().await?;
                let repeated = match connection.client.initialize().await {
                    Ok(_) => json!({"ok":true}),
                    Err(error) => json!({"ok":false,"error":error.to_string()}),
                };
                Ok(json!({"ok":true,"result":initialized,"repeated":repeated}))
            } else {
                phase = "opening";
                let opened = super::open_session(
                    &mut connection,
                    &runner.journal,
                    Path::new(fixture["workspace"].as_str().unwrap()),
                    None,
                    &runner.state.track_id,
                    None,
                    crate::reconcile::Expected::default(),
                )
                .await?;
                Ok(json!({"ok":true,"sessionId":opened.id}))
            }
        }
        .await;
        let outcome = match attempt {
            Ok(value) => value,
            Err(error) => json!({"ok":false,"phase":phase,"error":error.to_string()}),
        };
        connection.close();
        drop(connection);
        let exit = match tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await
        {
            Ok(exit) => exit?,
            Err(_) => {
                child.kill().await?;
                child.wait().await?
            }
        };
        let sent = std::fs::read_to_string(&transcript)?
            .lines()
            .map(serde_json::from_str::<Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        results.push(json!({"id":case["id"],"outcome":outcome,"sent":sent,"exit":{"code":exit.code(),"signal":exit.signal()}}));
    }
    std::fs::write(
        directory.join("rust.json"),
        serde_json::to_vec_pretty(&json!({"cases":results}))?,
    )?;
    let oracle: Value = serde_json::from_slice(&std::fs::read(directory.join("ts.json"))?)?;
    fn normalized(frames: &Value) -> Vec<Value> {
        frames
            .as_array()
            .unwrap()
            .iter()
            .map(|frame| {
                let mut frame = frame.clone();
                if frame["params"] == json!({}) {
                    frame.as_object_mut().unwrap().remove("params");
                }
                if frame["params"].get("commandId").is_some() {
                    frame["params"]["commandId"] = "command".into();
                }
                frame
            })
            .collect()
    }
    for (actual, expected) in results.iter().zip(oracle["cases"].as_array().unwrap()) {
        assert_eq!(actual["id"], expected["id"]);
        assert_eq!(
            actual["outcome"]["ok"], expected["outcome"]["ok"],
            "{}: {}",
            actual["id"], actual["outcome"]
        );
        let mut sent = normalized(&expected["sent"]);
        if actual["id"] == "unverified-fingerprint" {
            // The product deliberately refuses a binary outside its verified
            // fingerprint, where the general-purpose SDK only warns.
            sent.retain(|frame| frame["method"] != "initialized");
        }
        assert_eq!(normalized(&actual["sent"]), sent, "{}", actual["id"]);
        if actual["id"] == "initialize-once" {
            assert_eq!(actual["outcome"]["repeated"]["ok"], false);
            assert_eq!(expected["outcome"]["repeated"]["ok"], false);
        }
    }
    Ok(())
}
async fn wait_for_lost(journal: &Journal, database: &Path, expected: i64) -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(8), async {
        loop {
            let snapshot = journal.snapshot().await?;
            let db=rusqlite::Connection::open(database)?;
            let lost:i64=db.query_row("SELECT COUNT(*) FROM events WHERE kind='worker.state' AND json_extract(payload,'$.phase')='lost'",[],|row|row.get(0))?;
            if snapshot["workers"][0]["phase"] == "lost" && lost==expected {
                let attempts:i64=db.query_row("SELECT COUNT(*) FROM events WHERE kind='worker.state' AND json_extract(payload,'$.phase')='reconnecting'",[],|row|row.get(0))?;
                assert_eq!(attempts,3*expected);
                let starts:i64=db.query_row("SELECT COUNT(*) FROM operations WHERE kind='turn.start'",[],|row|row.get(0))?;
                assert_eq!(starts,0);
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;
    Ok(())
}
#[tokio::test]
async fn failed_reconnects_are_bounded_and_retry_reuses_the_existing_worker() -> Result<()> {
    let (root, mut runner, port, service, mut scheduled, _stop) =
        reconnect_fixture("waiting").await?;
    let journal = runner.journal.clone();
    let track = runner.state.track_id.clone();
    let task = tokio::spawn(async move {
        let result = runner.manage().await;
        (result, runner.state)
    });
    wait_for_lost(&journal, &root.path().join("db"), 1).await?;
    let id = Uuid::now_v7();
    service.retry(id, track).await?;
    service
        .retry(
            id,
            journal.snapshot().await?["tracks"][0]["id"]
                .as_str()
                .unwrap()
                .into(),
        )
        .await?;
    assert!(
        scheduled.try_recv().is_err(),
        "Retry must not create a second worker actor"
    );
    wait_for_lost(&journal, &root.path().join("db"), 2).await?;
    port.stop(Uuid::now_v7())?;
    let (result, state) = task.await?;
    result?;
    assert!(state.active_turn.is_none());
    assert!(!state.checkpoint_pending);
    assert_eq!(state.phase, "after-connection-loss");
    let snapshot = journal.snapshot().await?;
    assert_eq!(snapshot["workers"][0]["phase"], "after-connection-loss");
    assert_eq!(snapshot["utterances"][0]["state"], "held");
    assert!(
        journal
            .retry_worker(Uuid::now_v7(), state.track_id.clone())
            .await
            .is_err()
    );
    assert_eq!(state.detail.as_deref(), Some("stopped · connection lost"));
    Ok(())
}

#[tokio::test]
async fn a_restarted_daemon_stops_recorded_hosts_and_keeps_unfinished_work_held() -> Result<()> {
    use std::os::unix::{fs::PermissionsExt, process::CommandExt};
    let (root, runner, _port, service, _scheduled, _stop) =
        reconnect_fixture("Keep this unfinished request").await?;
    let mut state = runner.state.clone();
    drop(runner); // the old in-memory command receiver no longer exists
    let track = state.track_id.clone();
    service
        .journal
        .workspace_result(crate::preparation::Workspace {
            attachment_roots: Vec::new(),
            track_id: track.clone(),
            path: root.path().into(),
            reference: "refs/intercat/test".into(),
            base: "0".repeat(40),
            state: "ready".into(),
            error: None,
        })
        .await?;
    let host = root.path().join("hosts").join(&track).join("one");
    std::fs::create_dir_all(&host)?;
    std::fs::set_permissions(&host, std::fs::Permissions::from_mode(0o700))?;
    let nonce = Uuid::now_v7().to_string();
    let mut tracker = crate::process::Tracker::prepare(&host, root.path(), nonce.clone())?;
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("60")
        .env("INTERCAT_HOST", nonce)
        .process_group(0)
        .spawn()?;
    let identity = crate::process::identity(child.id())?;
    tracker.attach_process_group(identity)?;
    drop(tracker);
    state.identity = Some(identity);
    state.phase = "needs-recovery".into();
    service.journal.worker_state(state.clone()).await?;
    let ledger = host.join("processes.json");
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o644))?;
    let rejected = service.stop(Uuid::now_v7(), &track).await;
    assert!(rejected.is_err());
    assert!(crate::process::is_alive(identity));
    assert_eq!(
        service.journal.snapshot().await?["workers"][0]["phase"],
        "needs-recovery"
    );
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o600))?;
    // Reap independently: a zombie group is not yet confirmed absent.
    let reaper = tokio::task::spawn_blocking(move || child.wait());
    let id = Uuid::now_v7();
    let result = service.stop(id, &track).await;
    if crate::process::is_alive(identity) {
        unsafe {
            libc::kill(identity.pid as i32, libc::SIGKILL);
        }
    }
    reaper.await??;
    result?;
    service.stop(id, &track).await?;
    assert!(service.stop(id, "different-track").await.is_err());
    let snapshot = service.journal.snapshot().await?;
    assert_eq!(snapshot["workers"][0]["phase"], "after-connection-loss");
    assert_eq!(
        snapshot["workers"][0]["sessionId"],
        state.session_id.unwrap()
    );
    assert_eq!(snapshot["workers"][0]["activeTurn"], Value::Null);
    assert_eq!(snapshot["workers"][0]["checkpointPending"], false);
    assert_eq!(snapshot["utterances"][0]["state"], "held");
    assert!(!crate::process::is_alive(identity));
    assert!(
        service
            .journal
            .retry_worker(Uuid::now_v7(), track)
            .await
            .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn cancellation_interrupts_a_locked_recovery_and_retries_recorded_host_cleanup() -> Result<()>
{
    use crate::pickup::{CancelRequest, Change, Phase, Request};
    use std::os::unix::fs::PermissionsExt;
    let (root, runner, _port, service, mut scheduled, _stop) =
        reconnect_fixture("Keep this unfinished request").await?;
    let mut state = runner.state.clone();
    drop(runner);
    state.phase = "after-connection-loss".into();
    state.active_turn = None;
    state.checkpoint_pending = false;
    service.journal.worker_state(state.clone()).await?;
    service
        .journal
        .workspace_result(crate::preparation::Workspace {
            attachment_roots: Vec::new(),
            track_id: state.track_id.clone(),
            path: root.path().into(),
            reference: "refs/intercat/test".into(),
            base: "0".repeat(40),
            state: "ready".into(),
            error: None,
        })
        .await?;
    let request = Request {
        op_id: Uuid::now_v7(),
        track_id: state.track_id.clone(),
        session_id: state.session_id.clone().unwrap(),
    };
    service
        .journal
        .pickup(Change::Prepare(request.clone()))
        .await?;
    service
        .journal
        .pickup(Change::Stopped(request.op_id))
        .await?;
    service
        .journal
        .pickup(Change::Captured(
            request.op_id,
            crate::checkpoint::Checkpoint {
                turn_id: Uuid::new_v5(&request.op_id, b"recovery-snapshot"),
                sha: "1".repeat(40),
                reference: format!("refs/intercat/test-recovery/{}", request.op_id),
                unmerged_paths: vec![],
                live_processes: false,
            },
        ))
        .await?;
    let recovery = service
        .journal
        .pickup(Change::Published(request.op_id))
        .await?;
    service
        .journal
        .summary_plan(request.op_id, recovery.context.clone())
        .await?;
    let crate::handoff::Next::Task(task) = service.journal.summary_next(request.op_id).await?
    else {
        panic!("missing summary task")
    };
    let workspace = root
        .path()
        .join("summary-workspaces")
        .join(request.op_id.to_string());
    std::fs::create_dir_all(&workspace)?;
    let directory = root
        .path()
        .join("summary-hosts")
        .join(task.id.to_string())
        .join("host");
    std::fs::create_dir_all(&directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    let nonce = Uuid::now_v7().to_string();
    let mut tracker = crate::process::Tracker::prepare(&directory, &workspace, nonce.clone())?;
    let mut child = tokio::process::Command::new("/bin/sleep")
        .arg("60")
        .current_dir(&workspace)
        .env("INTERCAT_HOST", nonce)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let identity = crate::process::identity(child.id().unwrap())?;
    tracker.attach_process_group(identity)?;
    drop(tracker);
    let reaper = tokio::spawn(async move { child.wait().await });
    // A new-session host can have been left behind before activation, while
    // the durable worker still points at the old session.
    let new_host = root
        .path()
        .join("hosts")
        .join(&request.track_id)
        .join("new-session");
    std::fs::create_dir_all(&new_host)?;
    std::fs::set_permissions(&new_host, std::fs::Permissions::from_mode(0o700))?;
    let nonce = Uuid::now_v7().to_string();
    let mut tracker = crate::process::Tracker::prepare(&new_host, root.path(), nonce.clone())?;
    let mut new_child = tokio::process::Command::new("/bin/sleep")
        .arg("60")
        .current_dir(root.path())
        .env("INTERCAT_HOST", nonce)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let new_identity = crate::process::identity(new_child.id().unwrap())?;
    tracker.attach_process_group(new_identity)?;
    drop(tracker);
    let new_reaper = tokio::spawn(async move { new_child.wait().await });
    let mut unrelated = tokio::process::Command::new("/bin/sleep")
        .arg("60")
        .current_dir(&workspace)
        .kill_on_drop(true)
        .spawn()?;
    let unrelated_identity = crate::process::identity(unrelated.id().unwrap())?;
    let ledger = directory.join("processes.json");
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o644))?;
    let mut active = service.recoveries.begin(request.op_id)?;
    assert!(service.recoveries.begin(request.op_id).is_err());
    let lock = service.branches.clone().lock_owned().await;
    let journal = service.journal.clone();
    let operation = request.op_id;
    let preparing = tokio::spawn(async move {
        active.stopping.wait_for(|value| *value).await?;
        let saved = journal.operation(operation).await?.unwrap();
        assert_eq!(saved["payload"]["phase"], "cancelling");
        drop(lock);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let cancel = CancelRequest {
        op_id: Uuid::now_v7(),
        recovery_id: request.op_id,
        track_id: request.track_id.clone(),
        session_id: request.session_id.clone(),
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        service.cancel_pickup(cancel.clone()),
    )
    .await?;
    preparing.await??;
    assert!(result.is_err());
    new_reaper.await??;
    assert!(!crate::process::is_alive(new_identity));
    assert!(crate::process::is_alive(identity));
    assert_eq!(
        service
            .journal
            .pickup(Change::Prepare(request.clone()))
            .await?
            .phase,
        Phase::Cancelling
    );
    assert!(service.activate_pickup(request.op_id).await.is_err());
    std::fs::set_permissions(&ledger, std::fs::Permissions::from_mode(0o600))?;
    let done = service.cancel_pickup(cancel.clone()).await?;
    reaper.await??;
    assert_eq!(done["recovery"]["phase"], "cancelled");
    assert!(!crate::process::is_alive(identity));
    assert!(crate::process::is_alive(unrelated_identity));
    assert_eq!(
        service.cancel_pickup(cancel).await?["recovery"],
        done["recovery"]
    );
    assert!(scheduled.try_recv().is_err());
    let status = service.journal.snapshot().await?;
    assert_eq!(status["workers"][0]["sessionId"], request.session_id);
    assert_eq!(status["workers"][0]["phase"], "after-connection-loss");
    assert_eq!(status["utterances"][0]["state"], "held");
    assert_eq!(
        service
            .journal
            .pickup(Change::Prepare(request))
            .await?
            .snapshot
            .unwrap()
            .sha,
        "1".repeat(40)
    );
    unrelated.kill().await?;
    unrelated.wait().await?;
    if let Ok(output) = std::env::var("INTERCAT_CANCEL_OUTPUT") {
        let output = PathBuf::from(output).join(Uuid::now_v7().to_string());
        std::fs::create_dir_all(&output)?;
        let db = rusqlite::Connection::open(root.path().join("db"))?;
        db.execute(
            "VACUUM INTO ?1",
            [output.join("events.sqlite").to_str().unwrap()],
        )?;
        std::fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(&json!({
                "cancelled": true, "persistedBeforeUnlock": true, "damagedLedgerBlockedCompletion": true,
                "summaryHostStopped": true, "newSessionHostStopped": true, "unrelatedProcessPreserved": true,
                "sameCancellationRetried": true, "handoffDispatched": false, "paidTurns": 0,
            }))?,
        )?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires pinned Muse and saved authentication; no model turn"]
async fn real_idle_host_crash_reconnects_automatically_without_starting_a_turn() -> Result<()> {
    let (root, mut runner, _port, _service, _scheduled, stop) =
        reconnect_fixture("waiting").await?;
    let output =
        PathBuf::from(std::env::var("INTERCAT_RECONNECT_OUTPUT")?).join(Uuid::now_v7().to_string());
    std::fs::create_dir_all(&output)?;
    runner.config = Config {
        source: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        binary: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        credentials: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?),
        path: std::env::var("PATH")?,
    };
    runner.state.session_id = None;
    runner.state.active_turn = None;
    runner.state.checkpoint_pending = false;
    std::fs::create_dir(&runner.rpc)?;
    let journal = runner.journal.clone();
    let task = tokio::spawn(async move {
        let result = runner.manage().await;
        let cleanup = runner.stop_owned().await;
        (result, cleanup)
    });
    let verification=async {
        let first=tokio::time::timeout(std::time::Duration::from_secs(15),async {
            loop {let state=journal.snapshot().await?["workers"][0].clone();
                if state["phase"]=="ready" {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(state);}
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }).await??;
        let identity:crate::process::Identity=serde_json::from_value(first["identity"].clone())?;
        if crate::process::identity(identity.pid)?!=identity {return Err("Test host identity changed".into());}
        let began=std::time::Instant::now();
        // SAFETY: this is the exact PID and start time just read from our owned host.
        if unsafe{libc::kill(identity.pid as i32,libc::SIGKILL)}!=0{return Err(std::io::Error::last_os_error().into());}
        let recovered=tokio::time::timeout(std::time::Duration::from_secs(20),async {
            loop {let state=journal.snapshot().await?["workers"][0].clone();
                if state["phase"]=="ready"&&state["identity"]!=first["identity"] {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(state);}
                if state["phase"]=="lost"||state["phase"]=="failed" {return Err(format!("Recovery stopped: {}",state["detail"]).into());}
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }).await??;
        if recovered["sessionId"]!=first["sessionId"] {return Err("Recovery replaced the session".into());}
        let db=rusqlite::Connection::open(root.path().join("db"))?;
        let turns:i64=db.query_row("SELECT COUNT(*) FROM operations WHERE kind='turn.start'",[],|r|r.get(0))?;
        if turns!=0 {return Err("Idle recovery started a model turn".into());}
        std::fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"sameSession":true,"newHost":true,"recoveryMs":began.elapsed().as_millis(),"modelTurns":turns,"first":first,"recovered":recovered}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let _ = stop.send(true);
    let (finished, cleanup) = task.await?;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    if let Err(error) = &verification {
        std::fs::write(output.join("failure.txt"), error.to_string())?;
    }
    finished?;
    cleanup?;
    verification
}

fn native_git(path: &Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("/usr/bin/git")
        .current_dir(path)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Intercat fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()?;
    if !output.status.success() {
        return Err("Dummy Git setup failed".into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

#[tokio::test]
#[ignore = "one paid dummy Muse handoff; requires saved authentication and INTERCAT_PICKUP_OUTPUT"]
async fn real_file_pickup_preserves_edits_and_runs_one_handoff_in_a_new_session() -> Result<()> {
    let (root, mut old, _port, _unused, _scheduled, _stop) =
        reconnect_fixture("Do not change files. Reply PICKUP_OK.").await?;
    let output =
        PathBuf::from(std::env::var("INTERCAT_PICKUP_OUTPUT")?).join(Uuid::now_v7().to_string());
    std::fs::create_dir_all(&output)?;
    let config = Config {
        source: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        binary: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        credentials: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?),
        path: std::env::var("PATH")?,
    };
    let journal = old.journal.clone();
    let track = old.state.track_id.clone();
    let previous = old.state.session_id.clone().unwrap();
    let source = root.path().join("source");
    std::fs::create_dir(&source)?;
    native_git(&source, &["init", "-q", "-b", "main"])?;
    std::fs::write(source.join("base.txt"), "original\n")?;
    native_git(&source, &["add", "."])?;
    native_git(&source, &["commit", "-qm", "base"])?;
    let base = native_git(&source, &["rev-parse", "HEAD"])?;
    let path = root.path().join("worker");
    let inspected = crate::worktree::inspect(&source).await?;
    crate::worktree::GitTool::system()
        .await?
        .prepare_detached(&inspected, &path)
        .await?;
    let workspace = crate::preparation::Workspace {
        attachment_roots: crate::attachments::reserve(&path, &base)?,
        track_id: track.clone(),
        path: path.clone(),
        reference: "refs/intercat/test/A".into(),
        base: base.clone(),
        state: "ready".into(),
        error: None,
    };
    journal.workspace_result(workspace.clone()).await?;
    let checkpoint = crate::checkpoint::save(
        workspace.clone(),
        Uuid::now_v7(),
        None,
        false,
        journal.clone(),
    )
    .await?;
    old.state.phase = "lost".into();
    old.state.checkpoint = Some(checkpoint.clone());
    journal.worker_state(old.state.clone()).await?;
    let turn = old.state.active_turn.unwrap();
    let original = root.path().join("pickup-reference.txt");
    std::fs::write(&original, "REFERENCE_PICKUP_OK")?;
    let attachment = journal
        .import_attachment(Uuid::now_v7(), original.clone())
        .await?;
    std::fs::remove_file(original)?;
    let user_text = "Do not change any files. Read the attached reference file with read_file, then reply PICKUP_OK followed by the file's single line.";
    let word = journal
        .stage_utterance(
            Uuid::now_v7(),
            user_text.into(),
            crate::decision::Target::Lock(track.clone()),
            None,
            vec![attachment.id],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id,
            input.revision,
            crate::decision::Outcome::Classified(crate::decision::Decision {
                kind: crate::decision::Kind::Extend,
                track_id: Some(track.clone()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let references = journal
        .materialize_input_attachments(
            word["id"].as_str().unwrap().into(),
            track.clone(),
            previous.clone(),
            path.clone(),
        )
        .await?;
    let copy = references.files[0].path.clone();
    let command = Uuid::now_v7();
    journal
        .reserve_turn(
            command,
            word["id"].as_str().unwrap().into(),
            1,
            track.clone(),
            json!({"commandId":command,"sessionId":previous,"input":[]}),
        )
        .await?;
    journal.try_msp(track.clone(),previous.clone(),crate::clock::Stamp{epoch_ms:999,mono_ms:0,boot_id:"fixture".into(),timeline_ms:999},json!({"method":"turn/started","params":{"sessionId":previous,"turnId":turn,"commandId":command}}),None,None)?.await??;
    std::fs::remove_file(&copy)?;
    journal.try_msp(track.clone(),previous.clone(),crate::clock::Stamp{epoch_ms:1000,mono_ms:1,boot_id:"fixture".into(),timeline_ms:1000},json!({"method":"item/updated","params":{"sessionId":previous,"item":{"itemId":"old-user","turnId":turn,"kind":"userMessage","commandId":command,"revision":1,"status":"completed","content":[{"type":"text","text":user_text}]}}}),None,None)?.await??;
    std::fs::write(path.join("unfinished.txt"), "keep this unfinished edit\n")?;
    let index_path = PathBuf::from(native_git(&path, &["rev-parse", "--git-path", "index"])?);
    let index = std::fs::read(&index_path)?;
    let editor = native_editor(root.path()).await?;
    drop(old);
    journal
        .set_worker_config(Uuid::now_v7(), config.clone())
        .await?;
    let rpc = root.path().join("rpc");
    std::fs::create_dir(&rpc)?;
    let runtime = Runtime::start(
        root.path().into(),
        rpc.clone(),
        journal.clone(),
        editor.clone(),
        Arc::new(tokio::sync::Semaphore::new(1)),
        std::sync::Arc::new(crate::dispatch_control::Gate::new(false)),
    );
    let request = crate::pickup::Request {
        op_id: Uuid::now_v7(),
        track_id: track.clone(),
        session_id: previous.clone(),
    };
    let verification=async {
        let prepared=runtime.service.prepare_pickup(request.clone()).await?;
        let recovery:crate::pickup::Recovery=serde_json::from_value(prepared["recovery"].clone())?;
        assert_eq!(recovery.phase,crate::pickup::Phase::AwaitingReceipt);
        assert_eq!(std::fs::read_to_string(&copy)?,"REFERENCE_PICKUP_OK");
        std::fs::write(&copy,"ALTERED COPY")?;
        assert!(recovery.context["summary"]["changedFiles"].as_array().unwrap().iter().any(|path|path.as_str().is_some_and(|path|path.contains("unfinished.txt"))));
        assert_ne!(recovery.session_id,previous);
        // Reconstruct the durable crash window after session/start was recorded
        // complete but before the inherited fingerprints/next phase were saved.
        // This fixture has no fingerprints, so only the catalog revision changes.
        {
            let db=rusqlite::Connection::open(root.path().join("db"))?;
            db.execute_batch("BEGIN IMMEDIATE")?;
            db.execute("UPDATE operations SET payload=json_set(payload,'$.phase','starting') WHERE id=?1",[request.op_id.to_string()])?;
            db.execute("UPDATE secret_catalog SET revision=0 WHERE singleton=1",[])?;
            db.execute_batch("COMMIT")?;
        }
        let again=runtime.service.prepare_pickup(request.clone()).await?;
        assert_eq!(again["recovery"]["sessionId"],recovery.session_id);
        assert_eq!(native_git(&path,&["show",&format!("{}:unfinished.txt",recovery.snapshot.as_ref().unwrap().sha)])?,"keep this unfinished edit");
        assert_eq!(std::fs::read(&index_path)?,index);
        assert_eq!(native_git(&path,&["rev-parse",&format!("{}-checkpoint",workspace.reference)])?,checkpoint.sha);
        // Replaying the same session/start on another real host must keep its identity.
        let mut replay=host::Host::spawn(&config.binary,&root.path().join("replay-host"),&path,config.path.as_ref(),Some(&config.credentials),host::Boundary{profile: crate::host::Profile::Worker,
secrets:&[],rpc:&rpc,denied:&[]}).await?;
        replay.connection.client.initialize().await?;
        let response=replay.connection.client.request("session/start",json!({"commandId":recovery.start_command,"sessionId":recovery.session_id,"workspaceRoot":path,"approvalMode":"promptUnmatched","providerId":"meta","modelId":"muse-spark-1.3-contributor","recoveryMode":"on"})).await;
        if response.is_ok() {crate::pickup_run::release_new_session(&mut replay,&recovery.session_id).await?;}
        replay.shutdown().await?;
        assert_eq!(response?["session"]["sessionId"],recovery.session_id);
        assert!(runtime.service.activate_pickup(request.op_id).await.is_err());
        journal.confirm_fingerprints(journal.fingerprint_catalog().await?.current).await?;
        runtime.service.activate_pickup(request.op_id).await?;
        runtime.service.activate_pickup(request.op_id).await?;
        let result=tokio::time::timeout(std::time::Duration::from_secs(90),async {
            loop {
                let snapshot=journal.snapshot().await?;let worker=&snapshot["workers"][0];
                if ["failed","lost","needs-recovery"].contains(&worker["phase"].as_str().unwrap_or("")) {return Err(format!("Recovered worker failed: {}",worker["detail"]).into());}
                if worker["checkpoint"]["sha"]!=checkpoint.sha && worker["checkpoint"]["sha"].is_string() && worker["activeTurn"].is_null() && worker["checkpointPending"]==false {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(worker.clone());}
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }).await??;
        assert!(result["output"].as_str().unwrap_or("").contains("PICKUP_OK"));
        assert_eq!(result["sessionId"],recovery.session_id);
        assert!(result["output"].as_str().unwrap_or("").contains("REFERENCE_PICKUP_OK"));
        assert_eq!(std::fs::read_to_string(&copy)?,"REFERENCE_PICKUP_OK");
        assert_eq!(std::fs::read_to_string(path.join("unfinished.txt"))?,"keep this unfinished edit\n");
        assert_eq!(std::fs::read_to_string(source.join("base.txt"))?,"original\n");
        assert!(!source.join("unfinished.txt").exists());
        let db=rusqlite::Connection::open(root.path().join("db"))?;
        let turns:i64=db.query_row("SELECT COUNT(*) FROM operations WHERE kind='turn.start' AND json_extract(payload,'$.sessionId')=?1",[&recovery.session_id],|r|r.get(0))?;assert_eq!(turns,1);
        let summaries:i64=db.query_row("SELECT COUNT(*) FROM operations WHERE kind='handoff.summary' AND state='complete'",[],|r|r.get(0))?;assert_eq!(summaries,1);
        assert_eq!(db.query_row("SELECT COUNT(*) FROM utterances WHERE state='held'",[],|r|r.get::<_,i64>(0))?,1);
        std::fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"snapshot":recovery.snapshot,"newSession":recovery.session_id,"previousSession":previous,"modelTurns":turns,"summaryTurns":summaries,"indexUnchanged":true,"sourceUnchanged":true,"unfinishedEditPreserved":true,"worker":result,"schema":crate::journal::SCHEMA_VERSION,"attachment":attachment,"attachmentRestored":true}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    runtime.shutdown().await;
    editor.stop().await;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    if let Err(error) = &verification {
        std::fs::write(output.join("failure.txt"), error.to_string())?;
    }
    verification
}
async fn native_editor(root: &Path) -> Result<crate::editor::Manager> {
    let source = root.join("editor/intercat_editor");
    std::fs::create_dir_all(&source)?;
    std::fs::write(source.join("__init__.py"), "")?;
    std::fs::write(
        source.join("server.py"),
        r#"import json
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_GET(self):
  body=json.dumps({'accepting':True,'modelLoaded':True}).encode();self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_POST(self):
  self.rfile.read(int(self.headers['Content-Length']));body=json.dumps({'cachePrefix':False,'stages':{'decision':{'selected':' A'}},'inferenceMs':1}).encode();self.send_response(200);self.send_header('X-Intercat-Inference','started');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True)
server.serve_forever()
"#,
    )?;
    let model = root.join("dummy-model");
    std::fs::create_dir(&model)?;
    let editor = crate::editor::Manager::new(&root.join("editor"), &root.join("editor.log"))?;
    editor
        .configure(crate::editor::Config {
            python: "/usr/bin/python3".into(),
            model,
        })
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !editor.status().model_loaded {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await?;
    Ok(editor)
}

#[tokio::test]
#[ignore = "offline echo: native history-budget recovery, worker execution and checkpoint"]
async fn real_budget_omitted_history_recovers_and_runs_the_next_input_once() -> Result<()> {
    let (root, mut runner, port, service, _scheduled, stop) =
        reconnect_fixture("BUDGET_RECOVERY_NEXT_INPUT").await?;
    let output = PathBuf::from(std::env::var("INTERCAT_BUDGET_RUNTIME_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let binary = PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?).canonicalize()?;
    assert_eq!(host::hash(&binary)?, host::BINARY_SHA256);
    let credentials = root.path().join("offline-auth.json");
    std::fs::write(&credentials, r#"{"schema_version":1}"#)?;
    runner.config = Config {
        source: binary.clone(),
        binary: binary.clone(),
        credentials: credentials.clone(),
        path: std::env::var("PATH")?,
    };
    runner.state.active_turn = None;
    runner.state.checkpoint_pending = false;
    let source = root.path().join("source");
    std::fs::create_dir(&source)?;
    native_git(&source, &["init", "-q", "-b", "main"])?;
    std::fs::write(
        source.join("README.txt"),
        "Disposable offline worker recovery test.\n",
    )?;
    native_git(&source, &["add", "."])?;
    native_git(&source, &["commit", "-qm", "base"])?;
    let journal = runner.journal.clone();
    let track = runner.state.track_id.clone();
    journal
        .configure_project(
            Uuid::now_v7(),
            crate::project::inspect(source.clone()).await?,
        )
        .await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let project_id = journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    let key = crate::secrets::Key::new([7; 32]);
    let catalog = journal
        .prepare_project_secrets(crate::project_secrets::Prepare {
            op_id: Uuid::now_v7(),
            project_id: project_id.clone(),
            revision: 0,
            fingerprints: vec![],
            key_check: key.check().iter().map(|b| format!("{b:02x}")).collect(),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    service
        .activate_project_secrets(crate::project_secrets::Load {
            project_id,
            revision: 0,
            entries: vec![],
        })
        .await?;
    let preparation =
        crate::preparation::Preparation::start(&root.path().join("workspaces"), journal.clone())?;
    let workspace =
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                let snapshot = journal.snapshot().await?;
                if let Some(value) = snapshot["workspaces"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|w| w["trackId"] == track && w["state"] == "ready")
                {
                    return Ok::<
                        crate::preparation::Workspace,
                        Box<dyn std::error::Error + Send + Sync>,
                    >(serde_json::from_value(value.clone())?);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await??;
    preparation.shutdown().await;
    runner.path = workspace.path.clone();
    std::fs::create_dir(&runner.rpc)?;
    let mut seed = host::OFFLINE_ECHO
        .scope(
            (),
            Host::spawn(
                &binary,
                &root.path().join("seed-host"),
                &workspace.path,
                runner.config.path.as_ref(),
                Some(&credentials),
                host::Boundary {
                    profile: host::Profile::Worker,
                    secrets: &[],
                    rpc: &runner.rpc,
                    denied: &[std::env::current_dir()?],
                },
            ),
        )
        .await?;
    async fn receive(
        host: &mut Host,
        method: &str,
        pending: crate::msp::PendingRequest,
        frames: &mut Vec<Value>,
    ) -> Result<Value> {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                match host.connection.next_event().await {
                    Some(crate::msp::Event::Frame(received)) => {
                        let frame = received.frame;
                        if frame["id"].as_u64() == Some(pending.id) && frame.get("method").is_none()
                        {
                            if let Some(error) = frame.get("error") {
                                return Err(format!("Native {method} failed: {error}").into());
                            }
                            return Ok(frame["result"].clone());
                        }
                        frames.push(frame);
                    }
                    Some(crate::msp::Event::Closed(reason)) => return Err(reason.into()),
                    None => return Err("Native echo host closed".into()),
                }
            }
        })
        .await?
    }
    let seeded=async {
        eprintln!("Opening offline seed session");
        seed.connection.client.initialize().await?;
        let mut frames=vec![];
        let pending = seed.connection.client.query("session/start", json!({"commandId":Uuid::now_v7(),"workspaceRoot":workspace.path,"providerId":"echo","approvalMode":"promptUnmatched","recoveryMode":"on"})).await?;
        let started = receive(&mut seed, "session/start", pending, &mut frames).await?;
        assert_eq!(started["session"]["providerId"],"echo","Never submit this fixture to a paid provider");
        let session=started["session"]["sessionId"].as_str().ok_or("seed session")?.to_owned();
        let mut seed_count=0;
        let mut budget_read=None;
        for index in 0..96 {
            let command=Uuid::now_v7();
            let params=json!({"commandId":command,"sessionId":session,"input":[{"type":"text","text":format!("offline seed {index}\n{}","x".repeat(65480))}],"displayText":"d".repeat(65500)});
            let prepared = journal.screen_start(params).await?;
            journal.intent(&command.to_string(), "turn.start", prepared.params().clone()).await?;
            let pending = seed.connection.client.query_start(
                prepared,
                crate::msp::StartGuard::Stopped(Arc::new(std::sync::atomic::AtomicBool::new(false))),
            ).await?;
            let ack = receive(&mut seed, "turn/start", pending, &mut frames).await?;
            journal.finish(&command.to_string(),"dispatched").await?;
            tokio::time::timeout(std::time::Duration::from_secs(30),async {
                while !frames.iter().any(|f|f["method"]=="turn/completed"&&f["params"]["turnId"]==ack["turnId"]) {
                    match seed.connection.next_event().await {
                        Some(crate::msp::Event::Frame(received))=>frames.push(received.frame),
                        _=>return Err("Seed turn did not complete".into()),
                    }
                }
                Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
            }).await??;
            let terminal=frames.iter().find(|f|f["method"]=="turn/completed"&&f["params"]["turnId"]==ack["turnId"]).unwrap();
            if terminal["params"]["terminal"]!="completed" {
                return Err(format!("Offline seed failed: {}",terminal["params"]).into());
            }
            if index==0 {
                for frame in &frames {
                    journal.try_msp(track.clone(),session.clone(),crate::clock::Stamp::now(),crate::recording::sanitize(frame.clone(),&[]),None,None)?.await??;
                }
            }
            journal.finish(&command.to_string(),"complete").await?;
            frames.clear();
            seed_count=index+1;
            if seed_count%16==0 {eprintln!("Primed {seed_count} offline turns");}
            if seed_count>=64 && seed_count%16==0 {
                let pending = seed.connection.client.query("session/resume", json!({"commandId":Uuid::now_v7(),"sessionId":session,"excludeItems":false,"history":"snapshot"})).await?;
                let resumed = receive(&mut seed, "session/resume", pending, &mut frames).await?;
                if resumed["history"]["mode"]=="none" {budget_read=Some(resumed);break;}
            }
        }
        let resumed=budget_read.ok_or("Fixture did not reach the native history budget")?;
        std::fs::write(output.join("seed-resume.json"),serde_json::to_vec_pretty(&resumed)?)?;
        assert_eq!(resumed["history"]["mode"],"none");
        assert_eq!(resumed["history"]["noneReason"],"historyBudget");
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>((session,resumed["viewCursor"].clone(),seed_count))
    }.await;
    seed.shutdown().await?;
    if let Err(error) = &seeded {
        std::fs::write(output.join("seed-failure.txt"), error.to_string())?;
    }
    let (session, seed_head, seed_count) = seeded?;
    runner.state.session_id = Some(session.clone());
    journal.worker_state(runner.state.clone()).await?;
    let before = journal
        .reconcile_local(session.clone())
        .await?
        .view
        .items
        .len();
    let editor = native_editor(root.path()).await?;
    runner.execution.editor = editor.clone();
    let task = tokio::spawn(host::OFFLINE_ECHO.scope((), async move {
        let result = runner.manage().await;
        let cleanup = runner.stop_owned().await;
        (result, cleanup, runner.state)
    }));
    let verification=async {
        let restored=tokio::time::timeout(std::time::Duration::from_secs(60),async {
            loop {
                let state=journal.snapshot().await?["workers"][0].clone();
                if state["phase"]=="ready" && port.output_flow().is_some() {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(state);}
                if ["failed","lost","needs-recovery"].contains(&state["phase"].as_str().unwrap_or("")) {return Err(format!("Recovery stopped: {}",state["detail"]).into());}
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }).await??;
        assert_eq!(restored["sessionId"],session);
        assert!(restored["activeTurn"].is_null());
        let recovered=journal.reconcile_local(session.clone()).await?;
        assert!(recovered.view.items.len()>before);
        let db=rusqlite::Connection::open(root.path().join("db"))?;
        let raw:String=db.query_row("SELECT payload FROM events WHERE kind='worker.session' ORDER BY seq DESC LIMIT 1",[],|r|r.get(0))?;
        let runtime_open:Value=serde_json::from_str(&raw)?;
        assert_eq!(runtime_open["result"]["session"]["providerId"],"echo");
        assert_eq!(runtime_open["result"]["history"]["mode"],"none");
        assert_eq!(runtime_open["result"]["history"]["noneReason"],"historyBudget");
        assert_eq!(journal.view_cursor(track.clone(),session.clone()).await?.map(Value::String),Some(runtime_open["result"]["viewCursor"].clone()));
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'",[],|r|r.get::<_,i64>(0))?,seed_count as i64);
        port.start(Uuid::now_v7()).await?;
        let completed=branch_checkpoint(&journal,&track,None).await?;
        std::fs::write(output.join("observed.json"),serde_json::to_vec_pretty(&journal.snapshot().await?)?)?;
        assert_eq!(completed.session_id.as_deref(),Some(session.as_str()));
        if !completed.output.as_deref().unwrap_or("").contains("BUDGET_RECOVERY_NEXT_INPUT") {
            return Err("Next input output did not contain the expected marker; see observed.json".into());
        }
        let final_view=journal.reconcile_local(session.clone()).await?.view;
        let checkpoint_turn=completed.checkpoint.as_ref().ok_or("Next input has no checkpoint")?.turn_id;
        assert_eq!(final_view.turns[&checkpoint_turn].terminal.as_deref(),Some("completed"));
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'",[],|r|r.get::<_,i64>(0))?,seed_count as i64+1);
        assert_eq!(native_git(&source,&["status","--porcelain"])? ,"");
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(json!({"provider":"echo","paidTurns":0,"seedTurns":seed_count,"seedHead":seed_head,"runtimeResume":runtime_open["result"],"nextInputs":1,"sameSession":true,"restoredItems":recovered.view.items.len(),"beforeItems":before,"worker":completed,"sourceUnchanged":true,"schema":crate::journal::SCHEMA_VERSION}))
    }.await;
    let _ = stop.send(true);
    let (finished, cleanup, _) = task.await?;
    editor.stop().await;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    if let Err(error) = &verification {
        std::fs::write(output.join("failure.txt"), error.to_string())?;
    }
    finished?;
    cleanup?;
    let mut result = verification?;
    let projection = |view: crate::session_view::SessionView| {
        json!({
            "items":view.items,"turns":view.ordered_turns(),"activeTurnId":view.active_turn,
            "sessionState":view.session_state,"pendingApprovals":view.pending_approvals,
            "resolvedApprovals":view.resolved_approvals,"pendingUserInputs":view.pending_user_inputs,
            "settledUserInputs":view.settled_user_inputs
        })
    };
    let expected = projection(journal.reconcile_local(session.clone()).await?.view);
    let reopened = Journal::open(
        output.join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    let actual = projection(reopened.reconcile_local(session).await?.view);
    assert_eq!(
        actual, expected,
        "Saved history changed when reopening the journal"
    );
    result["restartMatches"] = true.into();
    result["persistedItems"] = actual["items"].as_array().unwrap().len().into();
    std::fs::write(
        output.join("reopened-view.json"),
        serde_json::to_vec_pretty(&actual)?,
    )?;
    std::fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    Ok(())
}

#[tokio::test]
#[ignore = "one paid disposable Muse turn; adds a local dependency and commits its checked result"]
async fn real_local_dependency_reaches_a_checked_commit() -> Result<()> {
    use crate::decision::{Decision, Kind, Outcome, Target};
    use crate::integration::{Action, Command, Status};
    let (root, mut runner, _port, service, _scheduled, stop) =
        reconnect_fixture("Held setup request; do not run.").await?;
    let output = PathBuf::from(std::env::var("INTERCAT_LOCAL_DEPENDENCIES_NATIVE")?)
        .join(Uuid::now_v7().to_string());
    std::fs::create_dir_all(&output)?;
    runner.config = Config {
        source: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
        binary: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
        credentials: std::env::var("INTERCAT_TEST_MUSE_AUTH")?.into(),
        path: std::env::var("PATH")?,
    };
    runner.state.session_id = None;
    runner.state.active_turn = None;
    runner.state.checkpoint_pending = false;
    let source = root.path().join("source");
    std::fs::create_dir_all(source.join("vendor/tiny-dep"))?;
    native_git(&source, &["init", "-q", "-b", "main"])?;
    std::fs::write(source.join(".gitignore"), "node_modules/\n")?;
    std::fs::write(
        source.join("vendor/tiny-dep/package.json"),
        r#"{"name":"tiny-dep","version":"1.0.0","main":"index.js"}"#,
    )?;
    std::fs::write(
        source.join("vendor/tiny-dep/index.js"),
        "module.exports = 42;\n",
    )?;
    std::fs::write(
        source.join("package.json"),
        r#"{"name":"local-fixture","version":"1.0.0","scripts":{"test":"node --test"}}"#,
    )?;
    std::fs::write(
        source.join("package-lock.json"),
        r#"{"name":"local-fixture","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"local-fixture","version":"1.0.0"}}}"#,
    )?;
    std::fs::write(source.join("login.cjs"), "module.exports = () => 0;\n")?;
    std::fs::write(
        source.join("login.test.cjs"),
        "const test=require('node:test'); const assert=require('node:assert/strict'); test('login',()=>assert.equal(require('./login.cjs')(),0));\n",
    )?;
    native_git(&source, &["add", "."])?;
    native_git(&source, &["commit", "-qm", "base"])?;
    let index = std::fs::read(source.join(".git/index"))?;
    let journal = runner.journal.clone();
    let track = runner.state.track_id.clone();
    journal
        .configure_project(
            Uuid::now_v7(),
            crate::project::inspect(source.clone()).await?,
        )
        .await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let project_id = journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    let key = crate::secrets::Key::new([7; 32]);
    let catalog = journal
        .prepare_project_secrets(crate::project_secrets::Prepare {
            op_id: Uuid::now_v7(),
            project_id: project_id.clone(),
            revision: 0,
            fingerprints: vec![],
            key_check: key.check().iter().map(|b| format!("{b:02x}")).collect(),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    service
        .activate_project_secrets(crate::project_secrets::Load {
            project_id,
            revision: 0,
            entries: vec![],
        })
        .await?;
    let preparation =
        crate::preparation::Preparation::start(&root.path().join("workspaces"), journal.clone())?;
    let workspace =
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                let snapshot = journal.snapshot().await?;
                if let Some(value) = snapshot["workspaces"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|w| w["trackId"] == track && w["state"] == "ready")
                {
                    return Ok::<
                        crate::preparation::Workspace,
                        Box<dyn std::error::Error + Send + Sync>,
                    >(serde_json::from_value(value.clone())?);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await??;
    preparation.shutdown().await;
    runner.path = workspace.path.clone();
    std::fs::create_dir(&runner.rpc)?;
    let lock = json!({"name":"local-fixture","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"local-fixture","version":"1.0.0","dependencies":{"tiny-dep":"file:vendor/tiny-dep"}},"node_modules/tiny-dep":{"resolved":"vendor/tiny-dep","link":true},"vendor/tiny-dep":{"version":"1.0.0"}}});
    let prompt = format!(
        "This is a disposable test fixture. Add tiny-dep from file:vendor/tiny-dep to package.json dependencies. Set login.cjs to module.exports = () => require('tiny-dep'); and update login.test.cjs to expect 42. Replace package-lock.json with this exact JSON: {lock}. Use file editing tools only. Do not run shell commands, access the network, alter vendor, or change any other file. Finish with a brief report. The application will install dependencies and run the tests after your turn."
    );
    journal
        .stage_utterance(
            Uuid::now_v7(),
            prompt,
            Target::Lock(track.clone()),
            Some(track.clone()),
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(track.clone()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let editor = native_editor(root.path()).await?;
    runner.execution.editor = editor.clone();
    let task = tokio::spawn(async move {
        let result = runner.manage().await;
        let cleanup = runner.stop_owned().await;
        (result, cleanup)
    });
    let worker = branch_checkpoint(&journal, &track, None).await;
    let _ = stop.send(true);
    let (finished, cleanup) = task.await?;
    editor.stop().await;
    let verification=async {
        finished?;cleanup?;let worker=worker?;
        let integration=crate::integration::Service::new(journal.clone(),root.path().join("integration"));
        let opened=integration.command(Command{op_id:Uuid::now_v7(),action:Action::Open{tracks:None}}).await?;
        integration.command(Command{op_id:Uuid::now_v7(),action:Action::Prepare{id:opened.id,revision:opened.revision}}).await?;
        let built=tokio::time::timeout(std::time::Duration::from_secs(60),async {
            loop {
                let job:crate::integration::Job=serde_json::from_value(journal.snapshot().await?["integration"].clone())?;
                if !matches!(job.status,Status::Building|Status::Staging) {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(job);}
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }).await??;
        assert_eq!(built.status,Status::Checked,"{:?}",built.error);
        let candidate=built.candidate.as_ref().unwrap();assert_eq!(candidate.check.as_ref().unwrap().code,Some(0));
        integration.command(Command{op_id:Uuid::now_v7(),action:Action::Commit{id:built.id,revision:built.revision,head:candidate.head.clone(),tree:candidate.tree.clone(),settings_hash:candidate.settings_hash.clone()}}).await?;
        let done=tokio::time::timeout(std::time::Duration::from_secs(30),async {
            loop {
                let job:crate::integration::Job=serde_json::from_value(journal.snapshot().await?["integration"].clone())?;
                if job.status!=Status::Committing{return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(job);}
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }).await??;
        assert_eq!(done.status,Status::Done,"{:?}",done.error);
        assert_eq!(std::fs::read(source.join(".git/index"))?,index);
        assert!(native_git(&source,&["status","--porcelain"])?.is_empty());
        assert!(!native_git(&source,&["ls-tree","-r","--name-only",&candidate.head])?.contains("node_modules/"));
        std::fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"worker":worker,"built":built,"done":done,"sourceUnchanged":true,"paidTurns":1}))?)?;
        integration.shutdown().await;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let turns: i64 = db.query_row(
        "SELECT count(*) FROM operations WHERE kind='turn.start'",
        [],
        |r| r.get(0),
    )?;
    std::fs::write(
        output.join("attempt.json"),
        serde_json::to_vec_pretty(&json!({"attemptedModelTurns":turns}))?,
    )?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    verification
}

#[tokio::test]
#[ignore = "one paid dummy Muse turn; verifies an attached private original reaches the worker"]
async fn real_attachment_original_is_copied_and_read_without_entering_the_checkpoint() -> Result<()>
{
    use crate::decision::{Decision, Kind, Outcome, Target};
    let (root, mut runner, _port, service, _scheduled, stop) =
        reconnect_fixture("Held setup request; do not run.").await?;
    let output = PathBuf::from(std::env::var("INTERCAT_ATTACHMENT_OUTPUT")?)
        .join(Uuid::now_v7().to_string());
    std::fs::create_dir_all(&output)?;
    runner.config = Config {
        source: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
        binary: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
        credentials: std::env::var("INTERCAT_TEST_MUSE_AUTH")?.into(),
        path: std::env::var("PATH")?,
    };
    runner.state.session_id = None;
    runner.state.active_turn = None;
    runner.state.checkpoint_pending = false;
    let source = root.path().join("source");
    std::fs::create_dir(&source)?;
    native_git(&source, &["init", "-q", "-b", "main"])?;
    std::fs::write(source.join("README.txt"), "Disposable attachment test.\n")?;
    native_git(&source, &["add", "."])?;
    native_git(&source, &["commit", "-qm", "base"])?;
    let project = crate::project::inspect(source.clone()).await?;
    let journal = runner.journal.clone();
    let track = runner.state.track_id.clone();
    journal
        .configure_project(Uuid::now_v7(), project.clone())
        .await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let project_id = journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    let key = crate::secrets::Key::new([7; 32]);
    let key_check = key.check().iter().map(|b| format!("{b:02x}")).collect();
    let catalog = journal
        .prepare_project_secrets(crate::project_secrets::Prepare {
            op_id: Uuid::now_v7(),
            project_id: project_id.clone(),
            revision: 0,
            fingerprints: vec![],
            key_check,
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    service
        .activate_project_secrets(crate::project_secrets::Load {
            project_id,
            revision: 0,
            entries: vec![],
        })
        .await?;
    let path = root.path().join("worker");
    let inspected = crate::worktree::inspect(&source).await?;
    crate::worktree::GitTool::system()
        .await?
        .prepare_detached(&inspected, &path)
        .await?;
    let workspace = crate::preparation::Workspace {
        attachment_roots: crate::attachments::reserve(&path, &project.base)?,
        track_id: track.clone(),
        path: path.clone(),
        reference: "refs/intercat/attachment/A".into(),
        base: project.base.clone(),
        state: "ready".into(),
        error: None,
    };
    journal.workspace_result(workspace.clone()).await?;
    runner.path = path.clone();
    std::fs::create_dir(&runner.rpc)?;
    let reference = root.path().join("reference.txt");
    let expected = "REFERENCE_ORIGINAL_OK_73ce";
    std::fs::write(&reference, format!("{expected}\n"))?;
    let attachment = journal
        .import_attachment(Uuid::now_v7(), reference.clone())
        .await?;
    std::fs::remove_file(&reference)?;
    let word=journal.stage_utterance(Uuid::now_v7(),"Use only the read_file tool to read the attached reference file. Report its single line exactly. Do not change files, run shell commands, access the web, or ask a question.".into(),Target::Lock(track.clone()),Some(track.clone()),vec![attachment.id]).await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(track.clone()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let editor = native_editor(root.path()).await?;
    runner.execution.editor = editor.clone();
    std::fs::write(
        output.join("attempt.json"),
        serde_json::to_vec(&json!({"maximumDummyTurns":1,"attachment":attachment}))?,
    )?;
    let task = tokio::spawn(async move {
        let result = runner.manage().await;
        let cleanup = runner.stop_owned().await;
        (result, cleanup)
    });
    let verification=async {
        let completed=branch_checkpoint(&journal,&track,None).await?;
        let checkpoint=completed.checkpoint.as_ref().unwrap();
        let tree=native_git(&path,&["ls-tree","-r","--name-only",&checkpoint.sha])?;
        if tree.contains("attachments") {return Err("The reference entered the checkpoint.".into());}
        let db=rusqlite::Connection::open(root.path().join("db"))?;
        let starts:i64=db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'",[],|r|r.get(0))?;
        let prompt:String=db.query_row("SELECT payload FROM operations WHERE kind='turn.start'",[],|r|r.get(0))?;
        let records:Vec<String>=db.prepare("SELECT payload FROM events WHERE session_id=?1")?.query_map([completed.session_id.as_ref().unwrap()],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        if starts!=1 || !prompt.contains(&attachment.id.to_string()) || records.join("\n").matches(expected).count()==0 {return Err("The worker did not read the verified attachment in one turn.".into());}
        if !native_git(&source,&["status","--porcelain"])?.is_empty() {return Err("The source checkout changed.".into());}
        std::fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"schema":45,"paidTurns":starts,"attachment":attachment,"inputId":word["id"],"readVerifiedOriginal":true,"checkpointExcluded":true,"sourceUnchanged":true,"completed":completed}))?)?;
        db.execute("VACUUM INTO ?1", [output.join("events.sqlite").to_str().unwrap()])?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let _ = stop.send(true);
    let (finished, cleanup) = task.await?;
    editor.stop().await;
    verification?;
    finished?;
    cleanup
}
#[tokio::test]
#[ignore = "one paid dummy Muse turn; kills owned host at approval and verifies automatic checkpoint recovery"]
async fn real_active_host_crash_recovers_without_replaying_its_turn_or_approval() -> Result<()> {
    let (root,mut runner,port,_service,_scheduled,stop)=reconnect_fixture("This is a disposable local test repository. Use the bash tool to run exactly `git push origin main`. Do not inspect or change any files. Wait for the approval result. Do not try another command or tool.").await?;
    let output =
        PathBuf::from(std::env::var("INTERCAT_RECONNECT_OUTPUT")?).join(Uuid::now_v7().to_string());
    std::fs::create_dir_all(&output)?;
    runner.config = Config {
        source: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        binary: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        credentials: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?),
        path: std::env::var("PATH")?,
    };
    runner.state.session_id = None;
    runner.state.active_turn = None;
    runner.state.checkpoint_pending = false;
    let project = root.path().join("project");
    std::fs::create_dir(&project)?;
    native_git(&project, &["init", "-q", "-b", "main"])?;
    std::fs::write(project.join("README.txt"), "Disposable recovery fixture.\n")?;
    native_git(&project, &["add", "README.txt"])?;
    native_git(&project, &["commit", "-qm", "fixture"])?;
    let base = native_git(&project, &["rev-parse", "HEAD"])?;
    let remote = root.path().join("remote");
    std::fs::create_dir(&remote)?;
    native_git(&remote, &["init", "--bare", "-q"])?;
    native_git(
        &project,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    )?;
    runner.path = project.clone();
    std::fs::create_dir(&runner.rpc)?;
    runner
        .journal
        .workspace_result(crate::preparation::Workspace {
            attachment_roots: Vec::new(),
            track_id: runner.state.track_id.clone(),
            path: project,
            reference: "refs/intercat/recovery/tracks/A".into(),
            base,
            state: "ready".into(),
            error: None,
        })
        .await?;
    let editor = native_editor(root.path()).await?;
    runner.execution.editor = editor.clone();
    let journal = runner.journal.clone();
    let capacity = runner.execution.capacity.clone();
    let task = tokio::spawn(async move {
        let result = runner.manage().await;
        let cleanup = runner.stop_owned().await;
        (result, cleanup)
    });
    let verification=async {
        tokio::time::timeout(std::time::Duration::from_secs(15),async {while port.output_flow().is_none(){tokio::time::sleep(std::time::Duration::from_millis(25)).await;}}).await?;
        std::fs::write(output.join("attempt.json"),serde_json::to_vec_pretty(&json!({"attemptedModelTurns":1}))?)?;
        port.start(Uuid::now_v7()).await?;
        let first=tokio::time::timeout(std::time::Duration::from_secs(100),async {
            loop {let state=journal.snapshot().await?["workers"][0].clone();
                if state["phase"]=="approval" {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(state);}
                if state["phase"]=="lost"||state["phase"]=="failed"{return Err(format!("Worker stopped before approval: {}",state["detail"]).into());}
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }).await??;
        if capacity.available_permits()!=0{return Err("Active worker did not hold its slot".into());}
        let identity:crate::process::Identity=serde_json::from_value(first["identity"].clone())?;
        if crate::process::identity(identity.pid)?!=identity{return Err("Test host identity changed".into());}
        let began=std::time::Instant::now();
        // SAFETY: this is the exact PID and start time just read from our owned host.
        if unsafe{libc::kill(identity.pid as i32,libc::SIGKILL)}!=0{return Err(std::io::Error::last_os_error().into());}
        let recovered=tokio::time::timeout(std::time::Duration::from_secs(20),async {
            loop {let state=journal.snapshot().await?["workers"][0].clone();
                if state["checkpoint"]["turnId"]==first["activeTurn"]&&state["checkpointPending"]==false&&state["activeTurn"].is_null(){return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(state);}
                if state["phase"]=="lost"||state["phase"]=="failed"||state["phase"]=="needs-recovery"{return Err(format!("Recovery stopped: {}",state["detail"]).into());}
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }).await??;
        if recovered["sessionId"]!=first["sessionId"]||recovered["identity"]==first["identity"]{return Err("Recovery did not retain the session with a new host".into());}
        if !recovered["approvals"].as_array().is_some_and(Vec::is_empty){return Err("Old approval remained actionable".into());}
        if !native_git(&remote,&["for-each-ref","--format=%(refname)"])?.is_empty(){return Err("The dummy push was executed".into());}
        let db=rusqlite::Connection::open(root.path().join("db"))?;
        let starts:i64=db.query_row("SELECT COUNT(*) FROM operations WHERE kind='turn.start'",[],|r|r.get(0))?;
        let approvals:i64=db.query_row("SELECT COUNT(*) FROM operations WHERE kind='approval.decide'",[],|r|r.get(0))?;
        if starts!=1||approvals!=0{return Err("Recovery replayed a side-effecting command".into());}
        std::fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"sameSession":true,"newHost":true,"checkpointSaved":true,"oldApprovalsCleared":true,"remoteUnchanged":true,"modelTurns":starts,"approvalDecisions":approvals,"recoveryMs":began.elapsed().as_millis(),"first":first,"recovered":recovered}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let _ = stop.send(true);
    let (finished, cleanup) = task.await?;
    editor.stop().await;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let attempted: i64 = db.query_row(
        "SELECT COUNT(*) FROM operations WHERE kind='turn.start'",
        [],
        |r| r.get(0),
    )?;
    std::fs::write(
        output.join("attempt.json"),
        serde_json::to_vec_pretty(&json!({"attemptedModelTurns":attempted}))?,
    )?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    if let Err(error) = &verification {
        std::fs::write(output.join("failure.txt"), error.to_string())?;
    }
    finished?;
    cleanup?;
    verification
}

async fn runtime_secret_request(
    runner: &WorkerRunner,
    root: &Path,
) -> Result<crate::secret_input::Request> {
    let track = runner.state.track_id.clone();
    rusqlite::Connection::open(root.join("db"))?.execute(
        "INSERT INTO msp_positions VALUES('retained',?1,NULL)",
        [&track],
    )?;
    let key = crate::secrets::Key::new([7; 32]);
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let value = "dummy secret for failed host";
    let catalog = runner
        .journal
        .register_fingerprint(crate::secrets::Registration {
            op_id: Uuid::now_v7(),
            track_id: track.clone(),
            session_id: "retained".into(),
            name: "NPM_TOKEN".into(),
            byte_len: value.len(),
            tag: hex(&key.tag(value.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    runner.journal.confirm_fingerprints(catalog.current).await?;
    Ok(crate::secret_input::Request {
        owner: crate::secret_input::Owner::Work,
        op_id: Uuid::now_v7(),
        track_id: track,
        session_id: "retained".into(),
        turn_id: Uuid::now_v7(),
        question: crate::secret_input::Question::Msp {
            id: "prompt".into(),
            revision: "a".repeat(64),
        },
        name: "NPM_TOKEN".into(),
        value: value.into(),
    })
}

#[tokio::test]
async fn secret_host_failure_requires_reentry_without_automatic_retry() -> Result<()> {
    let (root, mut runner, _port, _service, _retry, _stop) =
        reconnect_fixture("secret host failure").await?;
    let request = runtime_secret_request(&runner, root.path()).await?;
    let install = runner.journal.prepare_secret(request).await?.unwrap();
    runner.journal.secret_accepted(install.target.op_id).await?;
    runner
        .journal
        .secret_restarting(install.target.op_id)
        .await?;
    runner.secret = Some((install.target, 0));
    runner.environment = vec![install.value];
    tokio::time::timeout(std::time::Duration::from_secs(5), runner.manage())
        .await?
        .unwrap_err();
    assert!(runner.secret.is_none());
    assert!(runner.environment.is_empty());
    assert_eq!(
        runner.journal.snapshot().await?["secretInputs"][0]["state"],
        "failed"
    );
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='worker.reconnectFailed'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn losing_the_secret_rpc_caller_cannot_strand_its_prepared_record() -> Result<()> {
    let (root, mut runner, port, service, _retry, _stop) =
        reconnect_fixture("secret RPC disconnect").await?;
    let request = runtime_secret_request(&runner, root.path()).await?;
    let (stream, _peer) = tokio::io::duplex(65536);
    let (reader, writer) = tokio::io::split(stream);
    let connection = crate::msp::Connection::new(reader, writer);
    port.attach(connection.client.clone(), "retained".into());
    let (began, started) = tokio::sync::oneshot::channel();
    let caller = tokio::spawn(async move {
        let _ = began.send(());
        service.install_secret(request).await
    });
    started.await?;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let command = match tokio::time::timeout(
        std::time::Duration::from_secs(3),
        runner.execution.commands.recv(),
    )
    .await
    {
        Ok(Some(command)) => command,
        _ => {
            return Err(format!(
                "No retained operation: {}",
                runner.journal.snapshot().await?["secretInputs"]
            )
            .into());
        }
    };
    let crate::execution::Command::InstallSecret { install, reply } = command else {
        panic!("Missing retained secret operation");
    };
    drop(install);
    let _ = reply.send(Err("The question changed.".into()));
    for _ in 0..100 {
        if runner.journal.snapshot().await?["secretInputs"][0]["state"] == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        runner.journal.snapshot().await?["secretInputs"][0]["state"],
        "failed"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "three paid dummy Muse turns; verifies report secret, host restart, expiry and masked recovery"]
async fn real_report_secret_restarts_the_host_and_runs_with_a_masked_environment() -> Result<()> {
    let prompt = "This is a disposable test repository. Do not inspect files or run tools yet. You need DEMO_TOKEN in the environment before running python3 verify_token.py. Ask for it and end your final answer with this exact standalone line: Waiting for you: Please provide DEMO_TOKEN so I can run python3 verify_token.py. After the token is provided, run that script exactly once, report its output, and finish without asking another question.";
    let (root, mut runner, port, service, _scheduled, stop) = reconnect_fixture(prompt).await?;
    let output =
        PathBuf::from(std::env::var("INTERCAT_SECRET_OUTPUT")?).join(Uuid::now_v7().to_string());
    std::fs::create_dir_all(&output)?;
    runner.config = Config {
        source: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        binary: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        credentials: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?),
        path: std::env::var("PATH")?,
    };
    runner.state.session_id = None;
    runner.state.active_turn = None;
    runner.state.checkpoint_pending = false;
    let project = root.path().join("project");
    std::fs::create_dir(&project)?;
    let value = format!("dummy-report-secret-{}", Uuid::now_v7());
    use sha2::Digest;
    let digest = sha2::Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    std::fs::write(
        project.join("verify_token.py"),
        format!(
            "import hashlib,os\nfrom pathlib import Path\nvalue=os.environ.get('DEMO_TOKEN','')\nassert hashlib.sha256(value.encode()).hexdigest() == '{digest}', 'missing or wrong test token'\nprint(value)\nPath('verified.txt').write_text('verified\\n')\nprint('verified')\n"
        ),
    )?;
    std::fs::write(
        project.join("verify_absence.py"),
        "import os\nfrom pathlib import Path\nassert 'DEMO_TOKEN' not in os.environ, 'token was reinjected'\nPath('absent.txt').write_text('absent\\n')\nprint('absent')\n",
    )?;
    native_git(&project, &["init", "-q", "-b", "main"])?;
    native_git(&project, &["add", "verify_token.py", "verify_absence.py"])?;
    native_git(&project, &["commit", "-qm", "fixture"])?;
    let base = native_git(&project, &["rev-parse", "HEAD"])?;
    runner.path = project.clone();
    std::fs::create_dir(&runner.rpc)?;
    runner
        .journal
        .workspace_result(crate::preparation::Workspace {
            attachment_roots: Vec::new(),
            track_id: runner.state.track_id.clone(),
            path: project.clone(),
            reference: "refs/intercat/secret/tracks/A".into(),
            base,
            state: "ready".into(),
            error: None,
        })
        .await?;
    let editor = native_editor(root.path()).await?;
    runner.execution.editor = editor.clone();
    let journal = runner.journal.clone();
    let task = tokio::spawn(async move {
        let result = runner.manage().await;
        let cleanup = runner.stop_owned().await;
        (result, cleanup)
    });
    let verified=async {
        tokio::time::timeout(std::time::Duration::from_secs(20),async {while port.output_flow().is_none(){tokio::time::sleep(std::time::Duration::from_millis(20)).await;}}).await?;
        std::fs::write(output.join("attempt.json"),b"{\"attemptedModelTurns\":1}")?;
        port.start(Uuid::now_v7()).await?;
        let first=tokio::time::timeout(std::time::Duration::from_secs(120),async {
            loop {let state=journal.snapshot().await?;let worker=&state["workers"][0];
                if worker["activeTurn"].is_null() && worker["checkpointPending"]==false && !worker["checkpoint"].is_null() && !state["reportQuestions"].as_array().unwrap().is_empty(){return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(state);}
                if matches!(worker["phase"].as_str(),Some("failed"|"lost"|"needs-recovery")){return Err("Worker failed before the test token question".into());}
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }).await??;
        let question=&first["reportQuestions"][0]; let track=question["trackId"].as_str().unwrap(); let session=question["sessionId"].as_str().unwrap();
        let key=crate::secrets::Key::new([7;32]); let hex=|bytes:&[u8]|bytes.iter().map(|b|format!("{b:02x}")).collect::<String>();
        let catalog=journal.register_fingerprint(crate::secrets::Registration {op_id:Uuid::now_v7(),track_id:track.into(),session_id:session.into(),name:"DEMO_TOKEN".into(),byte_len:value.len(),tag:hex(&key.tag(value.as_bytes())),key_check:hex(&key.check())}).await?;
        service.reload_fingerprints(track,session).await?; journal.confirm_fingerprints(catalog.current).await?;
        service.install_secret(crate::secret_input::Request {owner: crate::secret_input::Owner::Work, op_id:Uuid::now_v7(),track_id:track.into(),session_id:session.into(),turn_id:question["turnId"].as_str().unwrap().parse()?,question:crate::secret_input::Question::Report {id:question["id"].as_str().unwrap().into(),revision:question["revision"].as_i64().unwrap()},name:"DEMO_TOKEN".into(),value:value.clone()}).await?;
        std::fs::write(output.join("attempt.json"),b"{\"attemptedModelTurns\":2}")?;
        let final_state=tokio::time::timeout(std::time::Duration::from_secs(120),async {
            let mut approved=std::collections::HashSet::new();
            loop {let state=journal.snapshot().await?; let worker=&state["workers"][0];
                for card in worker["approvals"].as_array().unwrap() {
                    let id=card["id"].as_str().unwrap(); if !approved.insert(id.to_owned()){continue;}
                    let subject=card["subject"].as_str().unwrap();
                    if !["python3 verify_token.py","python verify_token.py","/usr/bin/python3 verify_token.py"].contains(&subject){return Err(format!("Unexpected test command: {subject}").into());}
                    port.answer(Uuid::now_v7(),id.into(),card["requirement"].clone(),crate::approval::Response::Allow).await?;
                }
                if project.join("verified.txt").exists() && worker["activeTurn"].is_null() && worker["checkpointPending"]==false && worker["checkpoint"]["turnId"]!=first["workers"][0]["checkpoint"]["turnId"] {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(state);}
                if matches!(worker["phase"].as_str(),Some("failed"|"lost"|"needs-recovery")){return Err("Worker failed while using the test token".into());}
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }).await??;
        assert_eq!(final_state["workers"][0]["sessionId"],first["workers"][0]["sessionId"]);
        assert_ne!(final_state["workers"][0]["identity"],first["workers"][0]["identity"]);
        assert_eq!(std::fs::read_to_string(project.join("verified.txt"))?,"verified\n");
        let provided_state=final_state;
        let identity:crate::process::Identity=serde_json::from_value(provided_state["workers"][0]["identity"].clone())?;
        assert_eq!(crate::process::identity(identity.pid)?,identity);
        // This exact PID and start time belong to the host this fixture owns.
        if unsafe{libc::kill(identity.pid as i32,libc::SIGKILL)}!=0{return Err(std::io::Error::last_os_error().into());}
        let recovered=tokio::time::timeout(std::time::Duration::from_secs(30),async {
            loop {let state=journal.snapshot().await?;let worker=&state["workers"][0];
                if worker["identity"]!=provided_state["workers"][0]["identity"] && matches!(worker["phase"].as_str(),Some("ready"|"question"|"held")) && port.output_flow().is_some(){return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(state);}
                if matches!(worker["phase"].as_str(),Some("failed"|"lost"|"needs-recovery")){return Err("Worker failed after the secret host exited".into());}
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }).await??;
        assert_eq!(recovered["secretInputs"][0]["state"],"expired");
        for question in recovered["reportQuestions"].as_array().unwrap() {
            assert_eq!(question["kind"],"review");
            journal.respond_report(Uuid::now_v7(),question["id"].as_str().unwrap().into(),question["revision"].as_i64().unwrap(),crate::reports::Answer::Continue).await?;
        }
        let track=first["workers"][0]["trackId"].as_str().unwrap().to_owned();
        journal.stage_utterance(Uuid::now_v7(),"Run python3 verify_absence.py exactly once. Do not set or unset environment variables in the command: the script checks its inherited environment. Report its output and finish without a question.".into(),crate::decision::Target::Lock(track.clone()),Some(track.clone()), Vec::new()).await?;
        let word=journal.claim_input().await?.ok_or("Missing absence-check request")?;
        std::fs::write(output.join("attempt.json"),b"{\"attemptedModelTurns\":3}")?;
        journal.apply_input(word.id,word.revision,crate::decision::Outcome::Classified(crate::decision::Decision {kind:crate::decision::Kind::Extend,track_id:Some(track),boundary:Some("now".into()),command:None,confidence:1.0,inference_ms:0.0,timed_out:false,scores:json!({})})).await?;
        let final_state=tokio::time::timeout(std::time::Duration::from_secs(120),async {
            let mut approved=std::collections::HashSet::new();
            loop {let state=journal.snapshot().await?;let worker=&state["workers"][0];
                for card in worker["approvals"].as_array().unwrap() {
                    let id=card["id"].as_str().unwrap();if !approved.insert(id.to_owned()){continue;}
                    let subject=card["subject"].as_str().unwrap();
                    if !["python3 verify_absence.py","python verify_absence.py","/usr/bin/python3 verify_absence.py"].contains(&subject){return Err(format!("Unexpected absence-check command: {subject}").into());}
                    port.answer(Uuid::now_v7(),id.into(),card["requirement"].clone(),crate::approval::Response::Allow).await?;
                }
                if project.join("absent.txt").exists() && worker["activeTurn"].is_null() && worker["checkpointPending"]==false && worker["checkpoint"]["turnId"]!=provided_state["workers"][0]["checkpoint"]["turnId"] {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(state);}
                if matches!(worker["phase"].as_str(),Some("failed"|"lost"|"needs-recovery")){return Err("Worker failed during the absence check".into());}
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }).await??;
        assert_eq!(std::fs::read_to_string(project.join("absent.txt"))?,"absent\n");
        assert_eq!(final_state["workers"][0]["sessionId"],provided_state["workers"][0]["sessionId"]);
        assert!(!final_state.to_string().contains(&value));
        let db=rusqlite::Connection::open(root.path().join("db"))?;
        let event_values:i64=db.query_row("SELECT count(*) FROM events WHERE instr(payload,?1)>0",[&value],|r|r.get(0))?;
        let masks:i64=db.query_row("SELECT count(*) FROM events WHERE instr(payload,'<secret:DEMO_TOKEN>')>0",[],|r|r.get(0))?;
        let turns:i64=db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'",[],|r|r.get(0))?;
        assert_eq!(event_values,0); assert!(masks>0); assert_eq!(turns,3);
        std::fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"sameSession":true,"newHost":true,"verifiedFile":true,"notReinjected":true,"maskedRecovery":true,"rawSecretEvents":event_values,"maskedEvents":masks,"modelTurns":turns,"first":first,"provided":provided_state,"final":final_state}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let _ = stop.send(true);
    let (result, cleanup) = task.await?;
    editor.stop().await;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let turns: i64 = db.query_row(
        "SELECT count(*) FROM operations WHERE kind='turn.start'",
        [],
        |r| r.get(0),
    )?;
    std::fs::write(
        output.join("attempt.json"),
        serde_json::to_vec_pretty(&json!({"attemptedModelTurns":turns}))?,
    )?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    if let Err(error) = &verified {
        std::fs::write(output.join("failure.txt"), error.to_string())?;
    }
    result?;
    cleanup?;
    verified
}

#[tokio::test]
#[ignore = "two paid dummy Muse turns; redirects an active turn only after its durable checkpoint"]
async fn real_redirect_interrupts_then_runs_the_correction_once() -> Result<()> {
    let (root, mut runner, port, _service, _scheduled, stop) = reconnect_fixture(
        "This is a disposable local test. Run exactly `python3 slow.py` once with bash. It deliberately waits. Do not edit files or run another command; wait for its result.",
    ).await?;
    let output =
        PathBuf::from(std::env::var("INTERCAT_REDIRECT_OUTPUT")?).join(Uuid::now_v7().to_string());
    std::fs::create_dir_all(&output)?;
    runner.config = Config {
        source: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
        binary: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
        credentials: std::env::var("INTERCAT_TEST_MUSE_AUTH")?.into(),
        path: std::env::var("PATH")?,
    };
    runner.state.session_id = None;
    runner.state.active_turn = None;
    runner.state.checkpoint_pending = false;
    let project = root.path().join("project");
    std::fs::create_dir(&project)?;
    std::fs::write(
        project.join("slow.py"),
        "from pathlib import Path\nimport time\nPath('started.txt').write_text('started\\n')\ntime.sleep(90)\nPath('late.txt').write_text('not interrupted\\n')\n",
    )?;
    std::fs::write(
        project.join("finish.py"),
        "from pathlib import Path\nassert Path('started.txt').exists()\np=Path('corrected.txt')\nassert not p.exists(), 'correction ran twice'\np.write_text('corrected\\n')\nprint('correction verified')\n",
    )?;
    native_git(&project, &["init", "-q", "-b", "main"])?;
    native_git(&project, &["add", "."])?;
    native_git(&project, &["commit", "-qm", "fixture"])?;
    let base = native_git(&project, &["rev-parse", "HEAD"])?;
    runner.path = project.clone();
    std::fs::create_dir(&runner.rpc)?;
    runner
        .journal
        .workspace_result(crate::preparation::Workspace {
            attachment_roots: Vec::new(),
            track_id: runner.state.track_id.clone(),
            path: project.clone(),
            reference: "refs/intercat/redirect/tracks/A".into(),
            base,
            state: "ready".into(),
            error: None,
        })
        .await?;
    let editor = native_editor(root.path()).await?;
    runner.execution.editor = editor.clone();
    let journal = runner.journal.clone();
    let track = runner.state.track_id.clone();
    let task = tokio::spawn(async move {
        let result = runner.manage().await;
        let cleanup = runner.stop_owned().await;
        (result, cleanup)
    });
    let verification = async {
        tokio::time::timeout(std::time::Duration::from_secs(20), async { while port.output_flow().is_none() { tokio::time::sleep(std::time::Duration::from_millis(25)).await; } }).await?;
        port.start(Uuid::now_v7()).await?;
        tokio::time::timeout(std::time::Duration::from_secs(90), async { while !project.join("started.txt").exists() { tokio::time::sleep(std::time::Duration::from_millis(25)).await; } }).await?;
        let first = journal.snapshot().await?["workers"][0].clone();
        if first["activeTurn"].is_null() { return Err("The first turn ended before the redirect fixture.".into()); }
        let id = Uuid::now_v7();
        journal.stage_utterance(id, "The previous wait is cancelled. Do not restart slow.py. Instead run exactly `python3 finish.py` once with bash. Report its result, do not ask any question, and finish.".into(), crate::decision::Target::Lock(track.clone()), None, Vec::new()).await?;
        let input = journal.claim_input().await?.ok_or("The correction was not queued.")?;
        journal.apply_input(input.id, input.revision, crate::decision::Outcome::Classified(crate::decision::Decision { kind: crate::decision::Kind::Trim, track_id: Some(track.clone()), boundary: Some("now".into()), command: None, confidence: 1., inference_ms: 0., timed_out: false, scores: json!({}) })).await?;
        let final_state = tokio::time::timeout(std::time::Duration::from_secs(90), async {
            loop {
                let snapshot = journal.snapshot().await?;
                let worker = &snapshot["workers"][0];
                if snapshot["redirects"][0]["phase"] == "held" { return Err(format!("Correction held: {}", snapshot["utterances"]).into()); }
                if project.join("corrected.txt").exists() && worker["activeTurn"].is_null() && worker["checkpointPending"] == false { return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(snapshot); }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }).await??;
        if final_state["redirects"][0]["phase"] != "done" { return Err("Correction was not received by the same worker.".into()); }
        if final_state["workers"][0]["sessionId"] != first["sessionId"] { return Err("Correction changed the session.".into()); }
        if project.join("late.txt").exists() { return Err("The initial wait ran to its natural end.".into()); }
        let db = rusqlite::Connection::open(root.path().join("db"))?;
        let starts: i64 = db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'", [], |r| r.get(0))?;
        if starts != 2 { return Err("Unexpected number of model turns.".into()); }
        let checkpoint_seq: i64 = db.query_row("SELECT min(seq) FROM events WHERE kind='worker.state' AND json_extract(payload,'$.checkpoint.turnId')=?1 AND json_extract(payload,'$.checkpointPending')=0", [first["activeTurn"].as_str().unwrap()], |r| r.get(0))?;
        let correction_seq: i64 = db.query_row("SELECT min(seq) FROM events WHERE kind='intent.turn.start' AND json_extract(payload,'$.inputId')=?1", [id.to_string()], |r| r.get(0))?;
        if checkpoint_seq >= correction_seq { return Err("Correction preceded the saved checkpoint.".into()); }
        std::fs::write(output.join("result.json"), serde_json::to_vec_pretty(&json!({"modelTurns":starts,"sameSession":true,"checkpointSequence":checkpoint_seq,"correctionStartSequence":correction_seq,"first":first,"final":final_state,"correctedOnce":true,"naturalWaitCancelled":true}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let _ = stop.send(true);
    let (finished, cleanup) = task.await?;
    editor.stop().await;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let attempted: i64 = db.query_row(
        "SELECT count(*) FROM operations WHERE kind='turn.start'",
        [],
        |r| r.get(0),
    )?;
    std::fs::write(
        output.join("attempt.json"),
        serde_json::to_vec_pretty(&json!({"attemptedModelTurns":attempted}))?,
    )?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    if let Err(error) = &verification {
        std::fs::write(output.join("failure.txt"), error.to_string())?;
    }
    finished?;
    cleanup?;
    verification
}

async fn branch_checkpoint(
    journal: &Journal,
    track: &str,
    previous: Option<Uuid>,
) -> Result<WorkerState> {
    tokio::time::timeout(std::time::Duration::from_secs(90), async {
        loop {
            let snapshot = journal.snapshot().await?;
            if let Some(value) = snapshot["workers"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|w| w["trackId"] == track)
            {
                let worker: WorkerState = serde_json::from_value(value.clone())?;
                if matches!(worker.phase.as_str(), "failed" | "needs-recovery" | "lost") {
                    return Err(worker.detail.unwrap_or(worker.phase).into());
                }
                if worker.active_turn.is_none()
                    && !worker.checkpoint_pending
                    && matches!(worker.phase.as_str(), "ready" | "question")
                    && !snapshot["reports"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|report| report["trackId"] == track && report["pending"] == true)
                    && worker
                        .checkpoint
                        .as_ref()
                        .is_some_and(|c| Some(c.turn_id) != previous)
                {
                    return Ok(worker);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await?
}

#[tokio::test]
async fn shutdown_does_not_admit_a_new_branch() -> Result<()> {
    let (_root, runner, _port, service, _scheduled, stop) =
        reconnect_fixture("branch shutdown").await?;
    stop.send(true)?;
    assert!(
        service
            .prepare_branch(crate::branch::Request {
                op_id: Uuid::now_v7(),
                parent_track_id: runner.state.track_id.clone(),
                turn_id: Uuid::now_v7()
            })
            .await
            .is_err()
    );
    assert!(
        runner.journal.snapshot().await?["branches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}
async fn branch_input(
    journal: &Journal,
    track: &str,
    text: &str,
    attachments: Vec<Uuid>,
) -> Result<()> {
    for question in journal.snapshot().await?["reportQuestions"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if question["trackId"] == track && question["state"] == "open" {
            if question["kind"] != "review" {
                return Err("The fixture received an unexpected question.".into());
            }
            journal
                .respond_report(
                    Uuid::now_v7(),
                    question["id"].as_str().unwrap().into(),
                    question["revision"].as_i64().unwrap(),
                    crate::reports::Answer::Continue,
                )
                .await?;
        }
    }
    journal
        .stage_utterance(
            Uuid::now_v7(),
            text.into(),
            crate::decision::Target::Lock(track.into()),
            None,
            attachments,
        )
        .await?;
    let input = journal.claim_input().await?.ok_or("No branch input.")?;
    journal
        .apply_input(
            input.id,
            input.revision,
            crate::decision::Outcome::Classified(crate::decision::Decision {
                kind: crate::decision::Kind::Extend,
                track_id: Some(track.into()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await
}

#[tokio::test]
#[ignore = "three paid dummy Muse turns; branches an early checkpoint and verifies inherited masking"]
async fn real_branch_uses_the_early_tree_and_inherits_secret_fingerprints() -> Result<()> {
    let (root,mut runner,parent_port,service,_scheduled,stop)=reconnect_fixture("Run exactly `python3 first.py` once with bash. Do not use another tool or command. Report its output briefly and finish without a question.").await?;
    let output =
        PathBuf::from(std::env::var("INTERCAT_BRANCH_OUTPUT")?).join(Uuid::now_v7().to_string());
    std::fs::create_dir_all(&output)?;
    let config = Config {
        source: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
        binary: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
        credentials: std::env::var("INTERCAT_TEST_MUSE_AUTH")?.into(),
        path: std::env::var("PATH")?,
    };
    runner.config = config.clone();
    runner.state.session_id = None;
    runner.state.active_turn = None;
    runner.state.checkpoint_pending = false;
    let source = root.path().join("source");
    std::fs::create_dir(&source)?;
    let source = source.canonicalize()?;
    std::fs::write(
        source.join("first.py"),
        "import os\nfrom pathlib import Path\nassert [p.read_text() for p in Path('.intercat/attachments').glob('*.txt')]==['EARLY_ATTACHMENT']\nPath('state.txt').write_text('EARLY\\n')\nprint(os.environ['DEMO_TOKEN'])\n",
    )?;
    std::fs::write(
        source.join("late.py"),
        "from pathlib import Path\nassert sorted(p.read_text() for p in Path('.intercat/attachments').glob('*.txt'))==['EARLY_ATTACHMENT','LATE_ATTACHMENT']\nPath('state.txt').write_text('LATE\\n')\nprint('LATE_ONLY_TURN')\n",
    )?;
    std::fs::write(
        source.join("child.py"),
        "import os\nfrom pathlib import Path\nassert Path('state.txt').read_text()=='EARLY\\n'\nassert 'DEMO_TOKEN' not in os.environ\nassert [p.read_text() for p in Path('.intercat/attachments').glob('*.txt')]==['EARLY_ATTACHMENT']\nPath('child.txt').write_text('verified\\n')\nprint('EARLY_TREE_VERIFIED')\n",
    )?;
    native_git(&source, &["init", "-q", "-b", "main"])?;
    native_git(&source, &["add", "."])?;
    native_git(&source, &["commit", "-qm", "fixture"])?;
    let project = crate::project::inspect(source.clone()).await?;
    let journal = runner.journal.clone();
    let track = runner.state.track_id.clone();
    let initial = journal.snapshot().await?["utterances"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    journal.cut_input(Uuid::now_v7(), initial).await?;
    let original = root.path().join("early-reference.txt");
    std::fs::write(&original, "EARLY_ATTACHMENT")?;
    let early_attachment = journal
        .import_attachment(Uuid::now_v7(), original.clone())
        .await?;
    std::fs::remove_file(original)?;
    journal
        .configure_project(Uuid::now_v7(), project.clone())
        .await?;
    journal
        .set_worker_config(Uuid::now_v7(), config.clone())
        .await?;
    let project_id = journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    let key = crate::secrets::Key::new([7; 32]);
    let hex = |bytes: [u8; 32]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let catalog = journal
        .prepare_project_secrets(crate::project_secrets::Prepare {
            op_id: Uuid::now_v7(),
            project_id: project_id.clone(),
            revision: 0,
            fingerprints: vec![],
            key_check: hex(key.check()),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    service
        .activate_project_secrets(crate::project_secrets::Load {
            project_id,
            revision: 0,
            entries: vec![],
        })
        .await?;
    let secret = "DUMMY_FORK_SECRET_7f29_猫";
    runner.environment = vec![crate::secret_input::Value::new(
        "DEMO_TOKEN".into(),
        secret.into(),
    )?];
    runner.path = source.clone();
    std::fs::create_dir(&runner.rpc)?;
    let workspace = crate::preparation::Workspace {
        attachment_roots: crate::attachments::reserve(&source, &project.base)?,
        track_id: track.clone(),
        path: source.clone(),
        reference: "refs/intercat/branch/tracks/A".into(),
        base: project.base.clone(),
        state: "ready".into(),
        error: None,
    };
    journal.workspace_result(workspace).await?;
    let editor = native_editor(root.path()).await?;
    runner.execution.editor = editor.clone();
    let capacity = runner.execution.capacity.clone();
    let parent_task = tokio::spawn(async move {
        let result = runner.manage().await;
        let cleanup = runner.stop_owned().await;
        (result, cleanup)
    });
    let mut child_task = None;
    let verification=async {
        tokio::time::timeout(std::time::Duration::from_secs(20),async {while parent_port.output_flow().is_none() {tokio::time::sleep(std::time::Duration::from_millis(25)).await;}}).await?;
        let session=journal.snapshot().await?["workers"][0]["sessionId"].as_str().ok_or("No parent session.")?.to_owned();
        let catalog=journal.register_fingerprint(crate::secrets::Registration {op_id:Uuid::now_v7(),track_id:track.clone(),session_id:session.clone(),name:"DEMO_TOKEN".into(),byte_len:secret.len(),tag:hex(key.tag(secret.as_bytes())),key_check:hex(key.check())}).await?;
        journal.confirm_fingerprints(catalog.current).await?;service.reload_fingerprints(&track,&session).await?;
        branch_input(&journal,&track,"Run exactly `python3 first.py` once with bash. Do not use another tool or command. Report its output briefly and finish without a question.",vec![early_attachment.id]).await?;
        let first=branch_checkpoint(&journal,&track,None).await?.checkpoint.unwrap();
        eprintln!("branch fixture: first checkpoint saved");
        let late_original=root.path().join("late-reference.txt");std::fs::write(&late_original,"LATE_ATTACHMENT")?;
        let late_attachment=journal.import_attachment(Uuid::now_v7(),late_original.clone()).await?;std::fs::remove_file(late_original)?;
        branch_input(&journal,&track,"Run exactly `python3 late.py` once with bash. Do not use another command or tool. Report briefly and finish without a question.",vec![late_attachment.id]).await?;
        let late=branch_checkpoint(&journal,&track,Some(first.turn_id)).await?.checkpoint.unwrap();
        eprintln!("branch fixture: later checkpoint saved");
        let request=crate::branch::Request {op_id:Uuid::now_v7(),parent_track_id:track.clone(),turn_id:first.turn_id};
        let prepared=service.prepare_branch(request.clone()).await?;
        assert_eq!(prepared["branch"]["phase"],"awaiting-receipt");
        if service.activate_branch(request.op_id).await.is_ok() {return Err("Branch activated before the fingerprint receipt.".into());}
        let catalog=journal.fingerprint_catalog().await?;
        journal.confirm_fingerprints(catalog.current).await?;
        let branch=service.activate_branch(request.op_id).await?;
        eprintln!("branch fixture: inherited fingerprints confirmed");
        if std::fs::read_to_string(branch.workspace.path.join("state.txt"))?!="EARLY\n" || std::fs::read_to_string(source.join("state.txt"))?!="LATE\n" {return Err("Branch did not retain the early checkpoint tree.".into());}
        if native_git(&branch.workspace.path,&["rev-parse","HEAD"])?!=first.sha || !native_git(&branch.workspace.path,&["status","--porcelain"] )?.is_empty() {return Err("Branch worktree is not clean at the checkpoint.".into());}
        let again=service.prepare_branch(request).await?;
        if again["branch"]["sessionId"]!=prepared["branch"]["sessionId"] {return Err("Retry created another child session.".into());}
        let child_session=branch.session_id.clone().unwrap();
        let previous:WorkerState=serde_json::from_value(journal.snapshot().await?["workers"].as_array().unwrap().iter().find(|w|w["trackId"]==branch.track_id).unwrap().clone())?;
        let (port,commands)=crate::execution::Port::new(std::sync::Arc::new(crate::dispatch_control::Gate::new(false)));
        let mut child=WorkerRunner {config:config.clone(),data:root.path().into(),rpc:root.path().join("rpc"),journal:journal.clone(),path:branch.workspace.path.clone(),state:WorkerState::starting(branch.track_id.clone(),Some(&previous)),stopping:stop.subscribe(),execution:crate::execution::Context {project_secrets:service.project_secrets(),port:port.clone(),commands,editor:editor.clone(),capacity:capacity.clone()},continuity:crate::execution::Continuity::default(),tracker:None,stable:false,environment:vec![],secret:None};
        child_task=Some(tokio::spawn(async move {let result=child.manage().await;let cleanup=child.stop_owned().await;(result,cleanup)}));
        tokio::time::timeout(std::time::Duration::from_secs(20),async {while port.output_flow().is_none(){tokio::time::sleep(std::time::Duration::from_millis(25)).await;}}).await?;
        branch_checkpoint(&journal,&branch.track_id,None).await?;
        branch_input(&journal,&branch.track_id,"Run exactly `python3 child.py` once with bash. Do not use any other tool or command. Report the output, then repeat the environment value printed in the first turn of this conversation. Finish without a question.",vec![]).await?;
        let completed=branch_checkpoint(&journal,&branch.track_id,Some(first.turn_id)).await?;
        if !branch.workspace.path.join("child.txt").exists() {return Err("Child command did not run in its own workspace.".into());}
        let db=rusqlite::Connection::open(root.path().join("db"))?;
        let payloads:Vec<String>=db.prepare("SELECT payload FROM events WHERE session_id=?1")?.query_map([&child_session],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let text=payloads.join("\n");
        let ids:Vec<String>=db.prepare("SELECT DISTINCT attachment FROM attachment_turn_items WHERE session=?1")?.query_map([&child_session],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        assert_eq!(ids,vec![early_attachment.id.to_string()]);
        if text.contains(secret)||text.contains("LATE_ONLY_TURN")||!text.contains("<secret:DEMO_TOKEN>") {return Err("Child history was not cut and masked correctly.".into());}
        std::fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"branch":branch,"first":first,"late":late,"completed":completed,"inheritedMask":true,"lateHistoryExcluded":true,"cleanEarlyTree":true,"sealingRequired":true,"schema":crate::journal::SCHEMA_VERSION,"paidTurns":3,"earlyAttachment":early_attachment,"lateAttachment":late_attachment,"attachmentBoundaryVerified":true}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let _ = stop.send(true);
    let (finished, cleanup) = parent_task.await?;
    let child_result = match child_task {
        Some(task) => Some(task.await?),
        None => None,
    };
    editor.stop().await;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let attempts: i64 = db.query_row(
        "SELECT count(*) FROM operations WHERE kind='turn.start'",
        [],
        |r| r.get(0),
    )?;
    std::fs::write(
        output.join("attempt.json"),
        serde_json::to_vec_pretty(&json!({"attemptedModelTurns":attempts}))?,
    )?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    if let Err(error) = &verification {
        std::fs::write(output.join("failure.txt"), error.to_string())?;
    }
    finished?;
    cleanup?;
    if let Some((child, cleanup)) = child_result {
        child?;
        cleanup?;
    }
    verification
}
