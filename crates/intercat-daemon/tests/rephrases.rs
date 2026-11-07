use intercat_daemon::{
    Result,
    decision::{Outcome, Target},
    journal::Journal,
    rephrases::{Finish, Submission},
    secrets::Key,
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let directory = tempfile::tempdir()?;
    let journal = Journal::open(directory.path().join("events.sqlite"), Key::new([7; 32])).await?;
    Ok((directory, journal))
}
async fn hold(journal: &Journal, text: String, generate: bool) -> Result<String> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, text, Target::New, None, vec![])
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id.clone(),
            input.revision,
            Outcome::Rephrase { generate },
        )
        .await?;
    Ok(input.id)
}
async fn word(journal: &Journal, id: &str) -> Result<Value> {
    journal.read_utterance(id.into()).await
}

#[tokio::test]
async fn a_generated_draft_never_places_work_and_is_retained_after_restart() -> Result<()> {
    let (directory, journal) = fixture().await?;
    let original = "Dummy specification. Do not change any files. ".repeat(60);
    let id = hold(&journal, original.clone(), true).await?;
    let ticket = journal.claim_rephrase().await?.unwrap();
    assert_eq!(ticket.input.text, original);
    journal
        .finish_rephrase(
            ticket.clone(),
            Finish::Complete(
                "Explain the attached dummy specification without changing files.".into(),
            ),
        )
        .await?;
    journal
        .finish_rephrase(ticket, Finish::Complete("An obsolete duplicate".into()))
        .await?;
    let result = word(&journal, &id).await?;
    assert_eq!(result["state"], "asked");
    assert_eq!(result["text"], original);
    assert_eq!(
        result["rephrase"]["proposal"],
        "Explain the attached dummy specification without changing files."
    );
    assert_eq!(journal.snapshot().await?["tracks"], json!([]));
    drop(journal);
    let journal = Journal::open(directory.path().join("events.sqlite"), Key::new([7; 32])).await?;
    assert_eq!(word(&journal, &id).await?["rephrase"], result["rephrase"]);
    Ok(())
}

#[tokio::test]
async fn explicit_submission_attaches_the_full_original_and_replays_without_duplication()
-> Result<()> {
    let (_directory, journal) = fixture().await?;
    let original = "Dummy long request. Do not write files. ".repeat(80);
    let id = hold(&journal, original.clone(), true).await?;
    let request = Submission {
        op_id: Uuid::now_v7(),
        utterance_id: id.clone(),
        revision: 1,
        text: "Explain the attached request without changing files.".into(),
    };
    let first = journal.submit_rephrase(request.clone()).await?;
    let replayed = journal.submit_rephrase(request.clone()).await?;
    assert_eq!(first, replayed);
    assert_eq!(first["state"], "staged");
    assert_eq!(first["revision"], 2);
    assert_eq!(first["text"], request.text);
    assert_eq!(first["attachments"].as_array().unwrap().len(), 1);
    let attachment: Uuid = first["attachments"][0]["id"].as_str().unwrap().parse()?;
    assert_eq!(journal.read_pasted_text(attachment).await?, original);
    let input = journal.claim_input().await?.unwrap();
    assert_eq!(input.text, request.text);
    assert!(input.sources.is_empty());
    assert!(input.has_attachments);
    Ok(())
}

#[tokio::test]
async fn taking_back_or_editing_discards_the_proposal_and_prevents_old_submission() -> Result<()> {
    for edit in [false, true] {
        let (_directory, journal) = fixture().await?;
        let id = hold(&journal, "Dummy original".into(), true).await?;
        let ticket = journal.claim_rephrase().await?.unwrap();
        if edit {
            journal
                .edit_input(intercat_daemon::edit_hold::Request {
                    owner: Uuid::now_v7(),
                    action: intercat_daemon::edit_hold::Action::Begin {
                        token: Uuid::now_v7(),
                        input: id.clone(),
                        revision: 1,
                    },
                })
                .await?;
        } else {
            journal.cut_input(Uuid::now_v7(), id.clone()).await?;
        }
        journal
            .finish_rephrase(ticket, Finish::Complete("An old proposal".into()))
            .await?;
        assert!(word(&journal, &id).await?["rephrase"].is_null());
        assert!(
            journal
                .submit_rephrase(Submission {
                    op_id: Uuid::now_v7(),
                    utterance_id: id,
                    revision: 1,
                    text: "Do the stale request".into()
                })
                .await
                .is_err()
        );
        assert_eq!(journal.snapshot().await?["tracks"], json!([]));
    }
    Ok(())
}

#[tokio::test]
async fn generation_retries_twice_but_an_interruption_does_not_spend_a_try() -> Result<()> {
    let (_directory, journal) = fixture().await?;
    let id = hold(&journal, "Dummy original".into(), true).await?;
    for _ in 0..3 {
        let ticket = journal.claim_rephrase().await?.unwrap();
        journal.finish_rephrase(ticket, Finish::Interrupted).await?;
    }
    for _ in 0..2 {
        let ticket = journal.claim_rephrase().await?.unwrap();
        journal.finish_rephrase(ticket, Finish::Failed).await?;
    }
    assert!(journal.claim_rephrase().await?.is_none());
    assert_eq!(word(&journal, &id).await?["rephrase"]["state"], "failed");
    assert_eq!(word(&journal, &id).await?["state"], "asked");
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn failed_submission_is_atomic_and_uses_the_same_private_original_on_retry() -> Result<()> {
    use intercat_daemon::fault_injection::Request as Fault;
    let (directory, journal) = fixture().await?;
    let original = "Dummy request to preserve";
    let id = hold(&journal, original.into(), true).await?;
    let request = Submission {
        op_id: Uuid::now_v7(),
        utterance_id: id.clone(),
        revision: 1,
        text: "Review the original.".into(),
    };
    journal.test_fault(Fault::Fill).await?;
    assert!(journal.submit_rephrase(request.clone()).await.is_err());
    journal.test_fault(Fault::Restore).await?;
    assert_eq!(word(&journal, &id).await?["text"], original);
    assert_eq!(word(&journal, &id).await?["state"], "asked");
    let saved = journal.submit_rephrase(request).await?;
    assert_eq!(saved["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(saved["revision"], 2);
    assert!(directory.path().join("events.sqlite").is_file());
    Ok(())
}

#[tokio::test]
async fn processor_generates_a_draft_from_all_source_chunks_and_waits_for_submission() -> Result<()>
{
    use intercat_daemon::{
        descriptions::Service,
        editor::{Config, Manager},
        input::Processor,
        request_sources::{Role, Source, Submission as Document},
    };
    use std::{fs, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
    let (directory, journal) = fixture().await?;
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let python = directory.path().join("dummy-python");
    fs::write(
        &python,
        format!(
            "#!/usr/bin/python3\n{}",
            fs::read_to_string(project.join("tests/fixtures/rephrase_editor.py"))?
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
    let wake = Arc::new(tokio::sync::Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone()).await?;
    let service = Service::start(journal.clone(), editor.clone());
    let original = format!(
        "FIRST_MARKER\n{}\nLAST_MARKER: do not change files.",
        "Dummy reference requirements. ".repeat(5000)
    );
    let source = directory.path().join("original.txt");
    fs::write(&source, &original)?;
    let file = journal.import_pasted_text(Uuid::now_v7(), source).await?;
    let id = Uuid::now_v7();
    journal
        .stage_document(Document {
            op_id: id,
            text: String::new(),
            target: Target::New,
            selected_track_id: None,
            attachment_ids: vec![],
            sources: Some(vec![Source {
                attachment_id: file.id,
                role: Role::Request,
                after: String::new(),
            }]),
        })
        .await?;
    let started = std::time::Instant::now();
    let ready = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let current = word(&journal, &id.to_string()).await?;
            if current["rephrase"]["state"] == "complete" {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(current);
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await;
    if ready.is_err() {
        panic!(
            "timed out: state={}, editor={:?}, log={}, inputs={}",
            json!({"state":word(&journal, &id.to_string()).await?["state"],"rephrase":word(&journal, &id.to_string()).await?["rephrase"],"reason":word(&journal, &id.to_string()).await?["reason"]}),
            editor.status(),
            fs::read_to_string(directory.path().join("editor.log")).unwrap_or_default(),
            fs::read_to_string(directory.path().join("rephrase-inputs.jsonl"))
                .unwrap_or_default()
                .chars()
                .take(240)
                .collect::<String>()
        );
    }
    let ready = ready??;
    assert_eq!(ready["state"], "asked");
    assert_eq!(
        ready["rephrase"]["proposal"],
        "Explain the attached request without changing files."
    );
    assert_eq!(journal.snapshot().await?["tracks"], json!([]));
    let records = fs::read_to_string(directory.path().join("rephrase-inputs.jsonl"))?;
    assert!(records.contains("FIRST_MARKER"));
    assert!(records.contains("LAST_MARKER"));
    let chunks = records
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|value| value["mode"] == "notes")
        .count();
    assert!(chunks > 1);
    eprintln!(
        "Full-source draft completed in {:?}, from {chunks} chunks",
        started.elapsed()
    );
    let saved = journal
        .submit_rephrase(Submission {
            op_id: Uuid::now_v7(),
            utterance_id: id.to_string(),
            revision: 1,
            text: ready["rephrase"]["proposal"].as_str().unwrap().into(),
        })
        .await?;
    assert_eq!(saved["sources"], Value::Null);
    assert_eq!(journal.read_pasted_text(file.id).await?, original);
    wake.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        while word(&journal, &id.to_string()).await?["state"] != "placed" {
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await??;
    service.shutdown().await;
    processor.shutdown().await?;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
#[ignore = "local Glimmer model and Python; no paid API"]
async fn real_glimmer_offers_english_and_japanese_drafts_without_placing_work() -> Result<()> {
    use intercat_daemon::{
        descriptions::Service,
        editor::{Config, Manager},
    };
    use std::time::Duration;
    let (directory, journal) = fixture().await?;
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
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
    let service = Service::start(journal.clone(), editor.clone());
    let cases = [
        format!(
            "{}\nExplain why a retry can increment the counter twice. Do not change files or run commands.",
            "Context: The dummy service records request IDs and retry counts. ".repeat(24)
        ),
        format!(
            "{}\n再試行でカウンターが二重に増える理由だけを説明してください。ファイルを変更せず、コマンドも実行しないでください。",
            "背景：ダミーのサービスは依頼の識別子と再試行の回数を記録します。".repeat(24)
        ),
        format!(
            "{}\nAdd a test for the retry counter on Worker A, but wait for my go-ahead before starting.",
            "Background: This is an agent-created dummy specification of a request counter. "
                .repeat(24)
        ),
        format!(
            "Review validation errors in the dummy invoice parser on Worker C. Do not edit files.\n{}\nWait for my signal before starting.\n{}\nDeliver an explanation of the validation errors, without running commands.",
            "Background: The sample invoice has an identifier, a date and a total. ".repeat(150),
            "Background: These invoice records are invented for this local test. ".repeat(150),
        ),
    ];
    let mut outputs = Vec::new();
    for (index, original) in cases.into_iter().enumerate() {
        let id = hold(&journal, original.clone(), true).await?;
        let ready = tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                let current = word(&journal, &id).await?;
                if matches!(
                    current["rephrase"]["state"].as_str(),
                    Some("complete" | "failed")
                ) {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(current);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await??;
        assert_eq!(ready["rephrase"]["state"], "complete", "{ready}");
        assert_eq!(ready["state"], "asked");
        assert_eq!(ready["text"], original);
        assert_eq!(journal.snapshot().await?["tracks"], json!([]));
        if index == 1 {
            assert!(
                ready["rephrase"]["proposal"]
                    .as_str()
                    .unwrap()
                    .chars()
                    .any(|ch| ('\u{3040}'..='\u{30ff}').contains(&ch)),
                "Japanese instructions must retain their language: {ready}"
            );
        }
        outputs.push(json!({"original":original,"proposal":ready["rephrase"]["proposal"],"state":ready["state"]}));
    }
    if let Ok(path) = std::env::var("INTERCAT_REPHRASE_OUTPUT") {
        std::fs::write(path, serde_json::to_vec_pretty(&outputs)?)?;
    }
    service.shutdown().await;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn a_reference_only_request_waits_for_the_users_instruction_without_generation() -> Result<()>
{
    use intercat_daemon::request_sources::{Role, Source, Submission as Document};
    let (directory, journal) = fixture().await?;
    let path = directory.path().join("dummy.txt");
    std::fs::write(&path, "Dummy reference material.")?;
    let file = journal.import_pasted_text(Uuid::now_v7(), path).await?;
    let id = Uuid::now_v7();
    journal
        .stage_document(Document {
            op_id: id,
            text: String::new(),
            target: Target::New,
            selected_track_id: None,
            attachment_ids: vec![],
            sources: Some(vec![Source {
                attachment_id: file.id,
                role: Role::Reference,
                after: String::new(),
            }]),
        })
        .await?;
    assert!(journal.claim_input().await?.is_none());
    assert!(journal.claim_rephrase().await?.is_none());
    assert_eq!(
        word(&journal, &id.to_string()).await?["rephrase"]["state"],
        "manual"
    );
    let saved = journal
        .submit_rephrase(Submission {
            op_id: Uuid::now_v7(),
            utterance_id: id.to_string(),
            revision: 1,
            text: "Explain the dummy reference.".into(),
        })
        .await?;
    assert_eq!(saved["state"], "staged");
    assert_eq!(
        journal.read_pasted_text(file.id).await?,
        "Dummy reference material."
    );
    Ok(())
}

#[tokio::test]
async fn card_drafts_and_exact_pending_submissions_survive_restart_without_replacing_the_composer()
-> Result<()> {
    let (directory, journal) = fixture().await?;
    let current = journal.read_drafts("session".into()).await?;
    let mut book = current["book"].clone();
    book["entries"]["work"]["text"] = json!("Unrelated composer text");
    book["entries"]["rephrase:dummy"] = json!({"text":"My shorter instruction.","target":{"mode":"none"},"attachments":[],
        "context":{"kind":"rephrase","inputId":"dummy","revision":1,"touched":true,"seeded":false},
        "pending":{"kind":"rephrase","request":{"opId":Uuid::now_v7(),"utteranceId":"dummy","revision":1,"text":"My shorter instruction."}}});
    let make = |book: Value| {
        serde_json::from_value(
            json!({"session":"session","revision":0,"opId":Uuid::now_v7(),"parentOpId":null,"book":book}),
        )
    };
    let mut bad = book.clone();
    bad["active"] = json!("rephrase:dummy");
    assert!(journal.save_drafts(make(bad)?).await.is_err());
    let mut bad = book.clone();
    bad["entries"]["rephrase:dummy"]["pending"]["request"]["text"] = json!("Different words");
    assert!(journal.save_drafts(make(bad)?).await.is_err());
    journal.save_drafts(make(book.clone())?).await?;
    drop(journal);
    let journal = Journal::open(directory.path().join("events.sqlite"), Key::new([7; 32])).await?;
    assert_eq!(journal.read_drafts("session".into()).await?["book"], book);
    Ok(())
}

#[test]
fn manual_instructions_use_the_existing_text_budget_and_reject_line_breaks() {
    let mut request = Submission {
        op_id: Uuid::now_v7(),
        utterance_id: Uuid::now_v7().to_string(),
        revision: 1,
        text: "x".repeat(1000),
    };
    assert!(request.validate().is_ok());
    request.text = "First line\nSecond line".into();
    assert!(request.validate().is_err());
    request.text = "x".repeat(32 * 1024 + 1);
    assert!(request.validate().is_err());
}

#[tokio::test]
async fn restarted_proposals_log_the_old_request_without_finishing_the_new_one() -> Result<()> {
    let (directory, journal) = fixture().await?;
    let id = hold(&journal, "Dummy request for a draft".into(), true).await?;
    let old = journal.claim_rephrase().await?.unwrap();
    drop(journal);
    let journal = Journal::open(directory.path().join("events.sqlite"), Key::new([7; 32])).await?;
    let current = journal.claim_rephrase().await?.unwrap();
    assert_ne!(old.request.input, current.request.input);
    journal
        .finish_rephrase(old.clone(), Finish::Complete("Obsolete draft".into()))
        .await?;
    assert_eq!(word(&journal, &id).await?["rephrase"]["state"], "running");
    assert!(word(&journal, &id).await?["rephrase"]["proposal"].is_null());
    journal
        .finish_rephrase(current, Finish::Complete("Current draft".into()))
        .await?;
    assert_eq!(
        word(&journal, &id).await?["rephrase"]["proposal"],
        "Current draft"
    );
    let db = rusqlite::Connection::open(directory.path().join("events.sqlite"))?;
    let event: String = db.query_row("SELECT payload FROM events WHERE kind='rephrase.finished' AND json_extract(payload,'$.request')=?1", [&old.request.input], |r| r.get(0))?;
    let event: Value = serde_json::from_str(&event)?;
    assert_eq!(event["input"], id);
    assert_eq!(event["revision"], old.input.revision);
    assert_eq!(event["state"], "stale");
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='utterance.corrected'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}
