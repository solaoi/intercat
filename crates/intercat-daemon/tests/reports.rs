use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Resolution, Target},
    journal::Journal,
    reports::{self, Analysis, Answer, Report},
    run::{Effect, Hold, Run},
};
use serde_json::json;
use uuid::Uuid;
async fn fixture() -> Result<(tempfile::TempDir, Journal, String, String)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Normal work".into(),
            Target::New,
            None,
            Vec::new(),
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    let input_id = input.id.clone();
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: Some("now".into()),
                command: None,
                confidence: 1.0,
                inference_ms: 1.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let track = journal.snapshot().await?["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    Ok((root, journal, track, input_id))
}
#[test]
fn fixed_lines_ignore_examples_and_invalid_scores_never_give_permission() {
    assert_eq!(
        reports::explicit_questions("Waiting for you:"),
        vec![String::new()]
    );
    assert_eq!(
        reports::explicit_questions(
            "Done.\n```text\nWaiting for you: an example\n```\nWaiting for you: First?\nWaiting for you: Second?"
        ),
        vec!["First?", "Second?"]
    );
    let response = |selected: &str, values: [f64; 3]| json!({"stages":{"question":{"selected":selected,"logprobs":{"1":values[0].ln(),"0":values[1].ln(),"2":values[2].ln()},"probabilities":{"1":values[0],"0":values[1],"2":values[2]}}}});
    assert!(matches!(
        reports::classify(&response("0", [0.01, 0.98, 0.01]), "done"),
        Analysis::Clear
    ));
    assert!(matches!(
        reports::classify(&response("1", [0.98, 0.01, 0.01]), "Question?"),
        Analysis::Questions(_)
    ));
    assert!(matches!(
        reports::classify(&response("0", [0.3, 0.4, 0.3]), "unclear"),
        Analysis::Review
    ));
    let mut bad = response("0", [0.01, 0.98, 0.01]);
    bad["stages"]["question"]["logprobs"]["1"] = json!(0.0);
    assert!(matches!(
        reports::classify(&bad, "unclear"),
        Analysis::Review
    ));
    assert!(matches!(
        reports::classify(&json!({}), "unclear"),
        Analysis::Review
    ));
}
#[test]
fn replies_bypass_questions_but_keep_turn_checkpoint_editor_and_stop_guards() {
    let mut run = Run::new();
    run.question(Uuid::now_v7());
    run.question(Uuid::now_v7());
    run.set_hold(Hold::Report, true);
    assert!(!run.can_start());
    assert!(run.can_reply());
    let id = Uuid::now_v7();
    assert_eq!(run.request_reply_start(id), Some(Effect::PersistStart(id)));
    assert_eq!(run.start_persisted(id), Some(Effect::SendStart(id)));
    let turn = Uuid::now_v7();
    run.start_acknowledged(id, turn);
    assert!(!run.can_reply());
    run.completed(turn);
    assert!(!run.can_reply());
    run.checkpoint_saved(turn);
    assert!(run.can_reply());
    run.set_hold(Hold::Editor, true);
    assert!(!run.can_reply());
    run.set_hold(Hold::Editor, false);
    let next = Uuid::now_v7();
    run.request_reply_start(next);
    run.stop();
    assert_eq!(run.start_persisted(next), Some(Effect::Superseded(next)));
}
#[tokio::test]
async fn queued_replies_hold_until_sent_and_follow_question_order() -> Result<()> {
    let (_root, j, track, normal) = fixture().await?;
    let report = Report::new(
        track.clone(),
        "session".into(),
        Uuid::now_v7(),
        "item".into(),
        "Waiting for you: First?\nWaiting for you: Second?".into(),
    );
    assert!(!j.observe_report(report.clone()).await?.report.pending);
    let qs = j.snapshot().await?["reportQuestions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(qs.len(), 2);
    assert!(
        j.reserve_turn(
            Uuid::now_v7(),
            normal.clone(),
            1,
            track.clone(),
            json!({"sessionId":"session"})
        )
        .await
        .is_err()
    );
    let id2 = Uuid::now_v7();
    j.respond_report(
        id2,
        qs[1]["id"].as_str().unwrap().into(),
        1,
        Answer::Reply {
            text: "Second answer".into(),
        },
    )
    .await?;
    let id1 = Uuid::now_v7();
    let first = j
        .respond_report(
            id1,
            qs[0]["id"].as_str().unwrap().into(),
            1,
            Answer::Reply {
                text: "First answer".into(),
            },
        )
        .await?;
    assert_eq!(first["replyTo"]["id"], qs[0]["id"]);
    assert!(
        j.reserve_turn(
            Uuid::now_v7(),
            id2.to_string(),
            1,
            track.clone(),
            json!({"sessionId":"session"})
        )
        .await
        .is_err()
    );
    j.reserve_turn(
        Uuid::now_v7(),
        id1.to_string(),
        1,
        track.clone(),
        json!({"sessionId":"session"}),
    )
    .await?;
    assert_eq!(
        j.snapshot().await?["reportQuestions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    j.input_execution(id1.to_string(), 1, "sent").await?;
    assert_eq!(
        j.snapshot().await?["reportQuestions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        j.reserve_turn(
            Uuid::now_v7(),
            normal.clone(),
            1,
            track.clone(),
            json!({"sessionId":"session"})
        )
        .await
        .is_err()
    );
    j.cut_input(Uuid::now_v7(), id2.to_string()).await?;
    assert_eq!(j.snapshot().await?["reportQuestions"][0]["state"], "open");
    let second = j
        .respond_report(
            Uuid::now_v7(),
            qs[1]["id"].as_str().unwrap().into(),
            1,
            Answer::Reply {
                text: "Second again".into(),
            },
        )
        .await?;
    let id = second["id"].as_str().unwrap().to_owned();
    j.reserve_turn(
        Uuid::now_v7(),
        id.clone(),
        1,
        track.clone(),
        json!({"sessionId":"session"}),
    )
    .await?;
    j.input_execution(id.clone(), 1, "held").await?;
    assert_eq!(j.snapshot().await?["reportQuestions"][0]["state"], "open");
    let next = j
        .resolve_input(
            Uuid::now_v7(),
            id.clone(),
            1,
            Resolution::Retry {
                target: Target::Lock(track.clone()),
            },
        )
        .await?;
    assert_eq!(next["state"], "placed");
    assert_eq!(next["revision"], 2);
    j.reserve_turn(
        Uuid::now_v7(),
        id.clone(),
        2,
        track.clone(),
        json!({"sessionId":"session"}),
    )
    .await?;
    j.input_execution(id, 2, "sent").await?;
    assert!(
        j.snapshot().await?["reportQuestions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    j.reserve_turn(
        Uuid::now_v7(),
        normal,
        1,
        track,
        json!({"sessionId":"session"}),
    )
    .await?;
    j.observe_report(report).await?;
    assert!(
        j.snapshot().await?["reportQuestions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}
#[tokio::test]
async fn changed_questions_orphan_old_replies_and_ignore_stale_analysis() -> Result<()> {
    let (_root, j, track, normal) = fixture().await?;
    let mut report = Report::new(
        track.clone(),
        "session".into(),
        Uuid::now_v7(),
        "item".into(),
        "Waiting for you: Original?".into(),
    );
    let old = j.observe_report(report.clone()).await?;
    let q = j.snapshot().await?["reportQuestions"][0].clone();
    let reply = j
        .respond_report(
            Uuid::now_v7(),
            q["id"].as_str().unwrap().into(),
            1,
            Answer::Reply {
                text: "Old answer".into(),
            },
        )
        .await?;
    report.text = "Waiting for you: Changed?".into();
    assert_eq!(j.observe_report(report).await?.report.revision, 2);
    let word = j.snapshot().await?["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == reply["id"])
        .unwrap()
        .clone();
    assert_eq!(word["state"], "orphaned");
    assert!(
        j.respond_report(
            Uuid::now_v7(),
            q["id"].as_str().unwrap().into(),
            1,
            Answer::Reply {
                text: "Stale".into()
            }
        )
        .await
        .is_err()
    );
    j.analyze_report(old.report.id, 1, Analysis::Clear).await?;
    assert!(
        j.reserve_turn(
            Uuid::now_v7(),
            normal,
            1,
            track,
            json!({"sessionId":"session"})
        )
        .await
        .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn unclear_reports_survive_restart_and_require_review() -> Result<()> {
    let (root, j, track, normal) = fixture().await?;
    let report = j
        .observe_report(Report::new(
            track.clone(),
            "session".into(),
            Uuid::now_v7(),
            "item".into(),
            "Could you choose?".into(),
        ))
        .await?
        .report;
    assert!(report.pending);
    assert!(
        j.reserve_turn(
            Uuid::now_v7(),
            normal.clone(),
            1,
            track.clone(),
            json!({"sessionId":"session"})
        )
        .await
        .is_err()
    );
    j.analyze_report(report.id, 1, Analysis::Review).await?;
    let q = j.snapshot().await?["reportQuestions"][0].clone();
    drop(j);
    let j = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    assert_eq!(j.snapshot().await?["reportQuestions"][0]["id"], q["id"]);
    j.respond_report(
        Uuid::now_v7(),
        q["id"].as_str().unwrap().into(),
        1,
        Answer::Hold,
    )
    .await?;
    assert!(
        j.reserve_turn(
            Uuid::now_v7(),
            normal.clone(),
            1,
            track.clone(),
            json!({"sessionId":"session"})
        )
        .await
        .is_err()
    );
    j.respond_report(
        Uuid::now_v7(),
        q["id"].as_str().unwrap().into(),
        1,
        Answer::Continue,
    )
    .await?;
    j.reserve_turn(
        Uuid::now_v7(),
        normal,
        1,
        track,
        json!({"sessionId":"session"}),
    )
    .await?;
    Ok(())
}

#[tokio::test]
async fn a_replaced_reply_is_orphaned_and_a_changed_starting_reply_can_be_released() -> Result<()> {
    let (_root, j, track, _) = fixture().await?;
    let mut report = Report::new(
        track.clone(),
        "session".into(),
        Uuid::now_v7(),
        "item".into(),
        "Waiting for you: Choose one?".into(),
    );
    j.observe_report(report.clone()).await?;
    let q = j.snapshot().await?["reportQuestions"][0].clone();
    let qid = q["id"].as_str().unwrap().to_owned();
    let first_id = Uuid::now_v7();
    j.respond_report(
        first_id,
        qid.clone(),
        1,
        Answer::Reply {
            text: "First".into(),
        },
    )
    .await?;
    let second_id = Uuid::now_v7();
    j.respond_report(
        second_id,
        qid,
        1,
        Answer::Reply {
            text: "Replacement".into(),
        },
    )
    .await?;
    let state = j.snapshot().await?;
    assert_eq!(
        state["utterances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|word| word["id"] == first_id.to_string())
            .unwrap()["state"],
        "orphaned"
    );
    assert!(
        j.reserve_turn(
            Uuid::now_v7(),
            first_id.to_string(),
            1,
            track.clone(),
            json!({"sessionId":"session"})
        )
        .await
        .is_err()
    );
    j.reserve_turn(
        Uuid::now_v7(),
        second_id.to_string(),
        1,
        track,
        json!({"sessionId":"session"}),
    )
    .await?;
    report.text = "Waiting for you: A different choice?".into();
    j.observe_report(report).await?;
    j.input_execution(second_id.to_string(), 1, "held").await?;
    assert_eq!(j.snapshot().await?["reportQuestions"][0]["revision"], 2);
    Ok(())
}
