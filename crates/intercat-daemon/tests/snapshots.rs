use intercat_daemon::{Result, clock::Stamp, journal::Journal, secrets::Key};
use serde_json::{Value, json};
use uuid::Uuid;

fn snapshot(cursor: &str) -> Value {
    json!({"session":{"sessionId":"session"},"viewCursor":cursor,"history":{"mode":"snapshot","snapshot":{
    "schemaVersion":1,"viewCursor":cursor,"state":{
        "items":[{"itemId":"seed","kind":"agentMessage","revision":1,"status":"completed","recordedAt":"2026-11-05T00:00:00Z","text":"snapshot item"}],
        "activeTurn":null,"queuedTurns":[],"pendingApprovals":[],"pendingUserInputs":[],
        "approvalMode":{"mode":"promptUnmatched","source":"startup","lastCommandId":null},
        "effectiveModel":null,"goal":null,"todoList":null,"branch":null,"name":"snapshot name",
        "tokenUsage":{"promptTokens":3,"outputTokens":4,"totalTokens":7},"turnCount":2,
        "futureState":{"preserved":true}
    }}}})
}
async fn put(journal: &Journal, cursor: &str, method: &str, mut params: Value) -> Result<()> {
    params["sessionId"] = json!("session");
    params["viewCursor"] = json!(cursor);
    params["sourceRange"] = json!({});
    journal
        .try_msp(
            "track".into(),
            "session".into(),
            Stamp::now(),
            json!({"jsonrpc":"2.0","method":method,"params":params}),
            None,
            None,
        )?
        .await??;
    Ok(())
}
async fn seed(journal: &Journal, value: Value) -> Result<()> {
    let source = journal
        .msp_raw(
            "track".into(),
            "session".into(),
            Stamp::now(),
            json!({"jsonrpc":"2.0","id":1,"result":value}).into(),
            None,
        )
        .await?;
    journal
        .view_snapshot("track".into(), "session".into(), source)
        .await
}

#[tokio::test]
async fn a_late_snapshot_is_spliced_at_its_cursor_before_the_newer_suffix() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    put(
        &journal,
        "cut",
        "session/nameChanged",
        json!({"name":"old"}),
    )
    .await?;
    put(
        &journal,
        "later",
        "session/nameChanged",
        json!({"name":"newer than snapshot"}),
    )
    .await?;
    put(
        &journal,
        "head",
        "item/completed",
        json!({"item":{"itemId":"tail","revision":1,"text":"later item"}}),
    )
    .await?;
    seed(&journal, snapshot("cut")).await?;
    let view = journal.reconcile_local("session".into()).await?.view;
    assert_eq!(
        view.items
            .iter()
            .map(|i| i["itemId"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["seed", "tail"]
    );
    let state = view.snapshot_state().unwrap();
    assert_eq!(state["name"], "newer than snapshot");
    assert_eq!(state["futureState"]["preserved"], true);
    assert_eq!(state["tokenUsage"]["totalTokens"], 7);
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("head")
    );
    drop(journal);
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    assert_eq!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .snapshot_state(),
        Some(state)
    );
    Ok(())
}

#[tokio::test]
async fn snapshot_replaces_absent_families_and_preserves_nulls_and_unknown_item_fields()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    put(
        &journal,
        "model",
        "session/modelChanged",
        json!({"modelId":"stale"}),
    )
    .await?;
    put(
        &journal,
        "cut",
        "item/completed",
        json!({"item":{"itemId":"old","revision":99}}),
    )
    .await?;
    let mut read = snapshot("cut");
    read["history"]["snapshot"]["state"]["items"][0]["futureItem"] = json!(42);
    seed(&journal, read.clone()).await?;
    seed(&journal, read).await?;
    let view = journal.reconcile_local("session".into()).await?.view;
    assert!(!view.session_state.contains_key("session/modelChanged"));
    assert_eq!(
        view.session_state["session/goalChanged"]["goal"],
        Value::Null
    );
    assert_eq!(view.items.len(), 1);
    assert_eq!(view.items[0]["futureItem"], 42);
    assert!(
        !view
            .snapshot_state()
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("reasoningEffort")
    );
    Ok(())
}

#[tokio::test]
async fn snapshot_runtime_references_follow_terminal_and_request_events() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let turn = Uuid::now_v7();
    let queued = Uuid::now_v7();
    put(
        &journal,
        "cut",
        "session/nameChanged",
        json!({"name":"start"}),
    )
    .await?;
    let mut read = snapshot("cut");
    let state = &mut read["history"]["snapshot"]["state"];
    state["activeTurn"] = json!({"turnId":turn,"commandId":turn});
    state["queuedTurns"] = json!([{"turnId":queued,"commandId":queued,"displayText":"queued"}]);
    state["pendingApprovals"] = json!([{"approvalId":"a","viewCursor":"a-open"}]);
    state["pendingUserInputs"] = json!([{"userInputId":"q","viewCursor":"q-open"}]);
    seed(&journal, read).await?;
    assert_eq!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .active_turn,
        Some(turn)
    );
    put(
        &journal,
        "declined",
        "turn/retracted",
        json!({"turnId":turn,"commandId":turn,"outcome":"declined"}),
    )
    .await?;
    assert_eq!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .snapshot_state()
            .unwrap()["activeTurn"]["turnId"],
        turn.to_string()
    );
    put(
        &journal,
        "done",
        "turn/completed",
        json!({"turnId":turn,"terminal":"completed"}),
    )
    .await?;
    put(
        &journal,
        "unqueue",
        "turn/unqueued",
        json!({"turnId":queued,"commandId":queued}),
    )
    .await?;
    put(
        &journal,
        "a-done",
        "approval/resolved",
        json!({"approvalId":"a"}),
    )
    .await?;
    put(
        &journal,
        "q-done",
        "userInput/settled",
        json!({"userInputId":"q"}),
    )
    .await?;
    let state = journal
        .reconcile_local("session".into())
        .await?
        .view
        .snapshot_state()
        .unwrap();
    assert!(state["activeTurn"].is_null());
    for key in ["queuedTurns", "pendingApprovals", "pendingUserInputs"] {
        assert_eq!(state[key], json!([]));
    }
    assert_eq!(state["turnCount"], 3);
    Ok(())
}

#[tokio::test]
async fn unsupported_snapshots_are_ignored_and_unanchored_or_contradictory_ones_are_rejected()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    put(
        &journal,
        "cut",
        "session/nameChanged",
        json!({"name":"original"}),
    )
    .await?;
    let mut newer = snapshot("cut");
    newer["history"]["snapshot"] = json!({"schemaVersion":2});
    seed(&journal, newer).await?;
    assert!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .snapshot_state()
            .is_none()
    );
    assert!(seed(&journal, snapshot("unknown")).await.is_err());
    let turn = Uuid::now_v7();
    put(
        &journal,
        "done",
        "turn/completed",
        json!({"turnId":turn,"terminal":"completed"}),
    )
    .await?;
    let mut invalid = snapshot("done");
    invalid["history"]["snapshot"]["state"]["activeTurn"] = json!({"turnId":turn,"commandId":turn});
    assert!(seed(&journal, invalid).await.is_err());
    assert!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .snapshot_state()
            .is_none()
    );
    let db = rusqlite::Connection::open(path)?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='history.snapshot'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn replaying_an_anchor_does_not_revive_the_snapshot_turn() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let turn = Uuid::now_v7();
    let anchor = json!({"jsonrpc":"2.0","method":"turn/started","params":{"sessionId":"session","viewCursor":"cut","sourceRange":{},"turnId":turn,"commandId":turn}});
    journal
        .try_msp(
            "track".into(),
            "session".into(),
            Stamp::now(),
            anchor.clone(),
            None,
            None,
        )?
        .await??;
    let mut read = snapshot("cut");
    read["history"]["snapshot"]["state"]["activeTurn"] = json!({"turnId":turn,"commandId":turn});
    seed(&journal, read).await?;
    put(
        &journal,
        "end",
        "turn/completed",
        json!({"turnId":turn,"terminal":"completed"}),
    )
    .await?;
    let mut replay = anchor;
    replay["_intercatRecovered"] = json!(true);
    journal
        .try_msp(
            "track".into(),
            "session".into(),
            Stamp::now(),
            replay,
            None,
            None,
        )?
        .await??;
    let local = journal.reconcile_local("session".into()).await?;
    assert!(local.view.active_turn.is_none());
    assert!(local.view.snapshot_state().unwrap()["activeTurn"].is_null());
    assert_eq!(local.view.snapshot_state().unwrap()["turnCount"], 3);
    Ok(())
}

#[tokio::test]
async fn an_invalid_snapshot_receipt_cannot_replace_a_committed_seed() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    put(
        &journal,
        "cut",
        "session/nameChanged",
        json!({"name":"initial"}),
    )
    .await?;
    seed(&journal, snapshot("cut")).await?;
    let initial = journal
        .reconcile_local("session".into())
        .await?
        .view
        .snapshot_state();
    for mode in [
        "missing-items",
        "foreign-session",
        "not-a-response",
        "invalid-id",
    ] {
        let mut read = snapshot("cut");
        read["history"]["snapshot"]["state"]["name"] = json!("must not become current");
        if mode == "missing-items" {
            read["history"]["snapshot"]["state"]
                .as_object_mut()
                .unwrap()
                .remove("items");
        }
        if mode == "foreign-session" {
            read["session"]["sessionId"] = json!("foreign");
        }
        let mut frame = json!({"jsonrpc":"2.0","id":1,"result":read});
        if mode == "not-a-response" {
            frame["method"] = json!("bogus");
        }
        if mode == "invalid-id" {
            frame["id"] = json!(0);
        }
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
                .view_snapshot("track".into(), "session".into(), receipt)
                .await
                .is_err()
        );
        assert_eq!(
            journal
                .reconcile_local("session".into())
                .await?
                .view
                .snapshot_state(),
            initial
        );
        assert_eq!(
            journal
                .view_cursor("track".into(), "session".into())
                .await?
                .as_deref(),
            Some("cut")
        );
    }
    Ok(())
}

async fn window(journal: &Journal) -> Result<Value> {
    journal
        .timeline_window(intercat_daemon::timeline::Window {
            tracks: vec!["track".into()],
            start: 0,
            end: i64::MAX,
            since: None,
        })
        .await
}

#[tokio::test]
async fn snapshot_only_items_are_visible_without_inventing_a_turn_or_duration() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    put(
        &journal,
        "cut",
        "session/nameChanged",
        json!({"name":"initial"}),
    )
    .await?;
    let read = snapshot("cut");
    seed(&journal, read.clone()).await?;
    seed(&journal, read).await?;
    let first = window(&journal).await?;
    assert_eq!(first["clips"].as_array().unwrap().len(), 1);
    let clip = first["clips"][0].clone();
    assert_eq!(clip["category"], "snapshot");
    assert_eq!(clip["label"], "Recovered history");
    assert!(clip["turnId"].is_null());
    assert_eq!(clip["approximate"], true);
    assert_eq!(clip["running"], false);
    let id = clip["id"].as_str().unwrap().to_owned();
    assert_eq!(
        journal.clip_detail(id.clone()).await?["items"][0]["text"],
        "snapshot item"
    );
    put(&journal,"after","item/completed",json!({"item":{"itemId":"seed","kind":"agentMessage","revision":2,"status":"completed","text":"later text"}})).await?;
    assert_eq!(
        journal.clip_detail(id.clone()).await?["items"][0]["text"],
        "later text"
    );
    assert_eq!(window(&journal).await?["clips"][0]["id"], id);
    drop(journal);
    let journal = Journal::open(path, Key::new([7; 32])).await?;
    assert_eq!(
        journal.clip_detail(id).await?["items"][0]["text"],
        "later text"
    );
    Ok(())
}

#[tokio::test]
async fn snapshot_updates_keep_existing_clip_addresses_and_accept_the_next_revision() -> Result<()>
{
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let turn = Uuid::now_v7();
    put(&journal,"cut","item/completed",json!({"item":{"itemId":"seed","turnId":turn,"kind":"agentMessage","revision":9,"status":"completed","text":"old local"}})).await?;
    let before = window(&journal).await?["clips"][0].clone();
    let mut read = snapshot("cut");
    read["history"]["snapshot"]["state"]["items"][0]["turnId"] = json!(turn);
    seed(&journal, read).await?;
    let after = window(&journal).await?["clips"][0].clone();
    for key in ["id", "ordinal", "start"] {
        assert_eq!(before[key], after[key]);
    }
    let id = after["id"].as_str().unwrap().to_owned();
    assert_eq!(
        journal.clip_detail(id.clone()).await?["items"][0]["revision"],
        1
    );
    put(&journal,"next","item/completed",json!({"item":{"itemId":"seed","turnId":turn,"kind":"agentMessage","revision":2,"status":"completed","text":"next"}})).await?;
    assert_eq!(journal.clip_detail(id).await?["items"][0]["text"], "next");
    Ok(())
}

#[tokio::test]
async fn projecting_a_snapshot_user_message_does_not_mark_input_delivered_or_repeat_effects()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let input = Uuid::now_v7();
    let command = Uuid::now_v7();
    let turn = Uuid::now_v7();
    journal
        .stage_utterance(
            input,
            "waiting".into(),
            intercat_daemon::decision::Target::New,
            None,
            vec![],
        )
        .await?;
    journal
        .intent(
            &command.to_string(),
            "turn.start",
            json!({"sessionId":"session","commandId":command}),
        )
        .await?;
    {
        let db = rusqlite::Connection::open(&path)?;
        db.execute(
            "UPDATE utterances SET dispatch_command=?1,state='sent' WHERE id=?2",
            rusqlite::params![command.to_string(), input.to_string()],
        )?;
    }
    put(
        &journal,
        "cut",
        "session/nameChanged",
        json!({"name":"initial"}),
    )
    .await?;
    let mut read = snapshot("cut");
    read["history"]["snapshot"]["state"]["items"] = json!([{"itemId":"user","kind":"userMessage","revision":1,"status":"completed","recordedAt":"2026-11-05T00:00:00Z","text":"waiting","commandId":command,"turnId":turn}]);
    seed(&journal, read.clone()).await?;
    seed(&journal, read).await?;
    // A snapshot alone cannot present the app's pending statement as landed.
    assert_eq!(
        window(&journal).await?["marks"].as_array().unwrap().len(),
        0
    );
    assert_eq!(
        journal.operation(command).await?.unwrap()["state"],
        "prepared"
    );
    let db = rusqlite::Connection::open(path)?;
    assert_eq!(
        db.query_row(
            "SELECT state FROM utterances WHERE id=?1",
            [input.to_string()],
            |r| r.get::<_, String>(0)
        )?,
        "sent"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM effects", [], |r| r.get::<_, i64>(0))?,
        0
    );
    journal
        .input_execution(input.to_string(), 1, "landed")
        .await?;
    let marks = window(&journal).await?["marks"].clone();
    assert_eq!(marks.as_array().unwrap().len(), 1);
    assert_eq!(marks[0]["inputId"], input.to_string());
    Ok(())
}

#[tokio::test]
async fn invalid_snapshot_projection_rolls_back_both_the_seed_and_its_clips() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    put(
        &journal,
        "cut",
        "session/nameChanged",
        json!({"name":"initial"}),
    )
    .await?;
    let mut read = snapshot("cut");
    let mut invalid = read["history"]["snapshot"]["state"]["items"][0].clone();
    invalid["itemId"] = json!("invalid");
    invalid["revision"] = json!(0);
    read["history"]["snapshot"]["state"]["items"]
        .as_array_mut()
        .unwrap()
        .push(invalid);
    assert!(seed(&journal, read).await.is_err());
    assert!(
        window(&journal).await?["clips"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .snapshot_state()
            .is_none()
    );
    let db = rusqlite::Connection::open(path)?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='history.snapshot'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

async fn open(journal: &Journal, value: Value, covered_gap: Option<i64>) -> Result<bool> {
    let receipt = journal
        .msp_raw(
            "track".into(),
            "session".into(),
            Stamp::now(),
            json!({"jsonrpc":"2.0","id":7,"result":value}).into(),
            None,
        )
        .await?;
    journal
        .opening_snapshot("track".into(), "session".into(), receipt, covered_gap)
        .await
}

#[tokio::test]
async fn an_opening_snapshot_establishes_an_unobserved_cursor_without_fake_events() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    assert!(open(&journal, snapshot("unknown"), None).await?);
    assert!(
        journal
            .view_seen("session".into(), "unknown".into())
            .await?
    );
    assert_eq!(
        journal.view_floor("session".into()).await?.as_deref(),
        Some("unknown")
    );
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("unknown")
    );
    put(
        &journal,
        "suffix",
        "session/nameChanged",
        json!({"name":"newer"}),
    )
    .await?;
    let state = journal
        .reconcile_local("session".into())
        .await?
        .view
        .snapshot_state();
    assert_eq!(state.as_ref().unwrap()["name"], "newer");
    drop(journal);
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    assert_eq!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .snapshot_state(),
        state
    );
    let db = rusqlite::Connection::open(path)?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM msp_applied WHERE cursor='unknown'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn opening_snapshot_covers_only_gaps_that_precede_the_resume() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let gap = |next: &str| json!({"jsonrpc":"2.0","method":"view/gap","params":{"sessionId":"session","after":"missing","next":next}});
    let first = journal
        .msp_raw(
            "track".into(),
            "session".into(),
            Stamp::now(),
            gap("old").into(),
            None,
        )
        .await?;
    let later = journal
        .msp_raw(
            "track".into(),
            "session".into(),
            Stamp::now(),
            gap("new").into(),
            None,
        )
        .await?;
    assert!(
        open(&journal, snapshot("unknown"), Some(first))
            .await
            .is_err()
    );
    assert_eq!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .unwrap()
            .through,
        later
    );
    assert!(open(&journal, snapshot("unknown"), Some(later)).await?);
    assert!(
        journal
            .view_gap("track".into(), "session".into())
            .await?
            .is_none()
    );
    journal
        .msp_raw(
            "track".into(),
            "session".into(),
            Stamp::now(),
            gap("later").into(),
            None,
        )
        .await?;
    assert_eq!(
        journal
            .view_cursor("track".into(), "session".into())
            .await?
            .as_deref(),
        Some("unknown")
    );
    Ok(())
}

#[tokio::test]
async fn opening_snapshot_retains_submit_identity_and_replays_its_report() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let turn = Uuid::now_v7();
    let command = Uuid::now_v7();
    journal
        .intent(
            &command.to_string(),
            "turn.start",
            json!({"sessionId":"session","commandId":command}),
        )
        .await?;
    let mut read = snapshot("unknown");
    read["history"]["snapshot"]["state"]["items"] = json!([
        {"itemId":"user","kind":"userMessage","revision":1,"status":"completed","recordedAt":"2026-11-05T00:00:00Z","text":"request","commandId":command,"turnId":turn},
        {"itemId":"report","kind":"agentMessage","revision":1,"status":"completed","recordedAt":"2026-11-05T00:00:00Z","text":"finished work","messagePhase":"finalAnswer","turnId":turn}
    ]);
    open(&journal, read, None).await?;
    let local = journal.reconcile_local("session".into()).await?;
    assert_eq!(local.view.turns[&turn].command, Some(command));
    assert_eq!(
        local.view.turns[&turn].state,
        intercat_daemon::session_view::TurnState::Historical
    );
    put(
        &journal,
        "completed",
        "turn/completed",
        json!({"turnId":turn,"terminal":"completed"}),
    )
    .await?;
    let frames = journal.reconcile_replay("session".into(), turn).await?;
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].0["params"]["item"]["text"], "finished work");
    assert!(frames[0].0["params"].get("viewCursor").is_none());
    Ok(())
}

#[tokio::test]
async fn snapshot_preserves_display_only_messages_and_completes_a_known_submit_identity()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let turn = Uuid::now_v7();
    let command = Uuid::now_v7();
    put(
        &journal,
        "ended",
        "turn/completed",
        json!({"turnId":turn,"terminal":"completed"}),
    )
    .await?;
    let mut read = snapshot("unobserved");
    read["history"]["snapshot"]["state"]["items"] = json!([
        {"itemId":"display","kind":"userMessage","revision":1,"status":"completed","recordedAt":"2026-11-05T00:00:00Z","text":"No submit identity"},
        {"itemId":"known","kind":"userMessage","revision":1,"status":"completed","recordedAt":"2026-11-05T00:00:00Z","text":"Known submit","commandId":command,"turnId":turn}
    ]);
    open(&journal, read, None).await?;
    let local = journal.reconcile_local("session".into()).await?;
    assert_eq!(local.view.items.len(), 2);
    assert_eq!(local.view.turns.len(), 1);
    assert_eq!(local.view.turns[&turn].command, Some(command));
    assert_eq!(
        local.view.turns[&turn].terminal.as_deref(),
        Some("completed")
    );
    Ok(())
}

#[tokio::test]
async fn a_partial_snapshot_report_cannot_replay_as_a_completed_report() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let turn = Uuid::now_v7();
    put(&journal,"old","item/completed",json!({"item":{"itemId":"report","turnId":turn,"kind":"agentMessage","revision":1,"status":"completed","text":"superseded","messagePhase":"finalAnswer"}})).await?;
    let mut read = snapshot("unobserved");
    read["history"]["snapshot"]["state"]["items"] = json!([
        {"itemId":"report","kind":"agentMessage","revision":2,"status":"inProgress","recordedAt":"2026-11-05T00:00:00Z","text":"still writing","messagePhase":"finalAnswer","turnId":turn}
    ]);
    open(&journal, read, None).await?;
    assert!(
        journal
            .reconcile_replay("session".into(), turn)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn last_turn_restatement_does_not_count_a_snapshot_turn_twice() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let turn = Uuid::now_v7();
    let mut read = snapshot("unobserved");
    read["history"]["snapshot"]["state"]["turnCount"] = json!(1);
    read["history"]["snapshot"]["state"]["items"] = json!([
        {"itemId":"user","kind":"userMessage","revision":1,"status":"completed","recordedAt":"2026-11-05T00:00:00Z","text":"request","commandId":turn,"turnId":turn}
    ]);
    open(&journal, read, None).await?;
    journal.try_msp("track".into(),"session".into(),Stamp::now(),json!({"jsonrpc":"2.0","method":"turn/completed","params":{"sessionId":"session","turnId":turn,"terminal":"completed"},"_intercatRecovered":true}),None,None)?.await??;
    let local = journal.reconcile_local("session".into()).await?;
    assert_eq!(local.view.snapshot_state().unwrap()["turnCount"], 1);
    assert_eq!(
        local.view.turns[&turn].terminal.as_deref(),
        Some("completed")
    );
    let next = Uuid::now_v7();
    put(
        &journal,
        "next-start",
        "turn/started",
        json!({"turnId":next,"commandId":next}),
    )
    .await?;
    put(
        &journal,
        "next-end",
        "turn/completed",
        json!({"turnId":next,"terminal":"completed"}),
    )
    .await?;
    assert_eq!(
        journal
            .reconcile_local("session".into())
            .await?
            .view
            .snapshot_state()
            .unwrap()["turnCount"],
        2
    );
    Ok(())
}
