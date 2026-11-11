use intercat_daemon::{
    Result,
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    secrets::Key,
    timeline::Window,
};
use rusqlite::{Connection, params};
use serde_json::json;
use uuid::Uuid;

fn window(since: Option<i64>) -> Window {
    Window {
        tracks: vec![],
        start: 0,
        end: i64::MAX,
        since,
    }
}
fn outcome(kind: Kind) -> Outcome {
    Outcome::Classified(Decision {
        kind,
        track_id: None,
        boundary: Some("now".into()),
        command: None,
        confidence: 1.0,
        inference_ms: 1.0,
        timed_out: false,
        scores: json!({}),
    })
}
async fn stage(journal: &Journal, text: &str) -> Result<String> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, text.into(), Target::None, None, vec![])
        .await?;
    Ok(id.to_string())
}

#[tokio::test]
async fn unassigned_asks_and_parks_keep_the_creation_position_and_incremental_removals()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let id = stage(&journal, "A later idea").await?;
    assert_eq!(
        journal.timeline_window(window(None)).await?["marks"],
        json!([])
    );
    let pending = journal.claim_input().await?.unwrap();
    journal
        .apply_input(id.clone(), pending.revision, outcome(Kind::Park))
        .await?;
    let first = journal.timeline_window(window(None)).await?;
    let mark = &first["marks"][0];
    assert_eq!(mark["id"], format!("input/{id}"));
    assert_eq!(mark["operation"], "park");
    assert!(mark["trackId"].is_null());
    let db = Connection::open(&path)?;
    let creation:i64 = db.query_row("SELECT position FROM events WHERE kind='utterance.staged' AND json_extract(payload,'$.id')=?1",[&id],|r|r.get(0))?;
    assert_eq!(mark["at"], creation);
    assert_eq!(
        journal
            .timeline_window(window(first["revision"].as_i64()))
            .await?["marks"],
        json!([])
    );
    journal.cut_input(Uuid::now_v7(), id.clone()).await?;
    let cut = journal
        .timeline_window(window(first["revision"].as_i64()))
        .await?;
    assert_eq!(cut["removedMarks"], json!([format!("input/{id}")]));
    let ask = stage(&journal, "Where should this go?").await?;
    let pending = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            ask.clone(),
            pending.revision,
            Outcome::Asked("Choose a track".into()),
        )
        .await?;
    let asked = journal.timeline_window(window(None)).await?;
    assert_eq!(asked["marks"][0]["operation"], "ask");
    assert_eq!(asked["marks"][0]["inputId"], ask);
    assert!(asked["marks"][0]["trackId"].is_null());
    let plan = stage(&journal, "Break this into a plan").await?;
    let pending = journal.claim_input().await?.unwrap();
    journal
        .apply_input(plan.clone(), pending.revision, outcome(Kind::Plan))
        .await?;
    let planned = journal.timeline_window(window(None)).await?;
    assert_eq!(
        planned["marks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|mark| mark["inputId"] == plan)
            .unwrap()["operation"],
        "plan"
    );
    Ok(())
}

#[tokio::test]
async fn a_worker_receipt_uses_one_original_statement_mark_and_keeps_the_saved_decision()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let id = stage(&journal, "Start a new cache task").await?;
    let pending = journal.claim_input().await?.unwrap();
    journal
        .apply_input(id.clone(), pending.revision, outcome(Kind::Fork))
        .await?;
    let word = journal.read_utterance(id.clone()).await?;
    assert_eq!(word["decision"]["kind"], "fork");
    assert_eq!(
        journal.timeline_window(window(None)).await?["marks"],
        json!([])
    );
    let track = word["decision"]["trackId"].as_str().unwrap().to_owned();
    let command = Uuid::now_v7();
    journal
        .reserve_turn(
            command,
            id.clone(),
            pending.revision,
            track.clone(),
            json!({"sessionId":"session","commandId":command}),
            None,
        )
        .await?;
    let at = Stamp::now();
    journal.try_msp(track.clone(),"session".into(),at.clone(),json!({"method":"item/updated","params":{"sessionId":"session","item":{"itemId":"user","commandId":command,"turnId":command,"kind":"userMessage","text":"Start a new cache task","revision":1,"status":"completed"}}}),None,None)?.await??;
    let marks = journal.timeline_window(window(None)).await?["marks"].clone();
    assert_eq!(marks.as_array().unwrap().len(), 1);
    assert_eq!(marks[0]["id"], format!("input/{id}"));
    assert_eq!(marks[0]["operation"], "fork");
    assert_eq!(marks[0]["trackId"], track);
    assert!(marks[0]["at"].as_i64().unwrap() <= at.timeline_ms);
    let loaded = journal.read_utterance(id.clone()).await?;
    assert_eq!(loaded["state"], "landed");
    assert!(loaded["decision"]["scores"].is_null());
    let db = Connection::open(&path)?;
    db.execute(
        "UPDATE utterances SET text='Corrected title' WHERE id=?1",
        [&id],
    )?;
    assert_eq!(
        journal.timeline_window(window(None)).await?["marks"][0]["at"],
        marks[0]["at"]
    );
    drop(journal);
    db.execute("DELETE FROM input_timeline", [])?;
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    assert_eq!(
        journal.timeline_window(window(None)).await?["marks"][0]["at"],
        marks[0]["at"]
    );
    assert_eq!(journal.read_utterance(id).await?["text"], "Corrected title");
    Ok(())
}

#[tokio::test]
async fn old_statements_remain_readable_beyond_the_status_limit_and_track_viewport() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let mut db = Connection::open(&path)?;
    let tx = db.transaction()?;
    let ids: Vec<_> = (0..110).map(|_| Uuid::now_v7().to_string()).collect();
    for (index, id) in ids.iter().enumerate() {
        tx.execute("INSERT INTO utterances(id,text,created_at,target,state,revision,decision) VALUES(?1,?2,?3,'{\"mode\":\"none\"}','landed',1,?4)",params![id,format!("Earlier request {index}"),index as i64,json!({"kind":"schedule","trackId":"hidden","boundary":"after-turn","command":null,"confidence":1.0,"inferenceMs":1.0,"timedOut":false,"scores":{}}).to_string()])?;
        tx.execute("INSERT INTO events(ts,mono,boot,position,kind,payload) VALUES(?1,0,'test',?1,'utterance.staged',?2)",params![1000+index as i64,json!({"id":id}).to_string()])?;
    }
    tx.commit()?;
    assert_eq!(
        journal.snapshot().await?["utterances"]
            .as_array()
            .unwrap()
            .len(),
        100
    );
    assert_eq!(
        journal.read_utterance(ids[0].clone()).await?["text"],
        "Earlier request 0"
    );
    let result = journal
        .timeline_window(Window {
            tracks: vec!["visible".into()],
            start: 1000,
            end: 1010,
            since: None,
        })
        .await?;
    assert_eq!(result["marks"].as_array().unwrap().len(), 11);
    assert_eq!(result["marks"][0]["trackId"], "hidden");
    db.execute("DELETE FROM utterances WHERE id=?1", [&ids[0]])?;
    assert!(journal.read_utterance(ids[0].clone()).await.is_err());
    let updated = journal
        .timeline_window(Window {
            tracks: vec![],
            start: 1000,
            end: 1010,
            since: result["revision"].as_i64(),
        })
        .await?;
    assert_eq!(
        updated["removedMarks"],
        json!([format!("input/{}", ids[0])])
    );
    Ok(())
}

#[tokio::test]
async fn a_failed_mark_write_rolls_back_the_statement_and_retry_is_atomic() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let db = Connection::open(&path)?;
    db.execute_batch("CREATE TRIGGER refuse_mark BEFORE INSERT ON input_timeline BEGIN SELECT RAISE(ABORT,'injected mark failure'); END;")?;
    let id = Uuid::now_v7();
    assert!(
        journal
            .stage_utterance(id, "One request".into(), Target::None, None, vec![])
            .await
            .is_err()
    );
    assert_eq!(journal.snapshot().await?["utterances"], json!([]));
    assert_eq!(
        db.query_row("SELECT count(*) FROM operations", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    db.execute_batch("DROP TRIGGER refuse_mark;")?;
    journal
        .stage_utterance(id, "One request".into(), Target::None, None, vec![])
        .await?;
    journal
        .stage_utterance(id, "One request".into(), Target::None, None, vec![])
        .await?;
    assert_eq!(
        db.query_row("SELECT count(*) FROM input_timeline", [], |r| r
            .get::<_, i64>(0))?,
        1
    );
    Ok(())
}
