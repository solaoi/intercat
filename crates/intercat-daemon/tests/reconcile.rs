use intercat_daemon::{
    Result,
    clock::Stamp,
    history::Context,
    journal::Journal,
    msp::{Connection, Event},
    questions::{Phase, Prompt},
    reconcile::{self, Expected},
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

struct Step {
    method: &'static str,
    result: Value,
    notifications: Vec<Value>,
}
fn step(method: &'static str, result: Value) -> Step {
    Step {
        method,
        result,
        notifications: vec![],
    }
}
fn peer(steps: Vec<Step>) -> (Connection, tokio::task::JoinHandle<Vec<Value>>) {
    let (input, mut output) = tokio::io::duplex(65536);
    let (write, read) = tokio::io::duplex(65536);
    let server = tokio::spawn(async move {
        let mut read = BufReader::new(read);
        let mut calls = Vec::new();
        for step in steps {
            let mut line = String::new();
            if read.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["method"], step.method);
            if step.method == "session/read" {
                assert_eq!(request["params"]["excludeItems"], true);
            }
            for value in step.notifications {
                output
                    .write_all(format!("{value}\n").as_bytes())
                    .await
                    .unwrap();
            }
            let result = if step.result.is_null() {
                json!({"commandId":request["params"]["commandId"],"turnId":request["params"]["turnId"],"status":"accepted"})
            } else {
                step.result
            };
            output
                .write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            calls.push(request);
        }
        calls
    });
    (Connection::new(input, write), server)
}
fn context(journal: &Journal) -> Context<'_> {
    Context {
        journal,
        track: "track",
        session: "session",
        scanner: None,
    }
}
async fn anchor(journal: &Journal) -> Result<()> {
    journal.try_msp("track".into(),"session".into(),Stamp::now(),json!({"method":"session/updated","params":{"sessionId":"session","viewCursor":"head","sourceRange":{}}}),None,None)?.await??;
    Ok(())
}

#[tokio::test]
async fn retracted_turn_is_not_active_after_restart() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let turn = Uuid::now_v7();
    owned(&journal, turn).await?;
    journal
        .try_msp(
            "track".into(),
            "session".into(),
            Stamp::now(),
            json!({"method":"turn/retracted","params":{"sessionId":"session",
            "turnId":turn,"commandId":turn,"outcome":"accepted",
            "viewCursor":"retracted","sourceRange":{}}}),
            None,
            None,
        )?
        .await??;
    anchor(&journal).await?;
    drop(journal);
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let mut snapshot = read(None, vec![]);
    snapshot["session"]["turnCount"] = json!(1);
    let (mut connection, server) = peer(stable(snapshot, json!({"approvals":[],"userInputs":[]})));
    let state = reconcile::prepare(&mut connection, context(&journal), Expected::default()).await?;
    assert!(state.active_turn.is_none() && state.resume_turn.is_none());
    assert_eq!(server.await?.len(), 4);
    assert_ne!(
        journal.operation(turn).await?.unwrap()["state"],
        "superseded"
    );
    Ok(())
}

#[tokio::test]
async fn usage_decimal_preserves_the_wire_value_through_storage() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let (incoming, mut output) = tokio::io::duplex(4096);
    let mut connection = Connection::new(incoming, tokio::io::sink());
    output.write_all(br#"{"jsonrpc":"2.0","method":"session/tokenUsage","params":{"sessionId":"session","viewCursor":"numeric","sourceRange":{},"cumulative":{"cost":{"usd":0.0026103860000000006}}}}
"#).await?;
    let Some(Event::Frame(received)) = connection.next_event().await else {
        panic!("Missing usage");
    };
    assert_eq!(
        received.frame["params"]["cumulative"]["cost"]["usd"]
            .as_f64()
            .unwrap()
            .to_bits(),
        0.0026103860000000006_f64.to_bits()
    );
    journal
        .try_msp(
            "track".into(),
            "session".into(),
            Stamp::now(),
            received.frame,
            Some(received.lease),
            None,
        )?
        .await??;
    let local = journal.reconcile_local("session".into()).await?;
    assert_eq!(
        local.view.session_state["session/tokenUsage"]["cumulative"]["cost"]["usd"]
            .as_f64()
            .unwrap()
            .to_bits(),
        0.0026103860000000006_f64.to_bits()
    );
    Ok(())
}

#[tokio::test]
async fn revised_terminal_survives_restart_and_is_replayed() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let turn = Uuid::now_v7();
    owned(&journal, turn).await?;
    let completion = |cursor: &str, terminal: &str| {
        json!({"method":"turn/completed","params":{"sessionId":"session",
            "turnId":turn,"viewCursor":cursor,"sourceRange":{},"terminal":terminal}})
    };
    for frame in [
        completion("failed", "failed"),
        completion("cancelled", "cancelled"),
        completion("failed", "failed"),
    ] {
        journal
            .try_msp(
                "track".into(),
                "session".into(),
                Stamp::now(),
                frame,
                None,
                None,
            )?
            .await??;
    }
    drop(journal);
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let local = journal.reconcile_local("session".into()).await?;
    let replay = journal.reconcile_replay("session".into(), turn).await?;
    assert_eq!(replay.len(), 1);
    assert_eq!(
        (
            local.view.turns[&turn].terminal.as_deref(),
            replay[0].0["params"]["terminal"].as_str()
        ),
        (Some("cancelled"), Some("cancelled"))
    );
    Ok(())
}

#[tokio::test]
async fn nonterminal_hints_do_not_invent_a_start_or_completion() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let mut turns = Vec::new();
    for method in [
        "turn/foregroundCompleted",
        "turn/retryScheduled",
        "turn/modelRetryScheduled",
    ] {
        let turn = Uuid::now_v7();
        turns.push(turn);
        journal
            .try_msp(
                "track".into(),
                "session".into(),
                Stamp::now(),
                json!({"method":method,"params":{"sessionId":"session","turnId":turn}}),
                None,
                None,
            )?
            .await??;
    }
    let local = journal.reconcile_local("session".into()).await?;
    assert_eq!(local.view.turns.len(), turns.len());
    for id in turns {
        assert!(local.view.turns[&id].command.is_none());
        assert!(local.view.turns[&id].terminal.is_none());
    }
    assert!(local.operations.is_empty());
    Ok(())
}
async fn owned(journal: &Journal, turn: Uuid) -> Result<()> {
    journal
        .intent(
            &turn.to_string(),
            "turn.start",
            json!({"sessionId":"session","commandId":turn}),
        )
        .await?;
    journal.try_msp("track".into(),"session".into(),Stamp::now(),json!({"method":"turn/started","params":{"sessionId":"session","viewCursor":"start","sourceRange":{},"turnId":turn,"commandId":turn}}),None,None)?.await??;
    Ok(())
}
fn read(active: Option<Uuid>, requests: Vec<Value>) -> Value {
    json!({"session":{"sessionId":"session","activeTurnId":active,"turnCount":u8::from(active.is_some())},"viewCursor":"head","sourceRange":{},"history":{"mode":"snapshot","snapshot":{"schemaVersion":1,"viewCursor":"head","state":{
        "items":[],"activeTurn":active.map(|id|json!({"turnId":id,"commandId":id})),"queuedTurns":[],
        "pendingApprovals":[],"pendingUserInputs":[],
        "approvalMode":{"mode":"promptUnmatched","source":"startup","lastCommandId":null},
        "effectiveModel":null,"goal":null,"todoList":null,"branch":null,"name":null,
        "tokenUsage":{"promptTokens":0,"outputTokens":0,"totalTokens":0},"turnCount":0
    }}},"pendingRequests":requests})
}
fn approval(turn: Uuid, stage: &str) -> Value {
    json!({"sessionId":"session","approvalId":"approval","turnId":turn,"currentRequirementId":stage,"subject":{"kind":"shell","command":"echo recovered"},"options":[{"id":"allow_once"},{"id":"abort"}]})
}
fn prompt(turn: Uuid) -> Value {
    json!({"sessionId":"session","userInputId":"question","turnId":turn,"itemId":"item","toolCallId":"call","toolName":"request_user_input","viewCursor":"question-cursor","questions":[{"id":"one","header":"Choice","question":"Choose one","selection":{"mode":"single"},"options":[{"label":"First"},{"label":"Second"}]}]})
}
fn stable(read: Value, pending: Value) -> Vec<Step> {
    vec![
        step("session/read", read.clone()),
        step("approval/listPending", pending.clone()),
        step("session/read", read),
        step("approval/listPending", pending),
    ]
}

#[tokio::test]
async fn pending_requests_replace_stale_frames_and_preserve_inflight_question_answers() -> Result<()>
{
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let turn = Uuid::now_v7();
    owned(&journal, turn).await?;
    anchor(&journal).await?;
    let request = approval(turn, "stage-2");
    let prompt = prompt(turn);
    let read = read(Some(turn), vec![request.clone(), prompt.clone()]);
    let mut steps = stable(
        read,
        json!({"approvals":[request],"userInputs":[prompt.clone()]}),
    );
    steps[0].notifications.push(
        json!({"jsonrpc":"2.0","method":"approval/request","params":approval(turn,"stage-1")}),
    );
    let (mut connection, server) = peer(steps);
    let state = reconcile::prepare(&mut connection, context(&journal), Expected::default()).await?;
    assert_eq!(state.active_turn, Some(turn));
    let mut old = Prompt::parse(prompt)?;
    old.phase = Phase::AwaitingSettlement;
    old.operation = Some(Uuid::now_v7());
    let restored = state.restore_prompts(&[old.clone()]);
    assert_eq!(restored[0].phase, Phase::AwaitingSettlement);
    assert_eq!(restored[0].operation, old.operation);
    let mut approvals = Vec::new();
    while let Ok(Event::Frame(frame)) = connection.try_next_event() {
        if frame.frame["method"] == "approval/request" {
            approvals.push(frame.frame["params"]["currentRequirementId"].clone());
        }
    }
    assert_eq!(approvals, [json!("stage-2")]);
    assert!(server.await?.iter().all(|call| !matches!(
        call["method"].as_str(),
        Some("approval/decide" | "turn/start")
    )));
    Ok(())
}

#[tokio::test]
async fn decided_requirement_is_never_replayed_and_a_new_stage_is_distinct() -> Result<()> {
    for stage in ["old", "new"] {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(
            root.path().join("db"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        let turn = Uuid::now_v7();
        owned(&journal, turn).await?;
        anchor(&journal).await?;
        let op = Uuid::now_v7();
        journal
            .intent(
                &op.to_string(),
                "approval.decide",
                json!({"sessionId":"session","approvalId":"approval","requirementId":"old"}),
            )
            .await?;
        journal.finish(&op.to_string(), "dispatched").await?;
        journal.finish(&op.to_string(), "complete").await?;
        let (mut connection, server) = peer(stable(
            read(Some(turn), vec![approval(turn, stage)]),
            json!({"approvals":[approval(turn,stage)],"userInputs":[]}),
        ));
        let state =
            reconcile::prepare(&mut connection, context(&journal), Expected::default()).await?;
        assert_eq!(
            state
                .decided_approvals
                .contains(&reconcile::approval_key("approval", &json!(stage))),
            stage == "old"
        );
        server.await?;
    }
    Ok(())
}

#[tokio::test]
async fn changing_requirement_is_reread_before_releasing_the_session() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let turn = Uuid::now_v7();
    owned(&journal, turn).await?;
    anchor(&journal).await?;
    let snapshot = read(Some(turn), vec![approval(turn, "new")]);
    let mut steps = stable(
        snapshot.clone(),
        json!({"approvals":[approval(turn,"new")],"userInputs":[]}),
    );
    steps[1].result = json!({"approvals":[approval(turn,"old")],"userInputs":[]});
    steps.extend(stable(
        snapshot,
        json!({"approvals":[approval(turn,"new")],"userInputs":[]}),
    ));
    let (mut connection, server) = peer(steps);
    reconcile::prepare(&mut connection, context(&journal), Expected::default()).await?;
    assert_eq!(server.await?.len(), 8);
    Ok(())
}

#[tokio::test]
async fn only_an_owned_queued_turn_is_reclaimed_and_the_start_is_held() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let command = Uuid::now_v7();
    let turn = Uuid::now_v7();
    journal
        .intent(
            &command.to_string(),
            "turn.start",
            json!({"sessionId":"session","commandId":command}),
        )
        .await?;
    anchor(&journal).await?;
    let mut queued = read(None, vec![]);
    queued["history"]["snapshot"]["state"]["queuedTurns"] =
        json!([{"turnId":turn,"commandId":command}]);
    queued["queuedInputs"] = json!([{"text":"queued text","sequence":1}]);
    let mut steps = vec![
        step("session/read", queued),
        step("turn/unqueue", Value::Null),
    ];
    steps.extend(stable(
        read(None, vec![]),
        json!({"approvals":[],"userInputs":[]}),
    ));
    let (mut connection, server) = peer(steps);
    let state = reconcile::prepare(&mut connection, context(&journal), Expected::default()).await?;
    assert!(state.active_turn.is_none());
    let calls = server.await?;
    assert_eq!(calls[1]["params"]["turnId"], turn.to_string());
    let reclaim: Uuid = calls[1]["params"]["commandId"].as_str().unwrap().parse()?;
    assert_eq!(reclaim.get_version_num(), 7);
    assert_eq!(
        journal.operation(reclaim).await?.unwrap()["state"],
        "complete"
    );
    assert_eq!(
        journal.operation(command).await?.unwrap()["state"],
        "superseded"
    );
    Ok(())
}

#[tokio::test]
async fn foreign_queue_or_unknown_runtime_never_dispatches_a_command() -> Result<()> {
    for foreign in [true, false] {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(
            root.path().join("db"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        anchor(&journal).await?;
        let mut snapshot = read(None, vec![]);
        let steps = if foreign {
            snapshot["history"]["snapshot"]["state"]["queuedTurns"] =
                json!([{"turnId":Uuid::now_v7(),"commandId":Uuid::now_v7()}]);
            vec![step("session/read", snapshot)]
        } else {
            snapshot["history"] = json!({"mode":"none"});
            snapshot["session"]["turnCount"] = json!(8);
            stable(snapshot, json!({"approvals":[],"userInputs":[]}))
        };
        let (mut connection, server) = peer(steps);
        assert!(
            reconcile::prepare(&mut connection, context(&journal), Expected::default())
                .await
                .is_err()
        );
        assert!(
            server
                .await?
                .iter()
                .all(|call| call["method"] != "turn/unqueue")
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_completed_owned_turn_is_restored_for_checkpoint_without_a_second_start() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let turn = Uuid::now_v7();
    owned(&journal, turn).await?;
    anchor(&journal).await?;
    let mut snapshot = read(None, vec![]);
    snapshot["lastTurn"] = json!({"turnId":turn,"terminal":"cancelled"});
    snapshot["session"]["turnCount"] = json!(1);
    let (mut connection, server) = peer(stable(snapshot, json!({"approvals":[],"userInputs":[]})));
    let state = reconcile::prepare(
        &mut connection,
        context(&journal),
        Expected {
            turn: Some(turn),
            ..Expected::default()
        },
    )
    .await?;
    assert!(state.active_turn.is_none());
    assert_eq!(state.resume_turn, Some(turn));
    assert_eq!(state.command, Some(turn));
    let mut completed = 0;
    while let Ok(Event::Frame(frame)) = connection.try_next_event() {
        if frame.frame["method"] == "turn/completed" {
            completed += 1;
        }
    }
    assert_eq!(completed, 1);
    assert_eq!(server.await?.len(), 4);
    Ok(())
}

#[tokio::test]
async fn an_already_resolved_approval_cannot_reappear_as_a_new_permission() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let turn = Uuid::now_v7();
    owned(&journal, turn).await?;
    journal.try_msp("track".into(),"session".into(),Stamp::now(),json!({"method":"approval/resolved","params":{"sessionId":"session","viewCursor":"resolved","sourceRange":{},"approvalId":"approval"}}),None,None)?.await??;
    anchor(&journal).await?;
    let (mut connection, server) = peer(stable(
        read(Some(turn), vec![approval(turn, "old")]),
        json!({"approvals":[approval(turn,"old")],"userInputs":[]}),
    ));
    assert!(
        reconcile::prepare(&mut connection, context(&journal), Expected::default())
            .await
            .is_err()
    );
    server.await?;
    Ok(())
}

#[tokio::test]
async fn structured_requirement_ids_match_both_work_and_repair_decision_records() -> Result<()> {
    for kind in ["approval.decide", "approval/decide"] {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(
            root.path().join("db"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        let turn = Uuid::now_v7();
        owned(&journal, turn).await?;
        anchor(&journal).await?;
        let requirement = json!({"approvalId":"approval","sourceIndex":0});
        let record =
            json!({"sessionId":"session","approvalId":"approval","requirementId":requirement});
        journal
            .intent(
                &Uuid::now_v7().to_string(),
                kind,
                if kind == "approval.decide" {
                    record.clone()
                } else {
                    json!({"request":record,"requestHash":"test","controlHash":"test"})
                },
            )
            .await?;
        let mut request = approval(turn, "unused");
        request["currentRequirementId"] = requirement.clone();
        let (mut connection, server) = peer(stable(
            read(Some(turn), vec![request.clone()]),
            json!({"approvals":[request],"userInputs":[]}),
        ));
        let state =
            reconcile::prepare(&mut connection, context(&journal), Expected::default()).await?;
        assert!(
            state
                .decided_approvals
                .contains(&reconcile::approval_key("approval", &requirement))
        );
        server.await?;
    }
    Ok(())
}

#[tokio::test]
async fn reclaim_reuses_its_saved_identity_and_waits_for_removal_before_completion() -> Result<()> {
    for accepted in [false, true] {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(
            root.path().join("db"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        let command = Uuid::now_v7();
        let turn = Uuid::now_v7();
        let reclaim = Uuid::now_v7();
        journal
            .intent(
                &command.to_string(),
                "turn.start",
                json!({"sessionId":"session","commandId":command}),
            )
            .await?;
        journal
            .intent(
                &reclaim.to_string(),
                "turn.unqueue",
                json!({"sessionId":"session","commandId":reclaim,"turnId":turn,"reclaimedStart":command}),
            )
            .await?;
        if accepted {
            journal.finish(&reclaim.to_string(), "dispatched").await?;
        }
        anchor(&journal).await?;
        let mut queued = read(None, vec![]);
        queued["history"]["snapshot"]["state"]["queuedTurns"] =
            json!([{"turnId":turn,"commandId":command}]);
        let mut steps = vec![step("session/read", queued)];
        if !accepted {
            steps.push(step("turn/unqueue", Value::Null));
        }
        steps.extend(stable(
            read(None, vec![]),
            json!({"approvals":[],"userInputs":[]}),
        ));
        let (mut connection, server) = peer(steps);
        reconcile::prepare(&mut connection, context(&journal), Expected::default()).await?;
        let calls = server.await?;
        let sent: Vec<_> = calls
            .iter()
            .filter(|c| c["method"] == "turn/unqueue")
            .collect();
        assert_eq!(sent.len(), usize::from(!accepted));
        if !accepted {
            assert_eq!(sent[0]["params"]["commandId"], reclaim.to_string());
        }
        assert_eq!(
            journal.operation(reclaim).await?.unwrap()["state"],
            "complete"
        );
    }
    Ok(())
}

#[tokio::test]
async fn unattributed_queued_input_is_held_without_sending_any_action() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    anchor(&journal).await?;
    let mut snapshot = read(None, vec![]);
    snapshot["queuedInputs"] = json!([{"text":"unattributed","sequence":9}]);
    let (mut connection, server) = peer(stable(snapshot, json!({"approvals":[],"userInputs":[]})));
    assert!(
        reconcile::prepare(&mut connection, context(&journal), Expected::default())
            .await
            .is_err()
    );
    assert_eq!(server.await?.len(), 4);
    Ok(())
}

#[tokio::test]
async fn recorded_pinned_muse_pending_state_keeps_its_shared_cursor_notifications() -> Result<()> {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/reconcile-live.json"))?;
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let command: Uuid = fixture["commandId"].as_str().unwrap().parse()?;
    journal
        .intent(
            &command.to_string(),
            "turn.start",
            json!({"commandId":command,"sessionId":"session"}),
        )
        .await?;
    for frame in fixture["frames"].as_array().unwrap() {
        journal
            .try_msp(
                "track".into(),
                "session".into(),
                Stamp::now(),
                frame.clone(),
                None,
                None,
            )?
            .await??;
    }
    let (mut connection, server) =
        peer(stable(fixture["read"].clone(), fixture["pending"].clone()));
    let state = reconcile::prepare(
        &mut connection,
        context(&journal),
        Expected {
            turn: Some(command),
            ..Expected::default()
        },
    )
    .await?;
    assert_eq!(state.active_turn, Some(command));
    assert_eq!(state.pending_approvals, 1);
    server.await?;
    Ok(())
}

#[tokio::test]
async fn unfinished_turn_and_checkpoint_cannot_disappear_during_recovery() -> Result<()> {
    for missing_turn in [true, false] {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(
            root.path().join("db"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        anchor(&journal).await?;
        let (mut connection, server) = peer(stable(
            read(None, vec![]),
            json!({"approvals":[],"userInputs":[]}),
        ));
        let error = reconcile::prepare(
            &mut connection,
            context(&journal),
            Expected {
                turn: missing_turn.then(Uuid::now_v7),
                checkpoint_pending: true,
                ..Expected::default()
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains(if missing_turn {
            "outcome is still unknown"
        } else {
            "checkpoint is still unresolved"
        }));
        assert_eq!(server.await?.len(), 4);
    }
    Ok(())
}

#[tokio::test]
async fn another_owned_active_turn_cannot_replace_an_unfinished_turn() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let current = Uuid::now_v7();
    owned(&journal, current).await?;
    anchor(&journal).await?;
    let (mut connection, server) = peer(stable(
        read(Some(current), vec![]),
        json!({"approvals":[],"userInputs":[]}),
    ));
    let error = reconcile::prepare(
        &mut connection,
        context(&journal),
        Expected {
            turn: Some(Uuid::now_v7()),
            checkpoint_pending: true,
            ..Expected::default()
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("previous unfinished turn"));
    assert_eq!(server.await?.len(), 4);
    Ok(())
}

#[tokio::test]
async fn contradictory_history_facts_never_replace_an_established_turn_identity() -> Result<()> {
    for (method, first, next) in [
        (
            "turn/started",
            json!({"commandId":Uuid::now_v7()}),
            json!({"commandId":Uuid::now_v7()}),
        ),
        (
            "turn/completed",
            json!({"terminal":"completed"}),
            json!({"terminal":"unknown"}),
        ),
    ] {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(
            root.path().join("db"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        let turn = Uuid::now_v7();
        for (cursor, mut params) in [("first", first), ("next", next)] {
            params["sessionId"] = json!("session");
            params["turnId"] = json!(turn);
            params["viewCursor"] = json!(cursor);
            params["sourceRange"] = json!({});
            journal
                .try_msp(
                    "track".into(),
                    "session".into(),
                    Stamp::now(),
                    json!({"method":method,"params":params}),
                    None,
                    None,
                )?
                .await??;
        }
        anchor(&journal).await?;
        let (mut connection, server) = peer(stable(
            read(None, vec![]),
            json!({"approvals":[],"userInputs":[]}),
        ));
        assert!(
            reconcile::prepare(&mut connection, context(&journal), Expected::default())
                .await
                .is_err()
        );
        assert_eq!(server.await?.len(), 4);
    }
    Ok(())
}

#[tokio::test]
async fn snapshot_cannot_revive_a_turn_already_completed_in_history() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let turn = Uuid::now_v7();
    owned(&journal, turn).await?;
    journal.try_msp("track".into(),"session".into(),Stamp::now(),json!({"method":"turn/completed","params":{"sessionId":"session","turnId":turn,"terminal":"completed","viewCursor":"done","sourceRange":{}}}),None,None)?.await??;
    anchor(&journal).await?;
    let (mut connection, server) = peer(stable(
        read(Some(turn), vec![]),
        json!({"approvals":[],"userInputs":[]}),
    ));
    let error = reconcile::prepare(&mut connection, context(&journal), Expected::default())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("snapshot contradicts"));
    drop(connection);
    assert_eq!(server.await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn reconcile_persists_the_served_snapshot_without_dispatching() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    anchor(&journal).await?;
    let mut served = read(None, vec![]);
    served["history"]["snapshot"]["state"]["items"] = json!([{
        "itemId":"served","kind":"agentMessage","revision":1,"status":"completed",
        "recordedAt":"2026-11-05T00:00:00Z","text":"from the host snapshot"
    }]);
    served["history"]["snapshot"]["state"]["futureState"] = json!({"preserved":true});
    let (mut connection, server) = peer(stable(served, json!({"approvals":[],"userInputs":[]})));
    let state = reconcile::prepare(&mut connection, context(&journal), Expected::default()).await?;
    assert!(state.active_turn.is_none());
    let calls = server.await?;
    assert_eq!(
        calls
            .iter()
            .map(|c| c["method"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "session/read",
            "approval/listPending",
            "session/read",
            "approval/listPending"
        ]
    );
    let value = journal
        .reconcile_local("session".into())
        .await?
        .view
        .snapshot_state()
        .unwrap();
    assert_eq!(value["items"][0]["text"], "from the host snapshot");
    drop(connection);
    drop(journal);
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert_eq!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .snapshot_state(),
        Some(value)
    );
    Ok(())
}

#[tokio::test]
async fn pending_requests_from_another_turn_cannot_become_actionable() -> Result<()> {
    for question in [false, true] {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(
            root.path().join("db"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        let turn = Uuid::now_v7();
        owned(&journal, turn).await?;
        anchor(&journal).await?;
        let foreign = Uuid::now_v7();
        let (request, pending) = if question {
            let request = prompt(foreign);
            (
                request.clone(),
                json!({"approvals":[],"userInputs":[request]}),
            )
        } else {
            let request = approval(foreign, "stage");
            (
                request.clone(),
                json!({"approvals":[request],"userInputs":[]}),
            )
        };
        let (mut connection, server) = peer(stable(read(Some(turn), vec![request]), pending));
        let error = reconcile::prepare(&mut connection, context(&journal), Expected::default())
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not belong to the active turn")
        );
        assert_eq!(server.await?.len(), 4);
    }
    Ok(())
}

#[tokio::test]
async fn resume_restates_terminal_without_starting_another_turn() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let turn = Uuid::now_v7();
    owned(&journal, turn).await?;
    for (cursor, terminal, reason) in [
        ("failed", "failed", "crash_inferred"),
        (
            "cancelled",
            "cancelled",
            "resume_reconcile:orphaned_by_process_loss",
        ),
    ] {
        journal.try_msp("track".into(),"session".into(),Stamp::now(),json!({"method":"turn/completed","params":{"sessionId":"session","turnId":turn,"terminal":terminal,"reason":reason,"viewCursor":cursor,"sourceRange":{}}}),None,None)?.await??;
    }
    anchor(&journal).await?;
    let local = journal.reconcile_local("session".into()).await?;
    assert_eq!(
        local.view.turns[&turn].terminal.as_deref(),
        Some("cancelled")
    );
    let mut value = read(None, vec![]);
    value["lastTurn"] = json!({"turnId":turn,"terminal":"cancelled","reason":"resume_reconcile:orphaned_by_process_loss"});
    let (mut connection, server) = peer(stable(value, json!({"approvals":[],"userInputs":[]})));
    let recovered = reconcile::prepare(
        &mut connection,
        context(&journal),
        Expected {
            turn: Some(turn),
            checkpoint_pending: true,
            ..Expected::default()
        },
    )
    .await?;
    assert_eq!(recovered.resume_turn, Some(turn));
    assert!(recovered.active_turn.is_none());
    let mut terminal_frames = vec![];
    while let Ok(Event::Frame(frame)) = connection.try_next_event() {
        if frame.frame["method"] == "turn/completed" {
            terminal_frames.push(frame.frame);
        }
    }
    assert_eq!(terminal_frames.len(), 1);
    assert_eq!(terminal_frames[0]["params"]["terminal"], "cancelled");
    assert!(
        server
            .await?
            .iter()
            .all(|frame| frame["method"] != "turn/start")
    );
    Ok(())
}
