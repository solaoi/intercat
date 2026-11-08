use intercat_daemon::{
    Result,
    approval::{ReplyMode, Response},
    approval_reply::Request,
    checkpoint::Checkpoint,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    runtime::WorkerState,
    secret_input::Owner,
    secrets::Key,
};
use serde_json::json;
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal, WorkerState, Request)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    journal
        .stage_utterance(Uuid::now_v7(), "Start".into(), Target::New, None, vec![])
        .await?;
    let word = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            word.id,
            word.revision,
            Outcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: None,
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let track = journal.snapshot().await?["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let turn = Uuid::now_v7();
    journal
        .workspace_result(intercat_daemon::preparation::Workspace {
            track_id: track.clone(),
            path: root.path().join("worker"),
            reference: "refs/intercat/reply/tracks/A".into(),
            base: "0".repeat(40),
            state: "ready".into(),
            error: None,
            attachment_roots: vec![],
        })
        .await?;
    let worker = WorkerState {
        track_id: track.clone(),
        phase: "working".into(),
        session_id: Some("session".into()),
        secret_revision: None,
        identity: None,
        detail: None,
        active_turn: Some(turn),
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    journal.worker_state(worker.clone()).await?;
    let request = Request {
        op_id: Uuid::now_v7(),
        owner: Owner::Work,
        track_id: track,
        session_id: "session".into(),
        turn_id: turn,
        approval_id: "approval".into(),
        requirement: json!({"sourceIndex": 2}),
        text: "Keep the folder and explain the problem.".into(),
    };
    Ok((root, journal, worker, request))
}

#[tokio::test]
async fn reply_receipts_are_bound_to_the_exact_worker_or_repair() -> Result<()> {
    let (_root, journal, _worker, request) = fixture().await?;
    let response = Response::Reply {
        text: request.text.clone(),
        mode: ReplyMode::Restart,
    };
    journal.queue_approval_reply(request.clone()).await?;
    let correct = Owner::Work.key(&request.track_id);
    for scope in [
        Owner::Work.key("another"),
        Owner::Repair {
            run_id: Uuid::now_v7(),
        }
        .key(""),
    ] {
        assert!(
            intercat_daemon::approval::replay(
                &journal,
                request.op_id,
                &scope,
                &request.approval_id,
                &request.requirement,
                &response
            )
            .await
            .is_err()
        );
    }
    assert!(
        intercat_daemon::approval::replay(
            &journal,
            request.op_id,
            &correct,
            &request.approval_id,
            &request.requirement,
            &response
        )
        .await?
    );
    let id = Uuid::now_v7();
    let response = Response::Reply {
        text: "Keep this file.".into(),
        mode: ReplyMode::Feedback,
    };
    journal.intent(&id.to_string(), "approval.decide", json!({"scope":correct,"approvalId":"approval","requirementId":request.requirement,"response":response})).await?;
    journal.finish(&id.to_string(), "dispatched").await?;
    journal.finish(&id.to_string(), "complete").await?;
    assert!(
        intercat_daemon::approval::replay(
            &journal,
            id,
            &correct,
            "approval",
            &request.requirement,
            &response
        )
        .await?
    );
    assert!(
        intercat_daemon::approval::replay(
            &journal,
            id,
            &Owner::Work.key("another"),
            "approval",
            &request.requirement,
            &response
        )
        .await
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn stop_and_send_waits_for_the_original_checkpoint_and_survives_replay() -> Result<()> {
    let (root, journal, mut worker, request) = fixture().await?;
    journal.queue_approval_reply(request.clone()).await?;
    journal.queue_approval_reply(request.clone()).await?;
    let input = Uuid::new_v5(&request.op_id, b"approval-reply").to_string();
    let state = journal.read_utterance(input.clone()).await?;
    assert_eq!(state["text"], request.text);
    assert_eq!(state["state"], "redirecting");
    assert_eq!(
        state["target"],
        json!({"mode":"lock","trackId":request.track_id})
    );
    assert_eq!(state["decision"]["kind"], "trim");
    let mut changed = request.clone();
    changed.text = "Different response".into();
    assert!(journal.queue_approval_reply(changed).await.is_err());
    worker.active_turn = None;
    worker.checkpoint = Some(Checkpoint {
        turn_id: request.turn_id,
        sha: "a".repeat(40),
        reference: "refs/intercat/reply".into(),
        unmerged_paths: vec![],
        live_processes: false,
    });
    journal.worker_state(worker.clone()).await?;
    assert_ne!(
        journal.read_utterance(input.clone()).await?["state"],
        "placed"
    );
    // A completed checkpoint operation is required as well as its displayed state.
    let checkpoint = Uuid::new_v5(&request.turn_id, b"checkpoint");
    journal
        .intent(
            &checkpoint.to_string(),
            "checkpoint.save",
            json!({"trackId":request.track_id,"checkpoint":worker.checkpoint}),
        )
        .await?;
    journal
        .finish(&checkpoint.to_string(), "dispatched")
        .await?;
    journal.finish(&checkpoint.to_string(), "complete").await?;
    // An unverified checkpoint holds the request. Preserve that hold through restart.
    let state = journal.read_utterance(input.clone()).await?;
    assert_eq!(state["state"], "held");
    drop(journal);
    let reopened = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(reopened.read_utterance(input).await?["state"], "held");
    reopened.queue_approval_reply(request).await?;
    Ok(())
}

#[tokio::test]
async fn failing_to_save_a_reply_rolls_back_its_input_and_redirect() -> Result<()> {
    let (root, journal, _worker, request) = fixture().await?;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER reject_reply BEFORE INSERT ON operations WHEN NEW.kind='approval.reply' BEGIN SELECT RAISE(ABORT,'simulated full disk'); END;")?;
    assert!(journal.queue_approval_reply(request.clone()).await.is_err());
    assert!(journal.operation(request.op_id).await?.is_none());
    assert!(
        journal
            .read_utterance(Uuid::new_v5(&request.op_id, b"approval-reply").to_string())
            .await
            .is_err()
    );
    assert_eq!(journal.snapshot().await?["redirects"], json!([]));
    Ok(())
}

#[tokio::test]
async fn a_verified_original_checkpoint_releases_the_reply_without_reading_it_again() -> Result<()>
{
    let (_root, journal, mut worker, request) = fixture().await?;
    journal.queue_approval_reply(request.clone()).await?;
    worker.active_turn = None;
    worker.phase = "ready".into();
    worker.checkpoint = Some(Checkpoint {
        turn_id: request.turn_id,
        sha: "a".repeat(40),
        reference: "refs/intercat/reply".into(),
        unmerged_paths: vec![],
        live_processes: false,
    });
    let checkpoint = Uuid::new_v5(&request.turn_id, b"checkpoint");
    journal
        .intent(
            &checkpoint.to_string(),
            "checkpoint.save",
            json!({"trackId":request.track_id,"checkpoint":worker.checkpoint}),
        )
        .await?;
    journal
        .finish(&checkpoint.to_string(), "dispatched")
        .await?;
    journal.finish(&checkpoint.to_string(), "complete").await?;
    journal.worker_state(worker).await?;
    let input = journal
        .read_utterance(Uuid::new_v5(&request.op_id, b"approval-reply").to_string())
        .await?;
    assert_eq!(input["state"], "placed");
    assert_eq!(input["decision"]["kind"], "trim");
    assert_eq!(input["text"], request.text);
    assert!(journal.claim_input().await?.is_none());
    Ok(())
}
