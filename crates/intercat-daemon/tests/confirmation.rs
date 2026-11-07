use intercat_daemon::{
    Result,
    confirmation::{Accept, Action, Change, Policy},
    decision::{Decision, Kind, Outcome, Resolution, Target},
    edit_hold::{self, Boundary, Edit},
    input_feedback::Misread,
    journal::Journal,
    secrets::Key,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    Ok((root, journal))
}

fn decision() -> Decision {
    Decision {
        kind: Kind::Fork,
        track_id: None,
        boundary: Some("now".into()),
        command: None,
        confidence: 0.95,
        inference_ms: 1.0,
        timed_out: false,
        scores: json!({}),
    }
}

async fn stage(journal: &Journal) -> Result<Uuid> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, "A request 日本語".into(), Target::None, None, vec![])
        .await?;
    Ok(id)
}

async fn classify(journal: &Journal, id: Uuid, value: Decision) -> Result<Value> {
    let pending = journal.claim_input().await?.unwrap();
    assert_eq!(pending.id, id.to_string());
    journal
        .apply_input(pending.id, pending.revision, Outcome::Classified(value))
        .await?;
    journal.read_utterance(id.to_string()).await
}

async fn policy(journal: &Journal, value: Policy) -> Result<()> {
    let status = journal.snapshot().await?;
    journal
        .change_confirmation(Change {
            op_id: Uuid::now_v7(),
            revision: status["confirmation"]["revision"].as_u64().unwrap(),
            action: Action::Policy { policy: value },
        })
        .await?;
    Ok(())
}

fn accept(word: &Value) -> Accept {
    Accept {
        op_id: Uuid::now_v7(),
        utterance_id: word["id"].as_str().unwrap().parse().unwrap(),
        revision: word["revision"].as_i64().unwrap(),
        decision_id: word["confirmation"]["decisionId"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap(),
    }
}

async fn back(journal: &Journal) -> Result<()> {
    let status = journal.snapshot().await?;
    journal
        .change_confirmation(Change {
            op_id: Uuid::now_v7(),
            revision: status["confirmation"]["revision"].as_u64().unwrap(),
            action: Action::BackToAutomatic,
        })
        .await?;
    Ok(())
}

async fn history(root: &tempfile::TempDir, journal: &Journal) -> Result<Vec<Uuid>> {
    let mut inputs = vec![];
    for _ in 0..10 {
        let id = stage(journal).await?;
        classify(journal, id, decision()).await?;
        inputs.push(id);
    }
    let db = Connection::open(root.path().join("db"))?;
    intercat_daemon::clock::register(&db)?;
    db.execute("UPDATE utterances SET state='landed'", [])?;
    Ok(inputs)
}

async fn misread(journal: &Journal, id: Uuid) -> Result<()> {
    journal
        .mark_misread(Misread {
            op_id: Uuid::now_v7(),
            utterance_id: id,
            revision: 1,
        })
        .await?;
    Ok(())
}

#[tokio::test]
async fn only_four_distinct_explicit_corrections_in_ten_requests_enter_automatically() -> Result<()>
{
    let (root, journal) = fixture().await?;
    let inputs = history(&root, &journal).await?;
    for id in &inputs[..3] {
        misread(&journal, *id).await?;
    }
    misread(&journal, inputs[0]).await?;
    assert_eq!(journal.snapshot().await?["confirmation"]["active"], false);
    let db = Connection::open(root.path().join("db"))?;
    intercat_daemon::clock::register(&db)?;
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(0,'utterance.corrected',?1)",
        [json!({"inputId":inputs[3],"kind":"inferred"}).to_string()],
    )?;
    assert_eq!(
        journal.snapshot().await?["confirmation"]["rates"]["inferred"],
        1
    );
    assert_eq!(journal.snapshot().await?["confirmation"]["active"], false);
    misread(&journal, inputs[3]).await?;
    let status = journal.snapshot().await?;
    assert_eq!(status["confirmation"]["active"], true);
    assert_eq!(status["confirmation"]["rates"]["explicit"], 4);
    Ok(())
}

#[tokio::test]
async fn a_pending_model_result_uses_the_current_mode_and_ok_is_exact_and_idempotent() -> Result<()>
{
    let (_root, journal) = fixture().await?;
    let id = stage(&journal).await?;
    policy(&journal, Policy::Always).await?;
    let mut proposed = decision();
    proposed.boundary = Some("when-told".into());
    let word = classify(&journal, id, proposed).await?;
    assert_eq!(word["state"], "asked");
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let request = accept(&word);
    let saved = journal.confirm_input(request.clone()).await?;
    assert_eq!(saved["state"], "placed");
    assert_eq!(saved["decision"]["boundary"], "when-told");
    assert_eq!(
        journal.confirm_input(request.clone()).await?["decision"],
        saved["decision"]
    );
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut collision = request;
    collision.revision += 1;
    assert!(journal.confirm_input(collision).await.is_err());
    Ok(())
}

#[tokio::test]
async fn back_to_automatic_keeps_shown_interpretations_waiting_but_allows_new_ones() -> Result<()> {
    let (_root, journal) = fixture().await?;
    policy(&journal, Policy::Always).await?;
    let id = stage(&journal).await?;
    let shown = classify(&journal, id, decision()).await?;
    back(&journal).await?;
    assert_eq!(
        journal.read_utterance(id.to_string()).await?["confirmation"],
        shown["confirmation"]
    );
    let newer = stage(&journal).await?;
    assert_eq!(
        classify(&journal, newer, decision()).await?["state"],
        "placed"
    );
    assert_eq!(
        journal.confirm_input(accept(&shown)).await?["state"],
        "placed"
    );
    Ok(())
}

#[tokio::test]
async fn changing_the_destination_invalidates_the_old_ok_even_after_leaving_the_mode() -> Result<()>
{
    let (_root, journal) = fixture().await?;
    let first = stage(&journal).await?;
    let track = classify(&journal, first, decision()).await?["decision"]["trackId"]
        .as_str()
        .unwrap()
        .to_owned();
    policy(&journal, Policy::Always).await?;
    let id = stage(&journal).await?;
    let old = classify(&journal, id, decision()).await?;
    back(&journal).await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let opened = journal
        .edit_input(edit_hold::Request {
            owner,
            action: edit_hold::Action::Begin {
                token,
                input: id.to_string(),
                revision: 1,
            },
        })
        .await?;
    let changed = journal
        .edit_input(edit_hold::Request {
            owner,
            action: edit_hold::Action::End {
                token,
                revision: opened["revision"].as_i64().unwrap(),
                edit: Edit::ReadAs {
                    track: track.clone(),
                    boundary: Boundary::AfterTurn,
                    turn: None,
                },
            },
        })
        .await?;
    assert_ne!(
        old["confirmation"]["decisionId"],
        changed["confirmation"]["decisionId"]
    );
    assert!(journal.confirm_input(accept(&old)).await.is_err());
    assert_eq!(
        journal.confirm_input(accept(&changed)).await?["decision"]["trackId"],
        track
    );
    Ok(())
}

#[tokio::test]
async fn hold_and_restart_cannot_bypass_ok_and_confirming_does_not_release_the_hold() -> Result<()>
{
    let (root, journal) = fixture().await?;
    policy(&journal, Policy::Always).await?;
    let id = stage(&journal).await?;
    classify(&journal, id, decision()).await?;
    let held = journal
        .resolve_input(Uuid::now_v7(), id.to_string(), 1, Resolution::Hold)
        .await?;
    assert!(
        journal
            .start_held_input(
                Uuid::now_v7(),
                id.to_string(),
                held["revision"].as_i64().unwrap()
            )
            .await
            .is_err()
    );
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let shown = journal.read_utterance(id.to_string()).await?;
    let saved = journal.confirm_input(accept(&shown)).await?;
    assert_eq!(saved["state"], "held");
    assert!(saved["confirmation"].is_null());
    assert!(saved["decision"]["trackId"].is_string());
    assert_eq!(
        journal
            .start_held_input(
                Uuid::now_v7(),
                id.to_string(),
                saved["revision"].as_i64().unwrap()
            )
            .await?["state"],
        "placed"
    );
    Ok(())
}

#[tokio::test]
async fn the_lower_rate_is_suggested_once_and_never_leaves_the_mode_automatically() -> Result<()> {
    let (root, journal) = fixture().await?;
    let inputs = history(&root, &journal).await?;
    for id in &inputs[..4] {
        misread(&journal, *id).await?;
    }
    for _ in 0..3 {
        let id = stage(&journal).await?;
        classify(&journal, id, decision()).await?;
    }
    let status = journal.snapshot().await?;
    assert_eq!(status["confirmation"]["active"], true);
    assert_eq!(status["confirmation"]["suggested"], true);
    let revision = status["confirmation"]["revision"].clone();
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let id = stage(&journal).await?;
    classify(&journal, id, decision()).await?;
    assert_eq!(
        journal.snapshot().await?["confirmation"]["revision"],
        revision
    );
    back(&journal).await?;
    assert_eq!(journal.snapshot().await?["confirmation"]["active"], false);
    Ok(())
}

#[tokio::test]
async fn off_disables_automatic_entry_and_stale_settings_cannot_replace_a_newer_choice()
-> Result<()> {
    let (root, journal) = fixture().await?;
    let inputs = history(&root, &journal).await?;
    policy(&journal, Policy::Off).await?;
    for id in &inputs[..4] {
        misread(&journal, *id).await?;
    }
    assert_eq!(journal.snapshot().await?["confirmation"]["active"], false);
    assert!(
        journal
            .change_confirmation(Change {
                op_id: Uuid::now_v7(),
                revision: 0,
                action: Action::Policy {
                    policy: Policy::Always
                }
            })
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn save_failure_rolls_back_both_the_confirmation_and_new_track() -> Result<()> {
    let (root, journal) = fixture().await?;
    policy(&journal, Policy::Always).await?;
    let id = stage(&journal).await?;
    let shown = classify(&journal, id, decision()).await?;
    let db = Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER reject_confirmation BEFORE INSERT ON operations WHEN NEW.kind='utterance.confirm' BEGIN SELECT RAISE(ABORT,'save failed'); END;")?;
    let request = accept(&shown);
    assert!(journal.confirm_input(request.clone()).await.is_err());
    assert_eq!(
        journal.read_utterance(id.to_string()).await?["confirmation"],
        shown["confirmation"]
    );
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    db.execute_batch("DROP TRIGGER reject_confirmation")?;
    assert_eq!(journal.confirm_input(request).await?["state"], "placed");
    let count:i64=db.query_row("SELECT count(*) FROM events WHERE kind='utterance.confirmed' AND json_extract(payload,'$.inputId')=?1",params![id.to_string()],|row|row.get(0))?;
    assert_eq!(count, 1);
    Ok(())
}

#[tokio::test]
async fn a_trim_ok_cannot_interrupt_the_next_turn() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let first = stage(&journal).await?;
    let track = classify(&journal, first, decision()).await?["decision"]["trackId"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut worker = intercat_daemon::runtime::WorkerState {
        track_id: track.clone(),
        phase: "working".into(),
        session_id: Some("worker-session".into()),
        secret_revision: None,
        identity: None,
        detail: None,
        active_turn: Some(Uuid::now_v7()),
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    journal.worker_state(worker.clone()).await?;
    policy(&journal, Policy::Always).await?;
    let id = stage(&journal).await?;
    let mut proposed = decision();
    proposed.kind = Kind::Trim;
    proposed.track_id = Some(track);
    let shown = classify(&journal, id, proposed).await?;
    worker.active_turn = Some(Uuid::now_v7());
    journal.worker_state(worker).await?;
    let stale = journal.confirm_input(accept(&shown)).await?;
    assert_eq!(stale["state"], "asked");
    assert_eq!(stale["review"]["stage"], "stale-turn");
    assert_eq!(
        journal.snapshot().await?["redirects"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    Ok(())
}

#[tokio::test]
async fn an_unfinished_rereading_cannot_reuse_a_previous_confirmation_identity() -> Result<()> {
    let (_root, journal) = fixture().await?;
    policy(&journal, Policy::Always).await?;
    let id = stage(&journal).await?;
    let shown = classify(&journal, id, decision()).await?;
    journal
        .resolve_input(
            Uuid::now_v7(),
            id.to_string(),
            1,
            Resolution::Retry {
                target: Target::None,
            },
        )
        .await?;
    let mut timeout = decision();
    timeout.timed_out = true;
    let pending = classify(&journal, id, timeout).await?;
    assert!(pending["confirmation"].is_null());
    let mut stale = accept(&shown);
    stale.revision = pending["revision"].as_i64().unwrap();
    assert!(journal.confirm_input(stale).await.is_err());
    Ok(())
}

#[tokio::test]
async fn the_tenth_request_can_complete_the_window_but_back_does_not_reenter_on_old_corrections()
-> Result<()> {
    let (root, journal) = fixture().await?;
    let mut inputs = vec![];
    for _ in 0..9 {
        let id = stage(&journal).await?;
        classify(&journal, id, decision()).await?;
        inputs.push(id);
    }
    let db = Connection::open(root.path().join("db"))?;
    intercat_daemon::clock::register(&db)?;
    db.execute("UPDATE utterances SET state='landed'", [])?;
    for id in &inputs[..4] {
        misread(&journal, *id).await?;
    }
    assert_eq!(journal.snapshot().await?["confirmation"]["active"], false);
    let id = stage(&journal).await?;
    assert_eq!(classify(&journal, id, decision()).await?["state"], "asked");
    back(&journal).await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let editing = journal
        .edit_input(edit_hold::Request {
            owner,
            action: edit_hold::Action::Begin {
                token,
                input: id.to_string(),
                revision: 1,
            },
        })
        .await?;
    journal
        .edit_input(edit_hold::Request {
            owner,
            action: edit_hold::Action::End {
                token,
                revision: editing["revision"].as_i64().unwrap(),
                edit: Edit::Text {
                    text: "Reword the same pending request".into(),
                    sources: None,
                },
            },
        })
        .await?;
    classify(&journal, id, decision()).await?;
    assert_eq!(journal.snapshot().await?["confirmation"]["active"], false);
    Ok(())
}

#[tokio::test]
async fn confirmation_speech_records_only_actual_mode_changes() -> Result<()> {
    let (_root, journal) = fixture().await?;
    policy(&journal, Policy::Always).await?;
    policy(&journal, Policy::Always).await?;
    back(&journal).await?;
    let log = journal
        .editor_log(intercat_daemon::editor_log::Query {
            track_id: "dummy-track".into(),
            before: None,
        })
        .await?;
    let entries = log["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["scene"], "automatic");
    assert_eq!(entries[1]["scene"], "confirmation");
    assert!(!entries[1]["line"].as_str().unwrap().contains("wrong"));
    Ok(())
}
