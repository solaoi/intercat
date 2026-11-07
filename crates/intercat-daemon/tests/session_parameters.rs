use intercat_daemon::{Result, clock::Stamp, journal::Journal, runtime::WorkerState, secrets::Key};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use uuid::Uuid;

async fn worker(journal: &Journal, track: &str, session: &str, turn: Option<Uuid>) -> Result<()> {
    journal
        .worker_state(WorkerState {
            track_id: track.into(),
            phase: "working".into(),
            session_id: Some(session.into()),
            secret_revision: None,
            identity: None,
            detail: None,
            active_turn: turn,
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        })
        .await
}
async fn put(
    journal: &Journal,
    track: &str,
    session: &str,
    cursor: &str,
    method: &str,
    mut params: Value,
) -> Result<()> {
    params["sessionId"] = json!(session);
    params["viewCursor"] = json!(cursor);
    params["sourceRange"] = json!({});
    journal
        .try_msp(
            track.into(),
            session.into(),
            Stamp::now(),
            json!({"jsonrpc":"2.0","method":method,"params":params}),
            None,
            None,
        )?
        .await??;
    Ok(())
}
fn model(snapshot: &Value, track: &str) -> Value {
    snapshot["workers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["trackId"] == track)
        .unwrap()["model"]
        .clone()
}
fn tracks(db: &Connection) -> Result<()> {
    db.execute_batch(
        "INSERT INTO tracks VALUES('A','A','First','working'),('B','B','Second','working');",
    )?;
    Ok(())
}
fn snapshot(cursor: &str, model: Value, effort: Value) -> Value {
    json!({"session":{"sessionId":"a"},"viewCursor":cursor,"history":{"mode":"snapshot","snapshot":{
        "schemaVersion":1,"viewCursor":cursor,"state":{
            "items":[],"activeTurn":null,"queuedTurns":[],"pendingApprovals":[],"pendingUserInputs":[],
            "approvalMode":{"mode":"promptUnmatched","source":"startup","lastCommandId":null},
            "effectiveModel":model,"reasoningEffort":effort,"goal":null,"todoList":null,"branch":null,"name":"Test",
            "tokenUsage":{"promptTokens":0,"outputTokens":0,"totalTokens":0},"turnCount":0
    }}}})
}
async fn seed(journal: &Journal, value: Value, opening: bool) -> Result<()> {
    let source = journal
        .msp_raw(
            "A".into(),
            "a".into(),
            Stamp::now(),
            json!({"jsonrpc":"2.0","id":1,"result":value}).into(),
            None,
        )
        .await?;
    if opening {
        assert!(
            journal
                .opening_snapshot("A".into(), "a".into(), source, None)
                .await?
        );
        Ok(())
    } else {
        journal.view_snapshot("A".into(), "a".into(), source).await
    }
}

#[tokio::test]
async fn observed_settings_are_isolated_durable_and_cleared_for_a_new_session() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    tracks(&Connection::open(&path)?)?;
    worker(&journal, "A", "a", None).await?;
    worker(&journal, "B", "b", None).await?;
    assert!(model(&journal.snapshot().await?, "A").is_null());
    put(
        &journal,
        "A",
        "a",
        "a-model",
        "session/modelChanged",
        json!({"modelId":"muse-spark-1.3-contributor","providerId":"meta","source":"startup"}),
    )
    .await?;
    assert!(model(&journal.snapshot().await?, "A")["reasoningEffort"].is_null());
    put(
        &journal,
        "A",
        "a",
        "a-effort",
        "session/reasoningEffortChanged",
        json!({"reasoningEffort":"high","source":"startup"}),
    )
    .await?;
    put(
        &journal,
        "B",
        "b",
        "b-model",
        "session/modelChanged",
        json!({"modelId":"future-model","providerId":null,"source":"startup"}),
    )
    .await?;
    put(
        &journal,
        "B",
        "b",
        "b-effort",
        "session/reasoningEffortChanged",
        json!({"reasoningEffort":"low","source":"startup"}),
    )
    .await?;
    let saved = journal.snapshot().await?;
    assert_eq!(
        model(&saved, "A"),
        json!({"modelId":"muse-spark-1.3-contributor","providerId":"meta","reasoningEffort":"high"})
    );
    assert_eq!(
        model(&saved, "B"),
        json!({"modelId":"future-model","providerId":null,"reasoningEffort":"low"})
    );
    drop(journal);
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    assert_eq!(model(&journal.snapshot().await?, "A"), model(&saved, "A"));
    worker(&journal, "A", "new-a", None).await?;
    assert!(model(&journal.snapshot().await?, "A").is_null());
    assert_eq!(model(&journal.snapshot().await?, "B"), model(&saved, "B"));
    Ok(())
}

#[tokio::test]
async fn duplicate_delivery_is_ignored_and_recovery_uses_the_canonical_fact() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    tracks(&Connection::open(&path)?)?;
    worker(&journal, "A", "a", None).await?;
    put(
        &journal,
        "A",
        "a",
        "old",
        "session/modelChanged",
        json!({"modelId":"old"}),
    )
    .await?;
    put(
        &journal,
        "A",
        "a",
        "new",
        "session/modelChanged",
        json!({"modelId":"new"}),
    )
    .await?;
    put(
        &journal,
        "A",
        "a",
        "old",
        "session/modelChanged",
        json!({"modelId":"forged"}),
    )
    .await?;
    assert_eq!(model(&journal.snapshot().await?, "A")["modelId"], "new");
    journal.try_msp("A".into(),"a".into(),Stamp::now(),json!({"_intercatRecovered":true,"method":"session/modelChanged","params":{"sessionId":"a","viewCursor":"old","sourceRange":{},"modelId":"forged"}}),None,None)?.await??;
    assert_eq!(model(&journal.snapshot().await?, "A")["modelId"], "old");
    drop(journal);
    Connection::open(&path)?.execute("DELETE FROM session_parameters", [])?;
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    assert_eq!(model(&journal.snapshot().await?, "A")["modelId"], "old");
    Ok(())
}

#[tokio::test]
async fn snapshot_splices_rebuild_the_same_settings_and_opening_nulls_clear_them() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    tracks(&Connection::open(&path)?)?;
    worker(&journal, "A", "a", None).await?;
    put(
        &journal,
        "A",
        "a",
        "cut",
        "session/nameChanged",
        json!({"name":"old"}),
    )
    .await?;
    put(
        &journal,
        "A",
        "a",
        "later",
        "session/modelChanged",
        json!({"modelId":"newer-model","providerId":"meta"}),
    )
    .await?;
    seed(
        &journal,
        snapshot(
            "cut",
            json!({"modelId":"snapshot-model","providerId":null,"source":"startup"}),
            json!({"reasoningEffort":"high","source":"startup"}),
        ),
        false,
    )
    .await?;
    let expected = json!({"modelId":"newer-model","providerId":"meta","reasoningEffort":"high"});
    assert_eq!(model(&journal.snapshot().await?, "A"), expected);
    drop(journal);
    Connection::open(&path)?.execute("DELETE FROM session_parameters", [])?;
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    assert_eq!(model(&journal.snapshot().await?, "A"), expected);
    seed(
        &journal,
        snapshot("opening", Value::Null, Value::Null),
        true,
    )
    .await?;
    assert!(model(&journal.snapshot().await?, "A").is_null());
    drop(journal);
    Connection::open(&path)?.execute("DELETE FROM session_parameters", [])?;
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    assert!(model(&journal.snapshot().await?, "A").is_null());
    Ok(())
}

#[tokio::test]
async fn settings_commit_with_the_event_and_retry_after_a_write_failure() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let db = Connection::open(&path)?;
    tracks(&db)?;
    worker(&journal, "A", "a", None).await?;
    db.execute_batch("CREATE TRIGGER block_parameters BEFORE INSERT ON session_parameters BEGIN SELECT RAISE(ABORT,'injected settings failure'); END;")?;
    assert!(
        put(
            &journal,
            "A",
            "a",
            "model",
            "session/modelChanged",
            json!({"modelId":"observed"})
        )
        .await
        .is_err()
    );
    assert!(model(&journal.snapshot().await?, "A").is_null());
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='msp.applied'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='msp.received'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    db.execute_batch("DROP TRIGGER block_parameters;")?;
    put(
        &journal,
        "A",
        "a",
        "model",
        "session/modelChanged",
        json!({"modelId":"observed"}),
    )
    .await?;
    assert_eq!(
        model(&journal.snapshot().await?, "A")["modelId"],
        "observed"
    );
    Ok(())
}

#[tokio::test]
async fn a_recorded_turn_override_wins_over_the_session_default_and_reaches_clip_details()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let db = Connection::open(&path)?;
    tracks(&db)?;
    let turn = Uuid::now_v7();
    worker(&journal, "A", "a", Some(turn)).await?;
    put(
        &journal,
        "A",
        "a",
        "effort",
        "session/reasoningEffortChanged",
        json!({"reasoningEffort":"high"}),
    )
    .await?;
    journal
        .intent(
            &turn.to_string(),
            "turn.start",
            json!({"sessionId":"a","commandId":turn,"reasoningEffort":"minimal","input":[]}),
        )
        .await?;
    put(
        &journal,
        "A",
        "a",
        "started",
        "turn/started",
        json!({"turnId":turn,"commandId":turn}),
    )
    .await?;
    put(&journal, "A", "a", "item", "item/started", json!({"item":{"itemId":"reply","turnId":turn,"kind":"agentMessage","revision":1,"status":"inProgress","text":"Working"}})).await?;
    assert_eq!(
        model(&journal.snapshot().await?, "A")["reasoningEffort"],
        "minimal"
    );
    let clip: String = db.query_row("SELECT id FROM timeline_clips WHERE track='A'", [], |r| {
        r.get(0)
    })?;
    assert_eq!(
        journal.clip_detail(clip).await?["model"]["reasoningEffort"],
        "minimal"
    );
    // Another session cannot pick up this command's override.
    worker(&journal, "B", "b", Some(turn)).await?;
    assert!(model(&journal.snapshot().await?, "B").is_null());
    db.execute(
        "INSERT INTO operations VALUES(?1,'turn.start',?2,'prepared')",
        params![
            Uuid::now_v7().to_string(),
            json!({"sessionId":"a","reasoningEffort":"max"}).to_string()
        ],
    )?;
    assert_eq!(
        model(&journal.snapshot().await?, "A")["reasoningEffort"],
        "minimal"
    );
    worker(&journal, "A", "a", None).await?;
    assert_eq!(
        model(&journal.snapshot().await?, "A")["reasoningEffort"],
        "high"
    );
    Ok(())
}
