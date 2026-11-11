use intercat_daemon::{
    Result,
    backup_review::{self, Request},
    fingerprint_receipt::Receipt,
    journal::Journal,
    preparation::Workspace,
    process::{self, Tracker},
    runtime::WorkerState,
    secrets::{Key, Registration},
};
use serde_json::{Value, json};
use sha2::Digest;
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Stdio};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

const PRIVATE: &str = "DUMMY_RESTORED_HISTORY_MUST_NOT_BE_SHOWN";
fn git(path: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("/usr/bin/git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
async fn fixture() -> Result<(tempfile::TempDir, Journal, Receipt)> {
    let root = tempfile::tempdir()?;
    let project = root.path().join("project");
    fs::create_dir(&project)?;
    let project = project.canonicalize()?;
    git(&project, &["init", "-q", "-b", "main"]);
    fs::write(project.join("kept.txt"), "base\n")?;
    fs::write(project.join("removed.txt"), "remove after backup\n")?;
    fs::write(project.join("mode.sh"), "exit 0\n")?;
    fs::write(project.join("type.txt"), "a file before backup\n")?;
    git(&project, &["add", "."]);
    git(
        &project,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "base",
        ],
    );
    let base = git(&project, &["rev-parse", "HEAD"]);
    git(&project, &["update-ref", "refs/intercat/test/A", &base]);
    let original = Journal::open(root.path().join("original.sqlite"), Key::new([7; 32])).await?;
    rusqlite::Connection::open(root.path().join("original.sqlite"))?.execute(
        "INSERT INTO tracks VALUES('track','A','PRIVATE TITLE','waiting')",
        [],
    )?;
    original
        .workspace_result(Workspace {
            denied_roots: Vec::new(),
            attachment_roots: Vec::new(),
            track_id: "track".into(),
            path: project.clone(),
            reference: "refs/intercat/test/A".into(),
            base,
            state: "ready".into(),
            error: None,
        })
        .await?;
    original
        .worker_state(WorkerState {
            track_id: "track".into(),
            phase: "ready".into(),
            session_id: Some("session".into()),
            secret_revision: None,
            identity: None,
            detail: Some(PRIVATE.into()),
            active_turn: Some(Uuid::now_v7()),
            approvals: vec![],
            output: Some(PRIVATE.into()),
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        })
        .await?;
    let data = root.path().join("restored");
    fs::create_dir(&data)?;
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700))?;
    rusqlite::Connection::open(root.path().join("original.sqlite"))?.execute(
        "VACUUM INTO ?1",
        [data.join("events.sqlite").to_str().unwrap()],
    )?;
    let key = Key::new([7; 32]);
    let hex = |bytes: [u8; 32]| bytes.iter().map(|b| format!("{b:02x}")).collect();
    let catalog = original
        .register_fingerprint(Registration {
            op_id: Uuid::now_v7(),
            track_id: "track".into(),
            session_id: "session".into(),
            name: "TOKEN".into(),
            byte_len: PRIVATE.len(),
            tag: hex(key.tag(PRIVATE.as_bytes())),
            key_check: hex(key.check()),
        })
        .await?;
    let receipt = Receipt {
        catalog: catalog.current,
        proof: catalog.proof,
    };
    fs::write(project.join("kept.txt"), "newer file survives\n")?;
    git(&project, &["add", "."]);
    git(
        &project,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "after backup",
        ],
    );
    let newer = git(&project, &["rev-parse", "HEAD"]);
    git(&project, &["update-ref", "refs/intercat/test/A", &newer]);
    let journal = Journal::open(data.join("events.sqlite"), Key::new([7; 32])).await?;
    Ok((root, journal, receipt))
}
async fn process_host(
    data: &Path,
    project: &Path,
) -> Result<(
    process::Identity,
    tokio::task::JoinHandle<std::io::Result<std::process::ExitStatus>>,
)> {
    let path = data.join("summary-hosts/unrecorded-task/host");
    fs::create_dir_all(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    let nonce = Uuid::now_v7().to_string();
    let mut tracker = Tracker::prepare(&path, project, nonce.clone())?;
    let mut child = tokio::process::Command::new("/bin/sleep")
        .arg("60")
        .current_dir(project)
        .env("INTERCAT_HOST", nonce)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let identity = process::identity(child.id().unwrap())?;
    tracker.attach_process_group(identity)?;
    drop(tracker);
    Ok((identity, tokio::spawn(async move { child.wait().await })))
}
#[tokio::test]
async fn review_never_opens_history_and_stops_an_owned_host_missing_from_the_restored_log()
-> Result<()> {
    let (root, journal, receipt) = fixture().await?;
    let data = root.path().join("restored");
    let project = root.path().join("project");
    let before_file = fs::read(project.join("kept.txt"))?;
    let before_index = fs::read(project.join(".git/index"))?;
    let (identity, reaper) = process_host(&data, &project).await?;
    let mut unrelated = tokio::process::Command::new("/bin/sleep")
        .arg("60")
        .current_dir(&project)
        .kill_on_drop(true)
        .spawn()?;
    let other = process::identity(unrelated.id().unwrap())?;
    let review = backup_review::run(&journal, &data, receipt.clone(), Request::Inspect).await?;
    assert!(!serde_json::to_string(&review)?.contains(PRIVATE));
    assert!(!serde_json::to_string(&review)?.contains("PRIVATE TITLE"));
    assert_eq!(review["historyPaused"], true);
    assert_eq!(review["workspaces"][0]["savedReference"]["matches"], false);
    assert_eq!(review["hosts"][0]["knownToLog"], false);
    assert_eq!(
        review["hosts"][0]["workspace"],
        project.canonicalize()?.to_str().unwrap()
    );
    assert!(review["hosts"][0]["processes"].as_u64().unwrap() > 0);
    assert!(process::is_alive(identity));
    let mut wrong = identity;
    wrong.started_seconds += 1;
    assert!(
        backup_review::run(
            &journal,
            &data,
            receipt.clone(),
            Request::Stop {
                op_id: Uuid::now_v7(),
                host_id: "summary/unrecorded-task/host".into(),
                root: wrong
            }
        )
        .await
        .is_err()
    );
    assert!(process::is_alive(identity));
    let request = Request::Stop {
        op_id: Uuid::now_v7(),
        host_id: "summary/unrecorded-task/host".into(),
        root: identity,
    };
    let result = backup_review::run(&journal, &data, receipt.clone(), request.clone()).await?;
    reaper.await??;
    assert_eq!(result["hosts"][0]["processes"], 0);
    assert_eq!(result["hosts"][0]["groups"], 0);
    assert!(process::is_alive(other));
    // A crash after the host exits can leave the stop intent uncompleted.
    if let Request::Stop { op_id, .. } = &request {
        rusqlite::Connection::open(data.join("events.sqlite"))?.execute(
            "UPDATE operations SET state='dispatched' WHERE id=?1",
            [op_id.to_string()],
        )?;
    }
    let replay = backup_review::run(&journal, &data, receipt.clone(), request.clone()).await?;
    assert_eq!(replay["pendingOperations"], result["pendingOperations"]);
    if let Request::Stop { op_id, .. } = request {
        assert!(
            backup_review::run(
                &journal,
                &data,
                receipt.clone(),
                Request::Stop {
                    op_id,
                    host_id: "work/other/host".into(),
                    root: identity
                }
            )
            .await
            .is_err()
        );
    }
    assert_eq!(fs::read(project.join("kept.txt"))?, before_file);
    assert_eq!(fs::read(project.join(".git/index"))?, before_index);
    assert!(
        journal
            .fingerprint_catalog()
            .await?
            .accept(Some(&receipt))
            .is_err()
    );
    assert!(!data.join("run/rpc.sock").exists());
    assert!(!data.join("hosts").exists());
    if let Ok(output) = std::env::var("INTERCAT_BACKUP_REVIEW_OUTPUT") {
        let output = std::path::PathBuf::from(output);
        fs::create_dir_all(&output)?;
        fs::write(
            output.join("review.json"),
            serde_json::to_vec_pretty(&result)?,
        )?;
        fs::write(
            output.join("native.json"),
            serde_json::to_vec_pretty(
                &json!({"historyHidden":true,"unloggedHostStopped":true,"unrelatedProcessPreserved":true,"filesAndIndexUnchanged":true,"receiptStillBlocked":true,"paidTurns":0}),
            )?,
        )?;
    }
    unrelated.kill().await?;
    unrelated.wait().await?;
    Ok(())
}
#[tokio::test]
async fn unverified_or_missing_process_records_are_not_reported_as_stopped() -> Result<()> {
    let (root, journal, receipt) = fixture().await?;
    let data = root.path().join("restored");
    let project = root.path().join("project");
    let (identity, reaper) = process_host(&data, &project).await?;
    let ledger = data.join("summary-hosts/unrecorded-task/host/processes.json");
    fs::set_permissions(&ledger, fs::Permissions::from_mode(0o644))?;
    let review = backup_review::run(&journal, &data, receipt.clone(), Request::Inspect).await?;
    assert!(review["hosts"][0]["issue"].is_string());
    assert!(review["hosts"][0]["root"].is_null());
    assert!(
        backup_review::run(
            &journal,
            &data,
            receipt.clone(),
            Request::Stop {
                op_id: Uuid::now_v7(),
                host_id: "summary/unrecorded-task/host".into(),
                root: identity
            }
        )
        .await
        .is_err()
    );
    assert!(process::is_alive(identity));
    let db = rusqlite::Connection::open(data.join("events.sqlite"))?;
    db.execute(
        "UPDATE workers SET record=json_set(record,'$.identity',json(?1)) WHERE track_id='track'",
        [serde_json::to_string(&identity)?],
    )?;
    let review = backup_review::run(&journal, &data, receipt.clone(), Request::Inspect).await?;
    assert_eq!(review["missingProcessRecords"], json!(["work/track"]));
    fs::set_permissions(&ledger, fs::Permissions::from_mode(0o600))?;
    backup_review::run(
        &journal,
        &data,
        receipt,
        Request::Stop {
            op_id: Uuid::now_v7(),
            host_id: "summary/unrecorded-task/host".into(),
            root: identity,
        },
    )
    .await?;
    reaper.await??;
    Ok(())
}
#[tokio::test]
async fn private_review_command_never_starts_rpc_or_exposes_history() -> Result<()> {
    // macOS may hold freshly linked binaries in dyld before main. CLI waits
    // include that launch budget; RPC and Stop tests keep their own deadlines.
    let (root, journal, receipt) = fixture().await?;
    let data = root.path().join("restored");
    drop(journal);
    for request in [
        json!({"action":"inspect"}),
        json!({"action":"reviewFiles","opId":Uuid::now_v7(),"trackId":"track"}),
    ] {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_intercatd"))
            .args(["review-backup", data.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                format!(
                    "{}\n",
                    json!({"secretKey":"07".repeat(32),"receipt":receipt,"request":request})
                )
                .as_bytes(),
            )
            .await?;
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(90), child.wait_with_output())
                .await??;
        assert!(output.status.success());
        let result: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(result["review"]["historyPaused"], true);
        if request["action"] == "reviewFiles" {
            assert_eq!(result["review"]["fileReview"]["complete"], true);
        }
        assert!(!String::from_utf8_lossy(&output.stdout).contains(PRIVATE));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(PRIVATE));
        assert!(!data.join("run/rpc.sock").exists());
        assert!(!data.join("hosts").exists());
    }
    Ok(())
}

#[tokio::test]
async fn ordinary_verified_databases_cannot_use_the_offline_stop_interface() -> Result<()> {
    let (root, journal, _receipt) = fixture().await?;
    let catalog = journal.fingerprint_catalog().await?;
    let current = Receipt {
        catalog: catalog.current,
        proof: catalog.proof,
    };
    assert!(
        backup_review::run(
            &journal,
            &root.path().join("restored"),
            current,
            Request::Inspect
        )
        .await
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn file_review_covers_content_deletion_mode_ignored_files_and_detects_stale_receipts()
-> Result<()> {
    use std::os::unix::fs::symlink;
    let (root, journal, receipt) = fixture().await?;
    let data = root.path().join("restored");
    let project = root.path().join("project");
    fs::remove_file(project.join("removed.txt"))?;
    fs::remove_file(project.join("type.txt"))?;
    fs::create_dir(project.join("type.txt"))?;
    fs::write(project.join("type.txt/child.txt"), "now a directory\n")?;
    fs::set_permissions(project.join("mode.sh"), fs::Permissions::from_mode(0o755))?;
    fs::write(project.join(".gitignore"), "ignored-*\n")?;
    fs::write(project.join("ignored-work.txt"), PRIVATE)?;
    symlink(
        root.path().join("unread-external-target"),
        project.join("pointer"),
    )?;
    let before_index = fs::read(project.join(".git/index"))?;
    let request = Request::ReviewFiles {
        op_id: Uuid::now_v7(),
        track_id: "track".into(),
    };
    let result = backup_review::run(&journal, &data, receipt.clone(), request.clone()).await?;
    let review = &result["fileReview"];
    assert_eq!(review["complete"], true);
    assert_eq!(review["stale"], false);
    for (path, kind) in [
        ("kept.txt", "modified"),
        ("removed.txt", "deleted"),
        ("mode.sh", "mode-changed"),
        ("ignored-work.txt", "added"),
        ("pointer", "added"),
        ("type.txt", "type-changed"),
    ] {
        assert!(
            review["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["path"] == path && change["kind"] == kind),
            "{path}"
        );
    }
    assert_eq!(result["workspaces"][0]["filesReviewed"], true);
    assert_eq!(result["workspaces"][0]["attachmentsReviewed"], true);
    assert!(!serde_json::to_string(&result)?.contains(PRIVATE));
    let replay = backup_review::run(&journal, &data, receipt.clone(), request.clone()).await?;
    assert_eq!(replay["fileReview"], *review);
    fs::write(project.join("ignored-work.txt"), "later work")?;
    let stale = backup_review::run(&journal, &data, receipt.clone(), request.clone()).await?;
    assert_eq!(stale["fileReview"]["stale"], true);
    assert_eq!(stale["workspaces"][0]["filesReviewed"], false);
    let fresh = backup_review::run(
        &journal,
        &data,
        receipt.clone(),
        Request::ReviewFiles {
            op_id: Uuid::now_v7(),
            track_id: "track".into(),
        },
    )
    .await?;
    assert_ne!(fresh["fileReview"]["seal"], review["seal"]);
    if let Request::ReviewFiles { op_id, .. } = request {
        assert!(
            backup_review::run(
                &journal,
                &data,
                receipt,
                Request::ReviewFiles {
                    op_id,
                    track_id: "other".into()
                }
            )
            .await
            .is_err()
        );
    }
    assert_eq!(fs::read(project.join(".git/index"))?, before_index);
    let db = rusqlite::Connection::open(data.join("events.sqlite"))?;
    let payloads: String = db.query_row(
        "SELECT group_concat(payload) FROM operations WHERE kind='backup.files.review'",
        [],
        |r| r.get(0),
    )?;
    assert!(!payloads.contains(PRIVATE));
    let raw_digest = sha2::Sha256::digest(PRIVATE.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    assert!(!payloads.contains(&raw_digest));
    if let Ok(output) = std::env::var("INTERCAT_BACKUP_FILES_OUTPUT") {
        let output = std::path::PathBuf::from(output);
        fs::create_dir_all(&output)?;
        let database = format!(
            "events-{}.sqlite",
            fresh["fileReview"]["opId"].as_str().unwrap()
        );
        db.execute("VACUUM INTO ?1", [output.join(&database).to_str().unwrap()])?;
        fs::write(
            output.join("review.json"),
            serde_json::to_vec_pretty(&fresh)?,
        )?;
        fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(
                &json!({"database":database,"contentCompared":true,"ignoredFilesIncluded":true,"modeAndDeletionCompared":true,"staleReviewDetected":true,"indexUnchanged":true,"rawContentNotStored":true,"paidTurns":0}),
            )?,
        )?;
    }
    Ok(())
}

#[tokio::test]
async fn protected_special_hardlinked_and_attachment_files_never_receive_a_false_complete_review()
-> Result<()> {
    use std::os::unix::fs::symlink;
    let (root, journal, receipt) = fixture().await?;
    let data = root.path().join("restored");
    let project = root.path().join("project");
    fs::write(project.join(".env"), PRIVATE)?;
    fs::set_permissions(project.join(".env"), fs::Permissions::from_mode(0o000))?;
    fs::File::create(project.join("oversized"))?.set_len(64 * 1024 * 1024 + 1)?;
    fs::write(root.path().join("outside"), PRIVATE)?;
    fs::hard_link(root.path().join("outside"), project.join("hardlink"))?;
    symlink(root.path(), project.join("outside-link"))?;
    let pipe = std::ffi::CString::new(project.join("pipe").as_os_str().as_encoded_bytes())?;
    assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);
    fs::create_dir_all(project.join(".intercat/attachments"))?;
    fs::write(
        project.join(".intercat/attachments/unknown.txt"),
        "unverified copy",
    )?;
    let result = backup_review::run(
        &journal,
        &data,
        receipt,
        Request::ReviewFiles {
            op_id: Uuid::now_v7(),
            track_id: "track".into(),
        },
    )
    .await?;
    let review = &result["fileReview"];
    assert_eq!(review["complete"], false);
    assert_eq!(review["protected"], json!([".env"]));
    assert!(
        review["unsupported"]
            .as_array()
            .unwrap()
            .contains(&json!("pipe"))
    );
    assert!(
        review["unsupported"]
            .as_array()
            .unwrap()
            .contains(&json!("hardlink"))
    );
    assert!(
        review["unsupported"]
            .as_array()
            .unwrap()
            .contains(&json!("oversized"))
    );
    assert_eq!(
        review["attachments"]["copies"]
            .as_array()
            .unwrap()
            .iter()
            .find(|copy| copy["path"] == ".intercat/attachments/unknown.txt")
            .unwrap()["state"],
        "unowned"
    );
    assert_eq!(result["workspaces"][0]["attachmentsReviewed"], false);
    assert!(
        !review["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["path"].as_str().unwrap().starts_with("outside-link/"))
    );
    assert!(!serde_json::to_string(&result)?.contains(PRIVATE));
    Ok(())
}

#[tokio::test]
async fn file_review_requires_stopped_owned_processes_and_refuses_a_replaced_workspace()
-> Result<()> {
    use std::os::unix::fs::symlink;
    let (root, journal, receipt) = fixture().await?;
    let data = root.path().join("restored");
    let project = root.path().join("project");
    let (identity, reaper) = process_host(&data, &project).await?;
    assert!(
        backup_review::run(
            &journal,
            &data,
            receipt.clone(),
            Request::ReviewFiles {
                op_id: Uuid::now_v7(),
                track_id: "track".into()
            }
        )
        .await
        .is_err()
    );
    backup_review::run(
        &journal,
        &data,
        receipt.clone(),
        Request::Stop {
            op_id: Uuid::now_v7(),
            host_id: "summary/unrecorded-task/host".into(),
            root: identity,
        },
    )
    .await?;
    reaper.await??;
    let saved = root.path().join("saved-project");
    fs::rename(&project, &saved)?;
    symlink(&saved, &project)?;
    assert!(
        backup_review::run(
            &journal,
            &data,
            receipt,
            Request::ReviewFiles {
                op_id: Uuid::now_v7(),
                track_id: "track".into()
            }
        )
        .await
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn changed_saved_file_review_metadata_is_not_accepted_as_a_current_confirmation() -> Result<()>
{
    let (root, journal, receipt) = fixture().await?;
    let data = root.path().join("restored");
    let id = Uuid::now_v7();
    let request = Request::ReviewFiles {
        op_id: id,
        track_id: "track".into(),
    };
    let original = backup_review::run(&journal, &data, receipt.clone(), request.clone()).await?;
    assert!(
        !original["fileReview"]["changes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let db = rusqlite::Connection::open(data.join("events.sqlite"))?;
    db.execute(
        "UPDATE operations SET payload=json_set(payload,'$.review.changes',json('[]')) WHERE id=?1",
        [id.to_string()],
    )?;
    let result = backup_review::run(&journal, &data, receipt, request).await?;
    assert_eq!(result["fileReview"]["stale"], true);
    assert_eq!(result["workspaces"][0]["filesReviewed"], false);
    Ok(())
}

async fn referenced_fixture(
    pasted: bool,
) -> Result<(
    tempfile::TempDir,
    Journal,
    Receipt,
    intercat_daemon::attachment_store::Attachment,
    std::path::PathBuf,
)> {
    use intercat_daemon::decision::{Decision, Kind, Outcome, Target};
    let (root, journal, receipt) = fixture().await?;
    let mut workspace: Workspace =
        serde_json::from_value(journal.snapshot().await?["workspaces"][0].clone())?;
    workspace.attachment_roots =
        intercat_daemon::attachments::reserve(&workspace.path, &workspace.base)?;
    journal.workspace_result(workspace).await?;
    let source = root.path().join("reference.txt");
    let attachment = if pasted {
        fs::write(
            &source,
            format!(
                "{PRIVATE}\n{}",
                concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H")
            ),
        )?;
        journal
            .import_pasted_text(Uuid::now_v7(), source.clone())
            .await?
    } else {
        fs::write(&source, PRIVATE)?;
        journal
            .import_attachment(Uuid::now_v7(), source.clone())
            .await?
    };
    fs::remove_file(source)?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Read the reference".into(),
            Target::Lock("track".into()),
            None,
            vec![attachment.id],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id.clone(),
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some("track".into()),
                boundary: None,
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let references = journal
        .materialize_input_attachments(
            input.id.clone(),
            "track".into(),
            "session".into(),
            root.path().join("project"),
        )
        .await?;
    let command = Uuid::now_v7();
    journal
        .reserve_turn(
            command,
            input.id,
            input.revision,
            "track".into(),
            json!({"commandId":command,"sessionId":"session","input":[]}),
            None,
        )
        .await?;
    journal.try_msp("track".into(),"session".into(),intercat_daemon::clock::Stamp { epoch_ms:1,mono_ms:1,boot_id:"fixture".into(),timeline_ms:1 },json!({"method":"turn/started","params":{"commandId":command,"sessionId":"session","turnId":command}}),None,None)?.await??;
    Ok((
        root,
        journal,
        receipt,
        attachment,
        references.files[0].path.clone(),
    ))
}
fn compare_request() -> Request {
    Request::ReviewFiles {
        op_id: Uuid::now_v7(),
        track_id: "track".into(),
    }
}

#[tokio::test]
async fn pasted_original_and_redacted_copy_are_independently_verified_after_backup() -> Result<()> {
    let (root, journal, receipt, attachment, copy) = referenced_fixture(true).await?;
    assert_eq!(
        fs::read_to_string(&copy)?,
        format!("{PRIVATE}\n<redacted:api-key#1>")
    );
    let data = root.path().join("restored");
    let result = backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    assert_eq!(result["workspaces"][0]["attachmentsReviewed"], true);
    assert_eq!(
        result["fileReview"]["attachments"]["copies"][0]["state"],
        "matched"
    );
    let original = data.join("attachments").join(attachment.id.to_string());
    let mut bytes = fs::read(&original)?;
    let header = u32::from_be_bytes(bytes[8..12].try_into()?);
    bytes[12 + header as usize] ^= 1;
    fs::set_permissions(&original, fs::Permissions::from_mode(0o600))?;
    fs::write(&original, bytes)?;
    fs::set_permissions(&original, fs::Permissions::from_mode(0o400))?;
    let changed = backup_review::run(&journal, &data, receipt, compare_request()).await?;
    assert_eq!(changed["workspaces"][0]["attachmentsReviewed"], false);
    assert_eq!(
        changed["fileReview"]["attachments"]["invalidOriginals"][0],
        attachment.id.to_string()
    );
    assert_eq!(
        fs::read_to_string(copy)?,
        format!("{PRIVATE}\n<redacted:api-key#1>")
    );
    Ok(())
}
async fn recovery_command(data: &Path, receipt: &Receipt, request: Request) -> Result<Value> {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_intercatd"))
        .args(["review-backup", data.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            format!(
                "{}\n",
                json!({"secretKey":"07".repeat(32),"receipt":receipt,"request":request})
            )
            .as_bytes(),
        )
        .await?;
    let result = tokio::time::timeout(std::time::Duration::from_secs(90), child.wait_with_output())
        .await??;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!String::from_utf8_lossy(&result.stdout).contains(PRIVATE));
    assert!(!data.join("run/rpc.sock").exists());
    assert!(!data.join("hosts").exists());
    Ok(serde_json::from_slice::<Value>(&result.stdout)?["review"].clone())
}

#[tokio::test]
async fn restored_originals_and_owned_copies_are_verified_without_writes_and_changed_sets_are_stale()
-> Result<()> {
    let (root, journal, receipt, attachment, copy) = referenced_fixture(false).await?;
    let data = root.path().join("restored");
    let original = data.join("attachments").join(attachment.id.to_string());
    let before_original = fs::read(&original)?;
    let index = fs::read(root.path().join("project/.git/index"))?;
    let request = compare_request();
    let result = backup_review::run(&journal, &data, receipt.clone(), request.clone()).await?;
    assert_eq!(result["workspaces"][0]["attachmentsReviewed"], true);
    assert_eq!(result["fileReview"]["attachments"]["verifiedOriginals"], 1);
    assert_eq!(
        result["fileReview"]["attachments"]["copies"][0]["state"],
        "matched"
    );
    assert!(
        result["fileReview"]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| !c["path"]
                .as_str()
                .unwrap()
                .contains(".intercat/attachments"))
    );
    assert_eq!(
        backup_review::run(&journal, &data, receipt.clone(), request.clone()).await?["fileReview"],
        result["fileReview"]
    );
    fs::write(&copy, "CHANGED_PRIVATE_COPY")?;
    let stale = backup_review::run(&journal, &data, receipt.clone(), request).await?;
    assert_eq!(stale["fileReview"]["stale"], true);
    assert_eq!(stale["workspaces"][0]["attachmentsReviewed"], false);
    let changed = backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    assert_eq!(
        changed["fileReview"]["attachments"]["copies"][0]["state"],
        "changed"
    );
    fs::write(&copy, PRIVATE)?;
    let fresh = backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    assert_eq!(fresh["workspaces"][0]["attachmentsReviewed"], true);
    assert_eq!(fs::read(&original)?, before_original);
    assert_eq!(fs::read(&copy)?, PRIVATE.as_bytes());
    assert_eq!(fs::read(root.path().join("project/.git/index"))?, index);
    assert_eq!(journal.fingerprint_catalog().await?.current.revision, 0);
    let db = rusqlite::Connection::open(data.join("events.sqlite"))?;
    let payloads: String = db.query_row(
        "SELECT group_concat(payload) FROM operations WHERE kind='backup.files.review'",
        [],
        |r| r.get(0),
    )?;
    assert!(!payloads.contains(PRIVATE) && !payloads.contains("CHANGED_PRIVATE_COPY"));
    assert!(!payloads.contains(&attachment.sha256));
    drop(journal);
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_intercatd"))
        .args(["review-backup", data.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            format!(
                "{}\n",
                json!({"secretKey":"07".repeat(32),"receipt":receipt,"request":compare_request()})
            )
            .as_bytes(),
        )
        .await?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(90), child.wait_with_output())
        .await??;
    assert!(output.status.success());
    let cli: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(cli["review"]["workspaces"][0]["attachmentsReviewed"], true);
    assert_eq!(cli["review"]["historyPaused"], true);
    assert!(!String::from_utf8_lossy(&output.stdout).contains(PRIVATE));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(PRIVATE));
    assert!(!data.join("run/rpc.sock").exists() && !data.join("hosts").exists());
    if let Ok(destination) = std::env::var("INTERCAT_BACKUP_ATTACHMENTS_OUTPUT") {
        let destination = std::path::PathBuf::from(destination);
        fs::create_dir_all(&destination)?;
        db.execute(
            "VACUUM INTO ?1",
            [destination.join("events.sqlite").to_str().unwrap()],
        )?;
        fs::write(
            destination.join("result.json"),
            serde_json::to_vec_pretty(
                &json!({"schema":intercat_daemon::journal::SCHEMA_VERSION,"matched":result,"changed":changed,"stale":stale,"restored":fresh,"cli":cli,"attachment":attachment,"originalUnchanged":true,"indexUnchanged":true,"privateBytesNotRecorded":true,"paidTurns":0}),
            )?,
        )?;
    }
    Ok(())
}

#[tokio::test]
async fn missing_or_changed_originals_and_unsafe_copies_remain_unverified_without_recreation()
-> Result<()> {
    use std::os::unix::fs::symlink;
    let (root, journal, receipt, attachment, copy) = referenced_fixture(false).await?;
    let data = root.path().join("restored");
    let originals = data.join("attachments");
    let original = originals.join(attachment.id.to_string());
    let saved = fs::read(&original)?;
    fs::set_permissions(&original, fs::Permissions::from_mode(0o600))?;
    let mut corrupt = saved.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    fs::write(&original, &corrupt)?;
    fs::set_permissions(&original, fs::Permissions::from_mode(0o400))?;
    let invalid = backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    assert_eq!(
        invalid["fileReview"]["attachments"]["invalidOriginals"],
        json!([attachment.id])
    );
    assert_eq!(
        invalid["fileReview"]["attachments"]["copies"][0]["state"],
        "original-unverified"
    );
    fs::set_permissions(&original, fs::Permissions::from_mode(0o600))?;
    fs::write(&original, &saved)?;
    fs::set_permissions(&original, fs::Permissions::from_mode(0o400))?;
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o755))?;
    let executable =
        backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    assert_eq!(
        executable["fileReview"]["attachments"]["copies"][0]["state"],
        "changed"
    );
    fs::set_permissions(&copy, fs::Permissions::from_mode(0o600))?;
    fs::set_permissions(copy.parent().unwrap(), fs::Permissions::from_mode(0o777))?;
    let open_directory =
        backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    assert_eq!(
        open_directory["workspaces"][0]["attachmentsReviewed"],
        false
    );
    assert!(
        open_directory["fileReview"]["attachments"]["copies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["state"] == "unsafe-directory")
    );
    fs::set_permissions(copy.parent().unwrap(), fs::Permissions::from_mode(0o700))?;
    let outside = root.path().join("outside");
    fs::write(&outside, PRIVATE)?;
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o000))?;
    fs::remove_file(&copy)?;
    symlink(&outside, &copy)?;
    let unsafe_copy =
        backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    assert_eq!(
        unsafe_copy["fileReview"]["attachments"]["copies"][0]["state"],
        "changed"
    );
    assert_eq!(unsafe_copy["workspaces"][0]["attachmentsReviewed"], false);
    fs::remove_file(&copy)?;
    fs::hard_link(&outside, &copy)?;
    let linked = backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    assert_eq!(
        linked["fileReview"]["attachments"]["copies"][0]["state"],
        "changed"
    );
    fs::remove_file(&copy)?;
    fs::write(&copy, PRIVATE)?;
    fs::set_permissions(&original, fs::Permissions::from_mode(0o600))?;
    fs::OpenOptions::new()
        .write(true)
        .open(&original)?
        .set_len(1024 * 1024 * 1024 + 1)?;
    fs::set_permissions(&original, fs::Permissions::from_mode(0o400))?;
    let error = backup_review::run(&journal, &data, receipt.clone(), compare_request())
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Private originals exceed the review byte limit")
    );
    assert_eq!(fs::metadata(&original)?.len(), 1024 * 1024 * 1024 + 1);
    fs::remove_dir_all(&originals)?;
    let missing = backup_review::run(&journal, &data, receipt, compare_request()).await?;
    assert_eq!(
        missing["fileReview"]["attachments"]["missingOriginals"],
        json!([attachment.id])
    );
    assert!(!originals.exists());
    assert_eq!(missing["workspaces"][0]["attachmentsReviewed"], false);
    assert_eq!(fs::read(&copy)?, PRIVATE.as_bytes());
    assert!(!serde_json::to_string(&missing)?.contains(PRIVATE));
    Ok(())
}

#[tokio::test]
async fn newer_originals_and_unassigned_copies_are_reported_and_forged_reviews_are_stale()
-> Result<()> {
    let (root, journal, receipt, _attachment, copy) = referenced_fixture(false).await?;
    let data = root.path().join("restored");
    let request = compare_request();
    let first = backup_review::run(&journal, &data, receipt.clone(), request.clone()).await?;
    let extra = root.path().join("extra.txt");
    fs::write(&extra, "AFTER_BACKUP_ORIGINAL")?;
    let added = journal.import_attachment(Uuid::now_v7(), extra).await?;
    let db = rusqlite::Connection::open(data.join("events.sqlite"))?;
    db.execute(
        "DELETE FROM attachments WHERE id=?1",
        [added.id.to_string()],
    )?;
    db.execute("DELETE FROM operations WHERE id=?1", [added.id.to_string()])?;
    db.execute(
        "DELETE FROM events WHERE kind='attachment.saved' AND json_extract(payload,'$.id')=?1",
        [added.id.to_string()],
    )?;
    let extra_copy = copy.parent().unwrap().join(format!("{}.txt", added.id));
    fs::write(&extra_copy, "AFTER_BACKUP_COPY")?;
    assert_eq!(
        backup_review::run(&journal, &data, receipt.clone(), request).await?["fileReview"]["stale"],
        true
    );
    let request = compare_request();
    let newer = backup_review::run(&journal, &data, receipt.clone(), request.clone()).await?;
    assert_eq!(
        newer["fileReview"]["attachments"]["unrecordedOriginals"],
        json!([added.id.to_string()])
    );
    assert!(
        newer["fileReview"]["attachments"]["copies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["state"] == "unrecorded")
    );
    assert_eq!(newer["workspaces"][0]["attachmentsReviewed"], false);
    assert!(extra_copy.exists() && data.join("attachments").join(added.id.to_string()).exists());
    if let Request::ReviewFiles { op_id, .. } = request.clone() {
        db.execute("UPDATE operations SET payload=json_set(payload,'$.review.attachments.complete',json('true')) WHERE id=?1",[op_id.to_string()])?;
    }
    let forged = backup_review::run(&journal, &data, receipt, request).await?;
    assert_eq!(forged["fileReview"]["stale"], true);
    assert_eq!(forged["workspaces"][0]["attachmentsReviewed"], false);
    assert_eq!(first["historyPaused"], true);
    assert!(!serde_json::to_string(&newer)?.contains("AFTER_BACKUP_COPY"));
    Ok(())
}

fn session_choice(
    choice: intercat_daemon::backup_choices::Choice,
    review_id: Option<Uuid>,
    acknowledge_unread: bool,
) -> Request {
    Request::ChooseSession(intercat_daemon::backup_choices::Request {
        op_id: Uuid::now_v7(),
        session_id: "session".into(),
        choice,
        review_id,
        acknowledge_unread,
    })
}
#[tokio::test]
async fn session_choices_are_review_bound_durable_and_do_not_reopen_or_change_history() -> Result<()>
{
    use intercat_daemon::backup_choices::Choice;
    let (root, journal, receipt) = fixture().await?;
    let data = root.path().join("restored");
    let project = root.path().join("project");
    let files = fs::read(project.join("kept.txt"))?;
    let index = fs::read(project.join(".git/index"))?;
    let reviewed = compare_request();
    let review = backup_review::run(&journal, &data, receipt.clone(), reviewed.clone()).await?;
    let review_id: Uuid = review["fileReview"]["opId"].as_str().unwrap().parse()?;
    let request = session_choice(Choice::PickUp, Some(review_id), false);
    assert_eq!(serde_json::to_value(&request)?["action"], "chooseSession");
    let chosen = backup_review::run(&journal, &data, receipt.clone(), request.clone()).await?;
    assert_eq!(chosen["sessions"][0]["choice"]["choice"], "pick-up");
    assert_eq!(chosen["sessions"][0]["choice"]["stale"], false);
    assert_eq!(chosen["historyPaused"], true);
    assert_eq!(
        backup_review::run(&journal, &data, receipt.clone(), request.clone()).await?["sessions"],
        chosen["sessions"]
    );
    let catalog = journal.fingerprint_catalog().await?;
    drop(journal);
    let journal = Journal::open(data.join("events.sqlite"), Key::new([7; 32])).await?;
    assert_eq!(
        backup_review::run(&journal, &data, receipt.clone(), Request::Inspect).await?["sessions"],
        chosen["sessions"]
    );
    let changed = session_choice(Choice::DiscardHistory, Some(review_id), false);
    let result = backup_review::run(&journal, &data, receipt.clone(), changed.clone()).await?;
    assert_eq!(result["sessions"][0]["choice"]["choice"], "discard-history");
    assert!(
        backup_review::run(&journal, &data, receipt.clone(), request)
            .await
            .is_err()
    );
    assert_eq!(
        journal.fingerprint_catalog().await?.current,
        catalog.current
    );
    assert_eq!(fs::read(project.join("kept.txt"))?, files);
    assert_eq!(fs::read(project.join(".git/index"))?, index);
    assert!(!data.join("run/rpc.sock").exists() && !data.join("hosts").exists());
    let db = rusqlite::Connection::open(data.join("events.sqlite"))?;
    let records: String = db.query_row(
        "SELECT group_concat(payload) FROM operations WHERE kind='backup.session.choice'",
        [],
        |r| r.get(0),
    )?;
    assert!(!records.contains(PRIVATE));
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM operations WHERE kind='backup.session.choice'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        2
    );
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_intercatd"))
        .args(["review-backup", data.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(
            format!(
                "{}\n",
                json!({"secretKey":"07".repeat(32),"receipt":receipt,"request":changed})
            )
            .as_bytes(),
        )
        .await?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(90), child.wait_with_output())
        .await??;
    assert!(output.status.success());
    let cli: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        cli["review"]["sessions"][0]["choice"]["choice"],
        "discard-history"
    );
    assert_eq!(cli["review"]["historyPaused"], true);
    assert!(!String::from_utf8_lossy(&output.stdout).contains(PRIVATE));
    if let Ok(destination) = std::env::var("INTERCAT_BACKUP_CHOICES_OUTPUT") {
        let destination = std::path::PathBuf::from(destination);
        fs::create_dir_all(&destination)?;
        db.execute(
            "VACUUM INTO ?1",
            [destination.join("events.sqlite").to_str().unwrap()],
        )?;
        fs::write(
            destination.join("result.json"),
            serde_json::to_vec_pretty(
                &json!({"schema":intercat_daemon::journal::SCHEMA_VERSION,"picked":chosen,"discarded":result,"cli":cli,"filesUnchanged":true,"catalogUnchanged":true,"paidTurns":0}),
            )?,
        )?;
    }
    db.execute("UPDATE backup_session_choices SET record=json_set(record,'$.request.choice','pick-up') WHERE session='session'",[])?;
    assert!(
        backup_review::run(&journal, &data, receipt, Request::Inspect)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn changed_files_invalid_targets_and_unverified_originals_cannot_silently_receive_a_pickup_choice()
-> Result<()> {
    use intercat_daemon::backup_choices::Choice;
    let (root, journal, receipt) = referenced_fixture(false)
        .await
        .map(|(a, b, c, _, _)| (a, b, c))?;
    let data = root.path().join("restored");
    assert!(
        backup_review::run(
            &journal,
            &data,
            receipt.clone(),
            session_choice(Choice::PickUp, None, false)
        )
        .await
        .is_err()
    );
    let result = backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    let id = result["fileReview"]["opId"].as_str().unwrap().parse()?;
    let mut unknown = session_choice(Choice::DiscardHistory, Some(id), false);
    if let Request::ChooseSession(choice) = &mut unknown {
        choice.session_id = "unknown".into();
    }
    assert!(
        backup_review::run(&journal, &data, receipt.clone(), unknown)
            .await
            .is_err()
    );
    fs::write(root.path().join("project/kept.txt"), "later edit")?;
    assert!(
        backup_review::run(
            &journal,
            &data,
            receipt.clone(),
            session_choice(Choice::PickUp, Some(id), false)
        )
        .await
        .is_err()
    );
    fs::write(root.path().join("project/.env"), PRIVATE)?;
    fs::set_permissions(
        root.path().join("project/.env"),
        fs::Permissions::from_mode(0o000),
    )?;
    let result = backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    let id = result["fileReview"]["opId"].as_str().unwrap().parse()?;
    assert!(
        backup_review::run(
            &journal,
            &data,
            receipt.clone(),
            session_choice(Choice::PickUp, Some(id), true)
        )
        .await
        .is_err()
    );
    assert!(
        backup_review::run(
            &journal,
            &data,
            receipt.clone(),
            session_choice(Choice::DiscardHistory, Some(id), false)
        )
        .await
        .is_err()
    );
    let accepted = backup_review::run(
        &journal,
        &data,
        receipt.clone(),
        session_choice(Choice::DiscardHistory, Some(id), true),
    )
    .await?;
    assert_eq!(accepted["sessions"][0]["choice"]["acknowledgeUnread"], true);
    assert!(!serde_json::to_string(&accepted)?.contains(PRIVATE));
    let db = rusqlite::Connection::open(data.join("events.sqlite"))?;
    db.execute("UPDATE workspaces SET record=json_set(record,'$.reference','refs/intercat/changed') WHERE track_id='track'",[])?;
    assert_eq!(
        backup_review::run(&journal, &data, receipt, Request::Inspect).await?["sessions"][0]["choice"]
            ["stale"],
        true
    );
    Ok(())
}

#[tokio::test]
async fn sessions_missing_from_the_restored_workspace_require_acknowledged_discard() -> Result<()> {
    use intercat_daemon::backup_choices::Choice;
    let (root, journal, receipt) = fixture().await?;
    let data = root.path().join("restored");
    rusqlite::Connection::open(data.join("events.sqlite"))?
        .execute("DELETE FROM workspaces", [])?;
    assert!(
        backup_review::run(
            &journal,
            &data,
            receipt.clone(),
            session_choice(Choice::PickUp, None, true)
        )
        .await
        .is_err()
    );
    assert!(
        backup_review::run(
            &journal,
            &data,
            receipt.clone(),
            session_choice(Choice::DiscardHistory, None, false)
        )
        .await
        .is_err()
    );
    let result = backup_review::run(
        &journal,
        &data,
        receipt,
        session_choice(Choice::DiscardHistory, None, true),
    )
    .await?;
    assert_eq!(result["sessions"][0]["canPickUp"], false);
    assert_eq!(result["sessions"][0]["choice"]["choice"], "discard-history");
    assert!(root.path().join("project/kept.txt").exists());
    Ok(())
}

#[tokio::test]
async fn applied_file_pickup_retires_only_the_old_session_and_waits_for_external_confirmation()
-> Result<()> {
    use intercat_daemon::backup_choices::Choice;
    let (root, journal, receipt, attachment, copy) = referenced_fixture(false).await?;
    let data = root.path().join("restored");
    let project = root.path().join("project");
    let latest = "refs/intercat/test/A-checkpoint";
    let original_head = git(&project, &["rev-parse", "HEAD"]);
    git(&project, &["update-ref", latest, &original_head]);
    let index = fs::read(project.join(".git/index"))?;
    let files = fs::read(project.join("kept.txt"))?;
    let db = rusqlite::Connection::open(data.join("events.sqlite"))?;
    let older = root.path().join("before-apply.sqlite");
    db.execute("VACUUM INTO ?1", [older.to_str().unwrap()])?;
    let (input, command): (String, String) = db.query_row(
        "SELECT id,dispatch_command FROM utterances WHERE dispatch_command IS NOT NULL",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    db.execute(
        "INSERT INTO integration_jobs VALUES(?1,'project-session',0,?2)",
        rusqlite::params![
            Uuid::now_v7().to_string(),
            json!({"status":"done","tracks":[{"id":"track"}],"oldReport":PRIVATE}).to_string()
        ],
    )?;
    db.execute(
        "INSERT INTO operations VALUES(?1,'checkpoint.save',?2,'complete')",
        rusqlite::params![
            Uuid::now_v7().to_string(),
            json!({"trackId":"track","checkpoint":{"turnId":command}}).to_string()
        ],
    )?;
    journal.try_msp("track".into(),"session".into(),intercat_daemon::clock::Stamp {epoch_ms:2,mono_ms:2,boot_id:"fixture".into(),timeline_ms:2},json!({"method":"item/updated","params":{"sessionId":"session","item":{"itemId":"old-user","turnId":command,"commandId":command,"kind":"userMessage","revision":1,"status":"completed","content":[{"type":"text","text":PRIVATE}]}}}),None,None)?.await??;
    db.execute(
        "INSERT INTO tracks VALUES('other','B','Other','waiting')",
        [],
    )?;
    journal.try_msp("other".into(),"other-session".into(),intercat_daemon::clock::Stamp {epoch_ms:3,mono_ms:3,boot_id:"fixture".into(),timeline_ms:3},json!({"method":"turn/started","params":{"sessionId":"other-session","turnId":"other-turn"}}),None,None)?.await??;
    let reviewed = backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    let review = reviewed["fileReview"]["opId"].as_str().unwrap().parse()?;
    backup_review::run(
        &journal,
        &data,
        receipt.clone(),
        session_choice(Choice::PickUp, Some(review), false),
    )
    .await?;
    let operation = Uuid::now_v7();
    let request = Request::Apply {
        op_id: operation,
        acknowledge_unverified: false,
    };
    let prepared = backup_review::run(&journal, &data, receipt.clone(), request.clone()).await?;
    assert_eq!(prepared["apply"]["phase"], "awaiting-receipt");
    assert_eq!(prepared["historyPaused"], true);
    let target: Receipt = serde_json::from_value(prepared["publication"]["target"].clone())?;
    assert!(target.catalog.revision > receipt.catalog.revision);
    assert!(
        journal
            .fingerprint_catalog()
            .await?
            .accept(Some(&receipt))
            .is_err()
    );
    assert!(
        target
            .proof
            .sessions
            .iter()
            .any(|s| s.session_id == "session")
    );
    let old = Journal::open(&older, Key::new([7; 32])).await?;
    assert!(
        old.fingerprint_catalog()
            .await?
            .accept(Some(&target))
            .is_err()
    );
    let rollback = backup_review::run(&old, root.path(), target.clone(), Request::Inspect).await?;
    assert!(
        rollback["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["sessionId"] == "session")
    );
    assert!(journal.session_mask("track", "session").await.is_err());
    assert!(
        journal
            .stage_utterance(
                input.parse()?,
                "Do not replay".into(),
                intercat_daemon::decision::Target::Lock("track".into()),
                None,
                vec![]
            )
            .await
            .is_err()
    );
    assert!(
        journal
            .try_msp(
                "track".into(),
                "session".into(),
                intercat_daemon::clock::Stamp {
                    epoch_ms: 4,
                    mono_ms: 4,
                    boot_id: "fixture".into(),
                    timeline_ms: 4
                },
                json!({"method":"turn/started","params":{"sessionId":"session","turnId":"late"}}),
                None,
                None
            )?
            .await?
            .is_err()
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM timeline_turns WHERE session='session'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM integration_jobs", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM operations WHERE kind='checkpoint.save'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM timeline_turns WHERE session='other-session'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    let (new_session, resume): (String, String) = db.query_row(
        "SELECT json_extract(record,'$.sessionId'),record FROM backup_resumes WHERE track='track'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(
        git(&project, &["rev-parse", latest]),
        serde_json::from_str::<Value>(&resume)?["checkpoint"]["sha"]
            .as_str()
            .unwrap()
    );
    assert_ne!(new_session, "session");
    assert_eq!(
        db.query_row(
            "SELECT attachment FROM attachment_turn_items WHERE session=?1",
            [&new_session],
            |r| r.get::<_, String>(0)
        )?,
        attachment.id.to_string()
    );
    assert!(
        journal
            .backup_apply_recovery(Some(target.clone()))
            .await?
            .is_some()
    );
    assert_eq!(fs::read(&copy)?, PRIVATE.as_bytes());
    assert_eq!(fs::read(project.join(".git/index"))?, index);
    assert_eq!(fs::read(project.join("kept.txt"))?, files);
    drop(journal);
    // Simulate a crash after the DB commit but before publishing its ref intent.
    git(&project, &["update-ref", latest, &original_head]);
    let journal = Journal::open(data.join("events.sqlite"), Key::new([7; 32])).await?;
    assert_eq!(
        recovery_command(&data, &receipt, request).await?["publication"],
        prepared["publication"]
    );
    assert_eq!(
        git(&project, &["rev-parse", latest]),
        serde_json::from_str::<Value>(&resume)?["checkpoint"]["sha"]
            .as_str()
            .unwrap()
    );
    assert!(
        backup_review::run(
            &journal,
            &data,
            receipt.clone(),
            Request::ConfirmApply { op_id: operation }
        )
        .await
        .is_err()
    );
    let complete =
        recovery_command(&data, &target, Request::ConfirmApply { op_id: operation }).await?;
    assert_eq!(complete["apply"]["phase"], "complete");
    assert_eq!(complete["historyPaused"], false);
    assert!(
        journal
            .backup_apply_recovery(Some(target.clone()))
            .await?
            .is_none()
    );
    assert!(
        journal
            .fingerprint_catalog()
            .await?
            .accept(Some(&target))
            .is_ok()
    );
    assert!(
        journal
            .fingerprint_catalog()
            .await?
            .accept(Some(&receipt))
            .is_err()
    );
    let handoff:String=db.query_row("SELECT text FROM utterances WHERE state='placed' AND json_extract(decision,'$.trackId')='track'",[],|r|r.get(0))?;
    assert!(!handoff.contains(PRIVATE));
    assert!(handoff.contains("Previous session history, instructions and results are unavailable"));
    assert_eq!(fs::read(project.join(".git/index"))?, index);
    assert_eq!(fs::read(project.join("kept.txt"))?, files);
    if let Ok(destination) = std::env::var("INTERCAT_BACKUP_APPLY_OUTPUT") {
        let destination = std::path::PathBuf::from(destination);
        fs::create_dir_all(&destination)?;
        db.execute(
            "VACUUM INTO ?1",
            [destination.join("events.sqlite").to_str().unwrap()],
        )?;
        fs::write(
            destination.join("result.json"),
            serde_json::to_vec_pretty(
                &json!({"schema":intercat_daemon::journal::SCHEMA_VERSION,"prepared":prepared,"complete":complete,"newSession":new_session,"resume":serde_json::from_str::<Value>(&resume)?,"attachment":attachment,"sourceRejected":true,"oldHistoryRejected":true,"filesUnchanged":true,"indexUnchanged":true,"paidTurns":0}),
            )?,
        )?;
    }
    Ok(())
}

#[tokio::test]
async fn apply_requires_current_decisions_and_preserves_unverified_files_on_discard() -> Result<()>
{
    use intercat_daemon::backup_choices::Choice;
    let (root, journal, receipt) = fixture().await?;
    let data = root.path().join("restored");
    let project = root.path().join("project");
    let latest = "refs/intercat/test/A-checkpoint";
    git(
        &project,
        &["update-ref", latest, &git(&project, &["rev-parse", "HEAD"])],
    );
    assert!(
        backup_review::run(
            &journal,
            &data,
            receipt.clone(),
            Request::Apply {
                op_id: Uuid::now_v7(),
                acknowledge_unverified: false
            }
        )
        .await
        .is_err()
    );
    fs::write(project.join(".env"), PRIVATE)?;
    fs::set_permissions(project.join(".env"), fs::Permissions::from_mode(0o000))?;
    let checked = backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    let review = checked["fileReview"]["opId"].as_str().unwrap().parse()?;
    backup_review::run(
        &journal,
        &data,
        receipt.clone(),
        session_choice(Choice::DiscardHistory, Some(review), true),
    )
    .await?;
    let id = Uuid::now_v7();
    let result = backup_review::run(
        &journal,
        &data,
        receipt,
        Request::Apply {
            op_id: id,
            acknowledge_unverified: false,
        },
    )
    .await?;
    let target: Receipt = serde_json::from_value(result["publication"]["target"].clone())?;
    assert!(
        intercat_git::Repository::open(&project)?
            .reference_target(latest)?
            .is_none()
    );
    assert_eq!(
        fs::metadata(project.join(".env"))?.permissions().mode() & 0o777,
        0
    );
    let result =
        backup_review::run(&journal, &data, target, Request::ConfirmApply { op_id: id }).await?;
    assert_eq!(result["historyPaused"], false);
    assert_eq!(
        journal.snapshot().await?["workers"][0]["sessionId"],
        Value::Null
    );
    assert_eq!(
        rusqlite::Connection::open(data.join("events.sqlite"))?.query_row(
            "SELECT count(*) FROM backup_resumes",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn prepared_recovery_keeps_later_edits_and_rejects_a_changed_snapshot_reference() -> Result<()>
{
    use intercat_daemon::backup_choices::Choice;
    let (root, journal, receipt, _, _) = referenced_fixture(false).await?;
    let data = root.path().join("restored");
    let project = root.path().join("project");
    let review = backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    backup_review::run(
        &journal,
        &data,
        receipt.clone(),
        session_choice(
            Choice::PickUp,
            Some(review["fileReview"]["opId"].as_str().unwrap().parse()?),
            false,
        ),
    )
    .await?;
    let id = Uuid::now_v7();
    let prepared = backup_review::run(
        &journal,
        &data,
        receipt,
        Request::Apply {
            op_id: id,
            acknowledge_unverified: false,
        },
    )
    .await?;
    let target: Receipt = serde_json::from_value(prepared["publication"]["target"].clone())?;
    let (identity, reaper) = process_host(&data, &project).await?;
    let pending = backup_review::run(&journal, &data, target.clone(), Request::Inspect).await?;
    assert_eq!(pending["apply"]["opId"], id.to_string());
    assert!(pending["hosts"][0]["processes"].as_u64().unwrap() > 0);
    assert!(
        backup_review::run(&journal, &data, target.clone(), compare_request())
            .await
            .is_err()
    );
    assert!(
        backup_review::run(
            &journal,
            &data,
            target.clone(),
            Request::ConfirmApply { op_id: id }
        )
        .await
        .is_err()
    );
    let stopped = backup_review::run(
        &journal,
        &data,
        target.clone(),
        Request::Stop {
            op_id: Uuid::now_v7(),
            host_id: "summary/unrecorded-task/host".into(),
            root: identity,
        },
    )
    .await?;
    reaper.await??;
    assert_eq!(stopped["hosts"][0]["processes"], 0);
    assert_eq!(stopped["apply"]["opId"], id.to_string());
    let db = rusqlite::Connection::open(data.join("events.sqlite"))?;
    let record: String = db.query_row(
        "SELECT record FROM backup_resumes WHERE track='track'",
        [],
        |r| r.get(0),
    )?;
    let record: Value = serde_json::from_str(&record)?;
    let reference = record["checkpoint"]["reference"].as_str().unwrap();
    let sha = record["checkpoint"]["sha"].as_str().unwrap();
    fs::write(project.join("kept.txt"), "edited during recovery")?;
    git(
        &project,
        &[
            "update-ref",
            reference,
            &git(&project, &["rev-parse", "HEAD"]),
        ],
    );
    assert!(
        backup_review::run(
            &journal,
            &data,
            target.clone(),
            Request::ConfirmApply { op_id: id }
        )
        .await
        .is_err()
    );
    git(&project, &["update-ref", reference, sha]);
    let complete =
        backup_review::run(&journal, &data, target, Request::ConfirmApply { op_id: id }).await?;
    assert_eq!(complete["historyPaused"], false);
    assert_eq!(
        fs::read_to_string(project.join("kept.txt"))?,
        "edited during recovery"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "one paid disposable Muse turn; pinned binary and saved authentication required"]
async fn real_backup_recovery_starts_one_new_session_without_importing_retired_history()
-> Result<()> {
    use intercat_daemon::backup_choices::Choice;
    let (root, journal, receipt, _, copy) = referenced_fixture(false).await?;
    let data = root.path().join("restored");
    let project = root.path().join("project");
    fs::write(
        project.join("AGENTS.md"),
        format!(
            "# Disposable recovery fixture\nUse read_file to inspect kept.txt and {}. Report CURRENT_BACKUP_FILES_OK. Do not modify files or run shell commands.\n",
            copy.strip_prefix(project.canonicalize()?)?.display()
        ),
    )?;
    let marker = "DO_NOT_IMPORT_LOST_CONTEXT_a91f";
    rusqlite::Connection::open(data.join("events.sqlite"))?.execute(
        "UPDATE workers SET record=json_set(record,'$.output',?1,'$.detail',?1)",
        [marker],
    )?;
    journal
        .set_worker_config(
            Uuid::now_v7(),
            intercat_daemon::runtime::Config {
                source: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
                binary: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
                credentials: std::env::var("INTERCAT_TEST_MUSE_AUTH")?.into(),
                path: std::env::var("PATH")?,
            },
        )
        .await?;
    let review = backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    backup_review::run(
        &journal,
        &data,
        receipt.clone(),
        session_choice(
            Choice::PickUp,
            Some(review["fileReview"]["opId"].as_str().unwrap().parse()?),
            false,
        ),
    )
    .await?;
    let id = Uuid::now_v7();
    let prepared = backup_review::run(
        &journal,
        &data,
        receipt,
        Request::Apply {
            op_id: id,
            acknowledge_unverified: false,
        },
    )
    .await?;
    let target: Receipt = serde_json::from_value(prepared["publication"]["target"].clone())?;
    backup_review::run(&journal, &data, target, Request::ConfirmApply { op_id: id }).await?;
    let source = root.path().join("editor/intercat_editor");
    fs::create_dir_all(&source)?;
    fs::write(source.join("__init__.py"), "")?;
    fs::write(
        source.join("server.py"),
        r#"import json
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def respond(self,value):
  body=json.dumps(value).encode();self.send_response(200);self.send_header('X-Intercat-Inference','started');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_GET(self):self.respond({'accepting':True,'modelLoaded':True})
 def do_POST(self):
  self.rfile.read(int(self.headers['Content-Length']));self.respond({'cachePrefix':False,'stages':{'decision':{'selected':' A'}},'inferenceMs':1})
server=ThreadingHTTPServer(('127.0.0.1',0),Handler);print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True);server.serve_forever()
"#,
    )?;
    let model = root.path().join("dummy-model");
    fs::create_dir(&model)?;
    let editor = intercat_daemon::editor::Manager::new(
        &root.path().join("editor"),
        &root.path().join("editor.log"),
    )?;
    editor
        .configure(intercat_daemon::editor::Config {
            python: "/usr/bin/python3".into(),
            model,
        })
        .await?;
    let rpc = data.join("rpc");
    fs::create_dir(&rpc)?;
    let runtime = intercat_daemon::runtime::Runtime::start(
        intercat_daemon::host_memory::Pool::new(),
        data.clone(),
        rpc,
        journal.clone(),
        editor.clone(),
        std::sync::Arc::new(intercat_daemon::scheduler::Capacity::new(1)),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    let result = tokio::time::timeout(std::time::Duration::from_secs(180), async {
        let mut answered = std::collections::HashSet::new();
        loop {
            let status = journal.snapshot().await?;
            let worker = &status["workers"][0];
            for card in worker["approvals"].as_array().into_iter().flatten() {
                let approval: intercat_daemon::execution::Approval = serde_json::from_value(card.clone())?;
                let identity=serde_json::to_string(&(approval.id.clone(),approval.requirement.clone()))?;
                if answered.insert(identity) {
                    // Only this disposable fixture's observed read-only commands may be approved.
                    let allowed = ["ls -la", "ls -la .intercat/attachments/", "cat kept.txt", "ls -la; ls -la .intercat/attachments/ 2>&1; echo ---; for f in .intercat/attachments/*; do echo \"== $f\"; cat \"$f\"; echo; done"];
                    let structured=serde_json::from_str::<Value>(&approval.subject).ok();
                    let command=structured.as_ref().and_then(|v|v["command"].as_str()).unwrap_or(&approval.subject);
                    if !approval.can_allow || !allowed.contains(&command) { return Err(format!("Unexpected fixture approval: {}",approval.subject).into()); }
                    runtime.service.answer(Uuid::now_v7(),"track",approval.id,approval.requirement,intercat_daemon::approval::Response::Allow).await?;
                }
            }
            if worker["output"]
                .as_str()
                .is_some_and(|s| s.contains("CURRENT_BACKUP_FILES_OK"))
                && worker["checkpointPending"] == false
                && worker["activeTurn"].is_null()
            {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(worker.clone());
            }
            if matches!(
                worker["phase"].as_str(),
                Some("failed" | "needs-recovery" | "lost")
            ) {
                return Err(format!("Recovery worker failed: {}", worker["detail"]).into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    runtime.shutdown().await;
    editor.stop().await;
    let output = std::path::PathBuf::from(std::env::var("INTERCAT_BACKUP_NATIVE_OUTPUT")?)
        .join(Uuid::now_v7().to_string());
    fs::create_dir_all(&output)?;
    let db = rusqlite::Connection::open(data.join("events.sqlite"))?;
    let turns: i64 = db.query_row(
        "SELECT count(*) FROM operations WHERE kind='turn.start'",
        [],
        |r| r.get(0),
    )?;
    fs::write(
        output.join("attempt.json"),
        serde_json::to_vec_pretty(&json!({"attemptedModelTurns":turns}))?,
    )?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    let worker = result??;
    let session = worker["sessionId"].as_str().unwrap();
    assert_ne!(session, "session");
    assert_eq!(turns, 1);
    let texts: Vec<String> = db
        .prepare("SELECT payload FROM events WHERE session_id=?1")?
        .query_map([session], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert!(!texts.join("\n").contains(marker));
    fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(
            &json!({"schema":intercat_daemon::journal::SCHEMA_VERSION,"worker":worker,"retiredHistoryExcluded":true,"paidTurns":turns,"applyId":id}),
        )?,
    )?;
    Ok(())
}

#[tokio::test]
async fn interrupted_snapshot_preparation_can_be_cancelled_without_retiring_history() -> Result<()>
{
    use intercat_daemon::backup_choices::Choice;
    let (root, journal, receipt, _, _) = referenced_fixture(false).await?;
    let data = root.path().join("restored");
    let project = root.path().join("project");
    let index = fs::read(project.join(".git/index"))?;
    fs::write(project.join(".git/index"), "CORRUPT DUMMY INDEX")?;
    let checked = backup_review::run(&journal, &data, receipt.clone(), compare_request()).await?;
    backup_review::run(
        &journal,
        &data,
        receipt.clone(),
        session_choice(
            Choice::PickUp,
            Some(checked["fileReview"]["opId"].as_str().unwrap().parse()?),
            false,
        ),
    )
    .await?;
    let id = Uuid::now_v7();
    let request = Request::Apply {
        op_id: id,
        acknowledge_unverified: false,
    };
    assert!(
        backup_review::run(&journal, &data, receipt.clone(), request.clone())
            .await
            .is_err()
    );
    assert_eq!(
        journal
            .backup_apply_recovery(Some(receipt.clone()))
            .await?
            .unwrap()["pendingApply"]["phase"],
        "capturing"
    );
    let db = rusqlite::Connection::open(data.join("events.sqlite"))?;
    let saved: String = db.query_row("SELECT record FROM backup_applies", [], |r| r.get(0))?;
    db.execute(
        "UPDATE backup_applies SET record=json_set(record,'$.phase','complete')",
        [],
    )?;
    assert!(
        backup_review::run(&journal, &data, receipt.clone(), Request::Inspect)
            .await
            .is_err()
    );
    db.execute("UPDATE backup_applies SET record=?1", [saved])?;
    let cancelled = backup_review::run(
        &journal,
        &data,
        receipt.clone(),
        Request::CancelApply { op_id: id },
    )
    .await?;
    assert_eq!(cancelled["apply"]["phase"], "cancelled");
    assert!(
        journal
            .backup_apply_recovery(Some(receipt.clone()))
            .await?
            .is_none()
    );
    assert!(journal.session_mask("track", "session").await.is_ok());
    assert_eq!(
        fs::read_to_string(project.join(".git/index"))?,
        "CORRUPT DUMMY INDEX"
    );
    assert_eq!(
        backup_review::run(
            &journal,
            &data,
            receipt.clone(),
            Request::CancelApply { op_id: id }
        )
        .await?["apply"]["phase"],
        "cancelled"
    );
    assert!(
        backup_review::run(&journal, &data, receipt.clone(), request)
            .await
            .is_err()
    );
    fs::write(project.join(".git/index"), index)?;
    let resumed = backup_review::run(
        &journal,
        &data,
        receipt,
        Request::Apply {
            op_id: Uuid::now_v7(),
            acknowledge_unverified: false,
        },
    )
    .await?;
    assert_eq!(resumed["apply"]["phase"], "awaiting-receipt");
    Ok(())
}
