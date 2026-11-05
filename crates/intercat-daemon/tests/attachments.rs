use intercat_daemon::{
    Result,
    attachment_store::Attachment,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    preparation::Workspace,
    secrets::Key,
};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::Path,
    process::Command,
};
use uuid::Uuid;

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    fs::create_dir(root.path().join("data"))?;
    let journal = Journal::open(root.path().join("data/db"), Key::new([7; 32])).await?;
    Ok((root, journal))
}
async fn input_workspace(
    root: &Path,
    journal: &Journal,
    attachments: &[Attachment],
) -> Result<(String, Workspace)> {
    let input = journal
        .stage_utterance(
            Uuid::now_v7(),
            "Use these reference files.".into(),
            Target::New,
            None,
            attachments.iter().map(|a| a.id).collect(),
        )
        .await?;
    let claimed = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            claimed.id,
            claimed.revision,
            Outcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: serde_json::json!({}),
            }),
        )
        .await?;
    let state = journal.snapshot().await?;
    let track = state["tracks"][0]["id"].as_str().unwrap().to_owned();
    let path = root.join("workspace");
    fs::create_dir(&path)?;
    git(&path, &["init", "-q", "-b", "main"]);
    git(&path, &["config", "user.name", "Fixture"]);
    git(&path, &["config", "user.email", "fixture@example.invalid"]);
    fs::write(path.join("code.txt"), "user code")?;
    git(&path, &["add", "."]);
    git(&path, &["commit", "-qm", "base"]);
    let base = git(&path, &["rev-parse", "HEAD"]);
    let workspace = Workspace {
        attachment_roots: intercat_daemon::attachments::reserve(&path, &base)?,
        track_id: track,
        path,
        base,
        reference: "refs/intercat/attachment/A".into(),
        state: "ready".into(),
        error: None,
    };
    journal.workspace_result(workspace.clone()).await?;
    Ok((input["id"].as_str().unwrap().into(), workspace))
}

#[tokio::test]
async fn originals_survive_source_changes_restart_and_copy_replacement_without_entering_git()
-> Result<()> {
    let (root, journal) = fixture().await?;
    let source = root.path().join("brief.txt");
    let body = "DUMMY_PRIVATE_REFERENCE_BODY_猫";
    fs::write(&source, body)?;
    fs::set_permissions(&source, fs::Permissions::from_mode(0o755))?;
    let id = Uuid::now_v7();
    let attachment = journal.import_attachment(id, source.clone()).await?;
    fs::write(&source, "changed after import")?;
    assert_eq!(
        journal.import_attachment(id, source.clone()).await?,
        attachment
    );
    let (input, workspace) =
        input_workspace(root.path(), &journal, std::slice::from_ref(&attachment)).await?;
    let references = journal
        .materialize_input_attachments(
            input.clone(),
            workspace.track_id.clone(),
            "session".into(),
            workspace.path.clone(),
        )
        .await?;
    assert_eq!(references.files.len(), 1);
    let copy = references.files[0].path.clone();
    assert_eq!(fs::read_to_string(&copy)?, body);
    assert_eq!(fs::metadata(&copy)?.mode() & 0o777, 0o600);
    assert_eq!(
        journal.snapshot().await?["utterances"][0]["attachments"][0]["id"],
        id.to_string()
    );
    let outside = root.path().join("outside.txt");
    fs::write(&outside, "outside stays")?;
    fs::remove_file(&copy)?;
    std::os::unix::fs::symlink(&outside, &copy)?;
    fs::remove_file(&source)?;
    drop(journal);
    let journal = Journal::open(root.path().join("data/db"), Key::new([7; 32])).await?;
    assert_eq!(journal.import_attachment(id, source).await?, attachment);
    journal
        .materialize_input_attachments(
            input,
            workspace.track_id.clone(),
            "session".into(),
            workspace.path.clone(),
        )
        .await?;
    assert_eq!(fs::read_to_string(&copy)?, body);
    assert_eq!(fs::read_to_string(outside)?, "outside stays");
    git(
        &workspace.path,
        &[
            "add",
            "-f",
            copy.strip_prefix(&workspace.path)?.to_str().unwrap(),
        ],
    );
    let before = fs::read(workspace.path.join(".git/index"))?;
    let saved = intercat_daemon::checkpoint::save(
        workspace.clone(),
        Uuid::now_v7(),
        None,
        false,
        journal.clone(),
    )
    .await?;
    assert!(
        !git(
            &workspace.path,
            &["ls-tree", "-r", "--name-only", &saved.sha]
        )
        .contains("attachments")
    );
    assert_eq!(fs::read(workspace.path.join(".git/index"))?, before);
    let db = rusqlite::Connection::open(root.path().join("data/db"))?;
    let mut query =
        db.prepare("SELECT payload FROM operations UNION ALL SELECT payload FROM events")?;
    for row in query.query_map([], |r| r.get::<_, String>(0))? {
        assert!(!row?.contains(body));
    }
    Ok(())
}

#[tokio::test]
async fn corrupt_originals_or_metadata_never_produce_a_reference_copy() -> Result<()> {
    let (root, journal) = fixture().await?;
    let source = root.path().join("brief.txt");
    fs::write(&source, "original bytes")?;
    let a = journal
        .import_attachment(Uuid::now_v7(), source.clone())
        .await?;
    let (input, workspace) =
        input_workspace(root.path(), &journal, std::slice::from_ref(&a)).await?;
    let original = root.path().join("data/attachments").join(a.id.to_string());
    let bytes = fs::read(&original)?;
    fs::set_permissions(&original, fs::Permissions::from_mode(0o600))?;
    let mut corrupted = bytes.clone();
    *corrupted.last_mut().unwrap() ^= 1;
    fs::write(&original, &corrupted)?;
    fs::set_permissions(&original, fs::Permissions::from_mode(0o400))?;
    assert!(
        journal
            .materialize_input_attachments(
                input.clone(),
                workspace.track_id.clone(),
                "session".into(),
                workspace.path.clone()
            )
            .await
            .is_err()
    );
    fs::set_permissions(&original, fs::Permissions::from_mode(0o600))?;
    fs::write(&original, &bytes)?;
    fs::set_permissions(&original, fs::Permissions::from_mode(0o400))?;
    let db = rusqlite::Connection::open(root.path().join("data/db"))?;
    db.execute("UPDATE attachments SET record=json_set(record,'$.attachment.name','forged.txt') WHERE id=?1",[a.id.to_string()])?;
    assert!(
        journal
            .materialize_input_attachments(
                input,
                workspace.track_id.clone(),
                "session".into(),
                workspace.path.clone()
            )
            .await
            .is_err()
    );
    assert!(
        journal
            .import_attachment(a.id, root.path().join("different.txt"))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn unsafe_sources_limits_and_reused_operations_are_rejected() -> Result<()> {
    let (root, journal) = fixture().await?;
    let source = root.path().join("brief.txt");
    fs::write(&source, "plain")?;
    let link = root.path().join("link.txt");
    std::os::unix::fs::symlink(&source, &link)?;
    assert!(
        journal
            .import_attachment(Uuid::now_v7(), link)
            .await
            .is_err()
    );
    assert!(
        journal
            .import_attachment(Uuid::now_v7(), root.path().to_path_buf())
            .await
            .is_err()
    );
    let fifo = root.path().join("pipe.txt");
    let c = std::ffi::CString::new(fifo.to_str().unwrap())?;
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    assert!(
        journal
            .import_attachment(Uuid::now_v7(), fifo)
            .await
            .is_err()
    );
    let large = root.path().join("large.txt");
    fs::File::create(&large)?.set_len(intercat_daemon::attachment_store::FILE_LIMIT as u64 + 1)?;
    assert!(
        journal
            .import_attachment(Uuid::now_v7(), large)
            .await
            .is_err()
    );
    let id = Uuid::now_v7();
    journal
        .intent(&id.to_string(), "another.operation", serde_json::json!({}))
        .await?;
    assert!(journal.import_attachment(id, source.clone()).await.is_err());
    let a = journal.import_attachment(Uuid::now_v7(), source).await?;
    let before = journal.snapshot().await?["utterances"]
        .as_array()
        .unwrap()
        .len();
    assert!(
        journal
            .stage_utterance(
                Uuid::now_v7(),
                "use twice".into(),
                Target::New,
                None,
                vec![a.id, a.id]
            )
            .await
            .is_err()
    );
    assert!(
        journal
            .stage_utterance(
                Uuid::now_v7(),
                "unknown".into(),
                Target::New,
                None,
                vec![Uuid::now_v7()]
            )
            .await
            .is_err()
    );
    assert_eq!(
        journal.snapshot().await?["utterances"]
            .as_array()
            .unwrap()
            .len(),
        before
    );
    Ok(())
}

#[tokio::test]
async fn combined_reference_limit_rejects_the_entire_input_transaction() -> Result<()> {
    let (root, journal) = fixture().await?;
    let source = root.path().join("large-reference.txt");
    fs::File::create(&source)?.set_len(21 * 1024 * 1024)?;
    let mut ids = Vec::new();
    for _ in 0..5 {
        ids.push(
            journal
                .import_attachment(Uuid::now_v7(), source.clone())
                .await?
                .id,
        );
    }
    let accepted = journal
        .stage_utterance(
            Uuid::now_v7(),
            "Within the total limit.".into(),
            Target::New,
            None,
            ids[..4].to_vec(),
        )
        .await?;
    assert_eq!(accepted["attachments"].as_array().unwrap().len(), 4);
    assert!(
        journal
            .stage_utterance(Uuid::now_v7(), "Too much.".into(), Target::New, None, ids)
            .await
            .is_err()
    );
    let snapshot = journal.snapshot().await?;
    assert_eq!(snapshot["utterances"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["session"]["name"], "Within the total limit.");
    Ok(())
}

async fn additional_input(journal: &Journal, track: &str, attachment: Uuid) -> Result<String> {
    let value = journal
        .stage_utterance(
            Uuid::now_v7(),
            "Read this reference.".into(),
            Target::Lock(track.into()),
            None,
            vec![attachment],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(track.into()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: serde_json::json!({}),
            }),
        )
        .await?;
    Ok(value["id"].as_str().unwrap().into())
}
async fn dispatch_reference(
    journal: &Journal,
    workspace: &Workspace,
    input: &str,
    session: &str,
    arrived: bool,
) -> Result<Uuid> {
    let command = Uuid::now_v7();
    let turn = Uuid::now_v7();
    journal
        .reserve_turn(
            command,
            input.into(),
            1,
            workspace.track_id.clone(),
            serde_json::json!({"commandId":command,"sessionId":session,"input":[]}),
        )
        .await?;
    if arrived {
        for frame in [
            serde_json::json!({"method":"turn/started","params":{"sessionId":session,"turnId":turn,"commandId":command}}),
            serde_json::json!({"method":"item/completed","params":{"sessionId":session,"item":{"kind":"userMessage","itemId":Uuid::now_v7(),"turnId":turn,"commandId":command,"revision":1,"status":"completed","content":[]}}}),
            serde_json::json!({"method":"turn/completed","params":{"sessionId":session,"turnId":turn,"terminal":"completed"}}),
        ] {
            journal
                .try_msp(
                    workspace.track_id.clone(),
                    session.into(),
                    intercat_daemon::clock::Stamp {
                        epoch_ms: 1000,
                        mono_ms: 1,
                        boot_id: "attachment-test".into(),
                        timeline_ms: 1000,
                    },
                    frame,
                    None,
                    None,
                )?
                .await??;
        }
    }
    Ok(turn)
}

#[tokio::test]
async fn branch_restores_the_frozen_original_set_after_restart_and_excludes_parent_late_files()
-> Result<()> {
    use intercat_daemon::branch::{Change, Request};
    let (root, journal) = fixture().await?;
    let source = root.path().join("early.txt");
    fs::write(&source, "EARLY ORIGINAL")?;
    let early = journal
        .import_attachment(Uuid::now_v7(), source.clone())
        .await?;
    let (input, workspace) =
        input_workspace(root.path(), &journal, std::slice::from_ref(&early)).await?;
    let first = dispatch_reference(&journal, &workspace, &input, "parent-session", true).await?;
    intercat_daemon::checkpoint::save(workspace.clone(), first, None, false, journal.clone())
        .await?;
    let request = Request {
        op_id: Uuid::now_v7(),
        parent_track_id: workspace.track_id.clone(),
        turn_id: first,
    };
    let branch = journal.branch(Change::Prepare(request.clone())).await?;
    let late_source = root.path().join("late.txt");
    fs::write(&late_source, "LATE ORIGINAL")?;
    let late = journal
        .import_attachment(Uuid::now_v7(), late_source)
        .await?;
    let late_input = additional_input(&journal, &workspace.track_id, late.id).await?;
    dispatch_reference(&journal, &workspace, &late_input, "parent-session", true).await?;
    fs::remove_file(source)?;
    let mut source = intercat_git::Repository::worktree_source(&workspace.path, None)?;
    source.head = branch.checkpoint.sha.parse()?;
    intercat_daemon::worktree::GitTool::system()
        .await?
        .prepare_detached(&source, &branch.workspace.path)
        .await?;
    let roots =
        intercat_daemon::attachments::reserve(&branch.workspace.path, &branch.workspace.base)?;
    let branch = journal
        .branch(Change::WorkspaceReady {
            operation: request.op_id,
            roots,
        })
        .await?;
    journal
        .restore_frozen_attachments(request.op_id, branch.workspace.clone())
        .await?;
    let copy = branch
        .workspace
        .path
        .join(&branch.workspace.attachment_roots[0])
        .join(format!("{}.txt", early.id));
    assert_eq!(fs::read_to_string(&copy)?, "EARLY ORIGINAL");
    assert!(
        !branch
            .workspace
            .path
            .join(&branch.workspace.attachment_roots[0])
            .join(format!("{}.txt", late.id))
            .exists()
    );
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
    let branch = journal.branch(Change::Activate(request.op_id)).await?;
    fs::write(&copy, "changed copy")?;
    drop(journal);
    let journal = Journal::open(root.path().join("data/db"), Key::new([7; 32])).await?;
    journal
        .restore_session_attachments(
            session.clone(),
            branch.track_id.clone(),
            branch.workspace.path.clone(),
        )
        .await?;
    assert_eq!(fs::read_to_string(copy)?, "EARLY ORIGINAL");
    assert!(
        journal
            .restore_session_attachments(session, workspace.track_id, workspace.path)
            .await
            .is_err()
    );
    assert!(git(&branch.workspace.path, &["status", "--porcelain"]).is_empty());
    Ok(())
}

#[tokio::test]
async fn pickup_recreates_known_and_uncertain_references_but_keeps_unsent_inputs_held() -> Result<()>
{
    use intercat_daemon::{
        checkpoint::Checkpoint,
        handoff::{Document, Fields},
        pickup::{Change, Request},
        runtime::WorkerState,
    };
    let (root, journal) = fixture().await?;
    let source = root.path().join("early.txt");
    fs::write(&source, "EARLY")?;
    let early = journal
        .import_attachment(Uuid::now_v7(), source.clone())
        .await?;
    let (input, workspace) =
        input_workspace(root.path(), &journal, std::slice::from_ref(&early)).await?;
    let first = dispatch_reference(&journal, &workspace, &input, "old-session", true).await?;
    let checkpoint =
        intercat_daemon::checkpoint::save(workspace.clone(), first, None, false, journal.clone())
            .await?;
    fs::write(&source, "UNKNOWN DISPATCH")?;
    let uncertain = journal
        .import_attachment(Uuid::now_v7(), source.clone())
        .await?;
    let uncertain_input = additional_input(&journal, &workspace.track_id, uncertain.id).await?;
    let unknown_turn =
        dispatch_reference(&journal, &workspace, &uncertain_input, "old-session", false).await?;
    fs::write(&source, "FUTURE UNSENT")?;
    let future = journal
        .import_attachment(Uuid::now_v7(), source.clone())
        .await?;
    let future_input = journal
        .stage_utterance(
            Uuid::now_v7(),
            "Future unsent input".into(),
            Target::Lock(workspace.track_id.clone()),
            None,
            vec![future.id],
        )
        .await?;
    let worker = WorkerState {
        track_id: workspace.track_id.clone(),
        phase: "lost".into(),
        session_id: Some("old-session".into()),
        secret_revision: None,
        identity: None,
        detail: None,
        active_turn: Some(unknown_turn),
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: Some(checkpoint.clone()),
        prompts: vec![],
    };
    journal.worker_state(worker.clone()).await?;
    let request = Request {
        op_id: Uuid::now_v7(),
        track_id: workspace.track_id.clone(),
        session_id: "old-session".into(),
    };
    journal.pickup(Change::Prepare(request.clone())).await?;
    let mut stopped = worker;
    stopped.phase = "after-connection-loss".into();
    stopped.active_turn = None;
    journal
        .stop_lost_worker(Uuid::now_v7(), stopped, Some(unknown_turn))
        .await?;
    let recovery = journal.pickup(Change::Stopped(request.op_id)).await?;
    fs::remove_file(source)?;
    journal
        .restore_frozen_attachments(request.op_id, recovery.workspace.clone())
        .await?;
    let directory = workspace.path.join(&workspace.attachment_roots[0]);
    assert_eq!(
        fs::read_to_string(directory.join(format!("{}.txt", early.id)))?,
        "EARLY"
    );
    assert_eq!(
        fs::read_to_string(directory.join(format!("{}.txt", uncertain.id)))?,
        "UNKNOWN DISPATCH"
    );
    assert!(!directory.join(format!("{}.txt", future.id)).exists());
    journal
        .pickup(Change::Captured(
            request.op_id,
            Checkpoint {
                turn_id: Uuid::new_v5(&request.op_id, b"recovery-snapshot"),
                sha: checkpoint.sha,
                reference: format!("{}-recovery/{}", workspace.reference, request.op_id),
                unmerged_paths: vec![],
                live_processes: false,
            },
        ))
        .await?;
    journal.pickup(Change::Published(request.op_id)).await?;
    journal
        .pickup(Change::Summarized(
            request.op_id,
            Document {
                fields: Fields {
                    purpose: "Continue from files".into(),
                    constraints: vec![],
                    unresolved: vec![],
                    changed_files: vec![],
                    completed: vec![],
                    unverified: vec!["Unknown dispatch".into()],
                    next_checks: vec![],
                },
                source_addresses: vec![format!("track:{}", workspace.track_id)],
            },
        ))
        .await?;
    journal.pickup(Change::Received(request.op_id)).await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let ready = journal.pickup(Change::Activate(request.op_id)).await?;
    let snapshot = journal.snapshot().await?;
    assert_eq!(
        snapshot["utterances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["id"] == future_input["id"])
            .unwrap()["state"],
        "held"
    );
    let handoff = snapshot["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["state"] == "placed")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::remove_file(directory.join(format!("{}.txt", early.id)))?;
    drop(journal);
    let journal = Journal::open(root.path().join("data/db"), Key::new([7; 32])).await?;
    let context = journal
        .materialize_input_attachments(
            handoff,
            workspace.track_id.clone(),
            ready.session_id,
            workspace.path,
        )
        .await?;
    assert_eq!(context.files.len(), 2);
    assert_eq!(context.directory.as_ref(), Some(&directory));
    assert_eq!(
        fs::read_to_string(directory.join(format!("{}.txt", early.id)))?,
        "EARLY"
    );
    Ok(())
}
