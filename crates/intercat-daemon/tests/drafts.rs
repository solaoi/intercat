use intercat_daemon::{Result, drafts, journal::Journal};
use serde_json::{Value, json};
use uuid::Uuid;

async fn journal(path: &std::path::Path) -> Result<Journal> {
    Journal::open(path, intercat_daemon::secrets::Key::new([7; 32])).await
}
fn request(session: &str, current: &Value, book: Value, op: Uuid) -> drafts::Save {
    serde_json::from_value(json!({"session":session,"revision":current["revision"],"parentOpId":current["opId"],"opId":op,"book":book})).unwrap()
}

#[tokio::test]
async fn item_instruction_draft_keeps_scope_and_pending_identity_after_restart() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let store = journal(&path).await?;
    let current = store.read_drafts("session".into()).await?;
    let mut book = current["book"].clone();
    let plan = Uuid::now_v7();
    let item = Uuid::now_v7();
    let track = Uuid::now_v7();
    let operation = Uuid::now_v7();
    book["entries"]["work"]["text"] = json!("Keep the ordinary dummy draft.");
    book["active"] = json!("item-instruction");
    book["stack"] = json!(["work"]);
    book["entries"]["item-instruction"] = json!({"text":"Add a dummy check.","target":{"mode":"lock","trackId":track},"attachments":[],
        "context":{"kind":"continuation","planId":plan,"itemId":item,"trackId":track,"revision":4,"title":"Dummy item"},
        "pending":{"kind":"work","request":{"opId":operation,"text":"Add a dummy check.","target":{"mode":"lock","trackId":track},"planItem":{"planId":plan,"itemId":item,"revision":4}}}});
    for field in ["planId", "itemId", "revision"] {
        let mut wrong = book.clone();
        wrong["entries"]["item-instruction"]["pending"]["request"]["planItem"][field] = Value::Null;
        assert!(
            store
                .save_drafts(request("session", &current, wrong, Uuid::now_v7()))
                .await
                .is_err()
        );
    }
    let mut wrong = book.clone();
    wrong["entries"]["item-instruction"]["target"]["trackId"] = json!(Uuid::now_v7());
    assert!(
        store
            .save_drafts(request("session", &current, wrong, Uuid::now_v7()))
            .await
            .is_err()
    );
    store
        .save_drafts(request("session", &current, book.clone(), Uuid::now_v7()))
        .await?;
    drop(store);
    let reopened = journal(&path).await?;
    assert_eq!(reopened.read_drafts("session".into()).await?["book"], book);
    Ok(())
}

#[tokio::test]
async fn plan_revision_draft_preserves_its_exact_operation_after_restart() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let store = journal(&path).await?;
    let current = store.read_drafts("session".into()).await?;
    let mut book = current["book"].clone();
    let plan = Uuid::now_v7();
    let item = Uuid::now_v7();
    let token = Uuid::now_v7();
    let operation = Uuid::now_v7();
    book["entries"]["work"]["text"] = json!("Keep this ordinary dummy draft.");
    book["active"] = json!("revision");
    book["stack"] = json!(["work"]);
    book["entries"]["revision"] = json!({
        "text":"Use a blue dummy badge.", "target":{"mode":"none"}, "attachments":[],
        "context":{"kind":"plan-item","planId":plan,"itemId":item,"token":token,"revision":4,"title":"Dummy badge"},
        "pending":{"kind":"plan-item","request":{"kind":"submit","planId":plan,"token":token,"revision":4,"opId":operation,"text":"Use a blue dummy badge."}}
    });
    for field in ["planId", "token", "revision", "text"] {
        let mut invalid = book.clone();
        invalid["entries"]["revision"]["pending"]["request"][field] = Value::Null;
        assert!(
            store
                .save_drafts(request("session", &current, invalid, Uuid::now_v7()))
                .await
                .is_err()
        );
    }
    store
        .save_drafts(request("session", &current, book.clone(), Uuid::now_v7()))
        .await?;
    drop(store);
    let store = journal(&path).await?;
    assert_eq!(store.read_drafts("session".into()).await?["book"], book);
    Ok(())
}

#[tokio::test]
async fn approval_replies_keep_their_owner_and_pending_response_through_restart() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let store = journal(&path).await?;
    let current = store.read_drafts("session".into()).await?;
    let mut book = current["book"].clone();
    let owner = json!({"kind":"repair","runId":Uuid::now_v7()});
    let response = json!({"action":"reply","mode":"restart","text":"Keep the original files."});
    book["entries"]["reply"] = json!({"text":response["text"],"target":{"mode":"lock","trackId":"A"},"attachments":[],
        "context":{"kind":"approval","owner":owner,"trackId":"A","sessionId":"worker-session","approvalId":"approval","requirement":{"sourceIndex":0},"mode":"restart","label":"Worker A"},
        "pending":{"kind":"approval","request":{"opId":Uuid::now_v7(),"owner":owner,"trackId":"A","sessionId":"worker-session","approvalId":"approval","requirement":{"sourceIndex":0},"response":response}}});
    for field in ["owner", "trackId", "sessionId", "approvalId", "requirement"] {
        let mut wrong = book.clone();
        wrong["entries"]["reply"]["pending"]["request"][field] = Value::Null;
        assert!(
            store
                .save_drafts(request("session", &current, wrong, Uuid::now_v7()))
                .await
                .is_err()
        );
    }
    let mut wrong = book.clone();
    wrong["active"] = "reply".into();
    assert!(
        store
            .save_drafts(request("session", &current, wrong, Uuid::now_v7()))
            .await
            .is_err()
    );
    store
        .save_drafts(request("session", &current, book.clone(), Uuid::now_v7()))
        .await?;
    drop(store);
    let store = journal(&path).await?;
    assert_eq!(store.read_drafts("session".into()).await?["book"], book);
    Ok(())
}
#[tokio::test]
async fn session_drafts_and_pending_operations_survive_reopening_the_database() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let store = journal(&path).await?;
    let current = store.read_drafts("session-one".into()).await?;
    let mut book = current["book"].clone();
    book["entries"]["work"]["text"] = json!("下書きを復元する");
    book["entries"]["work"]["target"] = json!({"mode":"lock","trackId":"B"});
    book["entries"]["work"]["pending"] =
        json!({"kind":"work","request":{"opId":Uuid::now_v7(),"text":"original"}});
    let op = Uuid::parse_str("d6cf457c-e1bb-44cc-8b1e-877cdf8bf58e")?;
    let saved = store
        .save_drafts(request("session-one", &current, book.clone(), op))
        .await?;
    assert_eq!(saved["status"], "saved");
    assert_eq!(
        store
            .save_drafts(request("session-one", &current, book.clone(), op))
            .await?["revision"],
        1
    );
    drop(store);
    let store = journal(&path).await?;
    assert_eq!(store.read_drafts("session-one".into()).await?["book"], book);
    assert_eq!(
        store.read_drafts("session-two".into()).await?["revision"],
        0
    );
    Ok(())
}
#[tokio::test]
async fn competing_windows_cannot_overwrite_a_parent_they_did_not_read() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = journal(&root.path().join("db")).await?;
    let initial = store.read_drafts("s".into()).await?;
    let mut one = initial["book"].clone();
    one["entries"]["work"]["text"] = json!("one");
    let mut two = initial["book"].clone();
    two["entries"]["work"]["text"] = json!("two");
    let (a, b) = tokio::join!(
        store.save_drafts(request("s", &initial, one.clone(), Uuid::now_v7())),
        store.save_drafts(request("s", &initial, two, Uuid::now_v7()))
    );
    assert_eq!(a?["status"], "saved");
    assert_eq!(b?["status"], "conflict");
    let latest = store.read_drafts("s".into()).await?;
    assert_eq!(latest["book"], one);
    let mut wrong_parent = latest.clone();
    wrong_parent["opId"] = json!(Uuid::now_v7());
    assert_eq!(
        store
            .save_drafts(request("s", &wrong_parent, one, Uuid::now_v7()))
            .await?["status"],
        "conflict"
    );
    Ok(())
}
#[tokio::test]
async fn invalid_history_and_reused_operation_contents_are_rejected() -> Result<()> {
    let root = tempfile::tempdir()?;
    let store = journal(&root.path().join("db")).await?;
    let initial = store.read_drafts("s".into()).await?;
    let mut invalid = initial["book"].clone();
    invalid["active"] = json!("missing");
    assert!(
        store
            .save_drafts(request("s", &initial, invalid, Uuid::now_v7()))
            .await
            .is_err()
    );
    let op = Uuid::now_v7();
    store
        .save_drafts(request("s", &initial, initial["book"].clone(), op))
        .await?;
    let mut changed = initial["book"].clone();
    changed["entries"]["work"]["text"] = json!("changed");
    assert!(
        store
            .save_drafts(request("s", &initial, changed, op))
            .await
            .is_err()
    );
    Ok(())
}
#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn full_sqlite_keeps_the_saved_book_and_the_exact_retry_succeeds_after_recovery() -> Result<()>
{
    use intercat_daemon::fault_injection::{self, Request};
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let store = journal(&path).await?;
    drop(store);
    let mut db = rusqlite::Connection::open(&path)?;
    intercat_daemon::clock::register(&db)?;
    let initial = serde_json::to_value(drafts::read(&db, "s")?)?;
    let mut book = initial["book"].clone();
    book["entries"]["work"]["text"] = json!("saved");
    let current = drafts::save(&mut db, request("s", &initial, book, Uuid::now_v7()))?;
    let mut next = current["book"].clone();
    next["entries"]["work"]["text"] = json!("pending retry");
    let op = Uuid::now_v7();
    assert_eq!(
        fault_injection::apply(&mut db, Request::Fill)?["databasePagesUnchanged"],
        true
    );
    assert!(drafts::save(&mut db, request("s", &current, next.clone(), op)).is_err());
    assert_eq!(
        serde_json::to_value(drafts::read(&db, "s")?)?["book"],
        current["book"]
    );
    fault_injection::apply(&mut db, Request::Restore)?;
    assert_eq!(
        drafts::save(&mut db, request("s", &current, next.clone(), op))?["book"],
        next
    );
    Ok(())
}

#[tokio::test]
async fn native_rpc_saves_the_same_book_that_is_read_afterward() -> Result<()> {
    use intercat_daemon::{
        editor::Manager,
        lock::DaemonLock,
        rpc::{Server, Token},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let root = tempfile::tempdir_in("/private/tmp")?;
    let _lock = DaemonLock::acquire(root.path())?.unwrap();
    let store = journal(&root.path().join("db")).await?;
    let initial = store.read_drafts("s".into()).await?;
    let mut book = initial["book"].clone();
    book["entries"]["work"]["text"] = json!("Native draft RPC");
    let token = "a5".repeat(32);
    let server = Server::bind(
        root.path(),
        Token::parse(&token)?,
        store,
        Manager::new(root.path(), &root.path().join("editor.log"))?,
        &root.path().join("workspaces"),
    )?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let mut stream =
        BufReader::new(tokio::net::UnixStream::connect(root.path().join("rpc.sock")).await?);
    for (method, params) in [
        ("rpc.authenticate", json!({"token":token})),
        (
            "draft.save",
            json!({"session":"s","revision":0,"parentOpId":null,"opId":Uuid::now_v7(),"book":book}),
        ),
        ("draft.read", json!({"session":"s"})),
    ] {
        stream
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await?;
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            stream.read_line(&mut line),
        )
        .await??;
        let response: Value = serde_json::from_str(&line)?;
        assert!(response.get("result").is_some(), "{response}");
        if method == "draft.read" {
            assert_eq!(response["result"]["book"], book);
        }
    }
    stop.send(()).unwrap();
    task.await??;
    Ok(())
}

#[tokio::test]
async fn an_edit_draft_keeps_its_lease_and_exact_pending_finish_across_restart() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let store = journal(&path).await?;
    let original = store.read_drafts("s".into()).await?;
    let mut book = original["book"].clone();
    let token = Uuid::now_v7();
    book["entries"]["edit:a"] = json!({"text":"new request", "target":{"mode":"none"}, "attachments":[],
        "context":{"kind":"edit","inputId":"a","originalText":"old request","trackId":null,"state":"editing", "lease":{"owner":Uuid::now_v7(),"token":token,"revision":2}},
        "pending":{"kind":"edit","request":{"kind":"end","token":token,"revision":2,"edit":{"kind":"text","text":"new request"}}}});
    book["active"] = json!("edit:a");
    book["stack"] = json!(["work"]);
    store
        .save_drafts(request("s", &original, book.clone(), Uuid::now_v7()))
        .await?;
    drop(store);
    let store = journal(&path).await?;
    assert_eq!(store.read_drafts("s".into()).await?["book"], book);
    Ok(())
}

#[tokio::test]
async fn plan_reply_draft_keeps_the_normal_target_and_exact_reply_receipt_after_restart()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let store = journal(&path).await?;
    let current = store.read_drafts("session".into()).await?;
    let mut book = current["book"].clone();
    let plan = Uuid::now_v7();
    let proposal = Uuid::now_v7();
    let operation = Uuid::now_v7();
    let target = Uuid::now_v7();
    book["entries"]["work"]["text"] = json!("Keep the ordinary dummy draft.");
    book["entries"]["work"]["target"] = json!({"mode":"lock","trackId":target});
    book["active"] = json!("plan-reply");
    book["stack"] = json!(["work"]);
    book["entries"]["plan-reply"] = json!({"text":"Use separate dummy files.","target":{"mode":"none"},"attachments":[],
        "context":{"kind":"plan-reply","planId":plan,"questionId":proposal,"revision":4,"question":"Dummy overlap","label":"A + B"},
        "pending":{"kind":"plan-reply","request":{"opId":operation,"planId":plan,"questionId":proposal,"revision":4,"question":"Dummy overlap","text":"Use separate dummy files."}}});
    for field in ["planId", "questionId", "revision", "question", "text"] {
        let mut invalid = book.clone();
        invalid["entries"]["plan-reply"]["pending"]["request"][field] = Value::Null;
        assert!(
            store
                .save_drafts(request("session", &current, invalid, Uuid::now_v7()))
                .await
                .is_err()
        );
    }
    store
        .save_drafts(request("session", &current, book.clone(), Uuid::now_v7()))
        .await?;
    drop(store);
    let store = journal(&path).await?;
    assert_eq!(store.read_drafts("session".into()).await?["book"], book);
    Ok(())
}
