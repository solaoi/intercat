use intercat_daemon::{
    Result,
    checkpoint::Checkpoint,
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    preparation::Workspace,
    proposals::{Action, Command},
    reports::{Analysis, Report},
    runtime::WorkerState,
    secrets::Key,
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal, WorkerState)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Dummy proposal work".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: Some("now".into()),
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
    journal
        .workspace_result(Workspace {
            track_id: track.clone(),
            path: root.path().join("dummy-worktree"),
            reference: "refs/intercat/dummy".into(),
            base: "b".repeat(40),
            state: "ready".into(),
            error: None,
            attachment_roots: vec![],
        })
        .await?;
    let worker = WorkerState {
        track_id: track,
        phase: "working".into(),
        session_id: Some("dummy-session".into()),
        active_turn: Some(Uuid::now_v7()),
        secret_revision: None,
        identity: None,
        detail: None,
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    journal.worker_state(worker.clone()).await?;
    Ok((root, journal, worker))
}
async fn put(journal: &Journal, worker: &WorkerState, frame: Value) -> Result<()> {
    journal
        .try_msp(
            worker.track_id.clone(),
            worker.session_id.clone().unwrap(),
            Stamp::now(),
            frame,
            None,
            None,
        )?
        .await??;
    Ok(())
}
async fn report(journal: &Journal, worker: &WorkerState, text: &str, revision: i64) -> Result<()> {
    put(journal,worker,json!({"method":"item/completed","params":{"sessionId":worker.session_id,"item":{
        "itemId":"final","turnId":worker.active_turn,"kind":"agentMessage","messagePhase":"finalAnswer","revision":revision,"status":"completed","text":text
    }}})).await
}
async fn finish(journal: &Journal, worker: &mut WorkerState, kind: &str) -> Result<()> {
    report(
        journal,
        worker,
        &format!("Result: {kind} — Dummy remaining work"),
        1,
    )
    .await?;
    let turn = worker.active_turn.unwrap();
    put(journal,worker,json!({"method":"turn/completed","params":{"sessionId":worker.session_id,"turnId":turn,"terminal":"completed"}})).await?;
    let checkpoint = Checkpoint {
        turn_id: turn,
        sha: "a".repeat(40),
        reference: "refs/intercat/dummy-checkpoint".into(),
        unmerged_paths: vec![],
        live_processes: false,
    };
    worker.active_turn = None;
    worker.phase = "ready".into();
    worker.checkpoint = Some(checkpoint.clone());
    journal.worker_state(worker.clone()).await?;
    assert!(
        journal.snapshot().await?["proposal"].is_null(),
        "unsaved checkpoint must not produce a proposal"
    );
    let id = Uuid::now_v7().to_string();
    journal
        .intent(
            &id,
            "checkpoint.save",
            json!({"trackId":worker.track_id,"checkpoint":checkpoint}),
        )
        .await?;
    journal.finish(&id, "dispatched").await?;
    journal.finish(&id, "complete").await?;
    Ok(())
}
fn command(proposal: &Value, action: Action) -> Command {
    Command {
        op_id: Uuid::now_v7(),
        proposal_id: proposal["id"].as_str().unwrap().parse().unwrap(),
        action,
    }
}

#[tokio::test]
async fn only_saved_done_or_partial_reports_offer_two_actions() -> Result<()> {
    for kind in ["done", "partial", "failed", "unknown"] {
        let (_root, journal, mut worker) = fixture().await?;
        finish(&journal, &mut worker, kind).await?;
        let proposal = journal.snapshot().await?["proposal"].clone();
        if matches!(kind, "failed" | "unknown") {
            assert!(proposal.is_null());
            continue;
        }
        assert_eq!(
            proposal["source"]["turnId"],
            worker.checkpoint.as_ref().unwrap().turn_id.to_string()
        );
        assert_eq!(proposal["source"]["result"]["itemId"], "final");
        assert_eq!(proposal["options"].as_array().unwrap().len(), 2);
        assert!(proposal["createdAt"].as_i64().unwrap() > 0);
        assert_eq!(
            proposal["options"][0]["action"],
            if kind == "done" { "commit" } else { "continue" }
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_revised_report_or_new_turn_invalidates_the_old_proposal() -> Result<()> {
    let (_root, journal, mut worker) = fixture().await?;
    finish(&journal, &mut worker, "done").await?;
    let old = journal.snapshot().await?["proposal"].clone();
    let mut source = worker.clone();
    source.active_turn = Some(worker.checkpoint.as_ref().unwrap().turn_id);
    report(&journal, &source, "Result: partial — revised result", 2).await?;
    assert_ne!(journal.snapshot().await?["proposal"]["id"], old["id"]);
    assert!(
        journal
            .answer_proposal(command(&old, Action::Commit))
            .await
            .is_err()
    );
    let current = journal.snapshot().await?["proposal"].clone();
    worker.active_turn = Some(Uuid::now_v7());
    worker.phase = "working".into();
    journal.worker_state(worker).await?;
    assert!(journal.snapshot().await?["proposal"].is_null());
    assert!(
        journal
            .answer_proposal(command(&current, Action::Continue))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn continue_is_recorded_once_and_replays_after_restart_without_reclassifying_the_answer()
-> Result<()> {
    let (root, journal, mut worker) = fixture().await?;
    finish(&journal, &mut worker, "partial").await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    journal.pause_dispatch(Uuid::now_v7(), true).await?;
    let request = command(&proposal, Action::Continue);
    let result = journal.answer_proposal(request.clone()).await?;
    let input = journal
        .read_utterance(result["inputId"].as_str().unwrap().into())
        .await?;
    assert_eq!(input["target"]["trackId"], worker.track_id);
    assert_eq!(input["state"], "held");
    assert!(
        input["text"]
            .as_str()
            .unwrap()
            .contains(&worker.checkpoint.as_ref().unwrap().turn_id.to_string())
    );
    assert!(journal.snapshot().await?["proposal"].is_null());
    assert_eq!(journal.answer_proposal(request.clone()).await?, result);
    let mut conflict = request.clone();
    conflict.action = Action::Commit;
    assert!(journal.answer_proposal(conflict).await.is_err());
    assert!(
        journal
            .answer_proposal(command(&proposal, Action::Continue))
            .await
            .is_err()
    );
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(journal.answer_proposal(request).await?, result);
    assert_eq!(
        journal.snapshot().await?["utterances"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|value| value["id"] == result["inputId"])
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn commit_only_prepares_a_review_and_not_now_stays_dismissed() -> Result<()> {
    for action in [Action::Commit, Action::Later] {
        let (root, journal, mut worker) = fixture().await?;
        finish(&journal, &mut worker, "done").await?;
        let proposal = journal.snapshot().await?["proposal"].clone();
        let request = command(&proposal, action.clone());
        let result = journal.answer_proposal(request.clone()).await?;
        if action == Action::Commit {
            let word = journal
                .read_utterance(result["inputId"].as_str().unwrap().into())
                .await?;
            assert_eq!(word["presentation"]["command"], "commit");
            assert_eq!(word["presentation"]["trackId"], worker.track_id);
            assert_eq!(word["state"], "placed");
        } else {
            assert!(result["inputId"].is_null());
        }
        assert!(journal.snapshot().await?["integration"].is_null());
        drop(journal);
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        assert!(journal.snapshot().await?["proposal"].is_null());
        assert_eq!(journal.answer_proposal(request).await?, result);
    }
    Ok(())
}

#[tokio::test]
async fn report_questions_and_already_integrated_changes_suppress_commit_proposals() -> Result<()> {
    let (_root, journal, mut worker) = fixture().await?;
    finish(&journal, &mut worker, "done").await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    let report = Report::new(
        worker.track_id.clone(),
        worker.session_id.clone().unwrap(),
        worker.checkpoint.as_ref().unwrap().turn_id,
        "questions".into(),
        "Choose a dummy output.".into(),
    );
    journal.observe_report(report.clone()).await?;
    assert!(journal.snapshot().await?["proposal"].is_null());
    journal
        .analyze_report(report.id, 1, Analysis::Clear)
        .await?;
    assert_eq!(journal.snapshot().await?["proposal"]["id"], proposal["id"]);
    journal.append("integration.published",json!({"status":"done","tracks":[{"id":worker.track_id,"checkpoint":worker.checkpoint}],"candidate":{"commits":[{"track":worker.track_id}]}})).await?;
    assert!(journal.snapshot().await?["proposal"].is_null());
    assert!(
        journal
            .answer_proposal(command(&proposal, Action::Commit))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn a_failed_answer_write_rolls_back_the_followup_and_can_be_retried() -> Result<()> {
    let (root, journal, mut worker) = fixture().await?;
    finish(&journal, &mut worker, "partial").await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    let request = command(&proposal, Action::Continue);
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER dummy_proposal_disk_failure BEFORE INSERT ON proposal_answers BEGIN SELECT RAISE(ABORT,'dummy save failure'); END;")?;
    let count = journal.snapshot().await?["utterances"]
        .as_array()
        .unwrap()
        .len();
    assert!(journal.answer_proposal(request.clone()).await.is_err());
    assert_eq!(
        journal.snapshot().await?["utterances"]
            .as_array()
            .unwrap()
            .len(),
        count
    );
    assert_eq!(journal.snapshot().await?["proposal"]["id"], proposal["id"]);
    assert!(journal.operation(request.op_id).await?.is_none());
    db.execute_batch("DROP TRIGGER dummy_proposal_disk_failure;")?;
    assert!(journal.answer_proposal(request).await?["inputId"].is_string());
    Ok(())
}

#[tokio::test]
async fn private_rpc_rejects_invalid_answers_and_accepts_the_current_proposal() -> Result<()> {
    use intercat_daemon::{
        editor::Manager,
        lock::DaemonLock,
        rpc::{Server, Token},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    async fn call(
        stream: &mut BufReader<tokio::net::UnixStream>,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        stream
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":method,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await?;
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            stream.read_line(&mut line),
        )
        .await??;
        Ok(serde_json::from_str(&line)?)
    }
    let (root, journal, mut worker) = fixture().await?;
    finish(&journal, &mut worker, "done").await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    std::fs::create_dir(root.path().join("editor-source"))?;
    let editor = Manager::new(
        &root.path().join("editor-source"),
        &root.path().join("editor.log"),
    )?;
    let socket = tempfile::tempdir_in("/private/tmp")?;
    let _lock = DaemonLock::acquire(socket.path())?.unwrap();
    let token = "a7".repeat(32);
    let server = Server::bind(
        socket.path(),
        Token::parse(&token)?,
        journal.clone(),
        editor.clone(),
        &root.path().join("workspaces"),
    )?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let result = async {
        let mut stream =
            BufReader::new(tokio::net::UnixStream::connect(socket.path().join("rpc.sock")).await?);
        assert!(
            call(&mut stream, "rpc.authenticate", json!({"token":token})).await?["error"].is_null()
        );
        let mut request = serde_json::to_value(command(&proposal, Action::Later))?;
        request["extra"] = json!(true);
        assert_eq!(
            call(&mut stream, "proposal.respond", request).await?["error"]["code"],
            -32602
        );
        let request = command(&proposal, Action::Later);
        let response = call(
            &mut stream,
            "proposal.respond",
            serde_json::to_value(&request)?,
        )
        .await?;
        assert!(response["error"].is_null(), "{response}");
        assert_eq!(response["result"]["action"], "later");
        assert_eq!(
            call(
                &mut stream,
                "proposal.respond",
                serde_json::to_value(&request)?
            )
            .await?["result"],
            response["result"]
        );
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let _ = stop.send(());
    task.await??;
    editor.stop().await;
    result?;
    Ok(())
}
