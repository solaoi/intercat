use intercat_daemon::{
    Result, delivery::Inbox, history::Context, journal::Journal, msp::Connection,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

async fn put(writer: &mut tokio::io::DuplexStream, frame: Value) {
    writer
        .write_all(format!("{frame}\n").as_bytes())
        .await
        .unwrap();
}
fn started() -> Value {
    json!({"jsonrpc":"2.0","method":"item/started","params":{"sessionId":"s","item":{"itemId":"i","turnId":"t","kind":"agentMessage","revision":0,"text":"","status":"inProgress"}}})
}
fn delta(text: &str) -> Value {
    json!({"jsonrpc":"2.0","method":"item/delta","params":{"sessionId":"s","itemId":"i","delta":text}})
}
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn reader_exposes_live_text_before_consumption_but_persists_only_delta_metadata() -> Result<()>
{
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let (mut writer, read) = tokio::io::duplex(64 * 1024);
    let mut connection = Connection::new(read, tokio::io::sink());
    connection.client.configure_progress(
        "s".into(),
        intercat_daemon::progress::Privacy::KnownValues,
        Some("t".into()),
    );
    put(&mut writer, started()).await;
    put(&mut writer, delta("live before SQLite")).await;
    until(|| connection.client.progress().is_some()).await;
    assert_eq!(
        connection.client.progress().unwrap().text,
        "live before SQLite"
    );
    // No event has reached an actor or the journal yet.
    let mut inbox = Inbox::new(Context {
        journal: &journal,
        track: "track",
        session: "s",
        scanner: None,
    });
    let _ = inbox.next(&mut connection).await?;
    let event = inbox.next(&mut connection).await?;
    assert!(event.frame["params"].get("delta").is_none());
    assert_eq!(event.frame["params"]["deltaBytes"], 18);
    let db = rusqlite::Connection::open(root.path().join("events.sqlite"))?;
    let count: i64 = db.query_row(
        "SELECT count(*) FROM events WHERE payload LIKE '%live before SQLite%'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(count, 0);
    assert_eq!(
        connection.client.progress().unwrap().text,
        "live before SQLite"
    );
    Ok(())
}

#[tokio::test]
async fn repair_reader_releases_only_scanned_prefixes_and_still_resolves_replies() -> Result<()> {
    let (mut writer, read) = tokio::io::duplex(128 * 1024);
    let (write, requests) = tokio::io::duplex(4096);
    let mut requests = tokio::io::BufReader::new(requests);
    let mut connection = Connection::new(read, write);
    connection.client.configure_progress(
        "s".into(),
        intercat_daemon::progress::Privacy::Rules,
        Some("t".into()),
    );
    put(&mut writer, started()).await;
    let token = format!("ghp_{}", "A6k9P2mR".repeat(4) + "X4v7");
    put(&mut writer, delta(&format!("安全です。\n{}", &token[..20]))).await;
    until(|| connection.client.progress().is_some()).await;
    let preview = connection.client.progress().unwrap();
    assert!(preview.text.starts_with("安全です。"));
    assert!(!preview.text.contains("ghp"));
    assert!(!preview.text.contains("A6k9"));
    put(
        &mut writer,
        delta(&format!("{}\n確認しました。\n", &token[20..])),
    )
    .await;
    until(|| {
        connection
            .client
            .progress()
            .is_some_and(|p| p.text.contains("[redacted]"))
    })
    .await;
    // No actor has consumed any events. Raw deltas are absent from the queue.
    for _ in 0..3 {
        let Some(intercat_daemon::msp::Event::Frame(event)) = connection.next_event().await else {
            panic!("missing event");
        };
        assert!(event.frame["params"].get("delta").is_none());
    }
    put(&mut writer, delta(&"abc ".repeat(16_000))).await;
    let client = connection.client.clone();
    let reply = tokio::spawn(async move {
        client
            .request("session/status", json!({"sessionId":"s"}))
            .await
    });
    let mut line = String::new();
    requests.read_line(&mut line).await?;
    let request: Value = serde_json::from_str(&line)?;
    put(
        &mut writer,
        json!({"jsonrpc":"2.0","id":request["id"],"result":{"ok":true}}),
    )
    .await;
    let result = tokio::time::timeout(std::time::Duration::from_millis(250), reply).await???;
    assert_eq!(result["ok"], true);
    connection.client.clear_progress();
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert!(connection.client.progress().is_none());
    Ok(())
}

#[tokio::test]
async fn dropped_delta_discards_the_preview_instead_of_joining_nonadjacent_fragments() -> Result<()>
{
    let (mut writer, read) = tokio::io::duplex(64 * 1024);
    let mut connection = Connection::new(read, tokio::io::sink());
    connection.client.configure_progress(
        "s".into(),
        intercat_daemon::progress::Privacy::KnownValues,
        Some("t".into()),
    );
    put(&mut writer, started()).await;
    put(&mut writer, delta("old prefix")).await;
    until(|| connection.client.progress().is_some()).await;
    put(&mut writer, json!({"jsonrpc":"2.0","method":"item/updated","params":{"sessionId":"s","item":{"kind":"toolCall","text":"x".repeat(4_300_000)}}})).await;
    until(|| connection.client.output_flow().lagging).await;
    put(&mut writer, delta("missing middle")).await;
    until(|| connection.client.output_flow().dropped_deltas == 1).await;
    assert!(connection.client.progress().is_none());
    for _ in 0..3 {
        drop(connection.next_event().await);
    }
    put(&mut writer, delta("unrelated suffix")).await;
    until(|| connection.client.output_flow().queued_bytes > 0).await;
    assert!(connection.client.progress().is_none());
    put(&mut writer, started()).await;
    put(&mut writer, delta("fresh snapshot")).await;
    until(|| connection.client.progress().is_some()).await;
    assert_eq!(connection.client.progress().unwrap().text, "fresh snapshot");
    Ok(())
}
