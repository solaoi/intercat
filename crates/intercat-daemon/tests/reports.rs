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

#[tokio::test]
async fn late_question_keeps_its_turn_and_blocks_only_future_work_through_checkpoint_and_restart()
-> Result<()> {
    use intercat_daemon::{checkpoint::Checkpoint, runtime::WorkerState, secrets::Key};
    let (root, journal, track, normal) = fixture().await?;
    let original_turn = Uuid::now_v7();
    let next_turn = Uuid::now_v7();
    let mut worker = WorkerState {
        track_id: track.clone(),
        phase: "working".into(),
        session_id: Some("session".into()),
        secret_revision: None,
        identity: None,
        detail: None,
        active_turn: Some(next_turn),
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    journal.worker_state(worker.clone()).await?;
    let report = Report::new(
        track.clone(),
        "session".into(),
        original_turn,
        "late-report".into(),
        "Choose a database before continuing.".into(),
    );
    journal.observe_report(report.clone()).await?;
    assert!(
        journal
            .reserve_turn(
                Uuid::now_v7(),
                normal.clone(),
                1,
                track.clone(),
                json!({"sessionId":"session"}),
                None,
            )
            .await
            .is_err()
    );
    journal
        .analyze_report(report.id, 1, Analysis::Questions(vec![report.text]))
        .await?;
    let snapshot = journal.snapshot().await?;
    let question = snapshot["reportQuestions"][0].clone();
    assert_eq!(question["turnId"], original_turn.to_string());
    assert_eq!(question["askedBeforeLastMessage"], true);
    assert_eq!(snapshot["workers"][0]["activeTurn"], next_turn.to_string());
    assert_eq!(snapshot["workers"][0]["phase"], "working");

    worker.active_turn = None;
    worker.checkpoint_pending = true;
    worker.phase = "checkpoint".into();
    journal.worker_state(worker.clone()).await?;
    assert_eq!(
        journal.snapshot().await?["reportQuestions"][0]["askedBeforeLastMessage"],
        true
    );
    drop(journal);
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    assert_eq!(
        journal.snapshot().await?["reportQuestions"][0]["askedBeforeLastMessage"],
        true
    );

    worker.checkpoint_pending = false;
    worker.checkpoint = Some(Checkpoint {
        turn_id: next_turn,
        sha: "a".repeat(40),
        reference: "refs/intercat/test/A".into(),
        unmerged_paths: vec![],
        live_processes: false,
    });
    worker.phase = "ready".into();
    journal.worker_state(worker.clone()).await?;
    assert!(
        journal
            .reserve_turn(
                Uuid::now_v7(),
                normal.clone(),
                1,
                track.clone(),
                json!({"sessionId":"session"}),
                None,
            )
            .await
            .is_err()
    );
    let reply = Uuid::now_v7();
    let word = journal
        .respond_report(
            reply,
            question["id"].as_str().unwrap().into(),
            1,
            Answer::Reply {
                text: "Use SQLite.".into(),
            },
        )
        .await?;
    assert_eq!(word["decision"]["boundary"], "after-turn");
    assert_eq!(
        word["replyTo"]["question"]["turnId"],
        original_turn.to_string()
    );
    journal
        .reserve_turn(
            Uuid::now_v7(),
            reply.to_string(),
            1,
            track.clone(),
            json!({"sessionId":"session"}),
            None,
        )
        .await?;
    assert_eq!(
        journal.snapshot().await?["reportQuestions"][0]["state"],
        "queued"
    );
    journal
        .input_execution(reply.to_string(), 1, "sent")
        .await?;
    journal
        .reserve_turn(
            Uuid::now_v7(),
            normal,
            1,
            track,
            json!({"sessionId":"session"}),
            None,
        )
        .await?;

    let current_report = Report::new(
        worker.track_id.clone(),
        "session".into(),
        next_turn,
        "current-report".into(),
        "Waiting for you: Confirm?".into(),
    );
    journal.observe_report(current_report).await?;
    assert_eq!(
        journal.snapshot().await?["reportQuestions"][0]["askedBeforeLastMessage"],
        false
    );
    Ok(())
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
    let response = |selected: &str, values: [f64; 3]| json!({"cachePrefix":false,"stages":{"question":{"selected":selected,"logprobs":{"1":values[0].ln(),"0":values[1].ln(),"2":values[2].ln()},"probabilities":{"1":values[0],"0":values[1],"2":values[2]}}}});
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
    let mut wrong_mode = response("0", [0.01, 0.98, 0.01]);
    wrong_mode["cachePrefix"] = json!(true);
    assert!(matches!(
        reports::classify(&wrong_mode, "done"),
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
            json!({"sessionId":"session"}),
            None,
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
            json!({"sessionId":"session"}),
            None,
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
        None,
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
            json!({"sessionId":"session"}),
            None,
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
        None,
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
        None,
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
        None,
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
    let stale = j
        .respond_report(
            Uuid::now_v7(),
            q["id"].as_str().unwrap().into(),
            1,
            Answer::Reply {
                text: "Stale".into(),
            },
        )
        .await?;
    assert_eq!(stale["state"], "orphaned");
    assert_eq!(stale["reason"], "the question changed meanwhile");
    assert_eq!(stale["replyTo"]["question"]["text"], "Original?");
    j.analyze_report(old.report.id, 1, Analysis::Clear).await?;
    assert!(
        j.reserve_turn(
            Uuid::now_v7(),
            normal,
            1,
            track,
            json!({"sessionId":"session"}),
            None,
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
            json!({"sessionId":"session"}),
            None,
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
            json!({"sessionId":"session"}),
            None,
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
        None,
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
            json!({"sessionId":"session"}),
            None,
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
        None,
    )
    .await?;
    report.text = "Waiting for you: A different choice?".into();
    j.observe_report(report).await?;
    j.input_execution(second_id.to_string(), 1, "held").await?;
    assert_eq!(j.snapshot().await?["reportQuestions"][0]["revision"], 2);
    Ok(())
}

#[tokio::test]
async fn stale_drafts_keep_the_original_question_and_replay_after_restart() -> Result<()> {
    for answered in [false, true] {
        let (root, journal, track, _) = fixture().await?;
        let mut report = Report::new(
            track.clone(),
            "session".into(),
            Uuid::now_v7(),
            "original".into(),
            "Waiting for you: Original question?".into(),
        );
        journal.observe_report(report.clone()).await?;
        let question = journal.snapshot().await?["reportQuestions"][0].clone();
        let question_id = question["id"].as_str().unwrap().to_owned();
        if answered {
            let sent = Uuid::now_v7();
            journal
                .respond_report(
                    sent,
                    question_id.clone(),
                    1,
                    Answer::Reply {
                        text: "An earlier answer".into(),
                    },
                )
                .await?;
            journal
                .reserve_turn(
                    Uuid::now_v7(),
                    sent.to_string(),
                    1,
                    track.clone(),
                    json!({"sessionId":"session"}),
                    None,
                )
                .await?;
            journal.input_execution(sent.to_string(), 1, "sent").await?;
        } else {
            report.text = "Waiting for you: A different question?".into();
            journal.observe_report(report).await?;
        }
        let operation = Uuid::now_v7();
        let stale = journal
            .respond_report(
                operation,
                question_id.clone(),
                1,
                Answer::Reply {
                    text: "My saved draft".into(),
                },
            )
            .await?;
        assert_eq!(stale["state"], "orphaned");
        assert_eq!(
            stale["reason"],
            if answered {
                "the question was answered meanwhile"
            } else {
                "the question changed meanwhile"
            }
        );
        assert_eq!(stale["replyTo"]["question"]["text"], "Original question?");
        assert!(journal.claim_input().await?.is_none());
        assert!(
            journal
                .reserve_turn(
                    Uuid::now_v7(),
                    operation.to_string(),
                    1,
                    track,
                    json!({"sessionId":"session"}),
                    None,
                )
                .await
                .is_err()
        );
        drop(journal);
        let journal = Journal::open(
            root.path().join("events.sqlite"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        assert_eq!(
            journal
                .respond_report(
                    operation,
                    question_id.clone(),
                    1,
                    Answer::Reply {
                        text: "My saved draft".into()
                    }
                )
                .await?,
            stale
        );
        assert!(
            journal
                .respond_report(
                    operation,
                    question_id,
                    1,
                    Answer::Reply {
                        text: "Different retry".into()
                    }
                )
                .await
                .is_err()
        );
        let questions = journal.snapshot().await?["reportQuestions"].clone();
        assert_eq!(questions.as_array().unwrap().len(), usize::from(!answered));
        if !answered {
            assert_eq!(questions[0]["text"], "A different question?");
        }
    }
    Ok(())
}

#[tokio::test]
async fn an_orphaned_reply_can_be_placed_explicitly_without_reusing_the_question() -> Result<()> {
    let (_root, journal, track, _) = fixture().await?;
    let mut report = Report::new(
        track.clone(),
        "session".into(),
        Uuid::now_v7(),
        "question".into(),
        "Waiting for you: Before?".into(),
    );
    journal.observe_report(report.clone()).await?;
    let question_id = journal.snapshot().await?["reportQuestions"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    report.text = "Waiting for you: After?".into();
    journal.observe_report(report).await?;
    assert!(
        journal
            .respond_report(
                Uuid::now_v7(),
                question_id.clone(),
                99,
                Answer::Reply {
                    text: "Invalid version".into()
                }
            )
            .await
            .is_err()
    );
    let input = Uuid::now_v7();
    journal
        .respond_report(
            input,
            question_id,
            1,
            Answer::Reply {
                text: "An independent instruction".into(),
            },
        )
        .await?;
    assert!(
        journal
            .resolve_input(Uuid::now_v7(), input.to_string(), 1, Resolution::Hold)
            .await
            .is_err()
    );
    assert!(
        journal
            .resolve_input(
                Uuid::now_v7(),
                input.to_string(),
                1,
                Resolution::Retry {
                    target: Target::Lock("gone".into())
                }
            )
            .await
            .is_err()
    );
    let placed = journal
        .resolve_input(
            Uuid::now_v7(),
            input.to_string(),
            1,
            Resolution::Retry {
                target: Target::New,
            },
        )
        .await?;
    assert_eq!(placed["state"], "placed");
    assert!(placed["replyTo"].is_null());
    assert_eq!(placed["decision"]["kind"], "fork");
    assert_eq!(placed["decision"]["boundary"], "now");
    assert!(placed["startCondition"].is_null());
    assert!(journal.claim_input().await?.is_none());
    let new_track = placed["decision"]["trackId"].as_str().unwrap().to_owned();
    assert_ne!(new_track, track);
    journal
        .reserve_turn(
            Uuid::now_v7(),
            input.to_string(),
            2,
            new_track,
            json!({"sessionId":"other-session"}),
            None,
        )
        .await?;
    assert_eq!(
        journal.snapshot().await?["reportQuestions"][0]["state"],
        "open"
    );
    Ok(())
}

#[tokio::test]
async fn native_rpc_preserves_an_old_reply_and_places_only_the_explicit_instruction() -> Result<()>
{
    use intercat_daemon::{
        editor::Manager,
        lock::DaemonLock,
        rpc::{Server, Token},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (_data, journal, track, _) = fixture().await?;
    let mut report = Report::new(
        track,
        "session".into(),
        Uuid::now_v7(),
        "native".into(),
        "Waiting for you: Before the change?".into(),
    );
    journal.observe_report(report.clone()).await?;
    let question = journal.snapshot().await?["reportQuestions"][0]["id"].clone();
    report.text = "Waiting for you: After the change?".into();
    journal.observe_report(report).await?;
    let root = tempfile::tempdir_in("/private/tmp")?;
    let _lock = DaemonLock::acquire(root.path())?.unwrap();
    let token = "b4".repeat(32);
    let server = Server::bind(
        root.path(),
        Token::parse(&token)?,
        journal,
        Manager::new(root.path(), &root.path().join("editor.log"))?,
        &root.path().join("workspaces"),
    )?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let mut stream =
        BufReader::new(tokio::net::UnixStream::connect(root.path().join("rpc.sock")).await?);
    let reply_id = Uuid::now_v7();
    let reply = json!({"opId":reply_id,"questionId":question,"revision":1,"answer":{"action":"reply","text":"The saved answer"}});
    for (method, params) in [
        ("rpc.authenticate", json!({"token":token})),
        ("report.respond", reply.clone()),
        ("report.respond", reply),
        (
            "utterance.resolve",
            json!({"opId":Uuid::now_v7(),"utteranceId":reply_id,"revision":1,"resolution":{"action":"retry","target":{"mode":"new"}}}),
        ),
        ("system.status", json!({})),
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
        let value: serde_json::Value = serde_json::from_str(&line)?;
        assert!(value.get("result").is_some(), "{value}");
        let result = &value["result"];
        if method == "report.respond" {
            assert_eq!(result["state"], "orphaned");
            assert_eq!(result["revision"], 1);
            assert_eq!(result["replyTo"]["question"]["text"], "Before the change?");
        } else if method == "utterance.resolve" {
            assert_eq!(result["state"], "placed");
            assert!(result["replyTo"].is_null());
        } else if method == "system.status" {
            assert_eq!(result["reportQuestions"][0]["text"], "After the change?");
            assert_eq!(result["reportQuestions"][0]["state"], "open");
        }
    }
    stop.send(()).unwrap();
    task.await??;
    Ok(())
}

#[tokio::test]
async fn a_question_removed_from_a_report_is_changed_instead_of_answered() -> Result<()> {
    let (_root, journal, track, _) = fixture().await?;
    let mut report = Report::new(
        track,
        "session".into(),
        Uuid::now_v7(),
        "removed".into(),
        "Waiting for you: Original question?".into(),
    );
    journal.observe_report(report.clone()).await?;
    let question = journal.snapshot().await?["reportQuestions"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    report.text = "Finished without a question.".into();
    let updated = journal.observe_report(report).await?.report;
    journal
        .analyze_report(updated.id, updated.revision, Analysis::Clear)
        .await?;
    let saved = journal
        .respond_report(
            Uuid::now_v7(),
            question,
            1,
            Answer::Reply {
                text: "The old draft".into(),
            },
        )
        .await?;
    assert_eq!(saved["state"], "orphaned");
    assert_eq!(saved["reason"], "the question changed meanwhile");
    assert_eq!(saved["replyTo"]["question"]["text"], "Original question?");
    Ok(())
}

#[tokio::test]
#[cfg(feature = "fault-injection")]
async fn stale_analysis_is_recorded_atomically_without_changing_the_current_report() -> Result<()> {
    use intercat_daemon::fault_injection::Request as Fault;
    let (root, journal, track, _) = fixture().await?;
    let mut report = Report::new(
        track,
        "session".into(),
        Uuid::now_v7(),
        "audit".into(),
        "Old dummy report".into(),
    );
    let old = journal.observe_report(report.clone()).await?.report;
    report.text = "A newer dummy report needs review".into();
    let current = journal.observe_report(report).await?.report;
    let before = journal.snapshot().await?;
    journal.test_fault(Fault::Fill).await?;
    assert!(
        journal
            .analyze_report(old.id.clone(), old.revision, Analysis::Clear)
            .await
            .is_err()
    );
    journal.test_fault(Fault::Restore).await?;
    let db = rusqlite::Connection::open(root.path().join("events.sqlite"))?;
    let stale_count = || {
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='report.analysis-stale'",
            [],
            |r| r.get::<_, i64>(0),
        )
    };
    assert_eq!(stale_count()?, 0);
    let seq = journal
        .analyze_report(old.id.clone(), old.revision, Analysis::Clear)
        .await?;
    let event: String = db.query_row(
        "SELECT payload FROM events WHERE seq=?1 AND kind='report.analysis-stale'",
        [seq],
        |r| r.get(0),
    )?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&event)?,
        json!({"reportId":old.id,"revision":old.revision,"state":"stale"})
    );
    assert_eq!(journal.snapshot().await?["reports"], before["reports"]);
    assert_eq!(
        journal.snapshot().await?["reportQuestions"],
        before["reportQuestions"]
    );
    journal
        .analyze_report(current.id.clone(), current.revision, Analysis::Clear)
        .await?;
    assert!(
        journal.snapshot().await?["reports"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    journal
        .analyze_report(current.id, current.revision, Analysis::Review)
        .await?;
    assert!(
        journal.snapshot().await?["reportQuestions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(stale_count()?, 2);
    Ok(())
}

#[tokio::test]
async fn removed_reports_consume_late_results_and_keep_the_writer_available() -> Result<()> {
    let (root, journal, track, _) = fixture().await?;
    let old = journal
        .observe_report(Report::new(
            track.clone(),
            "session".into(),
            Uuid::now_v7(),
            "removed".into(),
            "Dummy removed report".into(),
        ))
        .await?
        .report;
    let current = journal
        .observe_report(Report::new(
            track,
            "session".into(),
            Uuid::now_v7(),
            "current".into(),
            "Dummy current report".into(),
        ))
        .await?
        .report;
    let db = rusqlite::Connection::open(root.path().join("events.sqlite"))?;
    db.execute("DELETE FROM reports WHERE id=?1", [&old.id])?;
    journal
        .analyze_report(old.id.clone(), old.revision, Analysis::Review)
        .await?;
    let seq = journal
        .analyze_background_reports(vec![(old.clone(), Analysis::Review)])
        .await?;
    let batch: String = db.query_row(
        "SELECT payload FROM events WHERE seq=?1 AND kind='report.batch'",
        [seq],
        |r| r.get(0),
    )?;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&batch)?["reports"],
        json!([{"reportId":old.id,"revision":old.revision,"state":"stale"}])
    );
    journal
        .analyze_report(current.id, current.revision, Analysis::Clear)
        .await?;
    let after = journal.snapshot().await?;
    assert!(after["reports"].as_array().unwrap().is_empty());
    assert!(after["reportQuestions"].as_array().unwrap().is_empty());
    drop(journal);
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    assert!(
        journal.snapshot().await?["reportQuestions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='report.analysis-stale'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}
