use intercat_daemon::{
    Result,
    journal::Journal,
    msp::{Connection, StartGuard},
    secrets::{Key, Registration},
};
use serde_json::{Value, json};
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    rusqlite::Connection::open(path)?
        .execute("INSERT INTO msp_positions VALUES('s','t',NULL)", [])?;
    Ok((root, journal))
}
async fn register(journal: &Journal, text: &str) -> Result<()> {
    let key = Key::new([7; 32]);
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let catalog = journal
        .register_fingerprint(Registration {
            op_id: Uuid::now_v7(),
            track_id: "t".into(),
            session_id: "s".into(),
            name: "TOKEN".into(),
            byte_len: text.len(),
            tag: hex(&key.tag(text.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await
}
fn params(text: &str) -> Value {
    json!({"commandId":Uuid::now_v7(),"sessionId":"s","input":[{"type":"text","text":text}]})
}
fn running() -> StartGuard {
    StartGuard::Stopped(Arc::new(AtomicBool::new(false)))
}

#[tokio::test]
async fn complete_start_payload_is_screened_and_sensitive_control_metadata_is_rejected()
-> Result<()> {
    let (_root, journal) = fixture().await?;
    let known = "known-start-secret";
    register(&journal, known).await?;
    let provider = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    let start = journal
        .screen_start(params(&format!(
            "BEGIN\n{known}\ntoken={provider}\nDo not change files."
        )))
        .await?;
    let text = start.params()["input"][0]["text"].as_str().unwrap();
    assert!(!text.contains(known));
    assert!(!text.contains(provider));
    assert!(text.starts_with("BEGIN\n"));
    assert!(text.ends_with("Do not change files."));
    drop(start);
    let mut metadata = params("Explain the reference");
    metadata["workspaceRoots"] = json!([format!("/tmp/{known}")]);
    assert!(journal.screen_start(metadata).await.is_err());
    Ok(())
}

#[tokio::test]
async fn fingerprint_registration_waits_for_the_frame_but_not_for_its_acknowledgement() -> Result<()>
{
    let (_root, journal) = fixture().await?;
    let expected = params(&"request ".repeat(50));
    let prepared = journal.screen_start(expected.clone()).await?;
    let (read, mut host_write) = tokio::io::duplex(4096);
    let (write, mut host_read) = tokio::io::duplex(1);
    let mut connection = Connection::new(read, write);
    let client = connection.client.clone();
    let started = tokio::spawn(async move { client.start(prepared, running()).await });
    let mut first = [0];
    host_read.read_exact(&mut first).await?;
    let writer = journal.clone();
    let mut registration =
        tokio::spawn(async move { register(&writer, "registered-after-write").await });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut registration)
            .await
            .is_err()
    );
    let mut host_read = BufReader::new(host_read);
    let mut tail = String::new();
    host_read.read_line(&mut tail).await?;
    let frame: Value = serde_json::from_str(&(String::from_utf8(first.to_vec())? + &tail))?;
    assert_eq!(frame["params"], expected);
    tokio::time::timeout(Duration::from_secs(2), registration).await???;
    assert!(!started.is_finished());
    host_write
        .write_all(
            format!(
                "{}\n",
                json!({"jsonrpc":"2.0","id":frame["id"],"result":{}})
            )
            .as_bytes(),
        )
        .await?;
    started.await??;
    connection.close();
    Ok(())
}

#[tokio::test]
async fn cancelling_a_queued_start_releases_screening_without_waiting_for_the_pipe() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let (read, _host_write) = tokio::io::duplex(4096);
    let (write, mut host_read) = tokio::io::duplex(1);
    let mut connection = Connection::new(read, write);
    connection
        .client
        .notify("fixture/block", json!({"text":"x".repeat(100)}))
        .await?;
    let mut first = [0];
    host_read.read_exact(&mut first).await?;
    let prepared = journal.screen_start(params("queued instruction")).await?;
    let client = connection.client.clone();
    let started = tokio::spawn(async move { client.start(prepared, running()).await });
    let writer = journal.clone();
    let mut registration =
        tokio::spawn(async move { register(&writer, "after-cancelled-start").await });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut registration)
            .await
            .is_err()
    );
    started.abort();
    let _ = started.await;
    tokio::time::timeout(Duration::from_secs(2), registration).await???;
    connection.client.notify("fixture/end", json!({})).await?;
    let mut reader = BufReader::new(host_read);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    line.clear();
    reader.read_line(&mut line).await?;
    assert_eq!(
        serde_json::from_str::<Value>(&line)?["method"],
        "fixture/end"
    );
    connection.close();
    Ok(())
}

#[tokio::test]
async fn raw_turn_start_is_rejected_by_every_public_generic_send_api() -> Result<()> {
    let (read, _host_write) = tokio::io::duplex(4096);
    let (write, mut host_read) = tokio::io::duplex(4096);
    let mut connection = Connection::new(read, write);
    assert!(
        connection
            .client
            .request("turn/start", params("raw"))
            .await
            .is_err()
    );
    assert!(
        connection
            .client
            .notify("turn/start", params("raw"))
            .await
            .is_err()
    );
    assert!(
        connection
            .client
            .control_now("turn/start", params("raw"))
            .is_err()
    );
    assert!(
        connection
            .client
            .notify_now("turn/start", params("raw"))
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), host_read.read_u8())
            .await
            .is_err()
    );
    connection.close();
    Ok(())
}
