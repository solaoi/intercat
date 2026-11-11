use intercat_daemon::{
    Result,
    checkpoint::Checkpoint,
    decision::{Decision, Kind, Outcome, Resolution, Target},
    journal::Journal,
    runtime::WorkerState,
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal, WorkerState)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Start".into(),
            Target::New,
            None,
            Vec::new(),
        )
        .await?;
    let first = journal.claim_input().await?.unwrap();
    journal
        .apply_input(first.id, first.revision, classified(Kind::Fork, None))
        .await?;
    let track = journal.snapshot().await?["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    journal
        .workspace_result(intercat_daemon::preparation::Workspace {
            denied_roots: Vec::new(),
            track_id: track.clone(),
            path: root.path().join("worker"),
            reference: "refs/intercat/test/tracks/A".into(),
            base: "0".repeat(40),
            state: "ready".into(),
            error: None,
            attachment_roots: vec![],
        })
        .await?;
    let worker = WorkerState {
        track_id: track,
        phase: "working".into(),
        session_id: Some("session".into()),
        secret_revision: None,
        identity: None,
        detail: None,
        active_turn: Some(Uuid::now_v7()),
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    journal.worker_state(worker.clone()).await?;
    Ok((root, journal, worker))
}
fn classified(kind: Kind, track: Option<String>) -> Outcome {
    Outcome::Classified(Decision {
        kind,
        track_id: track,
        boundary: None,
        command: None,
        confidence: 1.,
        inference_ms: 1.,
        timed_out: false,
        scores: json!({}),
    })
}
async fn correction(journal: &Journal, worker: &WorkerState) -> Result<String> {
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Do this instead".into(),
            Target::None,
            None,
            Vec::new(),
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id.clone(),
            input.revision,
            classified(Kind::Trim, Some(worker.track_id.clone())),
        )
        .await?;
    Ok(input.id)
}
fn word(snapshot: &Value, id: &str) -> Value {
    snapshot["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|word| word["id"] == id)
        .unwrap()
        .clone()
}
fn checkpoint(worker: &mut WorkerState, turn: Uuid) {
    worker.active_turn = None;
    worker.checkpoint_pending = false;
    worker.phase = "ready".into();
    worker.checkpoint = Some(Checkpoint {
        turn_id: turn,
        sha: "a".repeat(40),
        reference: "refs/intercat/fixture/A".into(),
        unmerged_paths: vec![],
        live_processes: false,
    });
}

async fn saved_checkpoint(journal: &Journal, worker: &WorkerState, turn: Uuid) -> Result<()> {
    let id = Uuid::new_v5(&turn, b"checkpoint");
    let checkpoint = Checkpoint {
        turn_id: turn,
        sha: "a".repeat(40),
        reference: "refs/intercat/fixture/A".into(),
        unmerged_paths: vec![],
        live_processes: false,
    };
    journal
        .intent(
            &id.to_string(),
            "checkpoint.save",
            json!({"trackId":worker.track_id,"checkpoint":checkpoint}),
        )
        .await?;
    journal.finish(&id.to_string(), "dispatched").await?;
    journal.finish(&id.to_string(), "complete").await?;
    Ok(())
}

#[tokio::test]
async fn correction_waits_for_its_exact_checkpoint_and_a_wrong_session_cannot_dispatch()
-> Result<()> {
    let (_root, journal, mut worker) = fixture().await?;
    let turn = worker.active_turn.unwrap();
    let id = correction(&journal, &worker).await?;
    let saved = journal.snapshot().await?;
    assert_eq!(word(&saved, &id)["state"], "redirecting");
    assert_eq!(saved["redirects"][0]["turnId"], turn.to_string());
    assert!(journal.claim_redirect(id.clone(), 1).await?.is_some());
    assert!(journal.cut_input(Uuid::now_v7(), id.clone()).await.is_err());
    assert!(
        journal
            .reserve_turn(
                Uuid::now_v7(),
                id.clone(),
                1,
                worker.track_id.clone(),
                json!({"sessionId":"session"}),
                None,
            )
            .await
            .is_err()
    );
    worker.active_turn = None;
    worker.checkpoint_pending = true;
    worker.phase = "checkpoint".into();
    journal.worker_state(worker.clone()).await?;
    assert_eq!(
        word(&journal.snapshot().await?, &id)["state"],
        "redirecting"
    );
    assert!(
        journal.snapshot().await?["editorEntries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["scene"] != "trim")
    );
    checkpoint(&mut worker, turn);
    saved_checkpoint(&journal, &worker, turn).await?;
    journal.worker_state(worker.clone()).await?;
    assert_eq!(word(&journal.snapshot().await?, &id)["state"], "placed");
    let editor_log = journal
        .editor_log(intercat_daemon::editor_log::Query {
            track_id: worker.track_id.clone(),
            before: None,
        })
        .await?;
    let trim = editor_log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["scene"] == "trim")
        .collect::<Vec<_>>();
    assert_eq!(trim.len(), 1);
    assert_eq!(trim[0]["inputId"], id);
    assert!(
        trim[0]["detail"]
            .as_str()
            .unwrap()
            .contains("checkpoint was saved")
    );

    assert!(
        journal
            .reserve_turn(
                Uuid::now_v7(),
                id.clone(),
                1,
                worker.track_id.clone(),
                json!({"sessionId":"other"}),
                None,
            )
            .await
            .is_err()
    );
    let operation = Uuid::now_v7();
    journal
        .reserve_turn(
            operation,
            id.clone(),
            1,
            worker.track_id.clone(),
            json!({"commandId":operation,"sessionId":"session"}),
            None,
        )
        .await?;
    journal.try_msp(worker.track_id, "session".into(), intercat_daemon::clock::Stamp::now(), json!({"method":"item/updated","params":{"sessionId":"session","turnId":operation,"item":{"itemId":"correction-received","turnId":operation,"kind":"userMessage","commandId":operation,"revision":1,"status":"completed"}}}), None, None)?.await??;
    assert_eq!(journal.snapshot().await?["redirects"][0]["phase"], "done");
    Ok(())
}

#[tokio::test]
async fn later_turn_is_not_redirected_and_restart_requires_an_explicit_retry() -> Result<()> {
    let (root, journal, mut worker) = fixture().await?;
    let original = worker.active_turn.unwrap();
    let id = correction(&journal, &worker).await?;
    worker.active_turn = Some(Uuid::now_v7());
    journal.worker_state(worker.clone()).await?;
    assert_eq!(word(&journal.snapshot().await?, &id)["state"], "held");
    let state = word(&journal.snapshot().await?, &id);
    assert!(
        journal
            .resolve_input(
                Uuid::now_v7(),
                id.clone(),
                state["revision"].as_i64().unwrap(),
                Resolution::Retry {
                    target: Target::Lock(worker.track_id.clone())
                }
            )
            .await
            .is_err()
    );
    saved_checkpoint(&journal, &worker, original).await?;
    journal
        .resolve_input(
            Uuid::now_v7(),
            id.clone(),
            state["revision"].as_i64().unwrap(),
            Resolution::Retry {
                target: Target::Lock(worker.track_id.clone()),
            },
        )
        .await?;
    assert_eq!(journal.snapshot().await?["redirects"][0]["phase"], "ready");
    drop(journal);
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    assert_eq!(word(&journal.snapshot().await?, &id)["state"], "held");
    assert_eq!(journal.snapshot().await?["redirects"][0]["phase"], "held");
    Ok(())
}

#[tokio::test]
async fn stop_holds_the_correction_and_stale_classification_cannot_create_one() -> Result<()> {
    let (_root, journal, mut worker) = fixture().await?;
    let id = correction(&journal, &worker).await?;
    worker.phase = "held".into();
    journal.worker_state(worker.clone()).await?;
    assert_eq!(word(&journal.snapshot().await?, &id)["state"], "held");
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Cancelled correction".into(),
            Target::None,
            None,
            Vec::new(),
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal.cut_input(Uuid::now_v7(), input.id.clone()).await?;
    journal
        .apply_input(
            input.id,
            input.revision,
            classified(Kind::Trim, Some(worker.track_id)),
        )
        .await?;
    assert_eq!(
        journal.snapshot().await?["redirects"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn execution_interrupts_only_the_captured_turn_without_a_running_editor() -> Result<()> {
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncBufReadExt, BufReader};
    for moved in [false, true] {
        let (root, journal, mut worker) = fixture().await?;
        let target = worker.active_turn.unwrap();
        correction(&journal, &worker).await?;
        if moved {
            worker.active_turn = Some(Uuid::now_v7());
        }
        let (_host_write, read) = tokio::io::duplex(4096);
        let (write, host_read) = tokio::io::duplex(4096);
        let mut connection = intercat_daemon::msp::Connection::new(read, write);
        let (port, commands) = intercat_daemon::execution::Port::new(std::sync::Arc::new(
            intercat_daemon::dispatch_control::Gate::new(false),
        ));
        port.attach(connection.client.clone(), "session".into(), None);
        let mut context = intercat_daemon::execution::Context {
            project_secrets: Default::default(),
            port,
            commands,
            editor: intercat_daemon::editor::Manager::new(
                root.path(),
                &root.path().join("editor.log"),
            )?,
            capacity: Arc::new(intercat_daemon::scheduler::Capacity::new(1)),
        };
        let identity = intercat_daemon::process::identity(std::process::id())?;
        let tracker = Arc::new(Mutex::new(intercat_daemon::process::Tracker::new(
            "redirect-test".into(),
            identity,
        )?));
        let (stop, mut stopping) = tokio::sync::watch::channel(false);
        let path = root.path().to_owned();
        let running = tokio::spawn(async move {
            let mut continuity = intercat_daemon::execution::Continuity::default();
            let result = intercat_daemon::execution::run(
                intercat_daemon::execution::WorkerIo {
                    connection: &mut connection,
                    tracker,
                    identity,
                    recovery: None,
                    continuity: &mut continuity,
                },
                &journal,
                &path,
                &mut worker,
                &mut context,
                &mut stopping,
            )
            .await;
            continuity.settle().await?;
            result
        });
        let mut reader = BufReader::new(host_read);
        let mut line = String::new();
        let received = tokio::time::timeout(
            std::time::Duration::from_millis(if moved { 200 } else { 2000 }),
            reader.read_line(&mut line),
        )
        .await;
        if moved {
            assert!(received.is_err(), "a later turn must not be interrupted");
        } else {
            received??;
            let request: Value = serde_json::from_str(&line)?;
            assert_eq!(request["method"], "turn/interrupt");
            assert_eq!(request["params"]["turnId"], target.to_string());
            assert_eq!(request["params"]["sessionId"], "session");
        }
        let _ = stop.send(true);
        running.await??;
    }
    Ok(())
}

#[tokio::test]
async fn a_queued_start_of_a_held_correction_cannot_override_a_newer_stop() -> Result<()> {
    use std::sync::{Arc, Mutex, atomic::Ordering};
    for newer_stop in [false, true] {
        let (root, journal, mut worker) = fixture().await?;
        let turn = worker.active_turn.unwrap();
        let input = correction(&journal, &worker).await?;
        checkpoint(&mut worker, turn);
        saved_checkpoint(&journal, &worker, turn).await?;
        worker.phase = "held".into();
        journal.worker_state(worker.clone()).await?;
        journal
            .resolve_input(
                Uuid::now_v7(),
                input.clone(),
                1,
                Resolution::Retry {
                    target: Target::Lock(worker.track_id.clone()),
                },
            )
            .await?;
        let (_incoming, read) = tokio::io::duplex(4096);
        let (write, _outgoing) = tokio::io::duplex(4096);
        let mut connection = intercat_daemon::msp::Connection::new(read, write);
        let (port, mut commands) = intercat_daemon::execution::Port::new(std::sync::Arc::new(
            intercat_daemon::dispatch_control::Gate::new(false),
        ));
        port.stop(Uuid::now_v7())?;
        let _ = commands.try_recv()?;
        port.attach(connection.client.clone(), "session".into(), None);
        let start_id = Uuid::now_v7();
        let start_port = port.clone();
        let start = tokio::spawn(async move { start_port.start(start_id).await });
        while commands.is_empty() {
            tokio::task::yield_now().await;
        }
        if newer_stop {
            port.stop(Uuid::now_v7())?;
        }
        let mut context = intercat_daemon::execution::Context {
            project_secrets: Default::default(),
            port: port.clone(),
            commands,
            editor: intercat_daemon::editor::Manager::new(
                root.path(),
                &root.path().join("editor.log"),
            )?,
            capacity: Arc::new(intercat_daemon::scheduler::Capacity::new(1)),
        };
        let identity = intercat_daemon::process::identity(std::process::id())?;
        let tracker = Arc::new(Mutex::new(intercat_daemon::process::Tracker::new(
            "redirect-resume".into(),
            identity,
        )?));
        let (shutdown, mut stopping) = tokio::sync::watch::channel(false);
        let path = root.path().to_owned();
        let observed = journal.clone();
        let running = tokio::spawn(async move {
            let mut continuity = intercat_daemon::execution::Continuity::default();
            let result = intercat_daemon::execution::run(
                intercat_daemon::execution::WorkerIo {
                    connection: &mut connection,
                    tracker,
                    identity,
                    recovery: None,
                    continuity: &mut continuity,
                },
                &observed,
                &path,
                &mut worker,
                &mut context,
                &mut stopping,
            )
            .await;
            continuity.settle().await?;
            result
        });
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), start).await??;
        assert_eq!(result.is_err(), newer_stop, "{result:?}");
        assert_eq!(port.stopped.load(Ordering::Acquire), newer_stop);
        if newer_stop {
            assert!(journal.operation(start_id).await?.is_none());
        } else {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if journal
                        .operation(start_id)
                        .await?
                        .is_some_and(|operation| operation["state"] == "complete")
                    {
                        return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await??;
        }
        let _ = shutdown.send(true);
        running.await??;
    }
    Ok(())
}

#[tokio::test]
async fn undo_take_back_keeps_the_original_stop_boundary_and_redo_stops_after_interrupt()
-> Result<()> {
    for changed_turn in [false, true] {
        let (_root, journal, mut worker) = fixture().await?;
        let input = correction(&journal, &worker).await?;
        journal.cut_input(Uuid::now_v7(), input.clone()).await?;
        let cut = journal.snapshot().await?;
        let entry = cut["editHistory"]["undo"]["entryId"]
            .as_str()
            .unwrap()
            .to_owned();
        if changed_turn {
            worker.active_turn = Some(Uuid::now_v7());
            journal.worker_state(worker.clone()).await?;
        }
        let result = journal
            .edit_history(intercat_daemon::timeline_history::Request {
                op_id: Uuid::now_v7(),
                action: intercat_daemon::timeline_history::Action::Undo { entry },
            })
            .await;
        if changed_turn {
            assert!(result.is_err());
            assert_eq!(word(&journal.snapshot().await?, &input)["state"], "cut");
            let draft = journal
                .edit_history(intercat_daemon::timeline_history::Request {
                    op_id: Uuid::now_v7(),
                    action: intercat_daemon::timeline_history::Action::RestoreDraft {
                        input: input.clone(),
                        revision: 2,
                    },
                })
                .await?;
            assert_eq!(word(&draft, &input)["state"], "held");
            assert!(word(&draft, &input)["decision"].is_null());
            assert!(journal.claim_redirect(input, 3).await?.is_none());
        } else {
            let restored = result?;
            let revision = word(&restored, &input)["revision"].as_i64().unwrap();
            let redirect = journal
                .claim_redirect(input.clone(), revision)
                .await?
                .unwrap();
            assert_eq!(redirect.turn_id, worker.active_turn.unwrap());
            assert!(
                journal
                    .edit_history(intercat_daemon::timeline_history::Request {
                        op_id: Uuid::now_v7(),
                        action: intercat_daemon::timeline_history::Action::Redo {
                            entry: restored["editHistory"]["redo"]["entryId"]
                                .as_str()
                                .unwrap()
                                .into()
                        },
                    })
                    .await
                    .is_err()
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn holding_an_interrupted_correction_does_not_make_its_stop_reversible() -> Result<()> {
    let (root, journal, worker) = fixture().await?;
    let input = correction(&journal, &worker).await?;
    journal.claim_redirect(input.clone(), 1).await?.unwrap();
    journal
        .resolve_input(Uuid::now_v7(), input.clone(), 1, Resolution::Hold)
        .await?;
    let held = journal.snapshot().await?;
    assert_eq!(word(&held, &input)["state"], "held");
    assert!(
        held["editHistory"]["undo"]["blocked"]
            .as_str()
            .unwrap()
            .contains("has started")
    );
    assert!(
        journal
            .edit_history(intercat_daemon::timeline_history::Request {
                op_id: Uuid::now_v7(),
                action: intercat_daemon::timeline_history::Action::Undo {
                    entry: held["editHistory"]["undo"]["entryId"]
                        .as_str()
                        .unwrap()
                        .into()
                },
            })
            .await
            .is_err()
    );
    drop(journal);
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    assert_eq!(
        word(&journal.snapshot().await?, &input)["interruptStarted"],
        true
    );
    assert!(
        journal
            .cut_input(Uuid::now_v7(), input.clone())
            .await
            .is_err()
    );
    assert!(
        journal
            .edit_input(intercat_daemon::edit_hold::Request {
                owner: Uuid::now_v7(),
                action: intercat_daemon::edit_hold::Action::Begin {
                    token: Uuid::now_v7(),
                    input,
                    revision: 2
                },
            })
            .await
            .is_err()
    );
    Ok(())
}
