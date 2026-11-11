use intercat_daemon::{
    Result,
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Resolution, Target},
    decision_review::Choice,
    edit_hold::{Action, Edit, Request as EditRequest},
    journal::{Admission, Journal},
    runtime::WorkerState,
    secrets::Key,
    start_condition::{Source, Turn},
};
use serde_json::{Value, json};
use uuid::Uuid;

fn decision(boundary: &str) -> Outcome {
    Outcome::Classified(Decision {
        kind: Kind::Fork,
        track_id: None,
        boundary: Some(boundary.into()),
        command: None,
        confidence: 1.0,
        inference_ms: 1.0,
        timed_out: false,
        scores: json!({}),
    })
}

async fn create(journal: &Journal, text: &str, boundary: &str) -> Result<Value> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, text.into(), Target::New, None, vec![])
        .await?;
    classify(journal, boundary).await
}

async fn classify(journal: &Journal, boundary: &str) -> Result<Value> {
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(input.id.clone(), input.revision, decision(boundary))
        .await?;
    word(journal, &input.id).await
}

async fn word(journal: &Journal, input: &str) -> Result<Value> {
    Ok(journal.snapshot().await?["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|word| word["id"] == input)
        .unwrap()
        .clone())
}

async fn frame(journal: &Journal, source: &Turn, method: &str) -> Result<()> {
    journal.try_msp(source.track_id.clone(), source.session_id.clone(), Stamp::now(),
        json!({"method":method,"params":{"sessionId":source.session_id,"turnId":source.turn_id,"terminal":"completed"}}),
        None, None)?.await??;
    Ok(())
}

fn worker(source: &Turn, active: bool) -> WorkerState {
    WorkerState {
        track_id: source.track_id.clone(),
        phase: "working".into(),
        session_id: Some(source.session_id.clone()),
        active_turn: active.then_some(source.turn_id),
        secret_revision: None,
        identity: None,
        detail: None,
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    }
}

async fn running(journal: &Journal, label: &str) -> Result<Turn> {
    let created = create(journal, label, "when-told").await?;
    let source = Turn {
        track_id: created["decision"]["trackId"].as_str().unwrap().into(),
        label: label.into(),
        session_id: format!("session-{label}"),
        turn_id: Uuid::now_v7(),
    };
    journal.worker_state(worker(&source, true)).await?;
    frame(journal, &source, "turn/started").await?;
    Ok(source)
}

async fn reserve(journal: &Journal, word: &Value) -> Result<Admission> {
    let op = Uuid::now_v7();
    journal
        .reserve_turn(
            op,
            word["id"].as_str().unwrap().into(),
            word["revision"].as_i64().unwrap(),
            word["decision"]["trackId"].as_str().unwrap().into(),
            json!({"commandId":op,"sessionId":"new-worker","input":[]}),
            None,
        )
        .await
}

#[tokio::test]
async fn waits_for_the_original_turn_even_if_classification_and_restart_are_later() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let source = running(&journal, "A").await?;
    let id = Uuid::now_v7();
    journal
        .stage_utterance(
            id,
            "After this turn, start independent work".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    let mut next = source.clone();
    next.turn_id = Uuid::now_v7();
    journal.worker_state(worker(&next, true)).await?;
    frame(&journal, &next, "turn/started").await?;
    frame(&journal, &next, "turn/completed").await?;
    let pending = classify(&journal, "after-turn").await?;
    assert_eq!(pending["state"], "placed");
    assert_eq!(
        pending["startCondition"]["source"],
        json!(Source::Turn(source.clone()))
    );
    assert_eq!(pending["startCondition"]["ready"], false);
    assert!(
        reserve(&journal, &pending)
            .await
            .unwrap_err()
            .is::<intercat_daemon::journal::StartSuperseded>()
    );
    journal.cut_input(Uuid::now_v7(), id.to_string()).await?;
    let history = journal.snapshot().await?;
    journal
        .edit_history(intercat_daemon::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: intercat_daemon::timeline_history::Action::Undo {
                entry: history["editHistory"]["undo"]["entryId"]
                    .as_str()
                    .unwrap()
                    .into(),
            },
        })
        .await?;
    let restored = word(&journal, &id.to_string()).await?;
    assert_eq!(
        restored["startCondition"]["source"],
        json!(Source::Turn(source.clone()))
    );
    assert_eq!(restored["startCondition"]["ready"], false);
    drop(journal);
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let pending = word(&journal, &id.to_string()).await?;
    assert_eq!(
        pending["startCondition"]["source"],
        json!(Source::Turn(source.clone()))
    );
    assert_eq!(pending["startCondition"]["ready"], false);
    frame(&journal, &source, "turn/completed").await?;
    let ready = word(&journal, &id.to_string()).await?;
    assert_eq!(ready["startCondition"]["ready"], true);
    assert!(matches!(reserve(&journal, &ready).await?, Admission::New));
    Ok(())
}

#[tokio::test]
async fn multiple_turns_require_a_recorded_choice_and_replays_keep_one_new_track() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let source = running(&journal, "A").await?;
    running(&journal, "B").await?;
    let asked = create(&journal, "After this turn, new work", "after-turn").await?;
    assert_eq!(asked["state"], "asked");
    assert_eq!(asked["review"]["stage"], "start-turn");
    assert_eq!(
        asked["startCondition"]["candidates"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(asked["decision"]["trackId"].is_null());
    let mut forged = source.clone();
    forged.turn_id = Uuid::now_v7();
    assert!(
        journal
            .resolve_input(
                Uuid::now_v7(),
                asked["id"].as_str().unwrap().into(),
                1,
                Resolution::Confirm {
                    choice: Choice::After {
                        source: Source::Turn(forged)
                    }
                }
            )
            .await
            .is_err()
    );
    assert_eq!(
        word(&journal, asked["id"].as_str().unwrap()).await?["revision"],
        1
    );
    let op = Uuid::now_v7();
    let resolution = Resolution::Confirm {
        choice: Choice::After {
            source: Source::Turn(source.clone()),
        },
    };
    let chosen = journal
        .resolve_input(
            op,
            asked["id"].as_str().unwrap().into(),
            1,
            resolution.clone(),
        )
        .await?;
    assert_eq!(chosen["state"], "placed");
    assert_eq!(
        chosen["startCondition"]["source"],
        json!(Source::Turn(source.clone()))
    );
    assert_eq!(chosen["startCondition"]["ready"], false);
    let replay = journal
        .resolve_input(op, asked["id"].as_str().unwrap().into(), 1, resolution)
        .await?;
    assert_eq!(replay["decision"]["trackId"], chosen["decision"]["trackId"]);
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn no_recorded_turn_offers_now_or_user_go_ahead() -> Result<()> {
    for choice in [Choice::Now, Choice::WhenTold] {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        let asked = create(&journal, "Start after this turn", "after-turn").await?;
        assert_eq!(asked["review"]["stage"], "start-turn");
        assert_eq!(asked["startCondition"]["candidates"], json!([]));
        let result = journal
            .resolve_input(
                Uuid::now_v7(),
                asked["id"].as_str().unwrap().into(),
                1,
                Resolution::Confirm { choice },
            )
            .await?;
        assert_eq!(result["state"], "placed");
        assert!(result["startCondition"].is_null());
        assert_eq!(
            journal.snapshot().await?["tracks"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    Ok(())
}

#[tokio::test]
async fn rewording_captures_current_turns_and_rejects_a_choice_from_the_old_card() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let a = running(&journal, "A").await?;
    let b = running(&journal, "B").await?;
    let asked = create(&journal, "After this turn", "after-turn").await?;
    let token = Uuid::now_v7();
    let owner = Uuid::now_v7();
    let opened = journal
        .edit_input(EditRequest {
            owner,
            action: Action::Begin {
                token,
                input: asked["id"].as_str().unwrap().into(),
                revision: 1,
            },
        })
        .await?;
    frame(&journal, &a, "turn/completed").await?;
    journal.worker_state(worker(&a, false)).await?;
    journal
        .edit_input(EditRequest {
            owner,
            action: Action::End {
                token,
                revision: opened["revision"].as_i64().unwrap(),
                edit: Edit::Text {
                    text: "After the currently running turn, start another task".into(),
                    sources: None,
                },
            },
        })
        .await?;
    let changed = classify(&journal, "after-turn").await?;
    assert_eq!(
        changed["startCondition"]["source"],
        json!(Source::Turn(b.clone()))
    );
    assert!(
        journal
            .resolve_input(
                Uuid::now_v7(),
                asked["id"].as_str().unwrap().into(),
                1,
                Resolution::Confirm {
                    choice: Choice::After {
                        source: Source::Turn(a)
                    }
                }
            )
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn saving_the_context_is_atomic_with_admission() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let source = running(&journal, "A").await?;
    let db = rusqlite::Connection::open(&path)?;
    db.execute_batch(
        "CREATE TRIGGER fail_start_context BEFORE INSERT ON input_start_conditions
        BEGIN SELECT RAISE(ABORT,'test storage failure'); END;",
    )?;
    let id = Uuid::now_v7();
    assert!(
        journal
            .stage_utterance(id, "Wait for this turn".into(), Target::New, None, vec![])
            .await
            .is_err()
    );
    assert!(
        !journal.snapshot().await?["utterances"]
            .as_array()
            .unwrap()
            .iter()
            .any(|word| word["id"] == id.to_string())
    );
    db.execute_batch("DROP TRIGGER fail_start_context;")?;
    journal
        .stage_utterance(id, "Wait for this turn".into(), Target::New, None, vec![])
        .await?;
    let pending = classify(&journal, "after-turn").await?;
    assert_eq!(
        pending["startCondition"]["source"],
        json!(Source::Turn(source.clone()))
    );
    Ok(())
}

#[tokio::test]
async fn a_late_result_keeps_the_timeout_card_without_a_start_choice() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "After this turn".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    let pending = journal.claim_input().await?.unwrap();
    let Outcome::Classified(mut late) = decision("after-turn") else {
        unreachable!()
    };
    late.timed_out = true;
    journal
        .apply_input(
            pending.id.clone(),
            pending.revision,
            Outcome::Classified(late),
        )
        .await?;
    let saved = word(&journal, &pending.id).await?;
    assert_eq!(saved["state"], "asked");
    assert!(saved["reason"].as_str().unwrap().contains("too long"));
    assert!(saved["review"].is_null());
    assert_eq!(journal.snapshot().await?["tracks"], json!([]));
    Ok(())
}

#[tokio::test]
async fn a_failed_source_choice_rolls_back_its_revision_and_track_creation() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let source = running(&journal, "A").await?;
    running(&journal, "B").await?;
    let asked = create(&journal, "After this turn", "after-turn").await?;
    let db = rusqlite::Connection::open(&path)?;
    db.execute_batch(
        "CREATE TRIGGER fail_start_choice BEFORE UPDATE ON input_start_conditions
        BEGIN SELECT RAISE(ABORT,'test storage failure'); END;",
    )?;
    let op = Uuid::now_v7();
    let resolution = Resolution::Confirm {
        choice: Choice::After {
            source: Source::Turn(source),
        },
    };
    assert!(
        journal
            .resolve_input(
                op,
                asked["id"].as_str().unwrap().into(),
                1,
                resolution.clone()
            )
            .await
            .is_err()
    );
    let unchanged = word(&journal, asked["id"].as_str().unwrap()).await?;
    assert_eq!(unchanged["revision"], 1);
    assert!(unchanged["startCondition"]["source"].is_null());
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    db.execute_batch("DROP TRIGGER fail_start_choice;")?;
    let saved = journal
        .resolve_input(op, asked["id"].as_str().unwrap().into(), 1, resolution)
        .await?;
    assert_eq!(saved["state"], "placed");
    assert_eq!(saved["revision"], 2);
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    Ok(())
}

#[tokio::test]
async fn selecting_the_source_and_confirming_ok_still_wait_for_the_original_turn() -> Result<()> {
    use intercat_daemon::confirmation::{Accept, Action, Change, Policy};
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let source = running(&journal, "A").await?;
    running(&journal, "B").await?;
    journal
        .change_confirmation(Change {
            op_id: Uuid::now_v7(),
            revision: 0,
            action: Action::Policy {
                policy: Policy::Always,
            },
        })
        .await?;
    let asked = create(&journal, "After this turn", "after-turn").await?;
    let chosen = journal
        .resolve_input(
            Uuid::now_v7(),
            asked["id"].as_str().unwrap().into(),
            1,
            Resolution::Confirm {
                choice: Choice::After {
                    source: Source::Turn(source.clone()),
                },
            },
        )
        .await?;
    assert_eq!(chosen["state"], "asked");
    assert_eq!(chosen["reason"], "waiting for OK");
    assert!(chosen["decision"]["trackId"].is_null());
    let accepted = journal
        .confirm_input(Accept {
            op_id: Uuid::now_v7(),
            utterance_id: asked["id"].as_str().unwrap().parse()?,
            revision: chosen["revision"].as_i64().unwrap(),
            decision_id: chosen["confirmation"]["decisionId"]
                .as_str()
                .unwrap()
                .parse()?,
        })
        .await?;
    assert_eq!(accepted["state"], "placed");
    assert_eq!(
        accepted["startCondition"]["source"],
        json!(Source::Turn(source.clone()))
    );
    assert_eq!(accepted["startCondition"]["ready"], false);
    assert!(
        reserve(&journal, &accepted)
            .await
            .unwrap_err()
            .is::<intercat_daemon::journal::StartSuperseded>()
    );
    frame(&journal, &source, "turn/completed").await?;
    assert!(matches!(
        reserve(&journal, &accepted).await?,
        Admission::New
    ));
    Ok(())
}
