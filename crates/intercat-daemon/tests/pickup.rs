use intercat_daemon::{
    Result,
    checkpoint::Checkpoint,
    journal::Journal,
    pickup::{CancelRequest, Change, Phase, Request},
    preparation::Workspace,
    runtime::WorkerState,
    secrets::{Key, Registration},
};
use rusqlite::{Connection, params};
use serde_json::json;
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal, Request, WorkerState)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let request = Request {
        op_id: Uuid::now_v7(),
        track_id: Uuid::now_v7().to_string(),
        session_id: Uuid::now_v7().to_string(),
    };
    let db = Connection::open(root.path().join("db"))?;
    db.execute(
        "INSERT INTO tracks VALUES(?1,'A','Preserve the existing files','waiting')",
        [&request.track_id],
    )?;
    let workspace = Workspace {
        attachment_roots: Vec::new(),
        track_id: request.track_id.clone(),
        path: root.path().into(),
        reference: "refs/intercat/test/A".into(),
        base: "0".repeat(40),
        state: "ready".into(),
        error: None,
    };
    journal.workspace_result(workspace.clone()).await?;
    let checkpoint = Checkpoint {
        turn_id: Uuid::now_v7(),
        sha: "1".repeat(40),
        reference: format!("{}-checkpoints/old", workspace.reference),
        unmerged_paths: vec![],
        live_processes: false,
    };
    let state = WorkerState {
        track_id: request.track_id.clone(),
        phase: "lost".into(),
        session_id: Some(request.session_id.clone()),
        secret_revision: None,
        identity: None,
        detail: None,
        active_turn: Some(Uuid::now_v7()),
        approvals: vec![],
        output: Some("Known value: DUMMY_PICKUP_SECRET".into()),
        checkpoint_pending: true,
        checkpoint: Some(checkpoint),
        prompts: vec![],
    };
    journal.worker_state(state.clone()).await?;
    db.execute("INSERT INTO utterances(id,text,created_at,target,state,revision,decision,provisional) VALUES('old-input','keep this request',0,'{}','placed',1,?1,?2)",params![json!({"trackId":request.track_id}).to_string(),request.track_id])?;
    Ok((root, journal, request, state))
}
async fn stopped(j: &Journal, mut state: WorkerState) -> Result<()> {
    let turn = state.active_turn.take();
    state.phase = "after-connection-loss".into();
    state.checkpoint_pending = false;
    j.stop_lost_worker(Uuid::now_v7(), state, turn).await
}
async fn advance(j: &Journal, r: &Request, state: WorkerState) -> Result<()> {
    stopped(j, state).await?;
    j.pickup(Change::Stopped(r.op_id)).await?;
    j.pickup(Change::Captured(
        r.op_id,
        Checkpoint {
            turn_id: Uuid::new_v5(&r.op_id, b"recovery-snapshot"),
            sha: "2".repeat(40),
            reference: format!("refs/intercat/test/A-recovery/{}", r.op_id),
            unmerged_paths: vec![],
            live_processes: false,
        },
    ))
    .await?;
    j.pickup(Change::Published(r.op_id)).await?;
    j.pickup(Change::Summarized(
        r.op_id,
        intercat_daemon::handoff::Document {
            fields: intercat_daemon::handoff::Fields {
                purpose: "Continue saved work".into(),
                constraints: vec![],
                unresolved: vec![],
                changed_files: vec![],
                completed: vec![],
                unverified: vec!["Not all checks ran".into()],
                next_checks: vec![],
            },
            source_addresses: vec![format!("track:{}", r.track_id)],
        },
    ))
    .await?;
    Ok(())
}
fn cancellation(request: &Request) -> CancelRequest {
    CancelRequest {
        op_id: Uuid::now_v7(),
        recovery_id: request.op_id,
        track_id: request.track_id.clone(),
        session_id: request.session_id.clone(),
    }
}

#[tokio::test]
async fn cancel_can_arrive_before_prepare_and_survives_reopening_without_reading_history()
-> Result<()> {
    let (root, journal, request, mut worker) = fixture().await?;
    worker.output = Some("x".repeat(intercat_daemon::handoff::CONTEXT_BYTES + 1));
    journal.worker_state(worker.clone()).await?;
    let cancel = cancellation(&request);
    let pending = journal.cancel_pickup(cancel.clone()).await?;
    assert_eq!(pending.phase, Phase::Cancelling);
    assert!(pending.context.is_null());
    assert!(pending.previous.output.is_none());
    assert!(
        journal
            .pickup(Change::Cancelled(request.op_id))
            .await
            .is_err()
    );
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(
        journal
            .pickup(Change::Prepare(request.clone()))
            .await?
            .phase,
        Phase::Cancelling
    );
    worker.output = None;
    stopped(&journal, worker).await?;
    journal.pickup(Change::Cancelled(request.op_id)).await?;
    assert_eq!(journal.cancel_pickup(cancel).await?.phase, Phase::Cancelled);
    assert_eq!(
        journal
            .pickup(Change::Prepare(request.clone()))
            .await?
            .phase,
        Phase::Cancelled
    );
    let next = Request {
        op_id: Uuid::now_v7(),
        ..request
    };
    assert_eq!(
        journal.pickup(Change::Prepare(next)).await?.phase,
        Phase::Stopping
    );
    Ok(())
}

#[tokio::test]
async fn cancel_every_pre_activation_phase_keeps_snapshots_fingerprints_and_held_inputs()
-> Result<()> {
    for phase in [
        Phase::Stopping,
        Phase::Capturing,
        Phase::Publishing,
        Phase::Summarizing,
        Phase::Starting,
        Phase::AwaitingReceipt,
    ] {
        let (root, journal, request, worker) = fixture().await?;
        let key = Key::new([7; 32]);
        let hex = |bytes: [u8; 32]| bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let fingerprints = journal
            .register_fingerprint(Registration {
                op_id: Uuid::now_v7(),
                track_id: request.track_id.clone(),
                session_id: request.session_id.clone(),
                name: "TOKEN".into(),
                byte_len: 19,
                tag: hex(key.tag(b"DUMMY_PICKUP_SECRET")),
                key_check: hex(key.check()),
            })
            .await?;
        journal.confirm_fingerprints(fingerprints.current).await?;
        journal.pickup(Change::Prepare(request.clone())).await?;
        if matches!(phase, Phase::Starting | Phase::AwaitingReceipt) {
            advance(&journal, &request, worker.clone()).await?;
            if phase == Phase::AwaitingReceipt {
                journal.pickup(Change::Received(request.op_id)).await?;
            }
        } else if phase != Phase::Stopping {
            stopped(&journal, worker.clone()).await?;
            journal.pickup(Change::Stopped(request.op_id)).await?;
            if phase != Phase::Capturing {
                journal
                    .pickup(Change::Captured(
                        request.op_id,
                        Checkpoint {
                            turn_id: Uuid::new_v5(&request.op_id, b"recovery-snapshot"),
                            sha: "2".repeat(40),
                            reference: format!("refs/intercat/test/A-recovery/{}", request.op_id),
                            unmerged_paths: vec![],
                            live_processes: false,
                        },
                    ))
                    .await?;
                if phase == Phase::Summarizing {
                    journal.pickup(Change::Published(request.op_id)).await?;
                }
            }
        }
        let before = journal.pickup(Change::Prepare(request.clone())).await?;
        assert_eq!(before.phase, phase);
        let catalog = journal.fingerprint_catalog().await?;
        let cancel = cancellation(&request);
        assert_eq!(
            journal.cancel_pickup(cancel.clone()).await?.phase,
            Phase::Cancelling
        );
        assert!(
            journal
                .pickup(Change::Activate(request.op_id))
                .await
                .is_err()
        );
        if phase == Phase::Stopping {
            stopped(&journal, worker).await?;
        }
        let after = journal.pickup(Change::Cancelled(request.op_id)).await?;
        assert_eq!(after.phase, Phase::Cancelled);
        assert_eq!(
            serde_json::to_value(&after.snapshot)?,
            serde_json::to_value(&before.snapshot)?
        );
        assert_eq!(after.context, before.context);
        assert_eq!(
            serde_json::to_value(journal.fingerprint_catalog().await?)?,
            serde_json::to_value(catalog)?
        );
        assert_eq!(
            journal.cancel_pickup(cancel.clone()).await?.phase,
            Phase::Cancelled
        );
        let db = Connection::open(root.path().join("db"))?;
        assert_eq!(
            db.query_row(
                "SELECT state FROM operations WHERE id=?1",
                [cancel.op_id.to_string()],
                |row| row.get::<_, String>(0)
            )?,
            "complete"
        );
        assert_eq!(
            db.query_row(
                "SELECT state FROM utterances WHERE id='old-input'",
                [],
                |row| row.get::<_, String>(0)
            )?,
            "held"
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM utterances", [], |row| row
                .get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            db.query_row(
                "SELECT json_extract(record,'$.sessionId') FROM workers",
                [],
                |row| row.get::<_, String>(0)
            )?,
            request.session_id
        );
        assert_eq!(
            db.query_row(
                "SELECT json_extract(record,'$.state') FROM workspaces",
                [],
                |row| row.get::<_, String>(0)
            )?,
            "ready"
        );
    }
    Ok(())
}

#[tokio::test]
async fn cancellation_rejects_reused_ids_stale_owners_and_already_activated_recoveries()
-> Result<()> {
    let (root, journal, request, worker) = fixture().await?;
    let cancel = cancellation(&request);
    let mut wrong = cancel.clone();
    wrong.session_id = "another-session".into();
    assert!(journal.cancel_pickup(wrong).await.is_err());
    journal.pickup(Change::Prepare(request.clone())).await?;
    let mut wrong = cancel.clone();
    wrong.track_id = "another-track".into();
    assert!(journal.cancel_pickup(wrong).await.is_err());
    advance(&journal, &request, worker).await?;
    journal.pickup(Change::Received(request.op_id)).await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    journal.pickup(Change::Activate(request.op_id)).await?;
    assert!(
        journal
            .cancel_pickup(cancel)
            .await
            .unwrap_err()
            .to_string()
            .contains("already continued")
    );
    let db = Connection::open(root.path().join("db"))?;
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='worker.pickup.cancel'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn cancellation_does_not_overwrite_a_changed_workspace_or_reassign_an_operation() -> Result<()>
{
    let (root, journal, request, worker) = fixture().await?;
    journal.pickup(Change::Prepare(request.clone())).await?;
    let cancel = cancellation(&request);
    journal.cancel_pickup(cancel.clone()).await?;
    let mut reused = cancel.clone();
    reused.recovery_id = Uuid::now_v7();
    assert!(journal.cancel_pickup(reused).await.is_err());
    stopped(&journal, worker).await?;
    let db = Connection::open(root.path().join("db"))?;
    db.execute(
        "UPDATE workspaces SET record=json_set(record,'$.path','/changed')",
        [],
    )?;
    assert!(journal.cancel_pickup(cancel).await.is_err());
    assert!(
        journal
            .pickup(Change::Cancelled(request.op_id))
            .await
            .is_err()
    );
    assert_eq!(
        db.query_row(
            "SELECT json_extract(record,'$.path') FROM workspaces",
            [],
            |row| row.get::<_, String>(0)
        )?,
        "/changed"
    );
    Ok(())
}
#[tokio::test]
async fn recovery_waits_for_stop_and_receipt_then_starts_only_one_new_handoff() -> Result<()> {
    let (root, j, r, state) = fixture().await?;
    let secret = "DUMMY_PICKUP_SECRET";
    let key = Key::new([7; 32]);
    let hex = |b: [u8; 32]| b.iter().map(|b| format!("{b:02x}")).collect();
    let catalog = j
        .register_fingerprint(Registration {
            op_id: Uuid::now_v7(),
            track_id: r.track_id.clone(),
            session_id: r.session_id.clone(),
            name: "TOKEN".into(),
            byte_len: secret.len(),
            tag: hex(key.tag(secret.as_bytes())),
            key_check: hex(key.check()),
        })
        .await?;
    j.confirm_fingerprints(catalog.current).await?;
    let pending = j.pickup(Change::Prepare(r.clone())).await?;
    assert_eq!(pending.phase, Phase::Stopping);
    assert_ne!(pending.session_id, r.session_id);
    assert!(!serde_json::to_string(&pending)?.contains(secret));
    assert!(j.pickup(Change::Stopped(r.op_id)).await.is_err());
    let retry = j.pickup(Change::Prepare(r.clone())).await?;
    assert_eq!(retry.session_id, pending.session_id);
    assert_eq!(retry.start_command, pending.start_command);
    advance(&j, &r, state.clone()).await?;
    let received = j.pickup(Change::Received(r.op_id)).await?;
    assert_eq!(received.phase, Phase::AwaitingReceipt);
    assert_ne!(
        j.session_mask(&r.track_id, &received.session_id)
            .await?
            .redact(secret),
        secret
    );
    assert!(j.pickup(Change::Activate(r.op_id)).await.is_err());
    j.confirm_fingerprints(j.fingerprint_catalog().await?.current)
        .await?;
    let late = Connection::open(root.path().join("db"))?;
    late.execute("INSERT INTO utterances(id,text,created_at,target,state,revision,decision,provisional) VALUES('late-input','arrived during recovery',1,'{}','placed',1,?1,?2)",params![json!({"trackId":r.track_id}).to_string(),r.track_id])?;
    let ready = j.pickup(Change::Activate(r.op_id)).await?;
    assert_eq!(ready.phase, Phase::Ready);
    j.pickup(Change::Activate(r.op_id)).await?;
    let db = Connection::open(root.path().join("db"))?;
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM utterances", [], |r| r
            .get::<_, i64>(0))?,
        3
    );
    assert_eq!(
        db.query_row(
            "SELECT state FROM utterances WHERE id='old-input'",
            [],
            |r| r.get::<_, String>(0)
        )?,
        "held"
    );
    let worker: WorkerState = serde_json::from_str(&db.query_row(
        "SELECT record FROM workers WHERE track_id=?1",
        [&r.track_id],
        |r| r.get::<_, String>(0),
    )?)?;
    assert_eq!(
        worker.session_id.as_deref(),
        Some(ready.session_id.as_str())
    );
    assert_eq!(
        worker.checkpoint.unwrap().sha,
        state.checkpoint.unwrap().sha
    );
    assert!(
        j.pickup_checkpoint(
            r.track_id,
            ready.session_id,
            pending.previous.checkpoint.unwrap().turn_id
        )
        .await?
    );
    let text: String = db.query_row(
        "SELECT text FROM utterances WHERE state='placed'",
        [],
        |r| r.get(0),
    )?;
    assert!(text.contains("recovery snapshot 2222"));
    assert!(!text.contains(secret));
    Ok(())
}
#[tokio::test]
async fn stale_sessions_reused_ids_and_concurrent_recoveries_do_not_freeze_another_worker()
-> Result<()> {
    let (_root, j, r, _state) = fixture().await?;
    let mut stale = r.clone();
    stale.session_id = Uuid::now_v7().to_string();
    assert!(j.pickup(Change::Prepare(stale)).await.is_err());
    j.pickup(Change::Prepare(r.clone())).await?;
    let mut another = r.clone();
    another.op_id = Uuid::now_v7();
    assert!(j.pickup(Change::Prepare(another)).await.is_err());
    let mut reused = r.clone();
    reused.track_id = Uuid::now_v7().to_string();
    assert!(j.pickup(Change::Prepare(reused)).await.is_err());
    assert!(j.pickup(Change::Published(r.op_id)).await.is_err());
    Ok(())
}
#[tokio::test]
async fn a_pending_recovery_survives_restart_without_changing_its_session_or_checkpoint()
-> Result<()> {
    let (root, j, r, state) = fixture().await?;
    let before = j.pickup(Change::Prepare(r.clone())).await?;
    advance(&j, &r, state).await?;
    drop(j);
    let j = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let after = j.pickup(Change::Prepare(r)).await?;
    assert_eq!(after.phase, Phase::Starting);
    assert_eq!(after.session_id, before.session_id);
    assert_eq!(after.start_command, before.start_command);
    assert_eq!(after.snapshot.unwrap().sha, "2".repeat(40));
    Ok(())
}

#[tokio::test]
async fn activation_rejects_a_changed_workspace_without_creating_a_handoff() -> Result<()> {
    let (root, journal, request, state) = fixture().await?;
    journal.pickup(Change::Prepare(request.clone())).await?;
    advance(&journal, &request, state).await?;
    journal.pickup(Change::Received(request.op_id)).await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let db = Connection::open(root.path().join("db"))?;
    db.execute(
        "UPDATE workspaces SET record=json_set(record,'$.path','/changed') WHERE track_id=?1",
        [&request.track_id],
    )?;
    assert!(
        journal
            .pickup(Change::Activate(request.op_id))
            .await
            .is_err()
    );
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM utterances", [], |row| row
            .get::<_, i64>(0))?,
        1
    );
    assert_eq!(
        journal.pickup(Change::Prepare(request)).await?.phase,
        Phase::AwaitingReceipt
    );
    Ok(())
}

#[tokio::test]
async fn oversized_context_is_not_truncated_and_leaves_the_workspace_available() -> Result<()> {
    let (root, journal, request, mut state) = fixture().await?;
    state.output = Some("x".repeat(intercat_daemon::handoff::CONTEXT_BYTES + 1));
    journal.worker_state(state).await?;
    assert!(
        journal
            .pickup(Change::Prepare(request.clone()))
            .await
            .is_err()
    );
    let db = Connection::open(root.path().join("db"))?;
    assert_eq!(
        db.query_row(
            "SELECT json_extract(record,'$.state') FROM workspaces WHERE track_id=?1",
            [request.track_id],
            |r| r.get::<_, String>(0)
        )?,
        "ready"
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='worker.pickup'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn handoff_includes_tools_saved_while_the_old_worker_was_stopping() -> Result<()> {
    let (root, journal, request, state) = fixture().await?;
    journal.pickup(Change::Prepare(request.clone())).await?;
    let db = Connection::open(root.path().join("db"))?;
    let clip =
        json!({"sessionId":request.session_id,"turnId":state.active_turn,"repairRunId":null});
    db.execute(
        "INSERT INTO timeline_clips VALUES('late-clip',?1,1,?2)",
        params![request.track_id, clip.to_string()],
    )?;
    let item = json!({"itemId":"late-tool","kind":"toolCall","status":"completed","tool":"write_file","args":"{}"});
    db.execute(
        "INSERT INTO timeline_items VALUES(?1,'late-tool','late-clip',1,?2,?2)",
        params![request.session_id, item.to_string()],
    )?;
    stopped(&journal, state).await?;
    let final_context = journal
        .pickup(Change::Stopped(request.op_id))
        .await?
        .context;
    assert_eq!(
        final_context["completedTools"][0]["tool"]["itemId"],
        "late-tool"
    );
    Ok(())
}

#[tokio::test]
async fn old_report_questions_are_carried_forward_without_blocking_the_new_session() -> Result<()> {
    let (root, journal, request, state) = fixture().await?;
    let db = Connection::open(root.path().join("db"))?;
    let report = intercat_daemon::reports::Report::new(
        request.track_id.clone(),
        request.session_id.clone(),
        state.active_turn.unwrap(),
        "old-report".into(),
        "An unresolved choice remains.".into(),
    );
    db.execute(
        "INSERT INTO reports VALUES(?1,?2,1,?3)",
        params![report.id, report.track_id, serde_json::to_string(&report)?],
    )?;
    let q = intercat_daemon::reports::Question {
        id: Uuid::now_v7().to_string(),
        report_id: report.id,
        track_id: request.track_id.clone(),
        session_id: request.session_id.clone(),
        turn_id: state.active_turn.unwrap(),
        revision: 1,
        kind: "question".into(),
        text: "Which option should we use?".into(),
        state: "open".into(),
        assumed: None,
        reply_utterance_id: None,
    };
    db.execute(
        "INSERT INTO report_questions VALUES(?1,?2,?3,'open',?4)",
        params![q.id, q.report_id, q.track_id, serde_json::to_string(&q)?],
    )?;
    let pending = journal.pickup(Change::Prepare(request.clone())).await?;
    assert_eq!(pending.context["unresolvedQuestions"][0]["text"], q.text);
    advance(&journal, &request, state).await?;
    journal.pickup(Change::Received(request.op_id)).await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    journal.pickup(Change::Activate(request.op_id)).await?;
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM report_questions WHERE state IN ('open','queued','held')",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        db.query_row("SELECT pending FROM reports", [], |r| r.get::<_, i64>(0))?,
        0
    );
    assert!(
        db.query_row(
            "SELECT text FROM utterances WHERE state='placed'",
            [],
            |r| r.get::<_, String>(0)
        )?
        .contains(&q.text)
    );
    Ok(())
}

#[tokio::test]
async fn a_protected_field_that_cannot_be_masked_in_full_is_not_silently_summarized() -> Result<()>
{
    let (root, journal, request, mut state) = fixture().await?;
    let key = Key::new([7; 32]);
    let secret = "DO_NOT_EXPOSE_THIS_VALUE";
    let hex = |b: [u8; 32]| b.iter().map(|b| format!("{b:02x}")).collect();
    let catalog = journal
        .register_fingerprint(Registration {
            op_id: Uuid::now_v7(),
            track_id: request.track_id.clone(),
            session_id: request.session_id.clone(),
            name: "TOKEN".into(),
            byte_len: secret.len(),
            tag: hex(key.tag(secret.as_bytes())),
            key_check: hex(key.check()),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    state.output = Some("x".repeat(1024 * 1024 + 1));
    journal.worker_state(state).await?;
    assert!(
        journal
            .pickup(Change::Prepare(request.clone()))
            .await
            .is_err()
    );
    let db = Connection::open(root.path().join("db"))?;
    assert_eq!(
        db.query_row(
            "SELECT json_extract(record,'$.state') FROM workspaces WHERE track_id=?1",
            [request.track_id],
            |r| r.get::<_, String>(0)
        )?,
        "ready"
    );
    Ok(())
}
