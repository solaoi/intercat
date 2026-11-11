use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Target},
    descriptions::{Description, Finish},
    edit_hold::{Action, Request},
    journal::Journal,
    secrets::Key,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn outcome(kind: Kind) -> Outcome {
    Outcome::Classified(Decision {
        kind,
        track_id: None,
        boundary: None,
        command: None,
        confidence: 1.0,
        inference_ms: 1.0,
        timed_out: false,
        scores: json!({}),
    })
}
fn description() -> Finish {
    Finish::Complete(Description {
        detail: "The recorded request is ready.".into(),
        line: "I placed the request.".into(),
        title: "Arithmetic tests".into(),
    })
}
async fn fixture(kind: Kind) -> Result<(tempfile::TempDir, Journal, String)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    let id = place(&journal, kind).await?;
    Ok((root, journal, id))
}
async fn place(journal: &Journal, kind: Kind) -> Result<String> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(
            id,
            "Add arithmetic tests".into(),
            Target::None,
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(input.id, input.revision, outcome(kind))
        .await?;
    Ok(id.to_string())
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
async fn a_description_survives_read_only_editing_but_not_take_back() -> Result<()> {
    let (_root, journal, id) = fixture(Kind::Fork).await?;
    let ticket = journal.claim_description().await?.unwrap();
    journal.finish_description(ticket, description()).await?;
    let before = word(&journal, &id).await?;
    assert_eq!(
        before["decision"]["description"]["title"],
        "Arithmetic tests"
    );
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    journal
        .edit_input(Request {
            owner,
            action: Action::Begin {
                token,
                input: id.clone(),
                revision: 1,
            },
        })
        .await?;
    assert_eq!(
        word(&journal, &id).await?["decision"]["description"],
        before["decision"]["description"]
    );
    journal
        .edit_input(Request {
            owner,
            action: Action::End {
                token,
                revision: 2,
                edit: intercat_daemon::edit_hold::Edit::Cancel,
            },
        })
        .await?;
    journal.cut_input(Uuid::now_v7(), id.clone()).await?;
    assert!(word(&journal, &id).await?["decision"]["description"].is_null());
    Ok(())
}

#[tokio::test]
async fn a_new_reading_discards_an_old_description_even_with_the_same_decision() -> Result<()> {
    let (_root, journal, id) = fixture(Kind::Park).await?;
    let old = journal.claim_description().await?.unwrap();
    place(&journal, Kind::Fork).await?;
    let reading = journal.claim_reread().await?.unwrap();
    journal
        .apply_reread(reading.ticket, outcome(Kind::Park))
        .await?;
    assert!(!journal.description_current(old.clone()).await?);
    journal.finish_description(old, description()).await?;
    assert!(word(&journal, &id).await?["decision"]["description"].is_null());
    Ok(())
}

#[tokio::test]
async fn only_one_retry_is_used_and_interruption_does_not_spend_it() -> Result<()> {
    let (_root, journal, _id) = fixture(Kind::Fork).await?;
    let first = journal.claim_description().await?.unwrap();
    journal
        .finish_description(first.clone(), Finish::Interrupted)
        .await?;
    let repeated = journal.claim_description().await?.unwrap();
    assert_ne!(first.request.input, repeated.request.input);
    journal.finish_description(repeated, Finish::Failed).await?;
    let second = journal.claim_description().await?.unwrap();
    journal.finish_description(second, Finish::Failed).await?;
    assert!(journal.claim_description().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn completed_text_survives_restart_and_a_duplicate_result_cannot_replace_it() -> Result<()> {
    let (root, journal, id) = fixture(Kind::Fork).await?;
    let ticket = journal.claim_description().await?.unwrap();
    journal
        .finish_description(ticket.clone(), description())
        .await?;
    journal
        .finish_description(
            ticket,
            Finish::Complete(Description {
                detail: "Wrong late explanation".into(),
                line: "Late".into(),
                title: "Late".into(),
            }),
        )
        .await?;
    drop(journal);
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    assert_eq!(
        word(&journal, &id).await?["decision"]["description"]["title"],
        "Arithmetic tests"
    );
    assert!(journal.claim_description().await?.is_none());
    Ok(())
}

#[tokio::test]
#[cfg(feature = "fault-injection")]
async fn a_failed_description_write_can_retry_the_same_result() -> Result<()> {
    use intercat_daemon::fault_injection::Request as Fault;
    let (_root, journal, id) = fixture(Kind::Fork).await?;
    let before = journal.snapshot().await?;
    let ticket = journal.claim_description().await?.unwrap();
    journal.test_fault(Fault::Fill).await?;
    assert!(
        journal
            .finish_description(ticket.clone(), description())
            .await
            .is_err()
    );
    journal.test_fault(Fault::Restore).await?;
    let recovered = journal.snapshot().await?;
    assert_eq!(recovered["session"], before["session"]);
    assert_eq!(recovered["tracks"], before["tracks"]);
    assert!(journal.description_current(ticket.clone()).await?);
    journal.finish_description(ticket, description()).await?;
    assert_eq!(
        word(&journal, &id).await?["decision"]["description"]["title"],
        "Arithmetic tests"
    );
    let saved = journal.snapshot().await?;
    assert_eq!(saved["session"]["name"], "Arithmetic tests");
    assert_eq!(saved["tracks"][0]["title"], "Arithmetic tests");
    Ok(())
}

#[test]
fn generated_text_requires_the_exact_schema() {
    for text in [
        "not JSON",
        "{}",
        r#"{"detail":"why","line":"ok","title":"title","command":"run"}"#,
    ] {
        assert!(Description::parse(text).is_err());
    }
}

async fn local_editor(
    root: &std::path::Path,
    real: bool,
    journal: &Journal,
) -> Result<intercat_daemon::editor::Manager> {
    use std::os::unix::fs::PermissionsExt;
    // Real requests pass the privacy gate before background explanations run.
    // Measure that cold initialization separately from scheduling/preemption.
    let started = std::time::Instant::now();
    journal
        .preview_privacy(vec!["Dummy description input 123".into()])
        .await?;
    eprintln!("Privacy scanner initialized in {:?}", started.elapsed());
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let editor = intercat_daemon::editor::Manager::new(
        &project.join("services/editor/src"),
        &root.join("editor.log"),
    )?;
    let config = if real {
        intercat_daemon::editor::Config {
            python: std::env::var("INTERCAT_TEST_EDITOR_PYTHON")?.into(),
            model: std::env::var("INTERCAT_TEST_EDITOR_MODEL")?.into(),
        }
    } else {
        let python = root.join("python");
        std::fs::write(
            &python,
            format!(
                "#!/usr/bin/python3\n{}",
                std::fs::read_to_string(project.join("tests/fixtures/park_editor.py"))?
            ),
        )?;
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700))?;
        let model = root.join("model");
        std::fs::create_dir(&model)?;
        intercat_daemon::editor::Config { python, model }
    };
    editor.configure(config).await?;
    Ok(editor)
}

async fn wait_description(journal: &Journal, id: &str) -> Result<Value> {
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let value = word(journal, id).await?;
            if value["decision"]["description"].is_object() {
                return Ok(value);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    match result {
        Ok(result) => result,
        Err(_) => Err(format!(
            "Description did not complete for {id}: {}",
            word(journal, id).await?
        )
        .into()),
    }
}

#[tokio::test]
async fn foreground_input_preempts_description_and_partial_text_is_not_saved() -> Result<()> {
    let (root, journal, initial) = fixture(Kind::Fork).await?;
    // This scenario times one paused background request, not completion of an
    // unrelated setup description ahead of it in the queue.
    let setup = journal.claim_description().await?.unwrap();
    assert_eq!(setup.input.id, initial);
    journal.finish_description(setup, description()).await?;
    let slow = Uuid::now_v7();
    journal
        .stage_utterance(slow, "slow explanation".into(), Target::New, None, vec![])
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(input.id, input.revision, outcome(Kind::Fork))
        .await?;
    let editor = local_editor(root.path(), false, &journal).await?;
    let service = intercat_daemon::descriptions::Service::start(journal.clone(), editor.clone());
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !root.path().join("description-running").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| "The paused background description did not start.")?;
    let processor = intercat_daemon::input::Processor::start(
        journal.clone(),
        editor.clone(),
        std::sync::Arc::new(tokio::sync::Notify::new()),
    )
    .await?;
    let next = Uuid::now_v7();
    let start = std::time::Instant::now();
    journal
        .stage_utterance(
            next,
            "new foreground request".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    let new = wait_description(&journal, &next.to_string()).await?;
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(new["state"], "placed");
    assert!(word(&journal, &slow.to_string()).await?["decision"]["description"].is_null());
    std::fs::write(root.path().join("release-description"), "release")?;
    wait_description(&journal, &slow.to_string()).await?;
    service.shutdown().await;
    processor.shutdown().await?;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
#[ignore = "local Glimmer model and Python, no paid API"]
async fn real_glimmer_generates_and_persists_a_description_for_an_accepted_decision() -> Result<()>
{
    let (root, journal, id) = fixture(Kind::Fork).await?;
    let editor = local_editor(root.path(), true, &journal).await?;
    let service = intercat_daemon::descriptions::Service::start(journal.clone(), editor.clone());
    let result = wait_description(&journal, &id).await?;
    assert!(
        result["decision"]["description"]["detail"]
            .as_str()
            .unwrap()
            .contains("test")
    );
    let detail = result["decision"]["description"]["detail"]
        .as_str()
        .unwrap()
        .to_lowercase();
    assert!(
        detail.contains("created") || detail.contains("new"),
        "{detail}"
    );
    assert!(!detail.contains("from worker"), "{detail}");
    let state = journal.snapshot().await?;
    let title = result["decision"]["description"]["title"]
        .as_str()
        .unwrap()
        .trim();
    assert_eq!(state["session"]["name"], title);
    assert_eq!(state["tracks"][0]["title"], title);
    if let Ok(path) = std::env::var("INTERCAT_DESCRIPTION_OUTPUT") {
        let mut evidence = result;
        evidence["namedSession"] = state["session"].clone();
        evidence["namedTracks"] = state["tracks"].clone();
        std::fs::write(path, serde_json::to_vec_pretty(&evidence)?)?;
    }
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn description_arriving_after_dispatch_remains_display_only() -> Result<()> {
    let (_root, journal, id) = fixture(Kind::Fork).await?;
    let ticket = journal.claim_description().await?.unwrap();
    let track = word(&journal, &id).await?["decision"]["trackId"]
        .as_str()
        .unwrap()
        .to_owned();
    journal
        .reserve_turn(
            Uuid::now_v7(),
            id.clone(),
            1,
            track,
            json!({"sessionId":"test"}),
            None,
        )
        .await?;
    journal.finish_description(ticket, description()).await?;
    let saved = word(&journal, &id).await?;
    assert_eq!(saved["state"], "starting");
    assert_eq!(saved["decision"]["kind"], "fork");
    assert_eq!(
        saved["decision"]["description"]["title"],
        "Arithmetic tests"
    );
    Ok(())
}

#[tokio::test]
async fn request_and_generated_description_are_both_screened() -> Result<()> {
    use intercat_daemon::secrets::Registration;
    let (root, journal, _) = fixture(Kind::Fork).await?;
    let secret = "generated-private-value-92349";
    rusqlite::Connection::open(root.path().join("events.sqlite"))?
        .execute("INSERT INTO msp_positions VALUES('s','t',NULL)", [])?;
    let key = Key::new([7; 32]);
    let hex = |bytes: &[u8]| {
        bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    };
    let catalog = journal
        .register_fingerprint(Registration {
            op_id: Uuid::now_v7(),
            track_id: "t".into(),
            session_id: "s".into(),
            name: "TOKEN".into(),
            byte_len: secret.len(),
            tag: hex(&key.tag(secret.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, format!("Explain {secret}"), Target::New, None, vec![])
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(input.id, input.revision, outcome(Kind::Fork))
        .await?;
    std::fs::write(root.path().join("description-secret"), secret)?;
    let editor = local_editor(root.path(), false, &journal).await?;
    let service = intercat_daemon::descriptions::Service::start(journal.clone(), editor.clone());
    let result = wait_description(&journal, &id.to_string()).await?;
    assert!(
        !result["decision"]["description"]
            .to_string()
            .contains(secret)
    );
    let requests = std::fs::read_to_string(root.path().join("model-generations.jsonl"))?;
    assert!(!requests.contains(secret));
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn generated_names_preserve_session_identity_and_later_requests_name_only_their_own_track()
-> Result<()> {
    let (_root, journal, first) = fixture(Kind::Fork).await?;
    let before = journal.snapshot().await?;
    let track = before["tracks"][0]["id"].clone();
    let original_session = before["session"].clone();
    let ticket = journal.claim_description().await?.unwrap();
    journal.finish_description(ticket, description()).await?;
    let renamed = journal.snapshot().await?;
    assert_eq!(renamed["tracks"][0]["title"], "Arithmetic tests");
    assert_eq!(renamed["tracks"][0]["id"], track);
    let mut expected_session = original_session;
    expected_session["name"] = "Arithmetic tests".into();
    assert_eq!(renamed["session"], expected_session);
    assert_eq!(
        word(&journal, &first).await?["text"],
        "Add arithmetic tests"
    );
    let second = place(&journal, Kind::Fork).await?;
    let ticket = journal.claim_description().await?.unwrap();
    assert_eq!(ticket.input.id, second);
    journal
        .finish_description(
            ticket,
            Finish::Complete(Description {
                detail: "Another independent request.".into(),
                line: "Another request.".into(),
                title: "Boundary tests".into(),
            }),
        )
        .await?;
    let after = journal.snapshot().await?;
    assert_eq!(after["session"], expected_session);
    assert_eq!(after["tracks"][0]["title"], "Arithmetic tests");
    assert_eq!(after["tracks"][1]["title"], "Boundary tests");
    Ok(())
}

#[tokio::test]
async fn an_invalid_generated_name_keeps_the_rule_based_names() -> Result<()> {
    for title in ["題".repeat(31), "First\nSecond".into()] {
        let (_root, journal, _id) = fixture(Kind::Fork).await?;
        let before = journal.snapshot().await?;
        let ticket = journal.claim_description().await?.unwrap();
        journal
            .finish_description(
                ticket,
                Finish::Complete(Description {
                    detail: "A recorded assignment.".into(),
                    line: "Request placed.".into(),
                    title,
                }),
            )
            .await?;
        let after = journal.snapshot().await?;
        assert_eq!(after["session"], before["session"]);
        assert_eq!(after["tracks"], before["tracks"]);
    }
    Ok(())
}

#[tokio::test]
async fn a_description_from_before_restart_cannot_rename_the_accepted_destination() -> Result<()> {
    let (root, journal, id) = fixture(Kind::Fork).await?;
    let old = journal.claim_description().await?.unwrap();
    drop(journal);
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    let current = journal.claim_description().await?.unwrap();
    assert_ne!(old.request.input, current.request.input);
    let before = journal.snapshot().await?;
    journal
        .finish_description(
            old.clone(),
            Finish::Complete(Description {
                title: "Obsolete name".into(),
                line: "Obsolete line".into(),
                detail: "Obsolete detail".into(),
            }),
        )
        .await?;
    assert_eq!(journal.snapshot().await?["session"], before["session"]);
    assert_eq!(journal.snapshot().await?["tracks"], before["tracks"]);
    assert!(word(&journal, &id).await?["decision"]["description"].is_null());
    assert!(journal.description_current(current.clone()).await?);
    journal.finish_description(current, description()).await?;
    assert_eq!(
        word(&journal, &id).await?["decision"]["description"]["title"],
        "Arithmetic tests"
    );
    let db = rusqlite::Connection::open(root.path().join("events.sqlite"))?;
    let event: String = db.query_row("SELECT payload FROM events WHERE kind='description.finished' AND json_extract(payload,'$.request')=?1", [&old.request.input], |r| r.get(0))?;
    assert_eq!(serde_json::from_str::<Value>(&event)?["state"], "stale");
    Ok(())
}

#[tokio::test]
async fn all_interpretation_kinds_receive_titles_without_starting_or_reassigning_work() -> Result<()>
{
    for kind in [
        Kind::Extend,
        Kind::Fork,
        Kind::Schedule,
        Kind::Trim,
        Kind::Park,
        Kind::Ask,
        Kind::Plan,
        Kind::Command,
    ] {
        let (_root, journal, _) = fixture(Kind::Fork).await?;
        let first = journal.claim_description().await?.unwrap();
        journal.finish_description(first, description()).await?;
        let id = Uuid::now_v7();
        journal
            .stage_utterance(
                id,
                "Describe the requested cache work".into(),
                Target::None,
                None,
                vec![],
            )
            .await?;
        let input = journal.claim_input().await?.unwrap();
        let mut decision = match outcome(kind.clone()) {
            Outcome::Classified(decision) => decision,
            _ => unreachable!(),
        };
        if matches!(kind, Kind::Command) {
            decision.command = Some("settings".into());
        }
        journal
            .apply_input(input.id, input.revision, Outcome::Classified(decision))
            .await?;
        let before = journal.snapshot().await?;
        let ticket = journal.claim_description().await?.unwrap();
        assert_eq!(ticket.input.id, id.to_string(), "{kind:?}");
        journal
            .finish_description(
                ticket,
                Finish::Complete(Description {
                    title: "Cache request".into(),
                    line: "Requested cache work.".into(),
                    detail: "An interpretation, not a record of execution.".into(),
                }),
            )
            .await?;
        let after = journal.snapshot().await?;
        let word = word(&journal, &id.to_string()).await?;
        assert_eq!(
            word["decision"]["description"]["title"], "Cache request",
            "{kind:?}"
        );
        assert_eq!(after["workers"], before["workers"], "{kind:?}");
        assert_eq!(
            word["state"],
            before["utterances"]
                .as_array()
                .unwrap()
                .iter()
                .find(|value| value["id"] == id.to_string())
                .unwrap()["state"],
            "{kind:?}"
        );
        if !matches!(kind, Kind::Fork) {
            assert_eq!(after["tracks"], before["tracks"], "{kind:?}");
        }
    }
    Ok(())
}

#[tokio::test]
async fn held_interpretations_keep_their_titles_until_taken_back() -> Result<()> {
    let (_root, journal, id) = fixture(Kind::Fork).await?;
    journal
        .hold_inputs(
            Uuid::now_v7(),
            word(&journal, &id).await?["decision"]["trackId"]
                .as_str()
                .unwrap()
                .into(),
        )
        .await?;
    assert_eq!(word(&journal, &id).await?["state"], "held");
    let ticket = journal.claim_description().await?.unwrap();
    journal.finish_description(ticket, description()).await?;
    assert_eq!(
        word(&journal, &id).await?["decision"]["description"]["title"],
        "Arithmetic tests"
    );
    journal.cut_input(Uuid::now_v7(), id.clone()).await?;
    assert!(word(&journal, &id).await?["decision"]["description"].is_null());
    Ok(())
}

#[tokio::test]
async fn an_unconfirmed_fork_gets_a_title_without_creating_a_track() -> Result<()> {
    use intercat_daemon::confirmation::{Action, Change, Policy};
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
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
    let id = place(&journal, Kind::Fork).await?;
    let before = word(&journal, &id).await?;
    assert!(before["confirmation"].is_object());
    assert!(before["decision"]["trackId"].is_null());
    let ticket = journal.claim_description().await?.unwrap();
    assert!(ticket.track_label.is_none());
    journal
        .finish_description(
            ticket,
            Finish::Complete(Description {
                title: "Arithmetic tests".into(),
                line: "Request for an arithmetic-test track.".into(),
                detail: "A proposed new track, awaiting confirmation.".into(),
            }),
        )
        .await?;
    let after = word(&journal, &id).await?;
    assert_eq!(after["confirmation"], before["confirmation"]);
    assert_eq!(
        after["decision"]["description"]["title"],
        "Arithmetic tests"
    );
    assert!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn a_first_ambiguous_request_names_the_session_without_creating_work() -> Result<()> {
    let (root, journal, id) = fixture(Kind::Ask).await?;
    assert_eq!(word(&journal, &id).await?["state"], "asked");
    let ticket = journal.claim_description().await?.unwrap();
    journal
        .finish_description(
            ticket,
            Finish::Complete(Description {
                title: "Parser destination".into(),
                line: "Choose a parser.".into(),
                detail: "The destination needs clarification; no work has started.".into(),
            }),
        )
        .await?;
    let state = journal.snapshot().await?;
    assert_eq!(state["session"]["name"], "Parser destination");
    assert!(state["tracks"].as_array().unwrap().is_empty());
    assert!(state["workers"].as_array().unwrap().is_empty());
    drop(journal);
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    assert_eq!(
        journal.snapshot().await?["session"]["name"],
        "Parser destination"
    );
    assert_eq!(
        word(&journal, &id).await?["decision"]["description"]["title"],
        "Parser destination"
    );
    assert_eq!(word(&journal, &id).await?["state"], "asked");
    Ok(())
}
