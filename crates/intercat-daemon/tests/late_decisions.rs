use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Resolution, Target},
    decision_review::Choice,
    edit_hold::{Action, Edit, Request},
    input_readings::Ticket,
    journal::Journal,
    secrets::Key,
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    Ok((root, journal))
}
async fn stage(journal: &Journal, target: Target) -> Result<Ticket> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, "Dummy request".into(), target, None, vec![])
        .await?;
    let input = journal.claim_input().await?.unwrap();
    Ok(Ticket {
        input: input.id,
        revision: input.revision,
        index: input.reading_index,
    })
}
fn outcome(track: Option<&str>) -> Outcome {
    Outcome::Classified(Decision {
        kind: if track.is_some() {
            Kind::Extend
        } else {
            Kind::Fork
        },
        track_id: track.map(str::to_owned),
        boundary: Some("after-turn".into()),
        command: None,
        confidence: 1.0,
        inference_ms: 1300.0,
        timed_out: true,
        scores: json!({}),
    })
}
async fn word(journal: &Journal, id: &str) -> Result<Value> {
    Ok(journal.snapshot().await?["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|word| word["id"] == id)
        .unwrap()
        .clone())
}

#[tokio::test]
async fn late_result_is_only_a_hint_and_survives_replay_and_restart() -> Result<()> {
    let (root, journal) = fixture().await?;
    let ticket = stage(&journal, Target::None).await?;
    journal.expire_input(ticket.clone()).await?;
    let before = word(&journal, &ticket.input).await?;
    assert_eq!(before["state"], "asked");
    assert_eq!(before["revision"], 1);
    assert!(before["decision"].is_null());
    assert!(before["deadline"]["recommendation"].is_null());
    journal
        .finish_reading(ticket.clone(), outcome(None))
        .await?;
    journal
        .finish_reading(ticket.clone(), outcome(Some("invalid")))
        .await?;
    let after = word(&journal, &ticket.input).await?;
    assert_eq!(after["state"], "asked");
    assert_eq!(after["revision"], before["revision"]);
    assert_eq!(after["decision"], before["decision"]);
    assert_eq!(after["deadline"]["recommendation"], json!({"trackId":null}));
    assert_eq!(journal.snapshot().await?["tracks"], json!([]));
    drop(journal);
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    assert_eq!(
        word(&journal, &ticket.input).await?["deadline"],
        after["deadline"]
    );
    Ok(())
}

#[tokio::test]
async fn user_actions_and_editing_make_late_results_stale() -> Result<()> {
    for action in ["hold", "cut", "retry", "edit"] {
        let (root, journal) = fixture().await?;
        let ticket = stage(&journal, Target::None).await?;
        journal.expire_input(ticket.clone()).await?;
        match action {
            "cut" => {
                journal
                    .cut_input(Uuid::now_v7(), ticket.input.clone())
                    .await?;
            }
            "edit" => {
                journal
                    .edit_input(Request {
                        owner: Uuid::now_v7(),
                        action: Action::Begin {
                            token: Uuid::now_v7(),
                            input: ticket.input.clone(),
                            revision: 1,
                        },
                    })
                    .await?;
            }
            _ => {
                journal
                    .resolve_input(
                        Uuid::now_v7(),
                        ticket.input.clone(),
                        1,
                        if action == "hold" {
                            Resolution::Hold
                        } else {
                            Resolution::Retry {
                                target: Target::New,
                            }
                        },
                    )
                    .await?;
            }
        }
        let before = word(&journal, &ticket.input).await?;
        journal
            .finish_reading(ticket.clone(), outcome(None))
            .await?;
        let after = word(&journal, &ticket.input).await?;
        assert_eq!(after["state"], before["state"], "{action}");
        assert_eq!(after["revision"], before["revision"], "{action}");
        assert!(after["deadline"].is_null(), "{action}");
        let reading: String = rusqlite::Connection::open(root.path().join("events.sqlite"))?
            .query_row(
                "SELECT state FROM input_readings WHERE input=?1 AND number=?2",
                rusqlite::params![ticket.input, ticket.index],
                |row| row.get(0),
            )?;
        assert_eq!(reading, "stale", "{action}");
        assert_eq!(journal.snapshot().await?["tracks"], json!([]));
    }
    Ok(())
}

#[tokio::test]
async fn a_locked_deadline_requires_explicit_instruction_and_ignores_late_result() -> Result<()> {
    let (root, journal) = fixture().await?;
    rusqlite::Connection::open(root.path().join("events.sqlite"))?.execute(
        "INSERT INTO tracks VALUES('a','A','Dummy track','waiting')",
        [],
    )?;
    let ticket = stage(&journal, Target::Lock("a".into())).await?;
    journal.expire_input(ticket.clone()).await?;
    assert_eq!(
        word(&journal, &ticket.input).await?["deadline"]["trackId"],
        "a"
    );
    let op = Uuid::now_v7();
    for _ in 0..2 {
        let accepted = journal
            .resolve_input(
                op,
                ticket.input.clone(),
                1,
                Resolution::Confirm {
                    choice: Choice::Instruction,
                },
            )
            .await?;
        assert_eq!(accepted["state"], "placed");
        assert_eq!(accepted["decision"]["trackId"], "a");
        assert_eq!(accepted["decision"]["kind"], "extend");
        assert_eq!(accepted["decision"]["boundary"], "after-turn");
        assert_eq!(accepted["revision"], 2);
    }
    journal
        .finish_reading(ticket.clone(), outcome(None))
        .await?;
    assert!(word(&journal, &ticket.input).await?["deadline"].is_null());
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn an_explicit_new_track_can_be_confirmed_after_a_deadline_without_rereading() -> Result<()> {
    use intercat_daemon::confirmation::{Accept, Action, Change, Policy};
    for confirmation in [false, true] {
        let (_root, journal) = fixture().await?;
        if confirmation {
            journal
                .change_confirmation(Change {
                    op_id: Uuid::now_v7(),
                    revision: journal.snapshot().await?["confirmation"]["revision"]
                        .as_u64()
                        .unwrap(),
                    action: Action::Policy {
                        policy: Policy::Always,
                    },
                })
                .await?;
        }
        let ticket = stage(&journal, Target::New).await?;
        journal.expire_input(ticket.clone()).await?;
        journal
            .finish_reading(ticket.clone(), outcome(None))
            .await?;
        assert_eq!(journal.snapshot().await?["tracks"], json!([]));
        let op = Uuid::now_v7();
        let selected = journal
            .resolve_input(
                op,
                ticket.input.clone(),
                1,
                Resolution::Confirm {
                    choice: Choice::Instruction,
                },
            )
            .await?;
        assert_eq!(selected["decision"]["kind"], "fork");
        assert_eq!(selected["decision"]["boundary"], "now");
        assert_eq!(
            journal
                .resolve_input(
                    op,
                    ticket.input.clone(),
                    1,
                    Resolution::Confirm {
                        choice: Choice::Instruction
                    }
                )
                .await?,
            selected
        );
        if confirmation {
            assert_eq!(selected["state"], "asked");
            assert_eq!(journal.snapshot().await?["tracks"], json!([]));
            journal
                .confirm_input(Accept {
                    op_id: Uuid::now_v7(),
                    utterance_id: ticket.input.parse()?,
                    revision: 2,
                    decision_id: selected["confirmation"]["decisionId"]
                        .as_str()
                        .unwrap()
                        .parse()?,
                })
                .await?;
        }
        assert_eq!(word(&journal, &ticket.input).await?["state"], "placed");
        assert_eq!(
            journal.snapshot().await?["tracks"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        journal
            .finish_reading(ticket.clone(), outcome(Some("old-target")))
            .await?;
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
async fn an_unassigned_deadline_cannot_create_a_track_without_a_destination_choice() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let ticket = stage(&journal, Target::None).await?;
    journal.expire_input(ticket.clone()).await?;
    assert!(
        journal
            .resolve_input(
                Uuid::now_v7(),
                ticket.input,
                1,
                Resolution::Confirm {
                    choice: Choice::Instruction
                }
            )
            .await
            .is_err()
    );
    assert_eq!(journal.snapshot().await?["tracks"], json!([]));
    Ok(())
}

#[tokio::test]
async fn pending_deadline_is_not_revived_after_recovery_or_track_removal() -> Result<()> {
    let (root, journal) = fixture().await?;
    let db = rusqlite::Connection::open(root.path().join("events.sqlite"))?;
    db.execute(
        "INSERT INTO tracks VALUES('a','A','Dummy track','waiting')",
        [],
    )?;
    let ticket = stage(&journal, Target::None).await?;
    journal.expire_input(ticket.clone()).await?;
    journal
        .finish_reading(ticket.clone(), outcome(Some("a")))
        .await?;
    assert_eq!(
        word(&journal, &ticket.input).await?["deadline"]["recommendation"]["trackId"],
        "a"
    );
    db.execute("DELETE FROM tracks WHERE id='a'", [])?;
    assert!(word(&journal, &ticket.input).await?["deadline"]["recommendation"].is_null());
    let pending = stage(&journal, Target::None).await?;
    journal.expire_input(pending.clone()).await?;
    drop(journal);
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    journal
        .finish_reading(pending.clone(), outcome(None))
        .await?;
    assert!(word(&journal, &pending.input).await?["deadline"].is_object());
    assert!(word(&journal, &pending.input).await?["deadline"]["recommendation"].is_null());
    assert_eq!(word(&journal, &pending.input).await?["state"], "asked");
    Ok(())
}

#[tokio::test]
async fn obsolete_ticket_does_not_expire_a_new_reading_and_cancelled_edit_stays_neutral()
-> Result<()> {
    let (_root, journal) = fixture().await?;
    let ticket = stage(&journal, Target::None).await?;
    let wrong = Ticket {
        index: ticket.index + 1,
        ..ticket.clone()
    };
    journal.expire_input(wrong).await?;
    assert_eq!(word(&journal, &ticket.input).await?["state"], "reading");
    journal.expire_input(ticket.clone()).await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let edit = journal
        .edit_input(Request {
            owner,
            action: Action::Begin {
                token,
                input: ticket.input.clone(),
                revision: 1,
            },
        })
        .await?;
    journal
        .finish_reading(ticket.clone(), outcome(None))
        .await?;
    journal
        .edit_input(Request {
            owner,
            action: Action::End {
                token,
                revision: edit["revision"].as_i64().unwrap(),
                edit: Edit::Cancel,
            },
        })
        .await?;
    assert!(word(&journal, &ticket.input).await?["deadline"].is_null());
    assert_eq!(word(&journal, &ticket.input).await?["state"], "asked");
    Ok(())
}

#[tokio::test]
async fn a_late_second_pass_releases_foreground_and_only_adds_a_recommendation() -> Result<()> {
    use intercat_daemon::{
        editor::{Config, Manager},
        input::Processor,
    };
    use std::{fs, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
    let (root, journal) = fixture().await?;
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let python = root.path().join("dummy-python");
    fs::write(
        &python,
        format!(
            "#!/usr/bin/python3\n{}",
            fs::read_to_string(project.join("tests/fixtures/late_editor.py"))?
        ),
    )?;
    fs::set_permissions(&python, fs::Permissions::from_mode(0o700))?;
    let model = root.path().join("model");
    fs::create_dir(&model)?;
    rusqlite::Connection::open(root.path().join("events.sqlite"))?.execute(
        "INSERT INTO tracks VALUES('a','A','Arithmetic','waiting')",
        [],
    )?;
    let editor = Manager::new(
        &project.join("services/editor/src"),
        &root.path().join("editor.log"),
    )?;
    editor.configure(Config { python, model }).await?;
    let wake = Arc::new(tokio::sync::Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone()).await?;
    let slow = Uuid::now_v7();
    journal
        .stage_utterance(slow, "slow dummy".into(), Target::None, None, vec![])
        .await?;
    let wait_state = |id: String, state: &'static str| {
        let journal = journal.clone();
        async move {
            tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    let word = word(&journal, &id).await?;
                    if word["state"] == state {
                        return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(word);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await?
        }
    };
    let asked = wait_state(slow.to_string(), "asked").await?;
    assert!(root.path().join("second-pass-started").exists());
    assert_eq!(asked["reading"]["state"], "late");
    assert!(asked["deadline"]["recommendation"].is_null());
    assert_eq!(asked["revision"], 1);
    let fast = Uuid::now_v7();
    journal
        .stage_utterance(fast, "fast dummy".into(), Target::New, None, vec![])
        .await?;
    wake.notify_one();
    wait_state(fast.to_string(), "placed").await?;
    assert_eq!(
        word(&journal, &slow.to_string()).await?["reading"]["state"],
        "late"
    );
    fs::write(root.path().join("release-second-pass"), "")?;
    let after = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let value = word(&journal, &slow.to_string()).await?;
            if value["reading"]["state"] == "complete" {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(value);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert_eq!(after["state"], "asked");
    assert_eq!(after["revision"], asked["revision"]);
    assert!(after["decision"].is_null());
    assert_eq!(after["deadline"]["recommendation"]["trackId"], "a");
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let db = rusqlite::Connection::open(root.path().join("events.sqlite"))?;
    let passes: Vec<(i64, String)> = db.prepare("SELECT json_extract(payload,'$.pass'),json_extract(payload,'$.outcome') FROM events WHERE kind='input.inference' AND json_extract(payload,'$.utteranceId')=?1 ORDER BY seq")?
        .query_map([slow.to_string()], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    assert_eq!(passes, vec![(1, "complete".into()), (2, "late".into())]);
    journal
        .resolve_input(
            Uuid::now_v7(),
            slow.to_string(),
            1,
            Resolution::Retry {
                target: Target::Lock("a".into()),
            },
        )
        .await?;
    wake.notify_one();
    let reread = wait_state(slow.to_string(), "placed").await?;
    assert_eq!(reread["revision"], 2);
    assert_eq!(reread["decision"]["kind"], "extend");
    assert_eq!(reread["decision"]["trackId"], "a");
    processor.shutdown().await?;
    editor.stop().await;
    Ok(())
}
