use intercat_daemon::{
    Result,
    clock::Stamp,
    history::{self, Context},
    journal::Journal,
    msp::{Connection, Event},
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

struct Peer {
    input: BufReader<DuplexStream>,
    output: DuplexStream,
}
impl Peer {
    async fn request(&mut self) -> Value {
        let mut line = String::new();
        self.input.read_line(&mut line).await.unwrap();
        serde_json::from_str(&line).unwrap()
    }
    async fn send(&mut self, value: Value) {
        self.output
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
    }
    async fn page(&mut self, request: &Value, events: Vec<Value>, next: Value) {
        self.send(json!({"jsonrpc":"2.0","id":request["id"],"result":{"events":events,"nextCursor":next}})).await;
    }
}
fn pair() -> (Connection, Peer) {
    let (incoming, output) = tokio::io::duplex(64 * 1024);
    let (outgoing, input) = tokio::io::duplex(64 * 1024);
    (
        Connection::new(incoming, outgoing),
        Peer {
            input: BufReader::new(input),
            output,
        },
    )
}
fn item(cursor: &str, id: &str) -> Value {
    json!({"jsonrpc":"2.0","method":"item/completed","params":{"sessionId":"session","viewCursor":cursor,"sourceRange":{},"item":{"itemId":id,"turnId":"turn","revision":1,"kind":"agentMessage","status":"completed","text":id}}})
}
async fn put(journal: &Journal, frame: Value) -> Result<()> {
    journal
        .try_msp(
            "track".into(),
            "session".into(),
            Stamp::now(),
            frame,
            None,
            None,
        )?
        .await??;
    Ok(())
}
fn context(journal: &Journal) -> Context<'_> {
    Context {
        journal,
        track: "track",
        session: "session",
        scanner: None,
    }
}

#[tokio::test]
async fn cursor_and_projection_commit_together_and_raw_duplicates_remain_auditable() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    put(&journal, item("opaque:z", "one")).await?;
    let mut duplicate = item("opaque:z", "one");
    duplicate["params"]["item"]["revision"] = json!(2);
    duplicate["params"]["item"]["text"] = json!("must not overwrite");
    put(&journal, duplicate).await?;
    let db = rusqlite::Connection::open(&path)?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='msp.received'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        2
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='msp.applied'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT json_extract(record,'$.text') FROM timeline_items",
            [],
            |r| r.get::<_, String>(0)
        )?,
        "one"
    );
    let bad = json!({"method":"turn/started","params":{"sessionId":"session","viewCursor":"opaque:a","turnId":"turn"}});
    assert!(
        journal
            .try_msp(
                "other".into(),
                "session".into(),
                Stamp::now(),
                bad,
                None,
                None
            )?
            .await?
            .is_err()
    );
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("opaque:z")
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='msp.received'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        3,
        "a failed fold still has its raw receipt"
    );
    drop(journal);
    let journal = Journal::open(path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("opaque:z")
    );
    Ok(())
}

#[tokio::test]
async fn multiple_pages_precede_live_events_without_reordering_the_raw_log() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    put(&journal, item("start", "initial")).await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let first = peer.request().await;
        assert_eq!(first["params"]["cursor"], "start");
        assert_eq!(first["params"]["maxBytes"], 512 * 1024);
        peer.send(item("future", "live")).await;
        peer.send(item("opaque:a", "two")).await;
        peer.page(
            &first,
            vec![item("opaque:z", "one"), item("opaque:a", "two")],
            json!("opaque:a"),
        )
        .await;
        let second = peer.request().await;
        assert_eq!(second["params"]["cursor"], "opaque:a");
        peer.page(&second, vec![item("opaque:m", "three")], Value::Null)
            .await;
    });
    history::recover(&mut connection, context(&journal), "ephemeral-head", None).await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("opaque:m")
    );
    let Some(Event::Frame(live)) = connection.next_event().await else {
        panic!("Missing held event")
    };
    assert_eq!(live.frame["params"]["item"]["itemId"], "live");
    assert!(live.source.is_some());
    journal
        .try_msp(
            "track".into(),
            "session".into(),
            live.stamp,
            live.frame,
            Some(live.lease),
            live.source,
        )?
        .await??;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("future")
    );
    let db = rusqlite::Connection::open(path)?;
    let mut query=db.prepare("SELECT coalesce(json_extract(payload,'$.params.viewCursor'),'response') FROM events WHERE kind='msp.received' ORDER BY seq")?;
    let raw = query
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    assert_eq!(raw, ["start", "future", "opaque:a", "response", "response"]);
    let mut query=db.prepare("SELECT json_extract(payload,'$.params.viewCursor') FROM events WHERE kind='msp.applied' ORDER BY seq")?;
    let applied = query
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    assert_eq!(
        applied,
        ["start", "opaque:z", "opaque:a", "opaque:m", "future"]
    );
    assert_eq!(
        journal
            .timeline_window(intercat_daemon::timeline::Window {
                tracks: vec!["track".into()],
                start: 0,
                end: i64::MAX,
                since: None
            })
            .await?["clips"][0]["approximate"],
        true
    );
    producer.await?;
    Ok(())
}

#[tokio::test]
async fn interrupted_paging_restarts_at_the_last_committed_page() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    put(&journal, item("start", "initial")).await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let first = peer.request().await;
        peer.page(&first, vec![item("first", "one")], json!("first"))
            .await;
        let second = peer.request().await;
        peer.page(&second, vec![], json!("first")).await;
    });
    let error = history::recover(&mut connection, context(&journal), "last", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("did not advance"));
    producer.await?;
    drop(connection);
    drop(journal);
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        assert_eq!(request["params"]["cursor"], "first");
        peer.page(&request, vec![item("last", "two")], Value::Null)
            .await;
    });
    history::recover(&mut connection, context(&journal), "last", None).await?;
    producer.await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("last")
    );
    let db = rusqlite::Connection::open(path)?;
    assert_eq!(
        db.query_row("SELECT count(*) FROM timeline_items", [], |r| r
            .get::<_, i64>(0))?,
        3
    );
    Ok(())
}

#[tokio::test]
async fn a_foreign_or_unpositioned_history_event_never_advances_the_cursor() -> Result<()> {
    for malformed in [json!("foreign"), Value::Null] {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(
            root.path().join("db"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        put(&journal, item("start", "initial")).await?;
        let (mut connection, mut peer) = pair();
        let producer = tokio::spawn(async move {
            let request = peer.request().await;
            let mut event = item("bad", "bad");
            if malformed.is_null() {
                event["params"]
                    .as_object_mut()
                    .unwrap()
                    .remove("viewCursor");
            } else {
                event["params"]["sessionId"] = malformed;
            }
            peer.page(&request, vec![event], Value::Null).await;
        });
        assert!(
            history::recover(&mut connection, context(&journal), "head", None)
                .await
                .is_err()
        );
        producer.await?;
        assert_eq!(
            journal
                .view_cursor("track".into(), "session".into())
                .await?
                .as_deref(),
            Some("start")
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_pruned_cursor_uses_the_hosts_explicit_compaction_boundary() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    put(&journal, item("pruned", "initial")).await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let first = peer.request().await;
        peer.send(json!({"jsonrpc":"2.0","id":first["id"],"error":{"code":-32011,"message":"cursor pruned"}})).await;
        let anchored = peer.request().await;
        assert!(anchored["params"].get("cursor").is_none());
        assert_eq!(anchored["params"]["anchor"], "latestCompaction");
        peer.send(json!({"jsonrpc":"2.0","id":anchored["id"],"result":{"events":[item("after-boundary","after-summary")],"nextCursor":null,"resolvedAnchor":{"boundaryCursor":"boundary"}}})).await;
    });
    history::recover(&mut connection, context(&journal), "ephemeral", None).await?;
    producer.await?;
    let db = rusqlite::Connection::open(path)?;
    assert_eq!(db.query_row("SELECT json_extract(payload,'$.boundaryCursor') FROM events WHERE kind='history.compacted'",[],|r|r.get::<_,String>(0))?,"boundary");
    Ok(())
}

#[tokio::test]
async fn expired_views_and_pruned_boundaries_retry_the_latest_compaction_once() -> Result<()> {
    for (code, kind) in [(-32040, "viewTruncated"), (-32042, "boundaryPruned")] {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(
            root.path().join("db"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        put(&journal, item("expired", "initial")).await?;
        let (mut connection, mut peer) = pair();
        let producer = tokio::spawn(async move {
            let first = peer.request().await;
            peer.send(json!({"jsonrpc":"2.0","id":first["id"],"error":{"code":code,"data":{"kind":kind,"earliestCursor":null,"latestBoundaryCursor":"boundary"}}})).await;
            let anchored =
                tokio::time::timeout(std::time::Duration::from_secs(2), peer.request()).await?;
            assert!(anchored["params"].get("cursor").is_none());
            assert_eq!(anchored["params"]["anchor"], "latestCompaction");
            peer.send(json!({"jsonrpc":"2.0","id":anchored["id"],"result":{"events":[item("head","retained")],"nextCursor":null,"resolvedAnchor":{"boundaryCursor":"boundary"}}})).await;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        });
        let recovered = history::recover(&mut connection, context(&journal), "head", None).await;
        assert!(recovered.is_ok(), "{kind}: {recovered:?}");
        producer.await??;
        assert_eq!(
            journal
                .view_cursor("track".into(), "session".into())
                .await?
                .as_deref(),
            Some("head")
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_budget_omitted_resume_retains_the_gap_until_paged_history_is_committed() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    put(&journal, item("before", "initial")).await?;
    journal.msp_raw("track".into(),"session".into(),Stamp::now(),json!({"jsonrpc":"2.0","method":"view/gap","params":{"sessionId":"session","after":"before","next":"head"}}).into(),None).await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        assert_eq!(request["method"], "session/resume");
        assert_eq!(request["params"]["history"], "snapshot");
        peer.send(json!({"jsonrpc":"2.0","id":request["id"],"result":{"session":{"sessionId":"session","permissionState":{"profileId":"intercat-worker"}},"viewCursor":"head","history":{"mode":"none","noneReason":"historyBudget","items":null,"snapshot":null}}})).await;
        let request = peer.request().await;
        assert_eq!(request["method"], "view/page");
        assert_eq!(request["params"]["cursor"], "before");
        peer.page(&request, vec![item("head", "recovered")], Value::Null)
            .await;
    });
    let resumed = history::resume(&mut connection, context(&journal), uuid::Uuid::now_v7()).await?;
    assert_eq!(resumed["history"]["noneReason"], "historyBudget");
    assert!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .is_some()
    );
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("before")
    );
    history::recover(&mut connection, context(&journal), "head", None).await?;
    producer.await?;
    assert!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .is_none()
    );
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("head")
    );
    Ok(())
}

#[tokio::test]
async fn an_unavailable_latest_boundary_preserves_the_gap_without_a_retry_loop() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    put(&journal, item("expired", "original")).await?;
    journal.msp_raw("track".into(),"session".into(),Stamp::now(),json!({"jsonrpc":"2.0","method":"view/gap","params":{"sessionId":"session","after":"expired","next":"head"}}).into(),None).await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let first = peer.request().await;
        peer.send(json!({"jsonrpc":"2.0","id":first["id"],"error":{"code":-32040,"data":{"kind":"viewTruncated","earliestCursor":null}}})).await;
        let second = peer.request().await;
        assert_eq!(second["params"]["anchor"], "latestCompaction");
        peer.send(json!({"jsonrpc":"2.0","id":second["id"],"error":{"code":-32042,"data":{"kind":"noBoundary","latestBoundaryCursor":null}}})).await;
        let mut line = String::new();
        assert_eq!(
            peer.input.read_line(&mut line).await.unwrap(),
            0,
            "Unexpected third recovery request: {line}"
        );
    });
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        history::recover(&mut connection, context(&journal), "head", None),
    )
    .await?;
    assert!(result.is_err());
    drop(connection);
    producer.await?;
    assert!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .is_some()
    );
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("expired")
    );
    assert_eq!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .items
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn a_known_compaction_boundary_still_rewinds_and_records_the_summary() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    put(&journal, item("boundary", "retained")).await?;
    put(&journal, item("pruned", "previous-head")).await?;
    for _ in 0..2 {
        let (mut connection, mut peer) = pair();
        let producer = tokio::spawn(async move {
            let request = peer.request().await;
            peer.send(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32011}}))
                .await;
            let request = peer.request().await;
            assert_eq!(request["params"]["anchor"], "latestCompaction");
            peer.send(json!({"jsonrpc":"2.0","id":request["id"],"result":{"events":[],"nextCursor":null,"resolvedAnchor":{"boundaryCursor":"boundary"}}})).await;
        });
        history::recover(&mut connection, context(&journal), "ephemeral", None).await?;
        producer.await?;
        assert_eq!(
            journal
                .view_cursor("track".into(), "session".into())
                .await?
                .as_deref(),
            Some("boundary")
        );
    }
    drop(journal);
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("boundary")
    );
    assert_eq!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .items
            .len(),
        2
    );
    let db = rusqlite::Connection::open(path)?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='history.compacted'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn compaction_requires_a_page_receipt_for_the_exact_boundary() -> Result<()> {
    for frame in [
        json!({"jsonrpc":"2.0","id":1,"result":{}}),
        json!({"jsonrpc":"2.0","id":1,"error":{"code":-32011}}),
        json!({"jsonrpc":"2.0","method":"session/nameChanged","params":{"sessionId":"session","name":"not a page"}}),
        json!({"jsonrpc":"2.0","id":1,"result":{"events":[],"nextCursor":null,"resolvedAnchor":{"boundaryCursor":"other"}}}),
    ] {
        let root = tempfile::tempdir()?;
        let path = root.path().join("db");
        let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
        put(&journal, item("original", "original")).await?;
        let receipt = journal
            .msp_raw(
                "track".into(),
                "session".into(),
                Stamp::now(),
                frame.into(),
                None,
            )
            .await?;
        assert!(
            journal
                .view_compacted("track".into(), "session".into(), "boundary".into(), receipt)
                .await
                .is_err(),
            "An unrelated receipt cannot authorize a history rewind"
        );
        assert_eq!(
            journal
                .view_cursor("track".into(), "session".into())
                .await?
                .as_deref(),
            Some("original")
        );
        let db = rusqlite::Connection::open(path)?;
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM events WHERE kind='history.compacted'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            0
        );
    }
    Ok(())
}

#[tokio::test]
async fn interrupted_compaction_fill_reopens_without_losing_the_gap_or_duplicating_items()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    put(&journal, item("pruned", "retained")).await?;
    journal.msp_raw("track".into(), "session".into(), Stamp::now(), json!({"jsonrpc":"2.0","method":"view/gap","params":{"sessionId":"session","after":"pruned","next":"head"}}).into(), None).await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        assert_eq!(request["params"]["cursor"], "pruned");
        peer.send(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32011}}))
            .await;
        let request = peer.request().await;
        assert_eq!(request["params"]["anchor"], "latestCompaction");
        peer.send(json!({"jsonrpc":"2.0","id":request["id"],"result":{"events":[item("prefix","prefix")],"nextCursor":"prefix","resolvedAnchor":{"boundaryCursor":"boundary"}}})).await;
        let request = peer.request().await;
        assert_eq!(request["params"]["cursor"], "prefix");
        assert!(request["params"].get("anchor").is_none());
        peer.send(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000}}))
            .await;
    });
    assert!(
        history::recover(&mut connection, context(&journal), "head", None)
            .await
            .is_err()
    );
    producer.await?;
    drop(connection);
    drop(journal);
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert_eq!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .unwrap()
            .after,
        "pruned"
    );
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        assert_eq!(request["params"]["cursor"], "pruned");
        peer.send(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32011}}))
            .await;
        let request = peer.request().await;
        assert_eq!(request["params"]["anchor"], "latestCompaction");
        peer.send(json!({"jsonrpc":"2.0","id":request["id"],"result":{"events":[item("prefix","prefix")],"nextCursor":"prefix","resolvedAnchor":{"boundaryCursor":"boundary"}}})).await;
        let request = peer.request().await;
        assert_eq!(request["params"]["cursor"], "prefix");
        assert!(request["params"].get("anchor").is_none());
        peer.page(&request, vec![item("head", "last")], Value::Null)
            .await;
    });
    history::recover(&mut connection, context(&journal), "head", None).await?;
    producer.await?;
    assert!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .is_none()
    );
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("head")
    );
    assert_eq!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .items
            .len(),
        3
    );
    drop(connection);
    drop(journal);
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .is_none()
    );
    assert_eq!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .items
            .len(),
        3
    );
    let db = rusqlite::Connection::open(path)?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='history.compacted'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}

#[tokio::test]
async fn paging_more_than_the_receive_budget_does_not_accumulate_page_bodies() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        for n in 0..24 {
            let request = peer.request().await;
            let cursor = format!("page-{n}");
            let mut event = item(&cursor, &cursor);
            event["params"]["item"]["text"] = json!("x".repeat(450_000));
            peer.page(
                &request,
                vec![event],
                if n == 23 { Value::Null } else { json!(cursor) },
            )
            .await;
        }
    });
    history::recover(&mut connection, context(&journal), "head", None).await?;
    producer.await?;
    assert!(!connection.client.output_flow().interrupted);
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("page-23")
    );
    Ok(())
}

#[tokio::test]
async fn recovered_active_turn_report_and_completion_reach_the_actor_once() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let turn = uuid::Uuid::now_v7();
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        let report = json!({"method":"item/completed","params":{"sessionId":"session","viewCursor":"report","sourceRange":{},"item":{"turnId":turn,"itemId":"answer","revision":1,"kind":"agentMessage","status":"completed","messagePhase":"finalAnswer","text":"Recovered final answer"}}});
        let done = json!({"method":"turn/completed","params":{"sessionId":"session","viewCursor":"done","sourceRange":{},"turnId":turn,"terminal":"completed"}});
        peer.page(&request, vec![report, done], Value::Null).await;
    });
    history::recover(&mut connection, context(&journal), "done", Some(turn)).await?;
    assert!(connection.client.output_flow().queued_bytes > 0);
    for method in ["item/completed", "turn/completed"] {
        let Some(Event::Frame(event)) = connection.next_event().await else {
            panic!("Missing focused event")
        };
        assert_eq!(event.frame["method"], method);
        assert!(event.source.is_some());
        journal
            .try_msp(
                "track".into(),
                "session".into(),
                event.stamp,
                event.frame,
                Some(event.lease),
                event.source,
            )?
            .await??;
    }
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("done")
    );
    assert_eq!(connection.client.output_flow().queued_bytes, 0);
    producer.await?;
    Ok(())
}

#[tokio::test]
async fn a_new_gap_extends_the_walk_even_when_the_old_page_reaches_its_end() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let first = peer.request().await;
        peer.send(json!({"jsonrpc":"2.0","method":"view/gap","params":{"sessionId":"session","after":"old","next":"new"}})).await;
        peer.page(&first, vec![item("old", "one")], Value::Null)
            .await;
        let second = peer.request().await;
        assert_eq!(second["params"]["cursor"], "old");
        peer.page(&second, vec![item("new", "two")], Value::Null)
            .await;
    });
    history::recover(&mut connection, context(&journal), "old", None).await?;
    producer.await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("new")
    );
    Ok(())
}

#[tokio::test]
async fn a_late_gap_rewinds_durably_before_paging_and_keeps_newer_items() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    put(&journal, item("after", "first")).await?;
    put(&journal, item("next", "last")).await?;
    put(
        &journal,
        json!({"method":"view/gap","params":{"sessionId":"session","after":"after","next":"next"}}),
    )
    .await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("after")
    );
    drop(journal);
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        assert_eq!(request["params"]["cursor"], "after");
        peer.page(
            &request,
            vec![item("missing", "middle"), item("next", "last")],
            Value::Null,
        )
        .await;
    });
    history::recover(&mut connection, context(&journal), "next", None).await?;
    producer.await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("next")
    );
    let db = rusqlite::Connection::open(path)?;
    assert_eq!(
        db.query_row("SELECT count(*) FROM timeline_items", [], |r| r
            .get::<_, i64>(0))?,
        3
    );
    Ok(())
}

#[tokio::test]
async fn a_late_gap_cannot_roll_session_state_back_after_restart() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let name = |cursor: &str, value: &str| {
        json!({"method":"session/nameChanged","params":{
        "sessionId":"session","viewCursor":cursor,"sourceRange":{},"name":value}})
    };
    put(&journal, name("after", "initial")).await?;
    put(&journal, name("next", "latest")).await?;
    put(
        &journal,
        json!({"method":"view/gap","params":{"sessionId":"session","after":"after","next":"next"}}),
    )
    .await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        peer.page(
            &request,
            vec![name("missing", "middle"), name("next", "latest")],
            Value::Null,
        )
        .await;
    });
    history::recover(&mut connection, context(&journal), "next", None).await?;
    producer.await?;
    drop(journal);
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let local = journal.reconcile_local("session".into()).await?;
    assert_eq!(
        local.view.session_state["session/nameChanged"]["name"],
        "latest"
    );
    let db = rusqlite::Connection::open(path)?;
    assert_eq!(db.query_row("SELECT count(*) FROM events WHERE kind='msp.applied' AND json_extract(payload,'$.method')='session/nameChanged'",[],|r|r.get::<_,i64>(0))?,3);
    Ok(())
}

#[tokio::test]
async fn a_replayed_completion_remains_the_latest_restore_fact() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let turn = uuid::Uuid::now_v7();
    let completion = move |cursor: &str, terminal: &str| {
        json!({"method":"turn/completed","params":{
        "sessionId":"session","turnId":turn,"viewCursor":cursor,"sourceRange":{},"terminal":terminal}})
    };
    put(&journal, item("after", "initial")).await?;
    put(&journal, completion("next", "cancelled")).await?;
    put(
        &journal,
        json!({"method":"view/gap","params":{"sessionId":"session","after":"after","next":"next"}}),
    )
    .await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        peer.page(
            &request,
            vec![
                completion("missing", "failed"),
                completion("next", "cancelled"),
            ],
            Value::Null,
        )
        .await;
    });
    history::recover(&mut connection, context(&journal), "next", None).await?;
    producer.await?;
    let local = journal.reconcile_local("session".into()).await?;
    assert_eq!(
        local.view.turns[&turn].terminal.as_deref(),
        Some("cancelled")
    );
    let replay = journal.reconcile_replay("session".into(), turn).await?;
    assert_eq!(replay.len(), 1);
    assert_eq!(replay[0].0["params"]["terminal"], "cancelled");
    Ok(())
}

#[tokio::test]
async fn a_raw_gap_rewinds_even_if_the_process_stops_before_projection() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    put(&journal, item("after", "one")).await?;
    put(&journal, item("next", "two")).await?;
    journal.msp_raw("track".into(),"session".into(),Stamp::now(),json!({"method":"view/gap","params":{"sessionId":"session","after":"after","next":"next"}}).into(),None).await?;
    drop(journal);
    let journal = Journal::open(path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("after")
    );
    Ok(())
}

#[tokio::test]
async fn coalesced_raw_gaps_keep_the_first_hole_after_restart() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    put(&journal, item("first", "first")).await?;
    put(&journal, item("later", "later")).await?;
    for (after, next) in [("first", "middle"), ("later", "last")] {
        journal.msp_raw("track".into(),"session".into(),Stamp::now(),
            json!({"method":"view/gap","params":{"sessionId":"session","after":after,"next":next}}).into(),None).await?;
    }
    drop(journal);
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("first")
    );
    let gap = journal
        .view_gap("track".into(), "session".into())
        .await?
        .unwrap();
    assert_eq!((&*gap.after, &*gap.next), ("first", "last"));
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        assert_eq!(request["params"]["cursor"], "first");
        peer.page(
            &request,
            vec![
                item("middle", "middle"),
                item("later", "later"),
                item("last", "last"),
            ],
            Value::Null,
        )
        .await;
    });
    // The restored upper bound must extend an older caller's target too.
    history::recover(&mut connection, context(&journal), "middle", None).await?;
    producer.await?;
    assert!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .is_none()
    );
    put(
        &journal,
        json!({"method":"view/gap","params":{"sessionId":"session","after":"last","next":"new"}}),
    )
    .await?;
    let gap = journal
        .view_gap("track".into(), "session".into())
        .await?
        .unwrap();
    assert_eq!((&*gap.after, &*gap.next), ("last", "new"));
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("last")
    );
    Ok(())
}

#[tokio::test]
async fn finishing_one_gap_keeps_a_later_receipt_and_its_lower_bound() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    put(&journal, item("first", "first")).await?;
    let first=journal.msp_raw("track".into(),"session".into(),Stamp::now(),json!({"method":"view/gap","params":{"sessionId":"session","after":"first","next":"second"}}).into(),None).await?;
    let mut recovered = item("second", "second");
    recovered["_intercatRecovered"] = json!(true);
    put(&journal, recovered).await?;
    let second=journal.msp_raw("track".into(),"session".into(),Stamp::now(),json!({"method":"view/gap","params":{"sessionId":"session","after":"second","next":"third"}}).into(),None).await?;
    put(&journal, item("live", "live")).await?;
    journal
        .gap_filled("track".into(), "session".into(), first)
        .await?;
    let pending = journal
        .view_gap("track".into(), "session".into())
        .await?
        .unwrap();
    assert_eq!(
        (
            pending.after.as_str(),
            pending.next.as_str(),
            pending.through
        ),
        ("second", "third", second)
    );
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("second")
    );
    journal
        .gap_filled("track".into(), "session".into(), first)
        .await?;
    assert!(
        journal
            .gap_filled("foreign".into(), "session".into(), second)
            .await
            .is_err()
    );
    assert!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn interrupted_gap_reopens_its_first_bound_and_does_not_duplicate_items() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    put(&journal, item("first", "first")).await?;
    put(
        &journal,
        json!({"method":"view/gap","params":{"sessionId":"session","after":"first","next":"last"}}),
    )
    .await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let first = peer.request().await;
        peer.page(&first, vec![item("middle", "middle")], json!("middle"))
            .await;
        let second = peer.request().await;
        peer.send(json!({"jsonrpc":"2.0","id":second["id"],"error":{"code":-32000}}))
            .await;
    });
    assert!(
        history::recover(&mut connection, context(&journal), "last", None)
            .await
            .is_err()
    );
    producer.await?;
    drop(connection);
    drop(journal);
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .is_some()
    );
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        assert_eq!(request["params"]["cursor"], "first");
        peer.page(
            &request,
            vec![item("middle", "middle"), item("last", "last")],
            Value::Null,
        )
        .await;
    });
    history::recover(&mut connection, context(&journal), "last", None).await?;
    producer.await?;
    assert!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .is_none()
    );
    assert_eq!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .items
            .len(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn the_first_gap_during_a_read_rebases_before_marking_it_filled() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    put(&journal, item("first", "first")).await?;
    put(&journal, item("later", "later")).await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        assert_eq!(request["params"]["cursor"], "later");
        peer.send(json!({"jsonrpc":"2.0","method":"view/gap","params":{"sessionId":"session","after":"first","next":"last"}})).await;
        peer.page(&request, vec![item("last", "last")], Value::Null)
            .await;
        let Ok(request) =
            tokio::time::timeout(std::time::Duration::from_secs(2), peer.request()).await
        else {
            return false;
        };
        assert_eq!(request["params"]["cursor"], "first");
        peer.page(
            &request,
            vec![
                item("missing", "missing"),
                item("later", "later"),
                item("last", "last"),
            ],
            Value::Null,
        )
        .await;
        true
    });
    history::recover(&mut connection, context(&journal), "last", None).await?;
    assert!(
        producer.await?,
        "The newly discovered lower bound must be read"
    );
    assert_eq!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .items
            .len(),
        4
    );
    assert!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn a_restored_gap_before_the_read_head_stops_at_that_head() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    put(&journal, item("first", "first")).await?;
    put(&journal,json!({"method":"view/gap","params":{"sessionId":"session","after":"first","next":"middle"}})).await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        peer.page(&request, vec![item("middle", "middle")], json!("middle"))
            .await;
        let request = peer.request().await;
        peer.page(&request, vec![item("head", "head")], json!("head"))
            .await;
    });
    history::recover(&mut connection, context(&journal), "head", None).await?;
    producer.await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("head")
    );
    assert!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn rebasing_a_new_gap_replays_the_head_after_the_missing_prefix() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    put(&journal, item("first", "first")).await?;
    put(&journal, item("later", "later")).await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        peer.send(json!({"jsonrpc":"2.0","method":"view/gap","params":{"sessionId":"session","after":"first","next":"middle"}})).await;
        peer.page(&request, vec![item("head", "head")], json!("head"))
            .await;
        let request = peer.request().await;
        assert_eq!(request["params"]["cursor"], "first");
        peer.page(&request, vec![item("middle", "middle")], json!("middle"))
            .await;
        let Ok(request) =
            tokio::time::timeout(std::time::Duration::from_secs(2), peer.request()).await
        else {
            return false;
        };
        peer.page(&request, vec![item("head", "head")], json!("head"))
            .await;
        true
    });
    history::recover(&mut connection, context(&journal), "head", None).await?;
    assert!(
        producer.await?,
        "The head must be restored after the older prefix"
    );
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("head")
    );
    Ok(())
}

#[tokio::test]
async fn status_and_presentation_companions_do_not_hide_the_durable_event_at_the_same_cursor()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let turn = uuid::Uuid::now_v7();
    put(&journal,json!({"method":"session/statusChanged","params":{"sessionId":"session","viewCursor":"started","status":"running"}})).await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?,
        None
    );
    put(&journal,json!({"method":"turn/started","params":{"sessionId":"session","viewCursor":"started","sourceRange":{},"turnId":turn,"commandId":turn}})).await?;
    let request = json!({"sessionId":"session","viewCursor":"approval","sourceRange":{},"approvalId":"approval","turnId":turn});
    put(&journal,json!({"method":"session/statusChanged","params":{"sessionId":"session","viewCursor":"approval","status":"waiting"}})).await?;
    put(
        &journal,
        json!({"method":"approval/request","params":request}),
    )
    .await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("started")
    );
    put(
        &journal,
        json!({"method":"approval/requested","params":request}),
    )
    .await?;
    let local = journal.reconcile_local("session".into()).await?;
    assert!(local.view.turns.contains_key(&turn));
    let db = rusqlite::Connection::open(path)?;
    assert_eq!(db.query_row("SELECT count(*) FROM events WHERE kind='msp.applied' AND json_extract(payload,'$.method')='approval/requested'",[],|r|r.get::<_,i64>(0))?,1);
    Ok(())
}

#[tokio::test]
async fn page_redaction_checks_strings_beyond_the_first_scanner_batch() -> Result<()> {
    let secret = "explicit-history-test-secret";
    let scanner = intercat_daemon::secret_scan::Engine::capture()?.start(&[secret.into()])?;
    let mut fields = vec![json!("safe"); 3000];
    fields[2999] = json!(secret);
    let redacted =
        intercat_daemon::recording::redact(&scanner, json!({"result":{"events":fields}})).await?;
    assert_eq!(redacted["result"]["events"][2999], "[redacted]");
    assert_eq!(redacted["result"]["events"][0], "safe");
    assert!(!redacted.to_string().contains(secret));
    Ok(())
}

#[tokio::test]
async fn an_empty_compacted_view_saves_its_boundary_before_the_next_resume() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    put(&journal, item("pruned", "initial")).await?;
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        peer.send(json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32011}}))
            .await;
        let request = peer.request().await;
        peer.send(json!({"jsonrpc":"2.0","id":request["id"],"result":{"events":[],"nextCursor":null,"resolvedAnchor":{"boundaryCursor":"compacted"}}})).await;
    });
    history::recover(&mut connection, context(&journal), "head", None).await?;
    producer.await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("compacted")
    );
    let summaries = journal.snapshot().await?["timeline"]["summaries"].clone();
    assert_eq!(summaries[0]["sessionId"], "session");
    assert!(summaries[0]["repairRunId"].is_null());
    drop(journal);
    let journal = Journal::open(path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert_eq!(
        journal.snapshot().await?["timeline"]["summaries"],
        summaries
    );
    let (mut connection, mut peer) = pair();
    let producer = tokio::spawn(async move {
        let request = peer.request().await;
        assert_eq!(request["params"]["cursor"], "compacted");
        peer.page(&request, vec![item("head", "later")], Value::Null)
            .await;
    });
    history::recover(&mut connection, context(&journal), "head", None).await?;
    producer.await?;
    Ok(())
}
