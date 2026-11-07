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

#[tokio::test]
async fn file_and_image_limits_use_the_contents_and_accept_the_exact_boundary() -> Result<()> {
    use intercat_daemon::attachment_store::{FILE_LIMIT, IMAGE_LIMIT};
    use std::io::Write;
    let (root, journal) = fixture().await?;
    for (index, size, image, accepted) in [
        (0, IMAGE_LIMIT, true, true),
        (1, IMAGE_LIMIT + 1, true, false),
        (2, FILE_LIMIT, false, true),
        (3, FILE_LIMIT + 1, false, false),
    ] {
        let source = root.path().join(format!("reference-{index}.bin"));
        let mut file = fs::File::create(&source)?;
        if image {
            file.write_all(b"\x89PNG\r\n\x1a\n")?;
        }
        file.set_len(size as u64)?;
        drop(file);
        let result = journal.import_attachment(Uuid::now_v7(), source).await;
        if accepted {
            assert_eq!(result?.byte_len, size as u64);
        } else {
            let message = result.unwrap_err().to_string();
            assert!(message.contains(if image { "10 MiB" } else { "25 MiB" }));
        }
    }
    Ok(())
}

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
async fn register_paste_secret(root: &Path, journal: &Journal, value: &str) -> Result<()> {
    use intercat_daemon::secrets::Registration;
    let key = Key::new([7; 32]);
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    rusqlite::Connection::open(root.join("data/db"))?.execute(
        "INSERT OR IGNORE INTO msp_positions VALUES('paste-session','paste-track',NULL)",
        [],
    )?;
    journal
        .register_fingerprint(Registration {
            op_id: Uuid::now_v7(),
            track_id: "paste-track".into(),
            session_id: "paste-session".into(),
            name: "PASTE_KEY".into(),
            byte_len: value.len(),
            tag: hex(&key.tag(value.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    Ok(())
}

#[tokio::test]
async fn pasted_text_keeps_the_original_private_and_restores_only_the_full_redacted_copy()
-> Result<()> {
    let (root, journal) = fixture().await?;
    let secret = "private-paste-value-猫";
    register_paste_secret(root.path(), &journal, secret).await?;
    let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    let original = format!(
        "{}\n{secret}\ntoken={token}\nEND_OF_PASTED_DOCUMENT",
        "x".repeat(1_100_000)
    );
    let source = root.path().join("pasted.txt");
    fs::write(&source, &original)?;
    let id = Uuid::now_v7();
    let attachment = journal.import_pasted_text(id, source.clone()).await?;
    assert!(journal.import_attachment(id, source.clone()).await.is_err());
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
    let copy = &references.files[0].path;
    let safe = fs::read_to_string(copy)?;
    assert!(!safe.contains(secret));
    assert!(!safe.contains(token));
    assert!(safe.starts_with(&"x".repeat(100)));
    assert!(safe.ends_with("END_OF_PASTED_DOCUMENT"));
    assert!(safe.len() > 1_100_000);
    let private = fs::read(root.path().join("data/attachments").join(id.to_string()))?;
    assert!(
        private
            .windows(secret.len())
            .any(|bytes| bytes == secret.as_bytes())
    );
    fs::remove_file(copy)?;
    fs::remove_file(&source)?;
    drop(journal);
    let journal = Journal::open(root.path().join("data/db"), Key::new([7; 32])).await?;
    assert_eq!(journal.import_pasted_text(id, source).await?, attachment);
    journal
        .materialize_input_attachments(input, workspace.track_id, "session".into(), workspace.path)
        .await?;
    assert_eq!(fs::read_to_string(copy)?, safe);
    let db = rusqlite::Connection::open(root.path().join("data/db"))?;
    let payloads = db
        .prepare("SELECT payload FROM operations UNION ALL SELECT payload FROM events")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    assert!(
        payloads
            .iter()
            .all(|payload| !payload.contains(secret) && !payload.contains(token))
    );
    Ok(())
}

#[tokio::test]
async fn new_fingerprints_hold_a_pasted_reference_before_any_workspace_copy() -> Result<()> {
    let (root, journal) = fixture().await?;
    let secret = "only-known-after-paste";
    let source = root.path().join("paste.txt");
    fs::write(&source, format!("Notes\n{secret}\nEnd"))?;
    let id = Uuid::now_v7();
    let attachment = journal.import_pasted_text(id, source.clone()).await?;
    let (input, workspace) =
        input_workspace(root.path(), &journal, std::slice::from_ref(&attachment)).await?;
    register_paste_secret(root.path(), &journal, secret).await?;
    assert_eq!(journal.import_pasted_text(id, source).await?, attachment);
    let result = journal
        .materialize_input_attachments(
            input,
            workspace.track_id,
            "session".into(),
            workspace.path.clone(),
        )
        .await;
    assert!(result.unwrap_err().to_string().contains("needs review"));
    let copies = fs::read_dir(workspace.path.join(&workspace.attachment_roots[0]))?
        .collect::<std::io::Result<Vec<_>>>()?;
    assert!(copies.iter().all(|entry| entry.file_name() == ".gitignore"));
    Ok(())
}

#[tokio::test]
async fn refreshed_screening_keeps_the_original_and_rechecks_new_fingerprints() -> Result<()> {
    use intercat_daemon::attachment_screening::Request;
    let (root, journal) = fixture().await?;
    let text = "Beginning\nfirst-private-value\nsecond-private-value\nEnd";
    let source = root.path().join("paste.txt");
    fs::write(&source, text)?;
    let attachment = journal
        .import_pasted_text(Uuid::now_v7(), source.clone())
        .await?;
    fs::remove_file(source)?;
    let saved = root
        .path()
        .join("data/attachments")
        .join(attachment.id.to_string());
    let before = fs::read(&saved)?;
    let (input, workspace) =
        input_workspace(root.path(), &journal, std::slice::from_ref(&attachment)).await?;
    register_paste_secret(root.path(), &journal, "first-private-value").await?;
    let request = Request {
        op_id: Uuid::now_v7(),
        attachment_id: attachment.id,
    };
    let receipt = journal.refresh_pasted_text(request.clone()).await?;
    assert!(receipt.changed);
    assert_eq!(
        journal.refresh_pasted_text(request.clone()).await?.sha256,
        receipt.sha256
    );
    assert_eq!(fs::read(&saved)?, before);
    assert_eq!(journal.read_pasted_text(attachment.id).await?, text);
    let references = journal
        .materialize_input_attachments(
            input.clone(),
            workspace.track_id.clone(),
            "session".into(),
            workspace.path.clone(),
        )
        .await?;
    let copy = &references.files[0].path;
    assert_eq!(
        copy.file_name().unwrap(),
        format!("{}.txt", attachment.id).as_str()
    );
    assert_eq!(
        fs::read_to_string(copy)?,
        "Beginning\n<secret:PASTE_KEY>\nsecond-private-value\nEnd"
    );
    assert!(
        journal
            .refresh_pasted_text(Request {
                attachment_id: Uuid::now_v7(),
                ..request.clone()
            })
            .await
            .is_err()
    );
    register_paste_secret(root.path(), &journal, "second-private-value").await?;
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
    journal
        .refresh_pasted_text(Request {
            op_id: Uuid::now_v7(),
            attachment_id: attachment.id,
        })
        .await?;
    drop(journal);
    let journal = Journal::open(root.path().join("data/db"), Key::new([7; 32])).await?;
    journal
        .materialize_input_attachments(
            input,
            workspace.track_id.clone(),
            "session".into(),
            workspace.path.clone(),
        )
        .await?;
    assert_eq!(
        fs::read_to_string(copy)?,
        "Beginning\n<secret:PASTE_KEY>\n<secret:PASTE_KEY>\nEnd"
    );
    assert_eq!(fs::read(saved)?, before);
    let listed = journal.pasted_references(workspace.track_id, None).await?;
    assert_eq!(listed.items, vec![attachment]);
    assert!(listed.next.is_none());
    assert!(
        journal
            .pasted_references("unrelated-track".into(), None)
            .await?
            .items
            .is_empty()
    );
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn screening_save_retries_one_immutable_copy_after_a_full_database() -> Result<()> {
    use intercat_daemon::{attachment_screening::Request, fault_injection::Request as Fault};
    let (root, journal) = fixture().await?;
    let source = root.path().join("paste.txt");
    fs::write(&source, "later-private-value")?;
    let original = journal.import_pasted_text(Uuid::now_v7(), source).await?;
    register_paste_secret(root.path(), &journal, "later-private-value").await?;
    let request = Request {
        op_id: Uuid::now_v7(),
        attachment_id: original.id,
    };
    journal.test_fault(Fault::Fill).await?;
    assert!(journal.refresh_pasted_text(request.clone()).await.is_err());
    assert_eq!(
        fs::read_dir(root.path().join("data/attachments"))?.count(),
        2
    );
    journal.test_fault(Fault::Restore).await?;
    let saved = journal.refresh_pasted_text(request.clone()).await?;
    assert_eq!(
        journal.refresh_pasted_text(request.clone()).await?.sha256,
        saved.sha256
    );
    assert_eq!(
        fs::read_dir(root.path().join("data/attachments"))?.count(),
        2
    );
    let db = rusqlite::Connection::open(root.path().join("data/db"))?;
    assert_eq!(
        db.query_row("SELECT count(*) FROM attachment_screenings", [], |row| row
            .get::<_, i64>(
            0
        ))?,
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn reference_pages_advance_over_regular_files_without_exposing_other_tracks() -> Result<()> {
    let (root, journal) = fixture().await?;
    let source = root.path().join("regular.txt");
    fs::write(&source, "reference")?;
    let mut attachments = Vec::new();
    for _ in 0..51 {
        attachments.push(
            journal
                .import_attachment(Uuid::now_v7(), source.clone())
                .await?,
        );
    }
    let pasted = journal.import_pasted_text(Uuid::now_v7(), source).await?;
    attachments.push(pasted.clone());
    let (_, workspace) = input_workspace(root.path(), &journal, &attachments).await?;
    let first = journal
        .pasted_references(workspace.track_id.clone(), None)
        .await?;
    assert!(first.items.is_empty());
    assert!(first.next.is_some());
    let second = journal
        .pasted_references(workspace.track_id, first.next)
        .await?;
    assert_eq!(second.items, vec![pasted]);
    assert!(second.next.is_none());
    assert!(
        journal
            .pasted_references("other".into(), None)
            .await?
            .items
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn pasted_text_validates_the_byte_limit_encoding_and_private_original() -> Result<()> {
    use intercat_daemon::attachment_store::TEXT_LIMIT;
    let (root, journal) = fixture().await?;
    let source = root.path().join("paste.txt");
    let exact = "字".repeat(TEXT_LIMIT / 3) + &"a".repeat(TEXT_LIMIT % 3);
    assert_eq!(exact.len(), TEXT_LIMIT);
    fs::write(&source, &exact)?;
    let attachment = journal
        .import_pasted_text(Uuid::now_v7(), source.clone())
        .await?;
    assert_eq!(attachment.byte_len, TEXT_LIMIT as u64);
    fs::write(&source, exact + "a")?;
    assert!(
        journal
            .import_pasted_text(Uuid::now_v7(), source.clone())
            .await
            .is_err()
    );
    fs::write(&source, [0xff, 0xfe])?;
    assert!(
        journal
            .import_pasted_text(Uuid::now_v7(), source.clone())
            .await
            .is_err()
    );
    let link = root.path().join("linked.txt");
    std::os::unix::fs::symlink(&source, &link)?;
    assert!(
        journal
            .import_pasted_text(Uuid::now_v7(), link)
            .await
            .is_err()
    );
    let file = journal
        .import_attachment(Uuid::now_v7(), source.clone())
        .await?;
    assert!(
        journal
            .import_pasted_text(file.id, source.clone())
            .await
            .is_err()
    );
    let private = root
        .path()
        .join("data/attachments")
        .join(attachment.id.to_string());
    let mut bytes = fs::read(&private)?;
    let header = u32::from_be_bytes(bytes[8..12].try_into()?);
    bytes[12 + header as usize] ^= 1;
    fs::set_permissions(&private, fs::Permissions::from_mode(0o600))?;
    fs::write(&private, bytes)?;
    fs::set_permissions(&private, fs::Permissions::from_mode(0o400))?;
    assert!(
        journal
            .import_pasted_text(attachment.id, source)
            .await
            .is_err()
    );
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn pasted_reference_save_retries_the_private_original_after_a_full_database() -> Result<()> {
    use intercat_daemon::fault_injection::Request as Fault;
    let (root, journal) = fixture().await?;
    let source = root.path().join("paste.txt");
    fs::write(
        &source,
        "A pasted reference to keep after a failed database save.",
    )?;
    let id = Uuid::now_v7();
    journal.test_fault(Fault::Fill).await?;
    assert!(
        journal
            .import_pasted_text(id, source.clone())
            .await
            .is_err()
    );
    assert!(
        root.path()
            .join("data/attachments")
            .join(id.to_string())
            .is_file()
    );
    fs::remove_file(&source)?;
    journal.test_fault(Fault::Restore).await?;
    let saved = journal.import_pasted_text(id, source).await?;
    assert_eq!(saved.id, id);
    let db = rusqlite::Connection::open(root.path().join("data/db"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM attachments WHERE id=?1",
            [id.to_string()],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn pasted_reference_import_uses_the_authenticated_rpc_and_replays_its_id() -> Result<()> {
    use intercat_daemon::{
        editor::Manager,
        lock::DaemonLock,
        rpc::{Server, Token},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (root, journal) = fixture().await?;
    let source = root.path().join("paste.txt");
    fs::write(&source, "A local pasted reference.")?;
    let socket = tempfile::tempdir_in("/private/tmp")?;
    let _lock = DaemonLock::acquire(socket.path())?.unwrap();
    let token = "a7".repeat(32);
    let server = Server::bind(
        socket.path(),
        Token::parse(&token)?,
        journal,
        Manager::new(socket.path(), &socket.path().join("editor.log"))?,
        &socket.path().join("workspaces"),
    )?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let mut stream =
        BufReader::new(tokio::net::UnixStream::connect(socket.path().join("rpc.sock")).await?);
    let id = Uuid::now_v7();
    let mut saved = None;
    let screening = Uuid::now_v7();
    for (method, params) in [
        ("rpc.authenticate", serde_json::json!({"token":token})),
        (
            "attachment.paste",
            serde_json::json!({"opId":id,"path":source}),
        ),
        (
            "attachment.paste.refresh",
            serde_json::json!({"opId":screening,"attachmentId":id}),
        ),
        (
            "attachment.paste.refresh",
            serde_json::json!({"opId":screening,"attachmentId":id}),
        ),
        (
            "attachment.paste.list",
            serde_json::json!({"trackId":"unrelated","after":null}),
        ),
        (
            "attachment.paste",
            serde_json::json!({"opId":id,"path":source}),
        ),
    ] {
        stream
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await?;
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(45),
            stream.read_line(&mut line),
        )
        .await??;
        let response: serde_json::Value = serde_json::from_str(&line)?;
        assert!(response.get("error").is_none(), "{response}");
        if method == "attachment.paste" {
            assert_eq!(response["result"]["id"], id.to_string());
            if let Some(previous) = saved {
                assert_eq!(response["result"], previous);
            }
            saved = Some(response["result"].clone());
        }
        if method == "attachment.paste.refresh" {
            assert_eq!(response["result"]["attachmentId"], id.to_string());
            assert_eq!(response["result"]["changed"], false);
        }
        if method == "attachment.paste.list" {
            assert_eq!(response["result"]["items"], serde_json::json!([]));
            assert!(response["result"]["next"].is_null());
        }
    }
    stop.send(()).unwrap();
    task.await??;
    Ok(())
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
    verify_branch_reference(false, false).await
}
#[tokio::test]
async fn branch_restores_only_the_redacted_pasted_reference() -> Result<()> {
    verify_branch_reference(true, false).await
}
#[tokio::test]
async fn branch_restores_a_refreshed_copy_at_the_original_reference_path() -> Result<()> {
    verify_branch_reference(true, true).await
}
async fn verify_branch_reference(pasted: bool, refresh: bool) -> Result<()> {
    use intercat_daemon::branch::{Change, Request};
    let (root, journal) = fixture().await?;
    let source = root.path().join("early.txt");
    let mut expected = if pasted {
        "EARLY ORIGINAL\n<secret:PASTE_KEY>"
    } else {
        "EARLY ORIGINAL"
    };
    let early = if pasted {
        register_paste_secret(root.path(), &journal, "private-branch-reference").await?;
        fs::write(&source, "EARLY ORIGINAL\nprivate-branch-reference")?;
        journal
            .import_pasted_text(Uuid::now_v7(), source.clone())
            .await?
    } else {
        fs::write(&source, "EARLY ORIGINAL")?;
        journal
            .import_attachment(Uuid::now_v7(), source.clone())
            .await?
    };
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
    if refresh {
        register_paste_secret(root.path(), &journal, "EARLY ORIGINAL").await?;
        journal
            .refresh_pasted_text(intercat_daemon::attachment_screening::Request {
                op_id: Uuid::now_v7(),
                attachment_id: early.id,
            })
            .await?;
        expected = "<secret:PASTE_KEY>\n<secret:PASTE_KEY>";
    }
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
    assert_eq!(fs::read_to_string(&copy)?, expected);
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
    assert_eq!(fs::read_to_string(copy)?, expected);
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
    verify_pickup_references(false).await
}
#[tokio::test]
async fn pickup_recreates_only_the_redacted_pasted_references() -> Result<()> {
    verify_pickup_references(true).await
}
async fn recovery_reference(journal: &Journal, source: &Path, pasted: bool) -> Result<Attachment> {
    if pasted {
        journal
            .import_pasted_text(Uuid::now_v7(), source.to_path_buf())
            .await
    } else {
        journal
            .import_attachment(Uuid::now_v7(), source.to_path_buf())
            .await
    }
}
async fn verify_pickup_references(pasted: bool) -> Result<()> {
    use intercat_daemon::{
        checkpoint::Checkpoint,
        handoff::{Document, Fields},
        pickup::{Change, Request},
        runtime::WorkerState,
    };
    let (root, journal) = fixture().await?;
    if pasted {
        register_paste_secret(root.path(), &journal, "private-recovery-reference").await?;
    }
    let source_text = |text: &str| {
        if pasted {
            format!("{text}\nprivate-recovery-reference")
        } else {
            text.to_owned()
        }
    };
    let expected_early = if pasted {
        "EARLY\n<secret:PASTE_KEY>"
    } else {
        "EARLY"
    };
    let expected_uncertain = if pasted {
        "UNKNOWN DISPATCH\n<secret:PASTE_KEY>"
    } else {
        "UNKNOWN DISPATCH"
    };
    let source = root.path().join("early.txt");
    fs::write(&source, source_text("EARLY"))?;
    let early = recovery_reference(&journal, &source, pasted).await?;
    let (input, workspace) =
        input_workspace(root.path(), &journal, std::slice::from_ref(&early)).await?;
    let first = dispatch_reference(&journal, &workspace, &input, "old-session", true).await?;
    let checkpoint =
        intercat_daemon::checkpoint::save(workspace.clone(), first, None, false, journal.clone())
            .await?;
    fs::write(&source, source_text("UNKNOWN DISPATCH"))?;
    let uncertain = recovery_reference(&journal, &source, pasted).await?;
    let uncertain_input = additional_input(&journal, &workspace.track_id, uncertain.id).await?;
    let unknown_turn =
        dispatch_reference(&journal, &workspace, &uncertain_input, "old-session", false).await?;
    fs::write(&source, source_text("FUTURE UNSENT"))?;
    let future = recovery_reference(&journal, &source, pasted).await?;
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
        expected_early
    );
    assert_eq!(
        fs::read_to_string(directory.join(format!("{}.txt", uncertain.id)))?,
        expected_uncertain
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
        expected_early
    );
    Ok(())
}
