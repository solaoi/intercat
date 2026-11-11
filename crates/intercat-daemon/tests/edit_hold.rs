use intercat_daemon::{
    Result,
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Target},
    edit_hold::{self, Action, Boundary, Edit, Request},
    journal::Journal,
    runtime::WorkerState,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn decision(kind: Kind, track: Option<String>) -> Outcome {
    Outcome::Classified(Decision {
        kind,
        track_id: track,
        boundary: Some("now".into()),
        command: None,
        confidence: 1.0,
        inference_ms: 0.0,
        timed_out: false,
        scores: json!({}),
    })
}
async fn fixture() -> Result<(tempfile::TempDir, Journal, WorkerState)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let input = stage(&journal).await?;
    journal
        .apply_input(input.clone(), 1, decision(Kind::Fork, None))
        .await?;
    let track = journal.snapshot().await?["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let worker = WorkerState {
        track_id: track,
        phase: "working".into(),
        session_id: Some("session".into()),
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
    Ok((root, journal, worker))
}
async fn stage(journal: &Journal) -> Result<String> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, "Please update this".into(), Target::None, None, vec![])
        .await?;
    Ok(journal.claim_input().await?.unwrap().id)
}
fn begin(owner: Uuid, token: Uuid, input: &str, revision: i64) -> Request {
    Request {
        owner,
        action: Action::Begin {
            token,
            input: input.into(),
            revision,
        },
    }
}
fn end(owner: Uuid, token: Uuid, revision: i64, edit: Edit) -> Request {
    Request {
        owner,
        action: Action::End {
            token,
            revision,
            edit,
        },
    }
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
fn expire(root: &tempfile::TempDir, changed_boot: bool) -> Result<()> {
    let mut db = rusqlite::Connection::open(root.path().join("db"))?;
    intercat_daemon::clock::register(&db)?;
    let mut now = Stamp::now();
    now.mono_ms += 30_001;
    if changed_boot {
        now.boot_id = "restarted-daemon".into();
    }
    edit_hold::expire(&mut db, &now)
}

#[tokio::test]
async fn opening_invalidates_in_flight_classification_and_cancel_resumes_reading() -> Result<()> {
    let (_root, journal, worker) = fixture().await?;
    let input = stage(&journal).await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    let opened = journal.edit_input(begin(owner, token, &input, 1)).await?;
    assert_eq!(opened["revision"], 2);
    assert_eq!(opened["state"], "staged");
    journal
        .apply_input(
            input.clone(),
            1,
            decision(Kind::Trim, Some(worker.track_id)),
        )
        .await?;
    assert!(journal.claim_input().await?.is_none());
    let cancelled = journal
        .edit_input(end(owner, token, 2, Edit::Cancel))
        .await?;
    assert!(cancelled["editHold"].is_null());
    assert_eq!(journal.claim_input().await?.unwrap().revision, 2);
    Ok(())
}

#[tokio::test]
async fn held_edit_blocks_reservation_and_other_window_mutations() -> Result<()> {
    let (_root, journal, worker) = fixture().await?;
    let input = stage(&journal).await?;
    journal
        .apply_input(
            input.clone(),
            1,
            decision(Kind::Extend, Some(worker.track_id.clone())),
        )
        .await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    journal.edit_input(begin(owner, token, &input, 1)).await?;
    assert!(
        journal
            .edit_input(begin(Uuid::now_v7(), Uuid::now_v7(), &input, 2))
            .await
            .is_err()
    );
    assert!(
        journal
            .reserve_turn(
                Uuid::now_v7(),
                input.clone(),
                2,
                worker.track_id,
                json!({"sessionId":"session","commandId":Uuid::now_v7()}),
                None,
            )
            .await
            .is_err()
    );
    assert!(
        journal
            .cut_input(Uuid::now_v7(), input.clone())
            .await
            .is_err()
    );
    assert!(
        journal
            .edit_input(end(Uuid::now_v7(), token, 2, Edit::Cancel))
            .await
            .is_err()
    );
    journal
        .edit_input(end(owner, token, 2, Edit::Cancel))
        .await?;
    Ok(())
}

#[tokio::test]
async fn trim_cannot_claim_interrupt_until_the_edit_is_explicitly_closed() -> Result<()> {
    let (_root, journal, worker) = fixture().await?;
    let input = stage(&journal).await?;
    journal
        .apply_input(
            input.clone(),
            1,
            decision(Kind::Trim, Some(worker.track_id)),
        )
        .await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    journal.edit_input(begin(owner, token, &input, 1)).await?;
    assert!(journal.claim_redirect(input.clone(), 1).await?.is_none());
    assert!(journal.claim_redirect(input.clone(), 2).await?.is_none());
    journal
        .edit_input(end(owner, token, 2, Edit::Cancel))
        .await?;
    assert!(journal.claim_redirect(input.clone(), 2).await?.is_some());
    assert!(journal.claim_redirect(input.clone(), 2).await?.is_none());
    assert!(
        journal
            .edit_input(begin(owner, Uuid::now_v7(), &input, 2))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn timeout_and_daemon_restart_hold_both_send_and_interrupt() -> Result<()> {
    for restart in [false, true] {
        let (root, journal, worker) = fixture().await?;
        let input = stage(&journal).await?;
        journal
            .apply_input(
                input.clone(),
                1,
                decision(Kind::Trim, Some(worker.track_id)),
            )
            .await?;
        let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
        journal.edit_input(begin(owner, token, &input, 1)).await?;
        expire(&root, restart)?;
        let held = word(&journal.snapshot().await?, &input);
        assert_eq!(held["state"], "held");
        assert_eq!(held["reason"], "editing was interrupted");
        assert_eq!(held["revision"], 3);
        assert!(journal.claim_redirect(input.clone(), 2).await?.is_none());
        assert!(journal.claim_input().await?.is_none());
        let closed = journal
            .edit_input(end(owner, token, 2, Edit::Cancel))
            .await?;
        assert_eq!(closed["state"], "held");
    }
    Ok(())
}

#[tokio::test]
async fn late_owner_cannot_overwrite_a_new_lease_but_can_commit_before_replacement() -> Result<()> {
    let (root, journal, _) = fixture().await?;
    let input = stage(&journal).await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    journal.edit_input(begin(owner, token, &input, 1)).await?;
    expire(&root, false)?;
    let text = Edit::Text {
        sources: None,
        text: "Explicit recovery".into(),
    };
    let saved = journal
        .edit_input(end(owner, token, 2, text.clone()))
        .await?;
    assert_eq!(saved["text"], "Explicit recovery");
    assert_eq!(saved["state"], "staged");
    assert_eq!(
        journal.edit_input(end(owner, token, 2, text)).await?["revision"],
        saved["revision"]
    );
    let revision = saved["revision"].as_i64().unwrap();
    let old = Uuid::now_v7();
    journal
        .edit_input(begin(owner, old, &input, revision))
        .await?;
    expire(&root, false)?;
    let new_owner = Uuid::now_v7();
    let new = Uuid::now_v7();
    journal
        .edit_input(begin(new_owner, new, &input, revision + 2))
        .await?;
    assert!(
        journal
            .edit_input(end(
                owner,
                old,
                revision + 1,
                Edit::Text {
                    sources: None,
                    text: "stale".into()
                }
            ))
            .await
            .is_err()
    );
    journal
        .edit_input(end(new_owner, new, revision + 3, Edit::Cancel))
        .await?;
    Ok(())
}

async fn save_pending_finish(journal: &Journal, input: &str, request: &Request) -> Result<()> {
    let current = journal.read_drafts("session".into()).await?;
    let mut book = current["book"].clone();
    let Action::End {
        token, revision, ..
    } = request.action
    else {
        panic!("expected a finish");
    };
    let id = format!("edit:{input}");
    book["active"] = json!(id);
    book["stack"] = json!(["work"]);
    book["entries"][&id] = json!({
        "text":"Saved revision", "target":{"mode":"none"}, "attachments":[],
        "context":{"kind":"edit","inputId":input,"originalText":"original","trackId":null,"state":"editing",
            "lease":{"owner":request.owner,"token":token,"revision":revision}},
        "pending":{"kind":"edit","request":request.action}
    });
    journal
        .save_drafts(serde_json::from_value(json!({
            "session":"session","revision":current["revision"],"parentOpId":current["opId"],
            "opId":Uuid::now_v7(),"book":book
        }))?)
        .await?;
    Ok(())
}

#[tokio::test]
async fn a_reopened_window_can_only_replay_the_exact_durable_finish() -> Result<()> {
    for completed in [false, true] {
        let (root, journal, _) = fixture().await?;
        let input = stage(&journal).await?;
        let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
        journal.edit_input(begin(owner, token, &input, 1)).await?;
        let finish = end(
            owner,
            token,
            2,
            Edit::Text {
                sources: None,
                text: "Saved revision".into(),
            },
        );
        let mut replay = finish.clone();
        replay.owner = Uuid::now_v7();
        assert!(journal.edit_input(replay.clone()).await.is_err());
        save_pending_finish(&journal, &input, &finish).await?;
        if completed {
            journal.edit_input(finish.clone()).await?;
        } else {
            journal
                .edit_input(Request {
                    owner,
                    action: Action::Disconnect,
                })
                .await?;
        }
        drop(journal);
        let journal = Journal::open(
            root.path().join("db"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        assert!(
            journal
                .edit_input(end(
                    replay.owner,
                    token,
                    2,
                    Edit::Text {
                        sources: None,
                        text: "Unrecorded change".into()
                    }
                ))
                .await
                .is_err()
        );
        let saved = journal.edit_input(replay.clone()).await?;
        assert_eq!(saved["text"], "Saved revision");
        let revision = saved["revision"].as_i64().unwrap();
        let next = journal
            .edit_input(begin(Uuid::now_v7(), Uuid::now_v7(), &input, revision))
            .await?;
        assert_eq!(journal.edit_input(replay).await?, next);
    }
    Ok(())
}

#[tokio::test]
async fn a_durable_finish_cannot_replace_a_newer_edit_after_reopening() -> Result<()> {
    let (_root, journal, _) = fixture().await?;
    let input = stage(&journal).await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    journal.edit_input(begin(owner, token, &input, 1)).await?;
    let mut finish = end(
        owner,
        token,
        2,
        Edit::Text {
            sources: None,
            text: "Saved revision".into(),
        },
    );
    save_pending_finish(&journal, &input, &finish).await?;
    journal
        .edit_input(Request {
            owner,
            action: Action::Disconnect,
        })
        .await?;
    let next = journal
        .edit_input(begin(Uuid::now_v7(), Uuid::now_v7(), &input, 3))
        .await?;
    finish.owner = Uuid::now_v7();
    assert!(journal.edit_input(finish).await.is_err());
    assert_eq!(word(&journal.snapshot().await?, &input), next);
    Ok(())
}

#[tokio::test]
async fn read_as_locks_the_destination_and_does_not_interrupt_after_turn() -> Result<()> {
    let (_root, journal, worker) = fixture().await?;
    let input = stage(&journal).await?;
    journal
        .apply_input(
            input.clone(),
            1,
            decision(Kind::Trim, Some(worker.track_id.clone())),
        )
        .await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    journal.edit_input(begin(owner, token, &input, 1)).await?;
    let saved = journal
        .edit_input(end(
            owner,
            token,
            2,
            Edit::ReadAs {
                track: worker.track_id.clone(),
                boundary: Boundary::AfterTurn,
                turn: None,
            },
        ))
        .await?;
    assert_eq!(saved["decision"]["kind"], "schedule");
    assert_eq!(
        saved["target"],
        json!({"mode":"lock","trackId":worker.track_id})
    );
    assert!(journal.claim_redirect(input, 3).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn lost_window_is_held_and_a_renewal_cannot_revive_it() -> Result<()> {
    let (_root, journal, _) = fixture().await?;
    let input = stage(&journal).await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    journal.edit_input(begin(owner, token, &input, 1)).await?;
    journal
        .edit_input(Request {
            owner,
            action: Action::Renew { token },
        })
        .await?;
    journal
        .edit_input(Request {
            owner,
            action: Action::Disconnect,
        })
        .await?;
    assert_eq!(word(&journal.snapshot().await?, &input)["state"], "held");
    assert!(
        journal
            .edit_input(Request {
                owner,
                action: Action::Renew { token }
            })
            .await
            .is_err()
    );
    assert!(journal.claim_input().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn read_as_now_rejects_a_replacement_turn_and_accepts_a_finished_turn() -> Result<()> {
    let (_root, journal, mut worker) = fixture().await?;
    let input = stage(&journal).await?;
    journal
        .apply_input(
            input.clone(),
            1,
            decision(Kind::Extend, Some(worker.track_id.clone())),
        )
        .await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    journal.edit_input(begin(owner, token, &input, 1)).await?;
    let edit = Edit::ReadAs {
        track: worker.track_id.clone(),
        boundary: Boundary::Now,
        turn: Some(intercat_daemon::decision_review::Turn {
            session_id: worker.session_id.clone().unwrap(),
            turn_id: worker.active_turn.unwrap(),
        }),
    };
    worker.active_turn = Some(Uuid::now_v7());
    journal.worker_state(worker.clone()).await?;
    assert!(
        journal
            .edit_input(end(owner, token, 2, edit.clone()))
            .await
            .is_err()
    );
    assert!(journal.claim_redirect(input.clone(), 2).await?.is_none());
    worker.active_turn = None;
    worker.phase = "ready".into();
    journal.worker_state(worker).await?;
    let saved = journal.edit_input(end(owner, token, 2, edit)).await?;
    assert_eq!(saved["decision"]["kind"], "extend");
    assert_eq!(saved["state"], "placed");
    Ok(())
}

#[tokio::test]
async fn a_reserved_start_cannot_be_edited_and_explicit_text_keeps_the_input_identity() -> Result<()>
{
    let (_root, journal, worker) = fixture().await?;
    let input = stage(&journal).await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    journal.edit_input(begin(owner, token, &input, 1)).await?;
    let saved = journal
        .edit_input(end(
            owner,
            token,
            2,
            Edit::Text {
                sources: None,
                text: "A revised instruction".into(),
            },
        ))
        .await?;
    assert_eq!(saved["id"], input);
    assert_eq!(saved["text"], "A revised instruction");
    let reading = journal.claim_input().await?.unwrap();
    assert_eq!(reading.revision, 3);
    journal
        .apply_input(
            input.clone(),
            3,
            decision(Kind::Extend, Some(worker.track_id.clone())),
        )
        .await?;
    journal
        .reserve_turn(
            Uuid::now_v7(),
            input.clone(),
            3,
            worker.track_id,
            json!({"sessionId":"session","commandId":Uuid::now_v7()}),
            None,
        )
        .await?;
    assert!(
        journal
            .edit_input(begin(owner, Uuid::now_v7(), &input, 3))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn rpc_edit_lease_is_owned_by_its_window_and_disconnect_holds_the_request() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (root, journal, _) = fixture().await?;
    let input = stage(&journal).await?;
    journal
        .apply_input(input.clone(), 1, decision(Kind::Park, None))
        .await?;
    let _lock = intercat_daemon::lock::DaemonLock::acquire(root.path())?.unwrap();
    let token = "a5".repeat(32);
    let server = intercat_daemon::rpc::Server::bind(
        root.path(),
        intercat_daemon::rpc::Token::parse(&token)?,
        journal,
        intercat_daemon::editor::Manager::new(root.path(), &root.path().join("editor.log"))?,
        &root.path().join("workspaces"),
    )?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let mut stream =
        BufReader::new(tokio::net::UnixStream::connect(root.path().join("rpc.sock")).await?);
    let (owner, edit_token) = (Uuid::now_v7(), Uuid::now_v7());
    for (method, params, expected) in [
        ("rpc.authenticate", json!({"token":token}), "result"),
        (
            "utterance.edit",
            json!({"owner":owner,"action":{"kind":"begin","token":edit_token,"utteranceId":input,"revision":1}}),
            "result",
        ),
        (
            "utterance.edit",
            json!({"owner":owner,"action":{"kind":"apply","opId":Uuid::now_v7(),"token":edit_token,"revision":2,"edit":{"kind":"text","text":"Changed over RPC"}}}),
            "result",
        ),
        (
            "utterance.edit",
            json!({"owner":Uuid::now_v7(),"action":{"kind":"renew","token":edit_token}}),
            "error",
        ),
        (
            "utterance.edit",
            json!({"owner":owner,"action":{"kind":"disconnect"}}),
            "result",
        ),
        ("system.status", json!({}), "result"),
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
        assert!(response.get(expected).is_some(), "{response}");
        if method == "system.status" {
            assert_eq!(word(&response["result"], &input)["state"], "held");
        }
    }
    stop.send(()).unwrap();
    task.await??;
    Ok(())
}

#[tokio::test]
async fn interrupted_trim_can_resume_only_its_original_running_turn() -> Result<()> {
    let (root, journal, worker) = fixture().await?;
    let input = stage(&journal).await?;
    journal
        .apply_input(
            input.clone(),
            1,
            decision(Kind::Trim, Some(worker.track_id.clone())),
        )
        .await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    journal.edit_input(begin(owner, token, &input, 1)).await?;
    expire(&root, false)?;
    let resumed = journal
        .resolve_input(
            Uuid::now_v7(),
            input.clone(),
            3,
            intercat_daemon::decision::Resolution::Retry {
                target: Target::Lock(worker.track_id.clone()),
            },
        )
        .await?;
    assert_eq!(resumed["state"], "redirecting");
    let redirect = journal.claim_redirect(input, 4).await?.unwrap();
    assert_eq!(redirect.turn_id, worker.active_turn.unwrap());
    Ok(())
}

#[tokio::test]
async fn when_told_waits_for_an_explicit_start_and_cannot_start_while_editing() -> Result<()> {
    let (_root, journal, worker) = fixture().await?;
    let input = stage(&journal).await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    journal.edit_input(begin(owner, token, &input, 1)).await?;
    let scheduled = journal
        .edit_input(end(
            owner,
            token,
            2,
            Edit::ReadAs {
                track: worker.track_id,
                boundary: Boundary::WhenTold,
                turn: None,
            },
        ))
        .await?;
    assert_eq!(scheduled["state"], "placed");
    assert_eq!(scheduled["decision"]["boundary"], "when-told");
    let editing = Uuid::now_v7();
    journal.edit_input(begin(owner, editing, &input, 3)).await?;
    assert!(
        journal
            .start_held_input(Uuid::now_v7(), input.clone(), 4)
            .await
            .is_err()
    );
    journal
        .edit_input(end(owner, editing, 4, Edit::Cancel))
        .await?;
    let started = journal.start_held_input(Uuid::now_v7(), input, 4).await?;
    assert_eq!(started["state"], "placed");
    assert_eq!(started["decision"]["boundary"], "now");
    Ok(())
}

#[tokio::test]
async fn immediate_changes_keep_the_lease_and_replays_do_not_repeat_them() -> Result<()> {
    let (_root, journal, worker) = fixture().await?;
    let input = stage(&journal).await?;
    let (owner, token, op_id) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    journal.edit_input(begin(owner, token, &input, 1)).await?;
    let change = Request {
        owner,
        action: Action::Apply {
            op_id,
            token,
            revision: 2,
            edit: Edit::Text {
                sources: None,
                text: "Updated in place".into(),
            },
        },
    };
    let applied = journal.edit_input(change.clone()).await?;
    assert_eq!(applied["revision"], 3);
    assert_eq!(applied["text"], "Updated in place");
    assert_eq!(applied["editHold"]["revision"], 3);
    assert_eq!(journal.edit_input(change).await?["revision"], 3);
    assert!(journal.claim_input().await?.is_none());
    let placed = journal
        .edit_input(Request {
            owner,
            action: Action::Apply {
                op_id: Uuid::now_v7(),
                token,
                revision: 3,
                edit: Edit::ReadAs {
                    track: worker.track_id.clone(),
                    boundary: Boundary::AfterTurn,
                    turn: None,
                },
            },
        })
        .await?;
    assert_eq!(placed["revision"], 4);
    assert!(!placed["editHold"].is_null());
    assert!(
        journal
            .reserve_turn(
                Uuid::now_v7(),
                input.clone(),
                4,
                worker.track_id.clone(),
                json!({"sessionId":"session","commandId":Uuid::now_v7()}),
                None,
            )
            .await
            .is_err()
    );
    let closed = journal
        .edit_input(end(owner, token, 4, Edit::Cancel))
        .await?;
    assert_eq!(closed["revision"], 4);
    journal
        .reserve_turn(
            Uuid::now_v7(),
            input,
            4,
            worker.track_id,
            json!({"sessionId":"session","commandId":Uuid::now_v7()}),
            None,
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn explicit_change_after_expiry_reclaims_only_the_unchanged_lease() -> Result<()> {
    let (root, journal, _) = fixture().await?;
    let input = stage(&journal).await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    journal.edit_input(begin(owner, token, &input, 1)).await?;
    expire(&root, false)?;
    let changed = journal
        .edit_input(Request {
            owner,
            action: Action::Apply {
                op_id: Uuid::now_v7(),
                token,
                revision: 2,
                edit: Edit::Text {
                    sources: None,
                    text: "Deliberate recovery".into(),
                },
            },
        })
        .await?;
    assert_eq!(changed["revision"], 4);
    assert_eq!(changed["editHold"]["revision"], 4);
    assert!(journal.claim_input().await?.is_none());
    journal
        .edit_input(end(owner, token, 4, Edit::Cancel))
        .await?;
    assert_eq!(
        journal.claim_input().await?.unwrap().text,
        "Deliberate recovery"
    );
    Ok(())
}

#[tokio::test]
async fn take_back_is_atomic_with_releasing_the_edit_and_cancels_an_unsent_trim() -> Result<()> {
    let (_root, journal, worker) = fixture().await?;
    let input = stage(&journal).await?;
    journal
        .apply_input(
            input.clone(),
            1,
            decision(Kind::Trim, Some(worker.track_id)),
        )
        .await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    journal.edit_input(begin(owner, token, &input, 1)).await?;
    let request = end(owner, token, 2, Edit::TakeBack);
    let cut = journal.edit_input(request.clone()).await?;
    assert_eq!(cut["state"], "cut");
    assert!(cut["editHold"].is_null());
    assert_eq!(
        journal.edit_input(request).await?["revision"],
        cut["revision"]
    );
    assert!(journal.claim_redirect(input, 3).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn editing_an_interrupted_orphaned_reply_releases_its_obsolete_question() -> Result<()> {
    let (root, journal, worker) = fixture().await?;
    let mut report = intercat_daemon::reports::Report::new(
        worker.track_id,
        "session".into(),
        Uuid::now_v7(),
        "question".into(),
        "Waiting for you: Original?".into(),
    );
    journal.observe_report(report.clone()).await?;
    let question = journal.snapshot().await?["reportQuestions"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let input = Uuid::now_v7();
    journal
        .respond_report(
            input,
            question,
            1,
            intercat_daemon::reports::Answer::Reply {
                text: "Original answer".into(),
            },
        )
        .await?;
    report.text = "Waiting for you: Updated?".into();
    journal.observe_report(report).await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    journal
        .edit_input(begin(owner, token, &input.to_string(), 2))
        .await?;
    expire(&root, false)?;
    let changed = journal
        .edit_input(end(
            owner,
            token,
            3,
            Edit::Text {
                sources: None,
                text: "An independent request".into(),
            },
        ))
        .await?;
    assert_eq!(changed["state"], "staged");
    assert!(changed["replyTo"].is_null());
    assert_eq!(journal.claim_input().await?.unwrap().id, input.to_string());
    assert_eq!(
        journal.snapshot().await?["reportQuestions"][0]["state"],
        "open"
    );
    Ok(())
}

#[test]
fn an_exact_pending_text_action_keeps_optional_source_edits_absent() -> Result<()> {
    let action = json!({"kind":"end","token":Uuid::now_v7(),"revision":2,"edit":{"kind":"text","text":"An exact saved request"}});
    let decoded: Action = serde_json::from_value(action.clone())?;
    assert_eq!(serde_json::to_value(decoded)?, action);
    Ok(())
}
