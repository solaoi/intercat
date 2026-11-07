use intercat_daemon::{
    Result,
    clip_summaries::{Finish, Summary},
    clock::Stamp,
    journal::Journal,
    secrets::Key,
    timeline::Window,
};
use serde_json::{Value, json};

fn item(id: &str, kind: &str, revision: i64, status: &str) -> Value {
    json!({"method":"item/updated","params":{"sessionId":"session","item":{
        "itemId":id,"turnId":"turn","kind":kind,"revision":revision,"status":status}}})
}

async fn put(journal: &Journal, time: i64, frame: Value) -> Result<()> {
    journal
        .try_msp(
            "track".into(),
            "session".into(),
            Stamp {
                epoch_ms: time + 1000,
                mono_ms: time,
                boot_id: "test".into(),
                timeline_ms: time + 1000,
            },
            frame,
            None,
            None,
        )?
        .await??;
    Ok(())
}

async fn close(journal: &Journal) -> Result<()> {
    put(
        journal,
        100,
        json!({"method":"turn/completed","params":{
        "sessionId":"session","turnId":"turn","terminal":"completed"}}),
    )
    .await
}

async fn window(journal: &Journal, since: Option<i64>) -> Result<Value> {
    journal
        .timeline_window(Window {
            tracks: vec!["track".into()],
            start: 0,
            end: i64::MAX,
            since,
        })
        .await
}

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    put(&journal, 0, item("user", "userMessage", 1, "completed")).await?;
    let mut message = item("answer", "agentMessage", 1, "completed");
    message["params"]["item"]["text"] = "Added the zero-case test. No tests were run.".into();
    put(&journal, 20, message).await?;
    Ok((root, journal))
}

fn summary() -> Finish {
    Finish::Complete(Summary {
        title: "Zero-case test".into(),
        summary: "Added a zero-case test without running the suite.".into(),
    })
}

#[tokio::test]
async fn only_closed_steps_are_summarized_and_a_saved_result_refreshes_the_window() -> Result<()> {
    let (_root, journal) = fixture().await?;
    assert!(journal.claim_clip_summary().await?.is_none());
    close(&journal).await?;
    let before = window(&journal, None).await?;
    let ticket = journal.claim_clip_summary().await?.unwrap();
    assert_eq!(
        ticket.context["items"][1]["text"],
        "Added the zero-case test. No tests were run."
    );
    journal
        .finish_clip_summary(ticket.clone(), summary())
        .await?;
    let after = window(&journal, before["revision"].as_i64()).await?;
    assert_eq!(after["clips"].as_array().unwrap().len(), 1);
    assert_eq!(after["clips"][0]["id"], ticket.clip);
    assert_eq!(after["clips"][0]["title"], "Zero-case test");
    assert_eq!(after["clips"][0]["label"], after["clips"][0]["summary"]);
    for name in [
        "id",
        "trackId",
        "ordinal",
        "sessionId",
        "turnId",
        "start",
        "end",
    ] {
        assert_eq!(after["clips"][0][name], before["clips"][0][name], "{name}");
    }
    assert_eq!(
        journal.clip_detail(ticket.clip).await?["clip"]["title"],
        "Zero-case test"
    );
    assert!(journal.claim_clip_summary().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn late_item_updates_discard_the_old_result_and_allow_a_new_summary() -> Result<()> {
    let (_root, journal) = fixture().await?;
    close(&journal).await?;
    let old = journal.claim_clip_summary().await?.unwrap();
    let mut update = item("answer", "agentMessage", 2, "completed");
    update["params"]["item"]["text"] = "The test now passes.".into();
    put(&journal, 120, update).await?;
    assert!(!journal.clip_summary_current(old.clone()).await?);
    journal.finish_clip_summary(old.clone(), summary()).await?;
    assert!(journal.clip_detail(old.clip.clone()).await?["clip"]["summary"].is_null());
    let new = journal.claim_clip_summary().await?.unwrap();
    assert_eq!(old.clip, new.clip);
    assert_ne!(old.request.input, new.request.input);
    assert_eq!(new.context["items"][1]["text"], "The test now passes.");
    journal.finish_clip_summary(new, summary()).await?;
    Ok(())
}

#[tokio::test]
async fn completed_results_survive_restart_and_duplicate_results_cannot_replace_them() -> Result<()>
{
    let (root, journal) = fixture().await?;
    close(&journal).await?;
    let ticket = journal.claim_clip_summary().await?.unwrap();
    journal
        .finish_clip_summary(ticket.clone(), summary())
        .await?;
    journal
        .finish_clip_summary(
            ticket.clone(),
            Finish::Complete(Summary {
                title: "Late".into(),
                summary: "Wrong late result.".into(),
            }),
        )
        .await?;
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(
        journal.clip_detail(ticket.clip).await?["clip"]["title"],
        "Zero-case test"
    );
    assert!(journal.claim_clip_summary().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn interrupted_generation_and_restart_preserve_the_single_retry_budget() -> Result<()> {
    let (root, journal) = fixture().await?;
    close(&journal).await?;
    let ticket = journal.claim_clip_summary().await?.unwrap();
    journal
        .finish_clip_summary(ticket, Finish::Interrupted)
        .await?;
    let ticket = journal.claim_clip_summary().await?.unwrap();
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let restarted = journal.claim_clip_summary().await?.unwrap();
    assert_ne!(ticket.request.input, restarted.request.input);
    journal
        .finish_clip_summary(restarted, Finish::Failed)
        .await?;
    assert!(journal.claim_clip_summary().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn one_failed_attempt_allows_one_retry() -> Result<()> {
    let (_root, journal) = fixture().await?;
    close(&journal).await?;
    for _ in 0..2 {
        let ticket = journal.claim_clip_summary().await?.unwrap();
        journal.finish_clip_summary(ticket, Finish::Failed).await?;
    }
    assert!(journal.claim_clip_summary().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn deleted_steps_and_recovered_history_cannot_receive_a_generated_summary() -> Result<()> {
    let (root, journal) = fixture().await?;
    close(&journal).await?;
    let ticket = journal.claim_clip_summary().await?.unwrap();
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let record: String = db.query_row(
        "SELECT record FROM timeline_clips WHERE id=?1",
        [&ticket.clip],
        |row| row.get(0),
    )?;
    db.execute("DELETE FROM timeline_clips WHERE id=?1", [&ticket.clip])?;
    assert!(!journal.clip_summary_current(ticket.clone()).await?);
    journal.finish_clip_summary(ticket, summary()).await?;
    let mut recovered: Value = serde_json::from_str(&record)?;
    recovered["id"] = "recovered".into();
    recovered["category"] = "snapshot".into();
    recovered["ordinal"] = 2.into();
    db.execute(
        "INSERT INTO timeline_clips VALUES('recovered','track',2,?1)",
        [recovered.to_string()],
    )?;
    assert!(journal.claim_clip_summary().await?.is_none());
    Ok(())
}

#[tokio::test]
#[cfg(feature = "fault-injection")]
async fn failed_persistence_rolls_back_the_timeline_revision_and_retries_the_same_result()
-> Result<()> {
    use intercat_daemon::fault_injection::Request;
    let (_root, journal) = fixture().await?;
    close(&journal).await?;
    let ticket = journal.claim_clip_summary().await?.unwrap();
    let before = window(&journal, None).await?;
    journal.test_fault(Request::Fill).await?;
    assert!(
        journal
            .finish_clip_summary(ticket.clone(), summary())
            .await
            .is_err()
    );
    journal.test_fault(Request::Restore).await?;
    assert_eq!(window(&journal, None).await?, before);
    assert!(journal.clip_summary_current(ticket.clone()).await?);
    journal.finish_clip_summary(ticket, summary()).await?;
    Ok(())
}

#[tokio::test]
async fn a_shell_tail_uses_its_original_item_and_invalidates_on_a_late_update() -> Result<()> {
    let (root, journal) = fixture().await?;
    let mut shell = item("shell", "toolCall", 1, "inProgress");
    shell["params"]["item"]["tool"] = "bash".into();
    shell["params"]["item"]["args"] = r#"{"command":"npm test"}"#.into();
    put(&journal, 40, shell.clone()).await?;
    close(&journal).await?;
    let first = journal.claim_clip_summary().await?.unwrap();
    journal.finish_clip_summary(first, summary()).await?;
    assert!(journal.claim_clip_summary().await?.is_none());
    shell["params"]["item"]["revision"] = 2.into();
    shell["params"]["item"]["status"] = "completed".into();
    shell["params"]["item"]["visibleOutput"] = "1 test passed".into();
    put(&journal, 150, shell.clone()).await?;
    let ticket = journal.claim_clip_summary().await?.unwrap();
    assert_eq!(ticket.context["category"], "shell");
    assert_eq!(ticket.context["items"][0]["visibleOutput"], "1 test passed");
    journal
        .finish_clip_summary(ticket.clone(), summary())
        .await?;
    shell["params"]["item"]["revision"] = 3.into();
    shell["params"]["item"]["visibleOutput"] = "2 tests passed".into();
    put(&journal, 160, shell).await?;
    assert!(journal.clip_detail(ticket.clip).await?["clip"]["summary"].is_null());
    drop(journal);
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let plan: String = db.query_row("EXPLAIN QUERY PLAN SELECT id FROM timeline_clips WHERE json_extract(record,'$.sessionId')='session' AND json_extract(record,'$.shellItem')='shell'", [], |row| row.get(3))?;
    assert!(plan.contains("clip_summary_shell"), "{plan}");
    Ok(())
}

#[test]
fn summary_schema_rejects_missing_fields_extra_fields_and_invalid_text() {
    for text in [
        "{}",
        "not JSON",
        r#"{"title":"ok","summary":"one","extra":1}"#,
        r#"{"title":"","summary":"one"}"#,
        r#"{"title":"ok","summary":"line\nbreak"}"#,
    ] {
        assert!(Summary::parse(text).is_err());
    }
    assert!(Summary::parse(&json!({"title":"a".repeat(31),"summary":"one"}).to_string()).is_err());
    assert!(Summary::parse(&json!({"title":"a".repeat(30),"summary":"one"}).to_string()).is_ok());
}

async fn thinking_fixture() -> Result<(tempfile::TempDir, Journal)> {
    let (root, journal) = fixture().await?;
    let mut update = item("answer", "reasoning", 2, "inProgress");
    update["params"]["item"]["text"] = "Examining the retry counter.".into();
    put(&journal, 30, update).await?;
    Ok((root, journal))
}

fn view() -> Option<intercat_daemon::clip_summaries::View> {
    Some(intercat_daemon::clip_summaries::View {
        track_id: "track".into(),
        repair_run_id: None,
    })
}

#[tokio::test]
async fn thinking_is_limited_to_viewed_lanes_and_old_observations_cannot_restore_a_selection()
-> Result<()> {
    let (_root, journal) = thinking_fixture().await?;
    assert!(journal.claim_clip_summary().await?.is_none());
    let owner = uuid::Uuid::now_v7();
    let mut wrong = view().unwrap();
    wrong.repair_run_id = Some("repair".into());
    journal.observe_timeline(owner, 1, Some(wrong)).await?;
    assert!(journal.claim_clip_summary().await?.is_none());
    journal.observe_timeline(owner, 2, view()).await?;
    let first = journal.claim_clip_summary().await?.unwrap();
    assert!(matches!(
        first.phase,
        intercat_daemon::clip_summaries::Phase::Thinking
    ));
    journal.observe_timeline(owner, 3, None).await?;
    journal.observe_timeline(owner, 2, view()).await?;
    assert!(!journal.clip_summary_current(first.clone()).await?);
    journal.finish_clip_summary(first, summary()).await?;
    assert!(journal.claim_clip_summary().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn appended_thinking_is_sampled_every_five_seconds_and_keeps_the_previous_summary()
-> Result<()> {
    let (_root, journal) = thinking_fixture().await?;
    journal
        .observe_timeline(uuid::Uuid::now_v7(), 1, view())
        .await?;
    let first = journal.claim_clip_summary().await?.unwrap();
    let mut update = item("answer", "reasoning", 3, "inProgress");
    update["params"]["item"]["text"] =
        "Examining the retry counter. Checking the cancellation branch.".into();
    put(&journal, 40, update.clone()).await?;
    assert!(journal.clip_summary_current(first.clone()).await?);
    journal
        .finish_clip_summary(first.clone(), summary())
        .await?;
    assert_eq!(
        journal.clip_detail(first.clip.clone()).await?["clip"]["title"],
        "Zero-case test"
    );
    assert!(journal.claim_clip_summary().await?.is_none());
    tokio::time::sleep(std::time::Duration::from_millis(5100)).await;
    let next = journal.claim_clip_summary().await?.unwrap();
    assert_eq!(next.clip, first.clip);
    assert_ne!(next.request.input, first.request.input);
    assert!(
        next.context["items"][1]["text"]
            .as_str()
            .unwrap()
            .ends_with("Checking the cancellation branch.")
    );
    assert_eq!(
        journal.clip_detail(first.clip.clone()).await?["clip"]["title"],
        "Zero-case test"
    );
    journal.finish_clip_summary(next, Finish::Failed).await?;
    assert_eq!(
        journal.clip_detail(first.clip).await?["clip"]["title"],
        "Zero-case test"
    );
    Ok(())
}

#[tokio::test]
async fn replacing_thinking_or_starting_tools_invalidates_the_snapshot() -> Result<()> {
    let (_root, journal) = thinking_fixture().await?;
    journal
        .observe_timeline(uuid::Uuid::now_v7(), 1, view())
        .await?;
    let first = journal.claim_clip_summary().await?.unwrap();
    let mut replacement = item("answer", "reasoning", 3, "inProgress");
    replacement["params"]["item"]["text"] = "A different investigation.".into();
    put(&journal, 40, replacement).await?;
    assert!(!journal.clip_summary_current(first.clone()).await?);
    journal
        .finish_clip_summary(first.clone(), summary())
        .await?;
    assert!(journal.clip_detail(first.clip.clone()).await?["clip"]["summary"].is_null());
    let mut tool = item("tool", "toolCall", 1, "inProgress");
    tool["params"]["item"]["tool"] = "read_file".into();
    put(&journal, 50, tool).await?;
    assert!(journal.claim_clip_summary().await?.is_none());
    close(&journal).await?;
    let closed = journal.claim_clip_summary().await?.unwrap();
    assert!(matches!(
        closed.phase,
        intercat_daemon::clip_summaries::Phase::Closed
    ));
    assert_eq!(closed.clip, first.clip);
    Ok(())
}

#[tokio::test]
async fn one_viewer_can_close_while_another_keeps_the_lane_observed() -> Result<()> {
    let (_root, journal) = thinking_fixture().await?;
    let first = uuid::Uuid::now_v7();
    let second = uuid::Uuid::now_v7();
    journal.observe_timeline(first, 1, view()).await?;
    journal.observe_timeline(second, 1, view()).await?;
    let ticket = journal.claim_clip_summary().await?.unwrap();
    journal.observe_timeline(first, 2, None).await?;
    assert!(journal.clip_summary_current(ticket.clone()).await?);
    journal.observe_timeline(second, 2, None).await?;
    assert!(!journal.clip_summary_current(ticket).await?);
    Ok(())
}

#[tokio::test]
async fn abandoned_views_expire_and_restart_drops_live_thinking_state() -> Result<()> {
    let (root, journal) = thinking_fixture().await?;
    journal
        .observe_timeline(uuid::Uuid::now_v7(), 1, view())
        .await?;
    let ticket = journal.claim_clip_summary().await?.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(15_100)).await;
    assert!(!journal.clip_summary_current(ticket.clone()).await?);
    journal.finish_clip_summary(ticket, summary()).await?;
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert!(journal.claim_clip_summary().await?.is_none());
    journal
        .observe_timeline(uuid::Uuid::now_v7(), 1, view())
        .await?;
    let ticket = journal.claim_clip_summary().await?.unwrap();
    assert!(journal.clip_detail(ticket.clip).await?["clip"]["summary"].is_null());
    Ok(())
}

async fn local_editor(
    root: &std::path::Path,
    real: bool,
    journal: &Journal,
) -> Result<intercat_daemon::editor::Manager> {
    use std::os::unix::fs::PermissionsExt;
    // The app inspects input before background summaries can run. Initialize
    // the real privacy scanner before timing model scheduling in this fixture.
    let started = std::time::Instant::now();
    journal
        .preview_privacy(vec!["Dummy input 123".into()])
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
    let ready = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while !editor.status().model_loaded {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    if ready.is_err() {
        let status = editor.status();
        let log = std::fs::read_to_string(root.join("editor.log")).unwrap_or_default();
        editor.stop().await;
        return Err(format!("Test Editor did not become ready: {status:?}\n{log}").into());
    }
    Ok(editor)
}

async fn wait_summary(journal: &Journal) -> Result<Value> {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let value = window(journal, None).await?;
            if let Some(clip) = value["clips"]
                .as_array()
                .unwrap()
                .iter()
                .find(|clip| clip["summary"].is_string())
            {
                return Ok(clip.clone());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?
}

#[tokio::test]
async fn summaries_use_the_background_service_and_screen_both_inputs_and_outputs() -> Result<()> {
    use intercat_daemon::secrets::Registration;
    let (root, journal) = fixture().await?;
    let secret = "clip-private-value-92349";
    rusqlite::Connection::open(root.path().join("db"))?
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
            op_id: uuid::Uuid::now_v7(),
            track_id: "t".into(),
            session_id: "s".into(),
            name: "TOKEN".into(),
            byte_len: secret.len(),
            tag: hex(&key.tag(secret.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    let mut update = item("answer", "agentMessage", 2, "completed");
    update["params"]["item"]["text"] = format!("Added a test. {secret}").into();
    put(&journal, 50, update).await?;
    close(&journal).await?;
    std::fs::write(root.path().join("description-secret"), secret)?;
    let editor = local_editor(root.path(), false, &journal).await?;
    let service = intercat_daemon::descriptions::Service::start(journal.clone(), editor.clone());
    let clip = match wait_summary(&journal).await {
        Ok(clip) => clip,
        Err(error) => {
            let db = rusqlite::Connection::open(root.path().join("db"))?;
            let summaries = db
                .prepare("SELECT clip,state,attempts,version FROM clip_summaries")?
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            eprintln!(
                "Editor status: {:?}; summaries: {summaries:?}",
                editor.status()
            );
            for name in ["editor.log", "model-generations.jsonl"] {
                eprintln!(
                    "{name}: {}",
                    std::fs::read_to_string(root.path().join(name)).unwrap_or_default()
                );
            }
            return Err(error);
        }
    };
    assert!(!clip.to_string().contains(secret));
    assert!(
        !std::fs::read_to_string(root.path().join("model-generations.jsonl"))?.contains(secret)
    );
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn foreground_input_preempts_a_summary_and_its_partial_output_is_discarded() -> Result<()> {
    use intercat_daemon::decision::Target;
    let (root, journal) = fixture().await?;
    let mut update = item("answer", "agentMessage", 2, "completed");
    update["params"]["item"]["text"] = "slow summary".into();
    put(&journal, 50, update).await?;
    close(&journal).await?;
    let editor = local_editor(root.path(), false, &journal).await?;
    let service = intercat_daemon::descriptions::Service::start(journal.clone(), editor.clone());
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !root.path().join("summary-running").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    let processor = intercat_daemon::input::Processor::start(
        journal.clone(),
        editor.clone(),
        std::sync::Arc::new(tokio::sync::Notify::new()),
    )
    .await?;
    let id = uuid::Uuid::now_v7();
    let start = std::time::Instant::now();
    journal
        .stage_utterance(
            id,
            "new foreground request".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if journal.snapshot().await?["utterances"]
                .as_array()
                .unwrap()
                .iter()
                .any(|word| word["id"] == id.to_string() && word["decision"].is_object())
            {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    assert!(window(&journal, None).await?["clips"][0]["summary"].is_null());
    std::fs::write(root.path().join("release-summary"), "release")?;
    wait_summary(&journal).await?;
    service.shutdown().await;
    processor.shutdown().await?;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn shutting_down_during_generation_stops_the_background_service() -> Result<()> {
    let (root, journal) = fixture().await?;
    let mut update = item("answer", "agentMessage", 2, "completed");
    update["params"]["item"]["text"] = "slow summary".into();
    put(&journal, 50, update).await?;
    close(&journal).await?;
    let editor = local_editor(root.path(), false, &journal).await?;
    let service = intercat_daemon::descriptions::Service::start(journal, editor.clone());
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !root.path().join("summary-running").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    tokio::time::timeout(std::time::Duration::from_secs(5), service.shutdown()).await?;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
#[ignore = "local Glimmer model and Python, no paid API"]
async fn real_glimmer_summarizes_a_closed_step_without_claiming_unrun_tests_passed() -> Result<()> {
    let (root, journal) = fixture().await?;
    close(&journal).await?;
    let editor = local_editor(root.path(), true, &journal).await?;
    let service = intercat_daemon::descriptions::Service::start(journal.clone(), editor.clone());
    let result = wait_summary(&journal).await;
    if let Ok(path) = std::env::var("INTERCAT_CLIP_SUMMARY_OUTPUT") {
        std::fs::copy(root.path().join("editor.log"), format!("{path}.editor.log"))?;
    }
    let clip = result?;
    let text = clip["summary"].as_str().unwrap().to_lowercase();
    assert!(text.contains("test"), "{text}");
    assert!(
        !text.contains("passed") && !text.contains("successfully ran"),
        "{text}"
    );
    if let Ok(path) = std::env::var("INTERCAT_CLIP_SUMMARY_OUTPUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&clip)?)?;
    }
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
#[ignore = "local Glimmer model and Python, no paid API"]
async fn real_glimmer_describes_live_thinking_as_an_investigation() -> Result<()> {
    let (root, journal) = thinking_fixture().await?;
    let editor = local_editor(root.path(), true, &journal).await?;
    journal
        .observe_timeline(uuid::Uuid::now_v7(), 1, view())
        .await?;
    let service = intercat_daemon::descriptions::Service::start(journal.clone(), editor.clone());
    let result = wait_summary(&journal).await;
    if let Ok(path) = std::env::var("INTERCAT_THINKING_OUTPUT") {
        std::fs::copy(root.path().join("editor.log"), format!("{path}.editor.log"))?;
    }
    let clip = result?;
    assert_eq!(clip["running"], true);
    let summary = clip["summary"].as_str().unwrap().to_lowercase();
    assert!(
        summary.contains("examin") || summary.contains("investigat"),
        "{summary}"
    );
    assert!(
        !summary.contains("fixed") && !summary.contains("completed"),
        "{summary}"
    );
    if let Ok(path) = std::env::var("INTERCAT_THINKING_OUTPUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&clip)?)?;
    }
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn leaving_the_lane_cancels_its_running_summary() -> Result<()> {
    let (root, journal) = thinking_fixture().await?;
    let mut update = item("answer", "reasoning", 3, "inProgress");
    update["params"]["item"]["text"] = "slow summary".into();
    put(&journal, 40, update).await?;
    let editor = local_editor(root.path(), false, &journal).await?;
    let owner = uuid::Uuid::now_v7();
    journal.observe_timeline(owner, 1, view()).await?;
    let service = intercat_daemon::descriptions::Service::start(journal.clone(), editor.clone());
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !root.path().join("summary-running").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    journal.observe_timeline(owner, 2, None).await?;
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !root.path().join("summary-cancelled").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    assert!(window(&journal, None).await?["clips"][0]["summary"].is_null());
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn restarted_summary_requests_record_stale_results_without_replacing_the_current_clip()
-> Result<()> {
    let (root, journal) = fixture().await?;
    close(&journal).await?;
    let old = journal.claim_clip_summary().await?.unwrap();
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let current = journal.claim_clip_summary().await?.unwrap();
    assert_ne!(old.request.input, current.request.input);
    let before = window(&journal, None).await?;
    journal
        .finish_clip_summary(
            old.clone(),
            Finish::Complete(Summary {
                title: "Obsolete title".into(),
                summary: "Obsolete summary".into(),
            }),
        )
        .await?;
    assert_eq!(window(&journal, None).await?["clips"], before["clips"]);
    journal.finish_clip_summary(current, summary()).await?;
    assert_eq!(
        window(&journal, None).await?["clips"][0]["title"],
        "Zero-case test"
    );
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let event: String = db.query_row("SELECT payload FROM events WHERE kind='clip.summary' AND json_extract(payload,'$.request')=?1", [&old.request.input], |r| r.get(0))?;
    assert_eq!(
        serde_json::from_str::<Value>(&event)?,
        json!({"clip":old.clip,"version":old.version,"request":old.request.input,"phase":"closed","state":"stale"})
    );
    Ok(())
}
