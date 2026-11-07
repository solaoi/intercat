use intercat_daemon::{Result, journal::Journal, secrets::Key};
use serde_json::{Value, json};

async fn screen(journal: &Journal, text: &str) -> Result<String> {
    let result = journal.screen_start(json!({"input":[{"type":"text","text":text}],"sessionId":"01960000-0000-7000-8000-000000000001"})).await?;
    Ok(result.params()["input"][0]["text"]
        .as_str()
        .unwrap()
        .to_owned())
}

#[tokio::test]
async fn repeated_values_keep_numbers_across_requests_restart_and_local_lookup() -> Result<()> {
    let root = tempfile::tempdir()?;
    let db = root.path().join("db");
    let journal = Journal::open(&db, Key::new([7; 32])).await?;
    let first = screen(
        &journal,
        "Mail dummy.person@example.invalid, then dummy.person@example.invalid. 電話 090-1234-5678",
    )
    .await?;
    assert_eq!(
        first,
        "Mail <redacted:email#1>, then <redacted:email#1>. 電話 <redacted:phone#2>"
    );
    assert_eq!(
        journal.privacy_original(1).await?,
        "dummy.person@example.invalid"
    );
    assert_eq!(
        screen(&journal, "dummy.person@example.invalid").await?,
        "<redacted:email#1>"
    );
    drop(journal);
    let journal = Journal::open(&db, Key::new([7; 32])).await?;
    assert_eq!(
        screen(&journal, "Call 090-1234-5678; other@example.invalid").await?,
        "Call <redacted:phone#2>; <redacted:email#3>"
    );
    assert!(journal.privacy_original(0).await.is_err());
    assert!(journal.privacy_original(400).await.is_err());
    Ok(())
}

#[tokio::test]
async fn credential_redaction_keeps_surrounding_words_and_multiline_offsets() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    let text = format!("日本語の説明は保つ。\nUse {token} for this dummy request.\n再び {token}.");
    let safe = screen(&journal, &text).await?;
    assert_eq!(
        safe,
        "日本語の説明は保つ。\nUse <redacted:api-key#1> for this dummy request.\n再び <redacted:api-key#1>."
    );
    assert_eq!(journal.privacy_original(1).await?, token);
    let snapshot = journal.snapshot().await?;
    assert!(!serde_json::to_string(&snapshot)?.contains(token));
    Ok(())
}

#[tokio::test]
async fn editor_json_and_pasted_delivery_use_the_same_catalog() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let input = json!({"prefix":"Classify this.","tail":serde_json::to_string(&json!({"request":"first@example.invalid\nthen +81 90 1234 5678"}))?});
    let safe = journal
        .screen_editor(input, intercat_daemon::editor::Priority::Input)
        .await?;
    let tail: Value = serde_json::from_str(safe.params()["tail"].as_str().unwrap())?;
    assert_eq!(
        tail["request"],
        "<redacted:email#1>\nthen <redacted:phone#2>"
    );
    drop(safe);
    let source = root.path().join("dummy.txt");
    std::fs::write(&source, "First: first@example.invalid\nEnd")?;
    let id = uuid::Uuid::now_v7();
    let attachment = journal.import_pasted_text(id, source).await?;
    assert_eq!(
        journal.read_pasted_text(id).await?,
        "First: first@example.invalid\nEnd"
    );
    use sha2::{Digest, Sha256};
    let expected = "First: <redacted:email#1>\nEnd";
    assert_eq!(attachment.byte_len, expected.len() as u64);
    assert_eq!(
        attachment.sha256,
        Sha256::digest(expected.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );

    Ok(())
}

#[tokio::test]
async fn japanese_full_width_phone_and_us_phone_do_not_mask_dates_or_identifiers() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let safe = screen(&journal, "０９０ー１２３４ー５６７８ / (212) 555-0199 / 2026-11-07 / 01960000-0000-7000-8000-000000000001").await?;
    assert_eq!(
        safe,
        "<redacted:phone#1> / <redacted:phone#2> / 2026-11-07 / 01960000-0000-7000-8000-000000000001"
    );
    Ok(())
}

#[tokio::test]
async fn sensitive_metadata_is_rejected_instead_of_rewriting_control_fields() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let error = journal
        .screen_start(
            json!({"sessionId":"dummy@example.invalid","input":[{"text":"A harmless request."}]}),
        )
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("metadata"));
    Ok(())
}

#[tokio::test]
async fn simultaneous_requests_share_the_same_value_identity() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let journal = journal.clone();
        tasks.spawn(async move { screen(&journal, "shared@example.invalid").await });
    }
    while let Some(result) = tasks.join_next().await {
        assert_eq!(result??, "<redacted:email#1>");
    }
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    assert_eq!(
        db.query_row("SELECT count(*) FROM privacy_values", [], |row| row
            .get::<_, i64>(0))?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn registered_credentials_stay_fingerprint_only_and_do_not_enter_the_value_catalog()
-> Result<()> {
    use intercat_daemon::secrets::Registration;
    let root = tempfile::tempdir()?;
    let db = root.path().join("db");
    let journal = Journal::open(&db, Key::new([7; 32])).await?;
    rusqlite::Connection::open(&db)?.execute(
        "INSERT INTO msp_positions VALUES('session','track',NULL)",
        [],
    )?;
    let key = Key::new([7; 32]);
    let secret = "dummy-registered-private-value";
    let hex = |value: &[u8]| {
        value
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    journal
        .register_fingerprint(Registration {
            op_id: uuid::Uuid::now_v7(),
            track_id: "track".into(),
            session_id: "session".into(),
            name: "TOKEN".into(),
            byte_len: secret.len(),
            tag: hex(&key.tag(secret.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    assert_eq!(
        screen(
            &journal,
            &format!("Use {secret}. Contact person@example.invalid")
        )
        .await?,
        "Use <secret:TOKEN>. Contact <redacted:email#1>"
    );
    let connection = rusqlite::Connection::open(db)?;
    assert_eq!(
        connection.query_row(
            "SELECT count(*) FROM privacy_values WHERE value=?1",
            [secret],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn a_multiline_dummy_key_is_replaced_once_without_losing_the_next_line() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let key = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
        "N5mK7x2Q9vT6sL3aD8jH4cR1pW0zYgUe".repeat(4)
    );
    assert_eq!(
        screen(&journal, &format!("Before\n{key}\nAfter")).await?,
        "Before\n<redacted:api-key#1>\nAfter"
    );
    assert_eq!(journal.privacy_original(1).await?, key);
    Ok(())
}

#[tokio::test]
async fn a_value_registered_later_cannot_be_revealed_through_its_old_number() -> Result<()> {
    use intercat_daemon::secrets::Registration;
    let root = tempfile::tempdir()?;
    let db = root.path().join("db");
    let key = Key::new([7; 32]);
    let journal = Journal::open(&db, Key::new([7; 32])).await?;
    let value = "later@example.invalid";
    assert_eq!(screen(&journal, value).await?, "<redacted:email#1>");
    assert_eq!(journal.privacy_original(1).await?, value);
    rusqlite::Connection::open(&db)?.execute(
        "INSERT INTO msp_positions VALUES('session','track',NULL)",
        [],
    )?;
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    journal
        .register_fingerprint(Registration {
            op_id: uuid::Uuid::now_v7(),
            track_id: "track".into(),
            session_id: "session".into(),
            name: "TOKEN".into(),
            byte_len: value.len(),
            tag: hex(&key.tag(value.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    assert!(journal.privacy_original(1).await.is_err());
    assert_eq!(screen(&journal, value).await?, "<secret:TOKEN>");
    Ok(())
}

#[tokio::test]
async fn nul_bytes_cannot_turn_a_request_into_unscanned_binary_content() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert!(
        screen(&journal, "dummy\0private@example.invalid")
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn structured_previews_keep_literal_markers_separate_and_do_not_expose_values() -> Result<()>
{
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let views = journal
        .preview_privacy(vec![
            "Literal <redacted:email#999>; real dummy@example.invalid".into(),
            "🙂電話 090(1234)5678".into(),
        ])
        .await?;
    let serialized = serde_json::to_value(&views)?;
    assert!(!serialized.to_string().contains("dummy@example.invalid"));
    assert_eq!(
        serialized[0]["parts"][0],
        json!({"kind":"text","text":"Literal <redacted:email#999>; real "})
    );
    assert_eq!(
        serialized[0]["parts"][1],
        json!({"kind":"redaction","id":1,"category":"email"})
    );
    assert_eq!(views[1].text, "🙂電話 <redacted:phone#2>");
    assert_eq!(journal.privacy_original(1).await?, "dummy@example.invalid");
    assert!(
        journal
            .preview_privacy(vec!["small".into(); 65])
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn authenticated_rpc_previews_and_reveals_only_through_the_local_endpoint() -> Result<()> {
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
        stream.read_line(&mut line).await?;
        Ok(serde_json::from_str(&line)?)
    }
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    use std::os::unix::fs::PermissionsExt;
    let previews = root.path().join("privacy-previews");
    let input_directory = previews.join("view-123456");
    std::fs::create_dir_all(&input_directory)?;
    std::fs::set_permissions(&previews, std::fs::Permissions::from_mode(0o700))?;
    std::fs::set_permissions(&input_directory, std::fs::Permissions::from_mode(0o700))?;
    let input_file = input_directory.join("input.json");
    std::fs::write(
        &input_file,
        serde_json::to_vec(&vec!["RPC dummy@example.invalid"])?,
    )?;
    std::fs::set_permissions(&input_file, std::fs::Permissions::from_mode(0o600))?;
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
        let mut rejected =
            BufReader::new(tokio::net::UnixStream::connect(socket.path().join("rpc.sock")).await?);
        assert!(
            call(&mut rejected, "privacy.original", json!({"id":1})).await?["error"].is_object()
        );
        let mut stream =
            BufReader::new(tokio::net::UnixStream::connect(socket.path().join("rpc.sock")).await?);
        assert!(
            call(&mut stream, "rpc.authenticate", json!({"token":token})).await?["error"].is_null()
        );
        let preview = call(&mut stream, "privacy.preview", json!({"id":"view-123456"})).await?;
        assert!(preview["error"].is_null(), "{preview}");
        assert_eq!(
            preview["result"][0]["parts"][1],
            json!({"kind":"redaction","id":1,"category":"email"})
        );
        assert!(!preview.to_string().contains("dummy@example.invalid"));
        assert_eq!(
            call(&mut stream, "privacy.original", json!({"id":1})).await?["result"]["textBase64"],
            base64::Engine::encode(&base64::prelude::BASE64_STANDARD, "dummy@example.invalid")
        );
        assert!(
            call(&mut stream, "privacy.original", json!({"id":-1})).await?["error"].is_object()
        );
        assert!(
            call(
                &mut stream,
                "privacy.preview",
                json!({"id":"view-123456","extra":true})
            )
            .await?["error"]
                .is_object()
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

#[tokio::test]
async fn an_initial_session_name_keeps_the_complete_source_until_screening() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let text = format!("{} dummy@example.invalid", "Context ".repeat(5));
    journal
        .stage_utterance(
            uuid::Uuid::now_v7(),
            text.clone(),
            intercat_daemon::decision::Target::None,
            None,
            vec![],
        )
        .await?;
    assert_eq!(journal.snapshot().await?["session"]["name"], text);
    let views = journal.preview_privacy(vec![text]).await?;
    assert!(views[0].text.ends_with("<redacted:email#1>"));
    Ok(())
}
