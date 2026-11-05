use intercat_daemon::{
    Result,
    journal::Journal,
    msp::{Connection, Event},
    secrets::{Key, Registration},
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn registration(name: &str, value: &str) -> Registration {
    let key = Key::new([7; 32]);
    Registration {
        op_id: Uuid::now_v7(),
        track_id: "track".into(),
        session_id: "session".into(),
        name: name.into(),
        byte_len: value.len(),
        tag: hex(&key.tag(value.as_bytes())),
        key_check: hex(&key.check()),
    }
}

#[test]
fn node_generated_hmac_vector_is_accepted_by_the_rust_mask() -> Result<()> {
    let key = Key::new([7; 32]);
    let request: Registration = serde_json::from_value(json!({
        "opId":"01a1a200-0000-7000-8000-000000000001","trackId":"track","sessionId":"session",
        "name":"NPM_TOKEN","byteLen":24,
        "tag":"5d5a3ea70c546ff4b7cf093accbd9eab884ffdcca3e2f8a8e916c455ed14efe2",
        "keyCheck":"34aba8963075a6c5ab6d9cdcabb229d3d2f85b2b0b22524de16894c71646e067"
    }))?;
    assert_eq!(
        key.mask(vec![request.fingerprint(&key)?])?
            .redact("fixture-secret-猫-value"),
        "<secret:NPM_TOKEN>"
    );
    Ok(())
}
async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let db = root.path().join("events.sqlite");
    let journal = Journal::open(&db, Key::new([7; 32])).await?;
    rusqlite::Connection::open(db)?.execute(
        "INSERT INTO msp_positions VALUES('session','track',NULL)",
        [],
    )?;
    Ok((root, journal))
}

#[tokio::test]
async fn saved_fingerprints_survive_reopen_without_values_keys_or_events() -> Result<()> {
    let (root, journal) = fixture().await?;
    let secret = "fixture-secret-猫-value";
    let first = registration("NPM_TOKEN", secret);
    let id = first.op_id;
    journal.register_fingerprint(first).await?;
    let mut retry = registration("NPM_TOKEN", secret);
    retry.op_id = id;
    journal.register_fingerprint(retry).await?;
    assert_eq!(
        journal
            .session_mask("track", "session")
            .await?
            .redact(secret),
        "<secret:NPM_TOKEN>"
    );
    assert!(journal.session_mask("other", "session").await.is_err());
    let db = rusqlite::Connection::open(root.path().join("events.sqlite"))?;
    assert_eq!(
        db.query_row("SELECT count(*) FROM secret_fingerprints", [], |r| r
            .get::<_, i64>(0))?,
        1
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))?,
        0
    );
    let rows = db
        .prepare("SELECT name,hex(tag) FROM secret_fingerprints")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(rows.len(), 1);
    drop(db);
    drop(journal);
    let reopened = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    assert_eq!(
        reopened
            .session_mask("track", "session")
            .await?
            .redact(secret),
        "<secret:NPM_TOKEN>"
    );
    for entry in std::fs::read_dir(root.path())? {
        let path = entry?.path();
        if path.is_file() {
            let bytes = std::fs::read(path)?;
            assert!(
                !bytes
                    .windows(secret.len())
                    .any(|window| window == secret.as_bytes())
            );
            assert!(
                !bytes
                    .windows(64)
                    .any(|window| window == hex(&[7; 32]).as_bytes())
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn changed_or_missing_key_verification_never_opens_as_an_empty_mask() -> Result<()> {
    let (root, journal) = fixture().await?;
    journal
        .register_fingerprint(registration("TOKEN", "never reveal this value"))
        .await?;
    drop(journal);
    let db = root.path().join("events.sqlite");
    assert!(Journal::open(&db, Key::new([8; 32])).await.is_err());
    let restored = Journal::open(&db, Key::new([7; 32])).await?;
    assert!(
        restored
            .session_mask("track", "session")
            .await?
            .redact("never reveal this value")
            .starts_with("<secret:")
    );
    drop(restored);
    rusqlite::Connection::open(&db)?.execute("DELETE FROM secret_key", [])?;
    assert!(Journal::open(db, Key::new([7; 32])).await.is_err());
    Ok(())
}

#[tokio::test]
async fn registration_checks_owner_operation_key_and_reserved_environment_names() -> Result<()> {
    let (_root, journal) = fixture().await?;
    for name in [
        "PATH",
        "HOME",
        "GIT_DIR",
        "INTERCAT_TOKEN",
        "MUSE_MODEL",
        "DYLD_INSERT_LIBRARIES",
        "XDG_CONFIG_HOME",
        "LD_PRELOAD",
        "SSH_AUTH_SOCK",
        "NODE_OPTIONS",
        "lowercase",
        "BAD-NAME",
    ] {
        assert!(
            journal
                .register_fingerprint(registration(name, "value"))
                .await
                .is_err(),
            "{name}"
        );
    }
    let mut wrong = registration("TOKEN", "value");
    wrong.track_id = "other".into();
    assert!(journal.register_fingerprint(wrong).await.is_err());
    let mut wrong = registration("TOKEN", "value");
    wrong.key_check = hex(&[0; 32]);
    assert!(journal.register_fingerprint(wrong).await.is_err());
    let first = registration("TOKEN", "first-value");
    let id = first.op_id;
    journal.register_fingerprint(first).await?;
    let mut reused = registration("TOKEN", "second-value");
    reused.op_id = id;
    assert!(journal.register_fingerprint(reused).await.is_err());
    assert_eq!(
        journal
            .session_mask("track", "session")
            .await?
            .redact("first-value second-value"),
        "<secret:TOKEN> second-value"
    );
    Ok(())
}

#[tokio::test]
async fn incoming_replies_live_items_and_delta_previews_are_masked_before_delivery() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let secret = "synthetic-long-secret";
    journal
        .register_fingerprint(registration("TOKEN", secret))
        .await?;
    let (host, io) = tokio::io::duplex(8192);
    let mut writer = BufReader::new(host);
    let (read, write) = tokio::io::split(io);
    let mut connection = Connection::new(read, write);
    connection
        .client
        .set_redaction(journal.session_mask("track", "session").await?);
    connection.client.configure_progress(
        "session".into(),
        intercat_daemon::progress::Privacy::KnownValues,
        Some("turn".into()),
    );
    let client = connection.client.clone();
    let pending = tokio::spawn(async move {
        client
            .request("view/page", json!({"sessionId":"session"}))
            .await
    });
    let mut request = String::new();
    writer.read_line(&mut request).await?;
    let request: serde_json::Value = serde_json::from_str(&request)?;
    let frames = [
        json!({"jsonrpc":"2.0","method":"item/started","params":{"sessionId":"session","item":{"itemId":"item","turnId":"turn","kind":"agentMessage","text":"","revision":0,"status":"inProgress"}}}),
        json!({"jsonrpc":"2.0","method":"item/delta","params":{"sessionId":"session","itemId":"item","delta":"before synthetic-"}}),
        json!({"jsonrpc":"2.0","method":"item/delta","params":{"sessionId":"session","itemId":"item","delta":"long-secret after enough safe padding to release"}}),
        json!({"jsonrpc":"2.0","id":request["id"],"result":{"events":[{"text":secret}],"output":secret}}),
    ];
    for frame in &frames {
        writer
            .get_mut()
            .write_all(format!("{frame}\n").as_bytes())
            .await?;
    }
    for _ in &frames {
        let Some(Event::Frame(received)) = connection.next_event().await else {
            panic!("missing frame")
        };
        let encoded = received.frame.to_string();
        assert!(!encoded.contains(secret));
        assert!(!encoded.contains("before synthetic-"));
        if received.frame["id"] == request["id"] {
            assert_eq!(
                received.frame["result"]["events"][0]["text"],
                "<secret:TOKEN>"
            );
        }
    }
    assert_eq!(pending.await??["events"][0]["text"], "<secret:TOKEN>");
    let preview = connection.client.progress().unwrap();
    assert!(preview.text.contains("<secret:TOKEN>"));
    assert!(!preview.text.contains(secret));
    Ok(())
}

#[test]
fn masking_bounds_expansion_and_hides_object_keys_without_collisions() -> Result<()> {
    let key = Key::new([7; 32]);
    let fingerprint = |name: &str, value: &str| intercat_daemon::progress::Fingerprint {
        name: name.into(),
        byte_len: value.len(),
        tag: key.tag(value.as_bytes()),
    };
    let mask = key.mask(vec![fingerprint("TOKEN", "x")])?;
    let mut large = json!({"body":{"body":"x|".repeat(200_000)}});
    mask.redact_value(&mut large)?;
    assert!(large["body"]["body"].as_str().unwrap().len() <= 1024 * 1024);
    assert!(!large["body"]["body"].as_str().unwrap().contains('x'));
    assert_eq!(large["_intercatTruncated"][0], "/body/body");
    let mask = key.mask(vec![
        fingerprint("TOKEN", "first-secret"),
        fingerprint("TOKEN", "second-secret"),
    ])?;
    let mut value = json!({"first-secret":"public"});
    mask.redact_value(&mut value)?;
    assert_eq!(value, json!({"<secret:TOKEN>":"public"}));
    let mut collision = json!({"first-secret":1,"second-secret":2});
    assert!(mask.redact_value(&mut collision).is_err());
    Ok(())
}
