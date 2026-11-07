use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    timeline_history::{Action, Request},
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal, String, String)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let id = Uuid::now_v7().to_string();
    journal
        .stage_utterance(
            id.parse()?,
            "First request".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
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
                inference_ms: 0.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let track = journal.snapshot().await?["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    Ok((root, journal, id, track))
}
fn word(snapshot: &Value, id: &str) -> Value {
    snapshot["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|word| word["id"] == id)
        .unwrap()
        .clone()
}
fn request(action: Action) -> Request {
    Request {
        op_id: Uuid::now_v7(),
        action,
    }
}
fn undo(snapshot: &Value) -> Action {
    Action::Undo {
        entry: snapshot["editHistory"]["undo"]["entryId"]
            .as_str()
            .unwrap()
            .into(),
    }
}
fn redo(snapshot: &Value) -> Action {
    Action::Redo {
        entry: snapshot["editHistory"]["redo"]["entryId"]
            .as_str()
            .unwrap()
            .into(),
    }
}

#[tokio::test]
async fn take_back_undo_redo_and_exact_retry_survive_restarting() -> Result<()> {
    let (root, journal, input, _) = fixture().await?;
    journal.cut_input(Uuid::now_v7(), input.clone()).await?;
    let snapshot = journal.snapshot().await?;
    assert_eq!(snapshot["editHistory"]["takenBack"][0]["id"], input);
    let operation = Uuid::now_v7();
    let action = undo(&snapshot);
    let restored = journal
        .edit_history(Request {
            op_id: operation,
            action,
        })
        .await?;
    assert_eq!(word(&restored, &input)["state"], "placed");
    let revision = word(&restored, &input)["revision"].clone();
    drop(journal);
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let repeated = journal
        .edit_history(Request {
            op_id: operation,
            action: undo(&snapshot),
        })
        .await?;
    assert_eq!(word(&repeated, &input)["revision"], revision);
    let cut = journal.edit_history(request(redo(&repeated))).await?;
    assert_eq!(word(&cut, &input)["state"], "cut");
    assert!(cut["editHistory"]["redo"].is_null());
    Ok(())
}

#[tokio::test]
async fn new_timeline_edits_drop_redo_and_stale_heads_cannot_change_the_stack() -> Result<()> {
    let (_root, journal, input, _) = fixture().await?;
    journal.cut_input(Uuid::now_v7(), input.clone()).await?;
    let first = journal.snapshot().await?;
    let restored = journal.edit_history(request(undo(&first))).await?;
    let second = Uuid::now_v7();
    journal
        .stage_utterance(second, "Second request".into(), Target::New, None, vec![])
        .await?;
    journal
        .cut_input(Uuid::now_v7(), second.to_string())
        .await?;
    let current = journal.snapshot().await?;
    assert!(current["editHistory"]["redo"].is_null());
    assert!(
        journal
            .edit_history(request(redo(&restored)))
            .await
            .is_err()
    );
    assert!(journal.edit_history(request(undo(&first))).await.is_err());
    assert_eq!(
        word(&journal.snapshot().await?, &second.to_string())["state"],
        "cut"
    );
    Ok(())
}

#[tokio::test]
async fn a_started_request_stops_history_but_an_older_cut_can_be_restored_individually()
-> Result<()> {
    let (_root, journal, input, track) = fixture().await?;
    let old = Uuid::now_v7();
    journal
        .stage_utterance(old, "Older request".into(), Target::New, None, vec![])
        .await?;
    journal.cut_input(Uuid::now_v7(), old.to_string()).await?;
    journal.cut_input(Uuid::now_v7(), input.clone()).await?;
    let cut = journal.snapshot().await?;
    let restored = journal.edit_history(request(undo(&cut))).await?;
    let revision = word(&restored, &input)["revision"].as_i64().unwrap();
    journal
        .reserve_turn(
            Uuid::now_v7(),
            input.clone(),
            revision,
            track,
            json!({"sessionId":"session"}),
        )
        .await?;
    let started = journal.snapshot().await?;
    assert!(
        started["editHistory"]["redo"]["blocked"]
            .as_str()
            .unwrap()
            .contains("has started")
    );
    assert!(journal.edit_history(request(redo(&started))).await.is_err());
    let restored_old = journal
        .edit_history(request(Action::Restore {
            input: old.to_string(),
            revision: 2,
        }))
        .await?;
    assert_eq!(word(&restored_old, &old.to_string())["state"], "held");
    assert_eq!(word(&restored_old, &input)["state"], "starting");
    Ok(())
}

#[tokio::test]
async fn two_windows_cannot_undo_the_same_head_twice() -> Result<()> {
    let (_root, journal, input, _) = fixture().await?;
    journal.cut_input(Uuid::now_v7(), input.clone()).await?;
    let snapshot = journal.snapshot().await?;
    let (one, two) = tokio::join!(
        journal.edit_history(request(undo(&snapshot))),
        journal.edit_history(request(undo(&snapshot)))
    );
    assert_ne!(one.is_ok(), two.is_ok());
    assert_eq!(word(&journal.snapshot().await?, &input)["revision"], 3);
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn disk_full_keeps_the_word_and_history_atomic_and_the_same_cut_can_retry() -> Result<()> {
    use intercat_daemon::fault_injection::Request as Fault;
    let (_root, journal, input, _) = fixture().await?;
    let operation = Uuid::now_v7();
    assert_eq!(
        journal.test_fault(Fault::Fill).await?["databasePagesUnchanged"],
        true
    );
    assert!(journal.cut_input(operation, input.clone()).await.is_err());
    let failed = journal.snapshot().await?;
    assert_eq!(word(&failed, &input)["state"], "placed");
    assert!(failed["editHistory"]["undo"].is_null());
    journal.test_fault(Fault::Restore).await?;
    journal.cut_input(operation, input.clone()).await?;
    let recovered = journal.snapshot().await?;
    assert_eq!(word(&recovered, &input)["state"], "cut");
    assert_eq!(recovered["editHistory"]["undo"]["label"], "Take back");
    Ok(())
}

#[tokio::test]
async fn undo_does_not_answer_a_question_that_changed_after_take_back() -> Result<()> {
    use intercat_daemon::reports::{Answer, Report};
    let (_root, journal, _, track) = fixture().await?;
    let mut report = Report::new(
        track,
        "worker-session".into(),
        Uuid::now_v7(),
        "question".into(),
        "Waiting for you: Original?".into(),
    );
    journal.observe_report(report.clone()).await?;
    let question = journal.snapshot().await?["reportQuestions"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let reply = Uuid::now_v7();
    journal
        .respond_report(
            reply,
            question,
            1,
            Answer::Reply {
                text: "My answer".into(),
            },
        )
        .await?;
    journal.cut_input(Uuid::now_v7(), reply.to_string()).await?;
    let cut = journal.snapshot().await?;
    report.text = "Waiting for you: Changed?".into();
    journal.observe_report(report).await?;
    let restored = journal.edit_history(request(undo(&cut))).await?;
    assert_eq!(word(&restored, &reply.to_string())["state"], "orphaned");
    assert_eq!(
        word(&restored, &reply.to_string())["replyTo"]["question"]["text"],
        "Original?"
    );
    assert_eq!(restored["reportQuestions"][0]["state"], "open");
    Ok(())
}

#[tokio::test]
async fn native_history_rpc_replays_an_undo_without_repeating_it() -> Result<()> {
    use intercat_daemon::{
        editor::Manager,
        lock::DaemonLock,
        rpc::{Server, Token},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (_data, journal, input, _) = fixture().await?;
    journal.cut_input(Uuid::now_v7(), input.clone()).await?;
    let snapshot = journal.snapshot().await?;
    let entry = snapshot["editHistory"]["undo"]["entryId"].clone();
    let root = tempfile::tempdir_in("/private/tmp")?;
    let _lock = DaemonLock::acquire(root.path())?.unwrap();
    let token = "a7".repeat(32);
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
    let undo = json!({"opId":Uuid::now_v7(),"action":{"kind":"undo","entryId":entry}});
    for (method, params) in [
        ("rpc.authenticate", json!({"token":token})),
        ("timeline.history", undo.clone()),
        ("timeline.history", undo),
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
        let response: Value = serde_json::from_str(&line)?;
        assert!(response.get("result").is_some(), "{response}");
        if method == "timeline.history" {
            let saved = word(&response["result"], &input);
            assert_eq!(saved["state"], "placed");
            assert_eq!(saved["revision"], 3);
        }
    }
    stop.send(()).unwrap();
    task.await??;
    Ok(())
}

#[tokio::test]
async fn cancelling_an_unchanged_editor_keeps_the_restore_undoable() -> Result<()> {
    use intercat_daemon::edit_hold::{self, Edit};
    let (_root, journal, input, _) = fixture().await?;
    journal.cut_input(Uuid::now_v7(), input.clone()).await?;
    let restored = journal
        .edit_history(request(Action::RestoreDraft {
            input: input.clone(),
            revision: 2,
        }))
        .await?;
    assert_eq!(word(&restored, &input)["state"], "held");
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    journal
        .edit_input(edit_hold::Request {
            owner,
            action: edit_hold::Action::Begin {
                token,
                input: input.clone(),
                revision: 3,
            },
        })
        .await?;
    journal
        .edit_input(edit_hold::Request {
            owner,
            action: edit_hold::Action::End {
                token,
                revision: 4,
                edit: Edit::Cancel,
            },
        })
        .await?;
    let current = journal.snapshot().await?;
    assert!(current["editHistory"]["undo"]["blocked"].is_null());
    let undone = journal.edit_history(request(undo(&current))).await?;
    assert_eq!(word(&undone, &input)["state"], "cut");
    Ok(())
}

#[tokio::test]
async fn hold_can_be_undone_and_redone_and_repeated_hold_is_not_another_edit() -> Result<()> {
    use intercat_daemon::decision::Resolution;
    let (root, journal, input, _) = fixture().await?;
    let operation = Uuid::now_v7();
    let held = journal
        .resolve_input(operation, input.clone(), 1, Resolution::Hold)
        .await?;
    assert_eq!(held["revision"], 2);
    journal
        .resolve_input(operation, input.clone(), 1, Resolution::Hold)
        .await?;
    let first = journal.snapshot().await?;
    assert_eq!(first["editHistory"]["undo"]["label"], "Hold");
    journal
        .resolve_input(Uuid::now_v7(), input.clone(), 2, Resolution::Hold)
        .await?;
    let repeated = journal.snapshot().await?;
    assert_eq!(
        first["editHistory"]["undo"],
        repeated["editHistory"]["undo"]
    );
    drop(journal);
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let restored = journal.edit_history(request(undo(&repeated))).await?;
    assert_eq!(word(&restored, &input)["state"], "placed");
    let held_again = journal.edit_history(request(redo(&restored))).await?;
    assert_eq!(word(&held_again, &input)["state"], "held");
    assert_eq!(word(&held_again, &input)["reason"], "Held by you");
    Ok(())
}

#[tokio::test]
async fn undo_hold_restores_the_original_request_question() -> Result<()> {
    use intercat_daemon::decision::Resolution;
    let (_root, journal, _, _) = fixture().await?;
    let input = Uuid::now_v7();
    journal
        .stage_utterance(input, "Unclear request".into(), Target::None, None, vec![])
        .await?;
    journal.claim_input().await?;
    journal
        .apply_input(
            input.to_string(),
            1,
            Outcome::Asked("Choose where to send this".into()),
        )
        .await?;
    journal
        .resolve_input(Uuid::now_v7(), input.to_string(), 1, Resolution::Hold)
        .await?;
    let held = journal.snapshot().await?;
    let restored = journal.edit_history(request(undo(&held))).await?;
    assert_eq!(word(&restored, &input.to_string())["state"], "asked");
    assert_eq!(
        word(&restored, &input.to_string())["reason"],
        "Choose where to send this"
    );
    Ok(())
}

#[tokio::test]
async fn consecutive_hold_and_take_back_can_be_undone_and_redone_in_order() -> Result<()> {
    use intercat_daemon::decision::Resolution;
    let (_root, journal, input, _) = fixture().await?;
    journal
        .resolve_input(Uuid::now_v7(), input.clone(), 1, Resolution::Hold)
        .await?;
    journal.cut_input(Uuid::now_v7(), input.clone()).await?;
    let cut = journal.snapshot().await?;
    let held = journal.edit_history(request(undo(&cut))).await?;
    assert_eq!(word(&held, &input)["state"], "held");
    let placed = journal.edit_history(request(undo(&held))).await?;
    assert_eq!(word(&placed, &input)["state"], "placed");
    let held = journal.edit_history(request(redo(&placed))).await?;
    assert_eq!(word(&held, &input)["state"], "held");
    let cut = journal.edit_history(request(redo(&held))).await?;
    assert_eq!(word(&cut, &input)["state"], "cut");
    Ok(())
}

#[tokio::test]
async fn later_timeline_edits_cannot_revive_a_hold_superseded_by_another_change() -> Result<()> {
    use intercat_daemon::decision::Resolution;
    let (_root, journal, input, track) = fixture().await?;
    journal
        .resolve_input(Uuid::now_v7(), input.clone(), 1, Resolution::Hold)
        .await?;
    journal
        .resolve_input(
            Uuid::now_v7(),
            input.clone(),
            2,
            Resolution::Retry {
                target: Target::Lock(track.clone()),
            },
        )
        .await?;
    let pending = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            pending.id,
            pending.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(track),
                boundary: Some("after-turn".into()),
                command: None,
                confidence: 1.0,
                inference_ms: 0.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    journal.cut_input(Uuid::now_v7(), input.clone()).await?;
    let cut = journal.snapshot().await?;
    let placed = journal.edit_history(request(undo(&cut))).await?;
    assert!(
        placed["editHistory"]["undo"]["blocked"]
            .as_str()
            .unwrap()
            .contains("changed")
    );
    assert!(journal.edit_history(request(undo(&placed))).await.is_err());
    assert_eq!(word(&journal.snapshot().await?, &input)["state"], "placed");
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn a_failed_hold_does_not_publish_either_its_state_or_history() -> Result<()> {
    use intercat_daemon::{decision::Resolution, fault_injection::Request as Fault};
    let (_root, journal, input, _) = fixture().await?;
    let operation = Uuid::now_v7();
    journal.test_fault(Fault::Fill).await?;
    assert!(
        journal
            .resolve_input(operation, input.clone(), 1, Resolution::Hold)
            .await
            .is_err()
    );
    let failed = journal.snapshot().await?;
    assert_eq!(word(&failed, &input)["state"], "placed");
    assert!(failed["editHistory"]["undo"].is_null());
    journal.test_fault(Fault::Restore).await?;
    journal
        .resolve_input(operation, input.clone(), 1, Resolution::Hold)
        .await?;
    let saved = journal.snapshot().await?;
    assert_eq!(word(&saved, &input)["state"], "held");
    assert_eq!(saved["editHistory"]["undo"]["label"], "Hold");
    Ok(())
}
