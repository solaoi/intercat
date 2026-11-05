use intercat_daemon::{
    Result,
    branch::{Branch, Change, Phase, Request},
    checkpoint::Checkpoint,
    journal::Journal,
    preparation::Workspace,
    runtime::WorkerState,
    secrets::{Key, Registration},
};
use serde_json::json;
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal, Request)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let request = Request {
        op_id: Uuid::now_v7(),
        parent_track_id: Uuid::now_v7().to_string(),
        turn_id: Uuid::now_v7(),
    };
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute(
        "INSERT INTO tracks VALUES(?1,'A','Original work','done')",
        [&request.parent_track_id],
    )?;
    let workspace = Workspace {
        attachment_roots: Vec::new(),
        track_id: request.parent_track_id.clone(),
        path: root.path().join("session/parent"),
        reference: "refs/intercat/test/tracks/A".into(),
        base: "0".repeat(40),
        state: "ready".into(),
        error: None,
    };
    journal.workspace_result(workspace.clone()).await?;
    save_checkpoint(
        &journal,
        &db,
        &request.parent_track_id,
        "parent-session",
        &workspace,
        request.turn_id,
    )
    .await?;
    Ok((root, journal, request))
}

#[tokio::test]
async fn a_checkpoint_keeps_its_original_session_after_file_recovery() -> Result<()> {
    let (root, journal, request) = fixture().await?;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute("UPDATE workers SET record=json_set(record,'$.sessionId','replacement-session') WHERE track_id=?1",[&request.parent_track_id])?;
    let branch = journal.branch(Change::Prepare(request)).await?;
    assert_eq!(branch.parent_session_id, "parent-session");
    Ok(())
}
async fn save_checkpoint(
    journal: &Journal,
    db: &rusqlite::Connection,
    track: &str,
    session: &str,
    workspace: &Workspace,
    turn: Uuid,
) -> Result<()> {
    let checkpoint = Checkpoint {
        turn_id: turn,
        sha: "1".repeat(40),
        reference: format!("{}-checkpoints/{turn}", workspace.reference),
        unmerged_paths: vec![],
        live_processes: false,
    };
    journal
        .worker_state(WorkerState {
            track_id: track.into(),
            phase: "stopped".into(),
            session_id: Some(session.into()),
            secret_revision: None,
            identity: None,
            detail: None,
            active_turn: None,
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: Some(checkpoint.clone()),
            prompts: vec![],
        })
        .await?;
    db.execute(
        "INSERT INTO timeline_turns VALUES(?1,?2,?3)",
        rusqlite::params![
            session,
            turn.to_string(),
            json!({"track":track}).to_string()
        ],
    )?;
    let op = Uuid::new_v5(&turn, b"checkpoint").to_string();
    journal
        .intent(
            &op,
            "checkpoint.save",
            json!({"trackId":track,"checkpoint":checkpoint}),
        )
        .await?;
    journal.finish(&op, "dispatched").await?;
    journal.finish(&op, "complete").await
}
async fn save_secret(journal: &Journal, track: &str) -> Result<()> {
    let key = Key::new([7; 32]);
    let secret = "DUMMY_BRANCH_SECRET_猫";
    let hex = |bytes: [u8; 32]| bytes.iter().map(|b| format!("{b:02x}")).collect();
    let catalog = journal
        .register_fingerprint(Registration {
            op_id: Uuid::now_v7(),
            track_id: track.into(),
            session_id: "parent-session".into(),
            name: "DEMO_TOKEN".into(),
            byte_len: secret.len(),
            tag: hex(key.tag(secret.as_bytes())),
            key_check: hex(key.check()),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await
}
async fn finish(journal: &Journal, request: Request) -> Result<Branch> {
    let pending = journal.branch(Change::Prepare(request.clone())).await?;
    assert_eq!(pending.phase, Phase::Preparing);
    journal
        .branch(Change::WorkspaceReady {
            operation: request.op_id,
            roots: vec![".intercat/attachments".into()],
        })
        .await?;
    let branch = journal
        .branch(Change::Received {
            operation: request.op_id,
            session: Uuid::now_v7().to_string(),
        })
        .await?;
    assert_eq!(branch.phase, Phase::AwaitingReceipt);
    assert!(
        journal
            .branch(Change::Activate(request.op_id))
            .await
            .is_err()
    );
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    journal.branch(Change::Activate(request.op_id)).await
}

#[tokio::test]
async fn child_waits_for_receipt_then_retains_parent_masks_across_restart() -> Result<()> {
    let (root, journal, request) = fixture().await?;
    save_secret(&journal, &request.parent_track_id).await?;
    let child = finish(&journal, request.clone()).await?;
    assert_eq!(journal.snapshot().await?["tracks"][1]["label"], "A2");
    let session = child.session_id.clone().unwrap();
    assert_eq!(child.workspace.base, "1".repeat(40));
    assert_eq!(
        journal
            .session_mask(&child.track_id, &session)
            .await?
            .redact("DUMMY_BRANCH_SECRET_猫"),
        "<secret:DEMO_TOKEN>"
    );
    assert_eq!(
        journal.branch(Change::Prepare(request)).await?.track_id,
        child.track_id
    );
    assert_eq!(
        journal
            .branch(Change::Activate(child.request.op_id))
            .await?
            .phase,
        Phase::Ready
    );
    assert!(
        journal
            .branch(Change::Received {
                operation: child.request.op_id,
                session: Uuid::now_v7().to_string()
            })
            .await
            .is_err()
    );
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let snapshot = journal.snapshot().await?;
    assert_eq!(
        snapshot["workspaces"][1]["attachmentRoots"],
        json!([".intercat/attachments"])
    );
    assert_eq!(
        snapshot["branches"][0]["workspace"]["attachmentRoots"],
        json!([".intercat/attachments"])
    );
    assert_eq!(
        journal
            .session_mask(&child.track_id, &session)
            .await?
            .redact("DUMMY_BRANCH_SECRET_猫"),
        "<secret:DEMO_TOKEN>"
    );
    Ok(())
}

#[tokio::test]
async fn grandchildren_inherit_but_a_fresh_session_does_not() -> Result<()> {
    let (root, journal, request) = fixture().await?;
    save_secret(&journal, &request.parent_track_id).await?;
    let child = finish(&journal, request).await?;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let turn = Uuid::now_v7();
    save_checkpoint(
        &journal,
        &db,
        &child.track_id,
        child.session_id.as_deref().unwrap(),
        &child.workspace,
        turn,
    )
    .await?;
    let grandchild = finish(
        &journal,
        Request {
            op_id: Uuid::now_v7(),
            parent_track_id: child.track_id.clone(),
            turn_id: turn,
        },
    )
    .await?;
    assert_eq!(journal.snapshot().await?["tracks"][2]["label"], "A3");
    db.execute(
        "DELETE FROM workers WHERE track_id=?1",
        [&child.request.parent_track_id],
    )?;
    assert_eq!(
        journal
            .session_mask(
                &grandchild.track_id,
                grandchild.session_id.as_deref().unwrap()
            )
            .await?
            .redact("DUMMY_BRANCH_SECRET_猫"),
        "<secret:DEMO_TOKEN>"
    );
    assert_eq!(
        journal
            .session_mask("fresh-track", "fresh-session")
            .await?
            .redact("DUMMY_BRANCH_SECRET_猫"),
        "DUMMY_BRANCH_SECRET_猫"
    );
    Ok(())
}

#[tokio::test]
async fn wrong_boundaries_and_reused_operations_never_create_another_track() -> Result<()> {
    let (_root, journal, request) = fixture().await?;
    let mut wrong = request.clone();
    wrong.turn_id = Uuid::now_v7();
    assert!(journal.branch(Change::Prepare(wrong)).await.is_err());
    let first = journal.branch(Change::Prepare(request.clone())).await?;
    let mut wrong = request.clone();
    wrong.parent_track_id = "other".into();
    assert!(journal.branch(Change::Prepare(wrong)).await.is_err());
    assert!(
        journal
            .branch(Change::Received {
                operation: request.op_id,
                session: "parent-session".into()
            })
            .await
            .is_err()
    );
    assert!(
        journal
            .branch(Change::Activate(request.op_id))
            .await
            .is_err()
    );
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        journal.branch(Change::Prepare(request)).await?.track_id,
        first.track_id
    );
    Ok(())
}

#[tokio::test]
async fn pending_empty_fingerprint_children_still_own_their_sessions() -> Result<()> {
    let (_root, journal, request) = fixture().await?;
    journal.branch(Change::Prepare(request.clone())).await?;
    let session = Uuid::now_v7().to_string();
    journal
        .branch(Change::Received {
            operation: request.op_id,
            session: session.clone(),
        })
        .await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let mut next = request.clone();
    next.op_id = Uuid::now_v7();
    journal.branch(Change::Prepare(next.clone())).await?;
    assert!(
        journal
            .branch(Change::Received {
                operation: next.op_id,
                session
            })
            .await
            .is_err()
    );
    let mut reused = request;
    reused.op_id = Uuid::now_v7();
    journal
        .intent(&reused.op_id.to_string(), "another.action", json!({}))
        .await?;
    assert!(journal.branch(Change::Prepare(reused)).await.is_err());
    assert_eq!(
        journal.snapshot().await?["branches"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    Ok(())
}
