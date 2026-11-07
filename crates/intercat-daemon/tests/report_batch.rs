use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Target},
    editor::{Config, Manager},
    journal::Journal,
    report_analysis::Service,
    reports::{Analysis, Report},
    secrets::Key,
};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal, Manager)> {
    let directory = tempfile::tempdir()?;
    let journal = Journal::open(directory.path().join("events.sqlite"), Key::new([7; 32])).await?;
    let project = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let python = directory.path().join("dummy-python");
    fs::write(
        &python,
        format!(
            "#!/usr/bin/python3\n{}",
            fs::read_to_string(project.join("tests/fixtures/report_batch_editor.py"))?
        ),
    )?;
    fs::set_permissions(&python, fs::Permissions::from_mode(0o700))?;
    let model = directory.path().join("model");
    fs::create_dir(&model)?;
    let editor = Manager::new(
        &project.join("services/editor/src"),
        &directory.path().join("editor.log"),
    )?;
    editor.configure(Config { python, model }).await?;
    let mut status = editor.subscribe();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !status.borrow().model_loaded {
            status.changed().await?;
        }
        Ok::<_, tokio::sync::watch::error::RecvError>(())
    })
    .await??;
    Ok((directory, journal, editor))
}

async fn report(journal: &Journal, text: &str) -> Result<Report> {
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Dummy work".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id.clone(),
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
    let word = journal.read_utterance(input.id).await?;
    let track = word["decision"]["trackId"].as_str().unwrap().to_owned();
    journal.hold_inputs(Uuid::now_v7(), track.clone()).await?;
    let report = Report::new(
        track.clone(),
        format!("dummy-{track}"),
        Uuid::now_v7(),
        "message".into(),
        text.into(),
    );
    journal.observe_report(report.clone()).await?;
    Ok(report)
}

async fn settled(journal: &Journal) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot = journal.snapshot().await?;
            if snapshot["reports"].as_array().unwrap().is_empty() {
                return Ok(snapshot);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

fn requests(path: &Path) -> Vec<Value> {
    fs::read_to_string(path.join("requests.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

async fn requested(path: &Path, priority: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        while !requests(path)
            .iter()
            .any(|value| value["priority"] == priority)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn all_background_tracks_share_one_request_and_keep_their_own_questions() -> Result<()> {
    let (directory, journal, editor) = fixture().await?;
    for index in 0..9 {
        report(
            &journal,
            if index % 3 == 0 {
                "[answer] Which dummy format should I use?"
            } else {
                "The dummy task is done."
            },
        )
        .await?;
    }
    let service = Service::start(journal.clone(), editor.clone());
    let snapshot = settled(&journal).await?;
    let calls: Vec<_> = requests(directory.path())
        .into_iter()
        .filter(|value| value["priority"] == "background")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["stages"].as_array().unwrap().len(), 9);
    assert_eq!(snapshot["reportQuestions"].as_array().unwrap().len(), 3);
    assert!(
        snapshot["reportQuestions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|question| question["kind"] == "question")
    );
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn new_follow_up_cancels_the_batch_and_uses_the_individual_question_priority() -> Result<()> {
    let (directory, journal, editor) = fixture().await?;
    let first = report(&journal, "[answer] Select the dummy database.").await?;
    let second = report(&journal, "The other dummy task is done.").await?;
    fs::write(directory.path().join("pause-background"), "")?;
    let service = Service::start(journal.clone(), editor.clone());
    requested(directory.path(), "background").await?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Then add dummy tests".into(),
            Target::Lock(first.track_id.clone()),
            None,
            vec![],
        )
        .await?;
    requested(directory.path(), "question").await?;
    fs::remove_file(directory.path().join("pause-background"))?;
    let snapshot = settled(&journal).await?;
    let calls = requests(directory.path());
    assert!(calls.iter().any(|value| value["cancelled"].is_string()));
    let priority = calls
        .iter()
        .find(|value| value["priority"] == "question")
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(priority["tail"].as_str().unwrap())?["report"],
        first.text
    );
    assert_eq!(snapshot["reportQuestions"][0]["reportId"], first.id);
    assert!(
        snapshot["reportQuestions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|question| question["reportId"] != second.id)
    );
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn new_input_preempts_background_inference_without_losing_the_batch() -> Result<()> {
    let (directory, journal, editor) = fixture().await?;
    report(&journal, "A completed dummy report.").await?;
    report(&journal, "[answer] Another dummy question.").await?;
    fs::write(directory.path().join("pause-background"), "")?;
    let service = Service::start(journal.clone(), editor.clone());
    requested(directory.path(), "background").await?;
    let request=journal.screen_editor(json!({"prefix":"Dummy foreground classification.","tail":json!({"report":"A foreground dummy input."}).to_string(),"stages":[{"name":"question","prompt":"Label:","candidates":["0","1","2"]}],"temperature":1.0,"cachePrefix":false}), intercat_daemon::editor::Priority::Input).await?;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        editor.analyze_report(
            request,
            intercat_daemon::editor::Priority::Input,
            Duration::from_secs(5),
            None,
        ),
    )
    .await??;
    assert_eq!(result["stages"]["question"]["selected"], "0");
    fs::remove_file(directory.path().join("pause-background"))?;
    assert_eq!(
        settled(&journal).await?["reportQuestions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        requests(directory.path())
            .iter()
            .any(|value| value["preempted"] == true && value["batch"] == true)
    );
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn background_save_rechecks_revision_and_priority_and_retries_an_atomic_write() -> Result<()>
{
    let (_directory, journal, editor) = fixture().await?;
    let original = report(&journal, "Original dummy report.").await?;
    let mut changed = original.clone();
    changed.text = "[answer] Changed dummy report.".into();
    let changed = journal.observe_report(changed).await?.report;
    journal
        .analyze_background_reports(vec![(original, Analysis::Clear)])
        .await?;
    assert_eq!(journal.snapshot().await?["reports"][0]["revision"], 2);
    let input = Uuid::now_v7();
    journal
        .stage_utterance(
            input,
            "Next dummy work".into(),
            Target::Lock(changed.track_id.clone()),
            None,
            vec![],
        )
        .await?;
    journal
        .analyze_background_reports(vec![(changed.clone(), Analysis::Clear)])
        .await?;
    assert_eq!(
        journal.snapshot().await?["reports"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    journal.cut_input(Uuid::now_v7(), input.to_string()).await?;
    #[cfg(feature = "fault-injection")]
    {
        journal
            .test_fault(intercat_daemon::fault_injection::Request::Fill)
            .await?;
        assert!(
            journal
                .analyze_background_reports(vec![(changed.clone(), Analysis::Clear)])
                .await
                .is_err()
        );
        journal
            .test_fault(intercat_daemon::fault_injection::Request::Restore)
            .await?;
        assert_eq!(
            journal.snapshot().await?["reports"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    journal
        .analyze_background_reports(vec![(changed, Analysis::Clear)])
        .await?;
    assert!(
        journal.snapshot().await?["reports"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn shutdown_cancels_generation_and_restart_rechecks_the_saved_reports() -> Result<()> {
    let (directory, journal, editor) = fixture().await?;
    report(&journal, "[answer] Dummy pending question.").await?;
    fs::write(directory.path().join("pause-background"), "")?;
    let service = Service::start(journal.clone(), editor.clone());
    requested(directory.path(), "background").await?;
    tokio::time::timeout(Duration::from_secs(2), service.shutdown()).await?;
    editor.stop().await;
    assert_eq!(
        journal.snapshot().await?["reports"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    drop(journal);
    fs::remove_file(directory.path().join("pause-background"))?;
    let journal = Journal::open(directory.path().join("events.sqlite"), Key::new([7; 32])).await?;
    editor
        .configure(Config {
            python: directory.path().join("dummy-python"),
            model: directory.path().join("model"),
        })
        .await?;
    let mut status = editor.subscribe();
    tokio::time::timeout(Duration::from_secs(20), async {
        while !status.borrow().model_loaded {
            status.changed().await?;
        }
        Ok::<_, tokio::sync::watch::error::RecvError>(())
    })
    .await??;
    let service = Service::start(journal.clone(), editor.clone());
    assert_eq!(
        settled(&journal).await?["reportQuestions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let calls = requests(directory.path());
    let ids: Vec<_> = calls
        .iter()
        .filter(|value| value["priority"] == "background")
        .map(|value| value["reading"].clone())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn malformed_batch_requires_review_instead_of_releasing_work() -> Result<()> {
    let (directory, journal, editor) = fixture().await?;
    report(&journal, "An ambiguous dummy report.").await?;
    fs::write(directory.path().join("malformed"), "")?;
    let service = Service::start(journal.clone(), editor.clone());
    let snapshot = settled(&journal).await?;
    assert_eq!(snapshot["reportQuestions"][0]["kind"], "review");
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn all_report_bodies_are_screened_before_the_shared_request() -> Result<()> {
    use intercat_daemon::secrets::Registration;
    let (directory, journal, editor) = fixture().await?;
    let secret = "dummy-batch\nprivate\"value\\731";
    let source = report(
        &journal,
        &format!("A completed dummy report containing {secret}."),
    )
    .await?;
    report(&journal, "A second completed dummy report.").await?;
    rusqlite::Connection::open(directory.path().join("events.sqlite"))?.execute(
        "INSERT INTO msp_positions VALUES(?1,?2,NULL)",
        rusqlite::params![source.session_id, source.track_id],
    )?;
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
            track_id: source.track_id,
            session_id: source.session_id,
            name: "DUMMY".into(),
            byte_len: secret.len(),
            tag: hex(&key.tag(secret.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    let service = Service::start(journal.clone(), editor.clone());
    settled(&journal).await?;
    let calls = requests(directory.path());
    let call = calls
        .iter()
        .find(|call| call["priority"] == "background")
        .unwrap();
    let rows: Value = serde_json::from_str(
        call["prefix"]
            .as_str()
            .unwrap()
            .split("REPORTS_JSON:\n")
            .nth(1)
            .unwrap(),
    )?;
    assert!(
        rows.as_array()
            .unwrap()
            .iter()
            .all(|row| !row["report"].as_str().unwrap().contains(secret))
    );
    assert_eq!(call["stages"].as_array().unwrap().len(), 2);
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn daemon_server_runs_the_batch_service_and_publishes_questions_over_rpc() -> Result<()> {
    use intercat_daemon::{
        lock::DaemonLock,
        rpc::{Server, Token},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (_directory, journal, editor) = fixture().await?;
    report(&journal, "[answer] Choose a dummy output format.").await?;
    report(&journal, "The other dummy work is complete.").await?;
    let socket = tempfile::tempdir_in("/private/tmp")?;
    let _lock = DaemonLock::acquire(socket.path())?.unwrap();
    let token = "b4".repeat(32);
    let server = Server::bind(
        socket.path(),
        Token::parse(&token)?,
        journal.clone(),
        editor.clone(),
        &socket.path().join("workspaces"),
    )?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let snapshot = settled(&journal).await?;
    assert_eq!(snapshot["reportQuestions"].as_array().unwrap().len(), 1);
    let mut stream =
        BufReader::new(tokio::net::UnixStream::connect(socket.path().join("rpc.sock")).await?);
    for (method, params) in [
        ("rpc.authenticate", json!({"token":token})),
        ("system.status", json!({})),
    ] {
        stream
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":method,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await?;
        let mut line = String::new();
        stream.read_line(&mut line).await?;
        let response: Value = serde_json::from_str(&line)?;
        assert!(response.get("error").is_none(), "{response}");
        if method == "system.status" {
            assert_eq!(
                response["result"]["reportQuestions"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
    }
    drop(stream);
    let _ = stop.send(());
    task.await??;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
#[ignore = "local Glimmer model and Python; no paid API"]
async fn real_glimmer_keeps_mixed_language_reports_separate_in_one_batch() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let journal = Journal::open(directory.path().join("events.sqlite"), Key::new([7; 32])).await?;
    let cases = [
        (
            "The dummy cache fix is complete. All tests passed. No input is required.",
            false,
        ),
        (
            "I cannot continue until you choose red or blue for the dummy label.",
            true,
        ),
        (
            "ダミーの保存形式をJSONとYAMLのどちらにするか選んでください。回答を待ってから続けます。",
            true,
        ),
        (
            "Would you like me to add extra sample data? The requested fix is complete.",
            false,
        ),
    ];
    let mut expected = Vec::new();
    for (text, question) in cases {
        expected.push((report(&journal, text).await?, question));
    }
    let project = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let editor = Manager::new(
        &project.join("services/editor/src"),
        &directory.path().join("editor.log"),
    )?;
    editor
        .configure(Config {
            python: std::env::var("INTERCAT_TEST_EDITOR_PYTHON")?.into(),
            model: std::env::var("INTERCAT_TEST_EDITOR_MODEL")?.into(),
        })
        .await?;
    let mut status = editor.subscribe();
    tokio::time::timeout(Duration::from_secs(120), async {
        while !status.borrow().model_loaded {
            status.changed().await?;
        }
        Ok::<_, tokio::sync::watch::error::RecvError>(())
    })
    .await??;
    let service = Service::start(journal.clone(), editor.clone());
    let snapshot = settled(&journal).await?;
    for (source, question) in expected {
        let found = snapshot["reportQuestions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["reportId"] == source.id);
        assert_eq!(found.is_some(), question, "{snapshot}");
        if let Some(found) = found {
            assert_eq!(found["kind"], "question");
            assert_eq!(found["text"], source.text);
        }
    }
    let db = rusqlite::Connection::open(directory.path().join("events.sqlite"))?;
    let batches: i64 = db.query_row(
        "SELECT count(*) FROM events WHERE kind='report.batch'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(batches, 1);
    if let Ok(path) = std::env::var("INTERCAT_BATCH_REPORT_OUTPUT") {
        fs::write(
            path,
            serde_json::to_vec_pretty(&snapshot["reportQuestions"])?,
        )?;
    }
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}
