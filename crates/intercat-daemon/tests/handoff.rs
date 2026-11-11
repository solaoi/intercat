use intercat_daemon::{
    Result,
    handoff::{Change, Fields, Identity, Next, Source, State},
    journal::Journal,
    secrets::Key,
};
use uuid::Uuid;
async fn fixture() -> Result<(tempfile::TempDir, Journal, Uuid)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    Ok((root, journal, Uuid::now_v7()))
}
fn source(id: Uuid, context: serde_json::Value) -> Source {
    Source {
        id,
        identity: Identity {
            track_id: Uuid::new_v5(&id, b"source-track").to_string(),
            session_id: Uuid::new_v5(&id, b"source-session").to_string(),
        },
        context,
    }
}
fn fields() -> Fields {
    Fields {
        purpose: "Preserve saved changes".into(),
        constraints: vec!["Keep the latest correction".into()],
        unresolved: vec![],
        changed_files: vec![],
        completed: vec![],
        unverified: vec!["No new tests ran".into()],
        next_checks: vec!["Inspect current files".into()],
    }
}

#[tokio::test]
async fn summary_parts_and_reduction_resume_without_repeating_completed_work() -> Result<()> {
    let (root, j, id) = fixture().await?;
    j.summary_plan(source(
        id,
        serde_json::json!({"history":"Keep all source bytes. ".repeat(7_000)}),
    ))
    .await?;
    let Next::Task(first) = j.summary_next(id).await? else {
        panic!("expected first task")
    };
    let same = match j.summary_next(id).await? {
        Next::Task(t) => t,
        _ => panic!(),
    };
    assert_eq!(same.id, first.id);
    j.summary_change(Change::Started(first.id)).await?;
    j.summary_change(Change::Running(first.id)).await?;
    drop(j);
    let j = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let Next::Task(resumed) = j.summary_next(id).await? else {
        panic!()
    };
    assert_eq!(resumed.id, first.id);
    assert_eq!(resumed.state, State::Running);
    j.summary_change(Change::Complete(first.id, fields()))
        .await?;
    let mut completed = 1;
    let mut reduced = false;
    loop {
        match j.summary_next(id).await? {
            Next::Task(t) => {
                assert_ne!(t.id, first.id);
                reduced |= t.level > 0;
                j.summary_change(Change::Started(t.id)).await?;
                j.summary_change(Change::Complete(t.id, fields())).await?;
                completed += 1;
            }
            Next::Complete(doc) => {
                assert!(reduced);
                assert!(completed >= 4);
                assert!(doc.source_addresses[0].starts_with("track:"));
                break;
            }
        }
    }
    assert!(matches!(j.summary_next(id).await?, Next::Complete(_)));
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='handoff.summary' AND state='complete'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        completed
    );
    Ok(())
}
#[tokio::test]
async fn only_a_known_failed_summary_gets_a_new_attempt() -> Result<()> {
    let (_root, j, id) = fixture().await?;
    j.summary_plan(source(id, serde_json::json!({"text":"source"})))
        .await?;
    let Next::Task(first) = j.summary_next(id).await? else {
        panic!()
    };
    assert!(
        j.summary_change(Change::Complete(first.id, fields()))
            .await
            .is_err()
    );
    j.summary_change(Change::Started(first.id)).await?;
    j.summary_change(Change::Running(first.id)).await?;
    let Next::Task(same) = j.summary_next(id).await? else {
        panic!()
    };
    assert_eq!(same.id, first.id);
    j.summary_change(Change::Failed(first.id, "Invalid JSON".into()))
        .await?;
    let Next::Task(retry) = j.summary_next(id).await? else {
        panic!()
    };
    assert_ne!(retry.id, first.id);
    assert_ne!(retry.session_id, first.session_id);
    assert_eq!(retry.input, first.input);
    assert_eq!(retry.attempt, 1);
    Ok(())
}
#[tokio::test]
async fn compact_keeps_the_current_request_tools_questions_and_original_addresses() -> Result<()> {
    let input = serde_json::json!({"unfinishedTurn":"current","instructions":[{"message":{"turnId":"past","text":"large old request"}},{"message":{"turnId":"current","text":"Keep the current request"}}],"completedTools":[{"address":"clip-1","tool":{"itemId":"tool-1","tool":"bash","status":"completed","visibleOutput":"output".repeat(10000)}}],"lastClips":[],"unresolvedQuestions":[{"text":"Which option?"}]});
    let doc = intercat_daemon::handoff::Document {
        fields: fields(),
        source_addresses: vec!["clip-1".into()],
    };
    let result = intercat_daemon::handoff::compact(&input, doc)?;
    assert_eq!(result["instructions"].as_array().unwrap().len(), 1);
    assert_eq!(result["completedTools"][0]["address"], "clip-1");
    assert!(result["completedTools"][0].get("visibleOutput").is_none());
    assert_eq!(result["unresolvedQuestions"][0]["text"], "Which option?");
    Ok(())
}

#[tokio::test]
async fn an_empty_final_purpose_can_be_retried_without_reusing_the_invalid_output() -> Result<()> {
    let (_root, j, id) = fixture().await?;
    j.summary_plan(source(
        id,
        serde_json::json!({"purpose":"Keep user constraints"}),
    ))
    .await?;
    let Next::Task(t) = j.summary_next(id).await? else {
        panic!()
    };
    j.summary_change(Change::Started(t.id)).await?;
    let mut empty = fields();
    empty.purpose.clear();
    j.summary_change(Change::Complete(t.id, empty)).await?;
    assert!(j.summary_next(id).await.is_err());
    let Next::Task(retry) = j.summary_next(id).await? else {
        panic!()
    };
    assert_ne!(retry.id, t.id);
    assert_eq!(retry.input, t.input);
    Ok(())
}

#[tokio::test]
async fn a_handoff_binds_one_source_and_keeps_original_addresses_after_restart() -> Result<()> {
    let (root, journal, id) = fixture().await?;
    let captured = source(
        id,
        serde_json::json!({"instructions":[{"address":"own-clip-1","message":{"text":"Latest correction: do not publish","address":"forged-address"}}],"snapshotChanges":{"address":"refs/intercat/own-checkpoint","changes":[]}}),
    );
    journal.summary_plan(captured.clone()).await?;
    journal.summary_plan(captured.clone()).await?;
    let Next::Task(first) = journal.summary_next(id).await? else {
        panic!("Own summary task missing");
    };
    assert_eq!(first.handoff_id, id);
    assert_eq!(first.source, captured.identity);
    for mutation in 0..3 {
        let mut changed = captured.clone();
        match mutation {
            0 => changed.identity.track_id = Uuid::now_v7().to_string(),
            1 => changed.identity.session_id = Uuid::now_v7().to_string(),
            _ => changed.context["instructions"][0]["message"]["text"] = "changed input".into(),
        };
        assert!(journal.summary_plan(changed).await.is_err());
    }
    journal.summary_change(Change::Started(first.id)).await?;
    journal
        .summary_change(Change::Complete(first.id, fields()))
        .await?;
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let Next::Complete(document) = journal.summary_next(id).await? else {
        panic!("Own summary did not complete");
    };
    assert_eq!(
        document.source_addresses,
        vec![
            "own-clip-1".to_owned(),
            "refs/intercat/own-checkpoint".to_owned(),
            format!("track:{}", captured.identity.track_id)
        ]
    );
    assert_eq!(journal.summary_tasks(id).await?, vec![first.id]);
    Ok(())
}
