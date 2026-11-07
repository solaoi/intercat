use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Target},
    descriptions::{Description, Finish},
    editor_log::Query,
    journal::Journal,
    runtime::WorkerState,
    secrets::Key,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn classified(kind: Kind, track: Option<String>) -> Outcome {
    Outcome::Classified(Decision {
        kind,
        track_id: track,
        boundary: None,
        command: None,
        confidence: 1.,
        inference_ms: 1.,
        timed_out: false,
        scores: json!({}),
    })
}
async fn fixture() -> Result<(tempfile::TempDir, Journal, String, String)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let input = stage(&journal, Kind::Fork, None).await?;
    let track = journal.snapshot().await?["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    Ok((root, journal, input, track))
}
async fn stage(journal: &Journal, kind: Kind, track: Option<String>) -> Result<String> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(
            id,
            "Dummy arithmetic tests".into(),
            Target::None,
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(input.id.clone(), input.revision, classified(kind, track))
        .await?;
    Ok(input.id)
}
async fn entries(journal: &Journal, track: &str) -> Result<Value> {
    journal
        .editor_log(Query {
            track_id: track.into(),
            before: None,
        })
        .await
}
fn description() -> Finish {
    Finish::Complete(Description {
        title: "Arithmetic tests".into(),
        line: "New track for A.".into(),
        detail: "Created a track for the dummy arithmetic tests. Work has not started.".into(),
    })
}
fn worker(track: &str) -> WorkerState {
    WorkerState {
        track_id: track.into(),
        phase: "ready".into(),
        session_id: Some("dummy-session".into()),
        secret_revision: None,
        identity: None,
        detail: None,
        active_turn: None,
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    }
}

#[tokio::test]
async fn fork_is_recorded_before_generation_and_keeps_its_original_time() -> Result<()> {
    let (_root, journal, input, track) = fixture().await?;
    let before = entries(&journal, &track).await?;
    assert_eq!(before["entries"][0]["line"], "New track for A.");
    let ticket = journal.claim_description().await?.unwrap();
    journal
        .finish_description(ticket.clone(), description())
        .await?;
    let after = entries(&journal, &track).await?;
    assert_eq!(after["entries"].as_array().unwrap().len(), 1);
    assert_eq!(
        after["entries"][0]["createdAt"],
        before["entries"][0]["createdAt"]
    );
    assert_eq!(after["entries"][0]["inputId"], input);
    assert_eq!(
        after["entries"][0]["detail"],
        "Created a track for the dummy arithmetic tests. Work has not started."
    );
    journal.finish_description(ticket, description()).await?;
    assert_eq!(entries(&journal, &track).await?, after);
    Ok(())
}

#[tokio::test]
async fn take_back_and_restart_do_not_erase_the_explanation() -> Result<()> {
    let (root, journal, input, track) = fixture().await?;
    let ticket = journal.claim_description().await?.unwrap();
    journal.finish_description(ticket, description()).await?;
    let before = entries(&journal, &track).await?;
    journal.cut_input(Uuid::now_v7(), input).await?;
    drop(journal);
    let restarted = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(entries(&restarted, &track).await?, before);
    Ok(())
}

#[tokio::test]
async fn stale_generation_cannot_rewrite_a_recorded_fact() -> Result<()> {
    let (_root, journal, input, track) = fixture().await?;
    let ticket = journal.claim_description().await?.unwrap();
    let before = entries(&journal, &track).await?;
    journal.cut_input(Uuid::now_v7(), input).await?;
    journal.finish_description(ticket, description()).await?;
    assert_eq!(entries(&journal, &track).await?, before);
    Ok(())
}

#[tokio::test]
async fn routine_descriptions_are_kept_in_the_log_without_speaking() -> Result<()> {
    let (_root, journal, _, track) = fixture().await?;
    let ticket = journal.claim_description().await?.unwrap();
    journal.finish_description(ticket, description()).await?;
    let input = stage(&journal, Kind::Extend, Some(track.clone())).await?;
    let ticket = journal.claim_description().await?.unwrap();
    journal
        .finish_description(
            ticket,
            Finish::Complete(Description {
                title: "More tests".into(),
                line: "Routine placement".into(),
                detail: "Added tests after the current turn.".into(),
            }),
        )
        .await?;
    let log = entries(&journal, &track).await?;
    assert_eq!(log["entries"][0]["inputId"], input);
    assert_eq!(log["entries"][0]["scene"], "interpretation");
    assert_eq!(log["entries"][0]["expiresAt"], 0);
    assert!(
        journal.snapshot().await?["editorEntries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["inputId"] != input)
    );
    Ok(())
}

#[tokio::test]
async fn persistent_worker_notices_end_on_recovery_without_replaying_unchanged_states() -> Result<()>
{
    let (_root, journal, _, track) = fixture().await?;
    let mut state = worker(&track);
    journal.worker_state(state.clone()).await?;
    state.phase = "reconnecting".into();
    journal.worker_state(state.clone()).await?;
    let once = entries(&journal, &track).await?;
    journal.worker_state(state.clone()).await?;
    assert_eq!(entries(&journal, &track).await?, once);
    assert!(once["entries"][0]["expiresAt"].is_null());
    state.phase = "lost".into();
    journal.worker_state(state.clone()).await?;
    let lost = entries(&journal, &track).await?;
    assert_eq!(lost["entries"][0]["scene"], "lost");
    assert!(lost["entries"][0]["expiresAt"].is_null());
    assert!(lost["entries"][1]["expiresAt"].as_i64().is_some());
    state.phase = "ready".into();
    journal.worker_state(state).await?;
    assert!(
        journal.snapshot().await?["editorEntries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["scene"] != "lost" && entry["scene"] != "reconnecting")
    );
    assert_eq!(
        entries(&journal, &track).await?["entries"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn log_pages_are_complete_track_scoped_and_reject_invalid_cursors() -> Result<()> {
    let (_root, journal, _, track) = fixture().await?;
    for _ in 0..30 {
        let mut state = worker(&track);
        state.phase = "lost".into();
        journal.worker_state(state.clone()).await?;
        state.phase = "ready".into();
        journal.worker_state(state).await?;
    }
    for _ in 0..24 {
        let mut state = worker(&track);
        state.phase = "reconnecting".into();
        journal.worker_state(state.clone()).await?;
        state.phase = "ready".into();
        journal.worker_state(state).await?;
    }
    let page = entries(&journal, &track).await?;
    assert_eq!(page["entries"].as_array().unwrap().len(), 50);
    let before = page["before"].as_i64().unwrap();
    let next = journal
        .editor_log(Query {
            track_id: track.clone(),
            before: Some(before),
        })
        .await?;
    assert_eq!(next["entries"].as_array().unwrap().len(), 5);
    assert!(next["before"].is_null());
    assert!(
        next["entries"]
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["id"].as_i64().unwrap() < before)
    );
    assert_eq!(
        entries(&journal, "other-track").await?["entries"],
        json!([])
    );
    assert!(
        journal
            .editor_log(Query {
                track_id: track,
                before: Some(0)
            })
            .await
            .is_err()
    );
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn a_full_database_cannot_publish_a_fork_without_its_log() -> Result<()> {
    use intercat_daemon::fault_injection::Request as Fault;
    let (_root, journal, _, track) = fixture().await?;
    let before = entries(&journal, &track).await?;
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, "Another dummy track".into(), Target::New, None, vec![])
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal.test_fault(Fault::Fill).await?;
    assert!(
        journal
            .apply_input(
                input.id.clone(),
                input.revision,
                classified(Kind::Fork, None)
            )
            .await
            .is_err()
    );
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(entries(&journal, &track).await?, before);
    journal.test_fault(Fault::Restore).await?;
    journal
        .apply_input(input.id, input.revision, classified(Kind::Fork, None))
        .await?;
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        journal.snapshot().await?["editorEntries"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn a_burst_keeps_the_status_bounded_without_losing_track_logs() -> Result<()> {
    let (_root, journal, _, track) = fixture().await?;
    for _ in 0..40 {
        stage(&journal, Kind::Fork, None).await?;
    }
    assert_eq!(
        journal.snapshot().await?["editorEntries"]
            .as_array()
            .unwrap()
            .len(),
        32
    );
    assert_eq!(
        entries(&journal, &track).await?["entries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    Ok(())
}
