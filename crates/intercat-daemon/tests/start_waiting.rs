use intercat_daemon::{
    Result,
    app_commands::WaitingRequest,
    confirmation::{Accept, Action as ConfirmationAction, Change, Policy},
    decision::{Decision, Kind, Outcome, Resolution, Target},
    edit_hold::{Action, Request},
    journal::{Admission, Journal},
    secrets::Key,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    Ok((root, journal))
}

fn decision(kind: Kind, track: Option<String>) -> Decision {
    Decision {
        kind: kind.clone(),
        track_id: track,
        boundary: (kind == Kind::Fork).then(|| "when-told".into()),
        command: (kind == Kind::Command).then(|| "start-waiting".into()),
        confidence: 1.0,
        inference_ms: 1.0,
        timed_out: false,
        scores: json!({}),
    }
}

async fn stage(journal: &Journal, text: &str, target: Target) -> Result<Uuid> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, text.into(), target, None, vec![])
        .await?;
    Ok(id)
}

async fn classify(journal: &Journal, id: Uuid, decision: Decision) -> Result<Value> {
    let input = journal.claim_input().await?.unwrap();
    assert_eq!(input.id, id.to_string());
    journal
        .apply_input(input.id, input.revision, Outcome::Classified(decision))
        .await?;
    journal.read_utterance(id.to_string()).await
}

async fn waiting(journal: &Journal, text: &str) -> Result<Value> {
    let id = stage(journal, text, Target::New).await?;
    classify(journal, id, decision(Kind::Fork, None)).await
}

async fn command(journal: &Journal) -> Result<Value> {
    let id = stage(journal, "Start waiting work", Target::None).await?;
    classify(journal, id, decision(Kind::Command, None)).await
}

fn choice(word: &Value) -> WaitingRequest {
    WaitingRequest::Input {
        input_id: word["id"].as_str().unwrap().parse().unwrap(),
        revision: word["revision"].as_i64().unwrap(),
    }
}

async fn read(journal: &Journal, word: &Value) -> Result<Value> {
    journal
        .read_utterance(word["id"].as_str().unwrap().into())
        .await
}

async fn select(journal: &Journal, command: &Value, word: &Value, op: Uuid) -> Result<Value> {
    journal
        .resolve_input(
            op,
            command["id"].as_str().unwrap().into(),
            command["revision"].as_i64().unwrap(),
            Resolution::StartWaiting {
                choice: choice(word),
            },
        )
        .await
}

#[tokio::test]
async fn sole_waiting_request_keeps_identity_and_uses_the_normal_dispatch_reservation() -> Result<()>
{
    let (_root, journal) = fixture().await?;
    let word = waiting(&journal, "あとでこの仕事を始める").await?;
    let started = command(&journal).await?;
    let current = read(&journal, &word).await?;
    assert_eq!(started["state"], "landed");
    assert_eq!(started["startWaiting"]["started"]["inputId"], word["id"]);
    assert_eq!(current["decision"]["boundary"], "now");
    assert_eq!(current["revision"], 2);
    assert_eq!(current["text"], word["text"]);
    assert_eq!(current["decision"]["trackId"], word["decision"]["trackId"]);
    let snapshot = journal.snapshot().await?;
    assert_eq!(snapshot["tracks"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["utterances"].as_array().unwrap().len(), 2);
    let op = Uuid::now_v7();
    assert!(matches!(
        journal
            .reserve_turn(
                op,
                current["id"].as_str().unwrap().into(),
                2,
                current["decision"]["trackId"].as_str().unwrap().into(),
                json!({"commandId":op,"sessionId":"worker","input":[]}),
                None,
            )
            .await?,
        Admission::New
    ));
    let db = Connection::open(_root.path().join("db"))?;
    let count: i64 = db.query_row(
        "SELECT count(*) FROM operations WHERE kind='utterance.start'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(count, 1);
    Ok(())
}

#[tokio::test]
async fn multiple_candidates_persist_and_selection_replay_starts_exactly_one() -> Result<()> {
    let (root, journal) = fixture().await?;
    let a = waiting(&journal, "First request").await?;
    let b = waiting(&journal, "Second request").await?;
    let asked = command(&journal).await?;
    assert_eq!(asked["state"], "asked");
    assert_eq!(
        asked["startWaiting"]["candidates"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let foreign = WaitingRequest::Input {
        input_id: Uuid::now_v7(),
        revision: 1,
    };
    assert!(
        journal
            .resolve_input(
                Uuid::now_v7(),
                asked["id"].as_str().unwrap().into(),
                1,
                Resolution::StartWaiting { choice: foreign }
            )
            .await
            .is_err()
    );
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(
        read(&journal, &asked).await?["startWaiting"],
        asked["startWaiting"]
    );
    let op = Uuid::now_v7();
    let selected = select(&journal, &asked, &b, op).await?;
    assert_eq!(selected["state"], "landed");
    assert_eq!(select(&journal, &asked, &b, op).await?, selected);
    assert_eq!(
        read(&journal, &a).await?["decision"]["boundary"],
        "when-told"
    );
    assert_eq!(read(&journal, &b).await?["revision"], 2);
    assert!(select(&journal, &asked, &a, Uuid::now_v7()).await.is_err());
    Ok(())
}

#[tokio::test]
async fn changed_candidate_is_unavailable_and_never_replaced_by_another_request() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let a = waiting(&journal, "First request").await?;
    let b = waiting(&journal, "Second request").await?;
    let asked = command(&journal).await?;
    journal
        .resolve_input(
            Uuid::now_v7(),
            a["id"].as_str().unwrap().into(),
            1,
            Resolution::Hold,
        )
        .await?;
    assert!(select(&journal, &asked, &a, Uuid::now_v7()).await.is_err());
    let current = read(&journal, &asked).await?;
    assert_eq!(current["state"], "asked");
    assert_eq!(current["startWaiting"]["candidates"][0]["available"], false);
    assert_eq!(
        read(&journal, &b).await?["decision"]["boundary"],
        "when-told"
    );
    select(&journal, &asked, &b, Uuid::now_v7()).await?;
    assert_eq!(read(&journal, &a).await?["state"], "held");
    Ok(())
}

#[tokio::test]
async fn admission_excludes_later_requests_and_cut_source_cannot_act() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let source = stage(&journal, "Start waiting work", Target::None).await?;
    let input = journal.claim_input().await?.unwrap();
    let later = waiting(&journal, "Later request").await?;
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(decision(Kind::Command, None)),
        )
        .await?;
    let asked = journal.read_utterance(source.to_string()).await?;
    assert_eq!(asked["state"], "asked");
    assert_eq!(asked["startWaiting"]["candidates"], json!([]));
    assert_eq!(
        read(&journal, &later).await?["decision"]["boundary"],
        "when-told"
    );
    let source = stage(&journal, "Start waiting work", Target::None).await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .cut_input(Uuid::now_v7(), source.to_string())
        .await?;
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(decision(Kind::Command, None)),
        )
        .await?;
    assert_eq!(
        journal.read_utterance(source.to_string()).await?["state"],
        "cut"
    );
    assert_eq!(read(&journal, &later).await?["revision"], 1);
    journal
        .resolve_input(
            Uuid::now_v7(),
            asked["id"].as_str().unwrap().into(),
            1,
            Resolution::Retry {
                target: Target::None,
            },
        )
        .await?;
    let refreshed = classify(
        &journal,
        asked["id"].as_str().unwrap().parse()?,
        decision(Kind::Command, None),
    )
    .await?;
    assert_eq!(refreshed["state"], "landed");
    assert_eq!(read(&journal, &later).await?["revision"], 2);
    Ok(())
}

#[tokio::test]
async fn locked_command_only_starts_the_named_track() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let a = waiting(&journal, "First request").await?;
    let b = waiting(&journal, "Second request").await?;
    let track = b["decision"]["trackId"].as_str().unwrap().to_owned();
    let source = stage(&journal, "Start waiting work", Target::Lock(track.clone())).await?;
    let result = classify(&journal, source, decision(Kind::Command, Some(track))).await?;
    assert_eq!(result["state"], "landed");
    assert_eq!(result["startWaiting"]["started"]["inputId"], b["id"]);
    assert_eq!(read(&journal, &a).await?["revision"], 1);
    Ok(())
}

#[tokio::test]
async fn held_and_currently_edited_requests_are_excluded_at_admission() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let a = waiting(&journal, "Held request").await?;
    journal
        .resolve_input(
            Uuid::now_v7(),
            a["id"].as_str().unwrap().into(),
            1,
            Resolution::Hold,
        )
        .await?;
    let b = waiting(&journal, "Editing request").await?;
    journal
        .edit_input(Request {
            owner: Uuid::now_v7(),
            action: Action::Begin {
                token: Uuid::now_v7(),
                input: b["id"].as_str().unwrap().into(),
                revision: 1,
            },
        })
        .await?;
    let result = command(&journal).await?;
    assert_eq!(result["state"], "asked");
    assert_eq!(result["reason"], "Nothing is waiting for your go.");
    assert_eq!(result["startWaiting"]["candidates"], json!([]));
    assert_eq!(read(&journal, &a).await?["state"], "held");
    assert_eq!(
        read(&journal, &b).await?["decision"]["boundary"],
        "when-told"
    );
    Ok(())
}

#[tokio::test]
async fn single_target_changed_during_inference_is_not_started() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let word = waiting(&journal, "Waiting request").await?;
    let source = stage(&journal, "Start waiting work", Target::None).await?;
    journal
        .resolve_input(
            Uuid::now_v7(),
            word["id"].as_str().unwrap().into(),
            1,
            Resolution::Hold,
        )
        .await?;
    let result = classify(&journal, source, decision(Kind::Command, None)).await?;
    assert_eq!(result["state"], "asked");
    assert_eq!(result["startWaiting"]["candidates"][0]["available"], false);
    assert_eq!(read(&journal, &word).await?["state"], "held");
    Ok(())
}

#[tokio::test]
async fn always_confirmation_requires_ok_before_starting_and_replay_is_idempotent() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let word = waiting(&journal, "Waiting request").await?;
    journal
        .change_confirmation(Change {
            op_id: Uuid::now_v7(),
            revision: 0,
            action: ConfirmationAction::Policy {
                policy: Policy::Always,
            },
        })
        .await?;
    let result = command(&journal).await?;
    assert_eq!(result["state"], "asked");
    assert_eq!(read(&journal, &word).await?["revision"], 1);
    assert!(
        select(&journal, &result, &word, Uuid::now_v7())
            .await
            .is_err()
    );
    let accept = Accept {
        op_id: Uuid::now_v7(),
        utterance_id: result["id"].as_str().unwrap().parse()?,
        revision: 1,
        decision_id: result["confirmation"]["decisionId"]
            .as_str()
            .unwrap()
            .parse()?,
    };
    assert_eq!(
        journal.confirm_input(accept.clone()).await?["state"],
        "landed"
    );
    assert_eq!(journal.confirm_input(accept).await?["state"], "landed");
    assert_eq!(read(&journal, &word).await?["revision"], 2);
    Ok(())
}

#[tokio::test]
async fn save_failure_rolls_back_target_source_and_operation_then_retry_succeeds() -> Result<()> {
    for multiple in [false, true] {
        let (root, journal) = fixture().await?;
        let a = waiting(&journal, "First request").await?;
        if multiple {
            waiting(&journal, "Second request").await?;
        }
        let source = stage(&journal, "Start waiting work", Target::None).await?;
        let db = Connection::open(root.path().join("db"))?;
        if multiple {
            classify(&journal, source, decision(Kind::Command, None)).await?;
        }
        db.execute_batch("CREATE TRIGGER fail_command BEFORE UPDATE ON utterances WHEN NEW.state='landed' AND json_extract(NEW.decision,'$.command')='start-waiting' BEGIN SELECT RAISE(ABORT,'simulated disk failure'); END;")?;
        let asked = journal.read_utterance(source.to_string()).await?;
        let op = Uuid::now_v7();
        if multiple {
            assert!(select(&journal, &asked, &a, op).await.is_err());
        } else {
            let input = journal.claim_input().await?.unwrap();
            assert!(
                journal
                    .apply_input(
                        input.id,
                        input.revision,
                        Outcome::Classified(decision(Kind::Command, None))
                    )
                    .await
                    .is_err()
            );
        }
        assert_eq!(read(&journal, &a).await?["revision"], 1);
        let starts: i64 = db.query_row(
            "SELECT count(*) FROM operations WHERE kind='utterance.start'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(starts, 0);
        db.execute_batch("DROP TRIGGER fail_command;")?;
        if multiple {
            assert_eq!(select(&journal, &asked, &a, op).await?["state"], "landed");
        } else {
            journal
                .apply_input(
                    source.to_string(),
                    1,
                    Outcome::Classified(decision(Kind::Command, None)),
                )
                .await?;
        }
        assert_eq!(read(&journal, &a).await?["revision"], 2);
    }
    Ok(())
}

#[tokio::test]
async fn admission_capture_and_source_insert_rollback_together() -> Result<()> {
    let (root, journal) = fixture().await?;
    waiting(&journal, "Waiting request").await?;
    let db = Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER fail_capture BEFORE INSERT ON input_command_candidates BEGIN SELECT RAISE(ABORT,'simulated disk failure'); END;")?;
    let id = Uuid::now_v7();
    assert!(
        journal
            .stage_utterance(id, "Start waiting work".into(), Target::None, None, vec![])
            .await
            .is_err()
    );
    let count: i64 = db.query_row(
        "SELECT count(*) FROM utterances WHERE id=?1",
        [id.to_string()],
        |row| row.get(0),
    )?;
    assert_eq!(count, 0);
    db.execute_batch("DROP TRIGGER fail_capture;")?;
    journal
        .stage_utterance(id, "Start waiting work".into(), Target::None, None, vec![])
        .await?;
    let result = classify(&journal, id, decision(Kind::Command, None)).await?;
    assert_eq!(result["state"], "landed");
    let window = journal
        .timeline_window(intercat_daemon::timeline::Window {
            tracks: vec![],
            start: 0,
            end: i64::MAX,
            since: None,
        })
        .await?;
    let mark = window["marks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|mark| mark["inputId"] == id.to_string())
        .unwrap();
    assert_eq!(mark["operation"], "command");
    assert_eq!(mark["command"], "start-waiting");
    Ok(())
}
