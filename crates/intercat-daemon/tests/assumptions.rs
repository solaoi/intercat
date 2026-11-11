use intercat_daemon::{
    Result,
    assumptions::{self, Answer, Command, Extracted, Finish, Ticket},
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    secrets::Key,
    timeline::Window,
};
use serde_json::{Value, json};
use uuid::Uuid;

const TURN: &str = "427bbff3-0671-4cde-9205-91b601c4949b";

async fn fixture() -> Result<(tempfile::TempDir, Journal, String)> {
    let directory = tempfile::tempdir()?;
    let journal = Journal::open(directory.path().join("db"), Key::new([7; 32])).await?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Build a cache".into(),
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
    Ok((directory, journal, track))
}

async fn message(
    journal: &Journal,
    track: &str,
    item: &str,
    revision: i64,
    text: &str,
) -> Result<()> {
    journal
        .try_msp(
            track.into(),
            format!("session-{track}"),
            Stamp::now(),
            json!({
                "method":"item/updated", "params":{"sessionId":format!("session-{track}"), "item":{
                    "itemId":item, "turnId":TURN, "kind":"agentMessage", "revision":revision,
                    "status":"inProgress", "text":text,
                }}
            }),
            None,
            None,
        )?
        .await??;
    Ok(())
}

fn extracted(ticket: &Ticket) -> Vec<Extracted> {
    ticket
        .sources
        .iter()
        .map(|source| Extracted {
            source: source.id.clone(),
            question: "Which database should the cache use?".into(),
            options: ["SQLite".into(), "PostgreSQL".into()],
            assumed: "SQLite".into(),
            evidence: "I assumed SQLite".into(),
        })
        .collect()
}

async fn assumption(journal: &Journal) -> Result<Value> {
    Ok(journal.snapshot().await?["assumptions"][0].clone())
}

fn command(assumption: &Value, answer: Answer) -> Command {
    Command {
        op_id: Uuid::now_v7(),
        assumption_id: assumption["id"].as_str().unwrap().parse().unwrap(),
        revision: assumption["revision"].as_i64().unwrap(),
        answer,
    }
}

#[tokio::test]
async fn all_tracks_share_one_batch_and_assumptions_do_not_open_a_question_gate() -> Result<()> {
    let (_directory, journal, track) = fixture().await?;
    message(&journal, &track, "a", 1, "I assumed SQLite and continued.").await?;
    message(
        &journal,
        "other-track",
        "b",
        1,
        "I assumed SQLite for another cache.",
    )
    .await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    assert_eq!(ticket.sources.len(), 2);
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    let snapshot = journal.snapshot().await?;
    assert_eq!(snapshot["assumptions"].as_array().unwrap().len(), 2);
    assert_eq!(snapshot["assumptions"][0]["source"], "default-placement");
    assert!(snapshot["reportQuestions"].as_array().unwrap().is_empty());
    assert!(snapshot["reports"].as_array().unwrap().is_empty());
    let detail = journal.clip_detail(ticket.sources[0].clip.clone()).await?;
    assert_eq!(detail["assumptions"][0]["text"], "SQLite");
    assert_eq!(detail["clip"]["assumptionCount"], 1);
    assert_eq!(
        detail["clip"]["assumptionIds"][0],
        snapshot["assumptions"][0]["id"]
    );
    assert!(journal.claim_assumptions().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn streaming_append_accepts_evidence_but_replacement_discards_the_old_result() -> Result<()> {
    let (_directory, journal, track) = fixture().await?;
    message(&journal, &track, "a", 1, "I assumed SQLite.").await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    message(
        &journal,
        &track,
        "a",
        2,
        "I assumed SQLite. Now adding tests.",
    )
    .await?;
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    assert_eq!(assumption(&journal).await?["text"], "SQLite");
    let newer = journal.claim_assumptions().await?.unwrap();
    message(
        &journal,
        &track,
        "a",
        3,
        "The user explicitly requested PostgreSQL.",
    )
    .await?;
    assert!(
        journal.snapshot().await?["assumptions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    journal
        .finish_assumptions(newer.clone(), Finish::Complete(extracted(&newer)))
        .await?;
    assert!(
        journal.snapshot().await?["assumptions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let latest = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(latest, Finish::Complete(vec![]))
        .await?;
    assert!(journal.claim_assumptions().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn keep_is_durable_and_cannot_be_changed_by_another_window_or_late_extraction() -> Result<()>
{
    let (directory, journal, track) = fixture().await?;
    message(&journal, &track, "a", 1, "I assumed SQLite.").await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    let original = assumption(&journal).await?;
    let keep = command(&original, Answer::Keep);
    let kept = journal.answer_assumption(keep.clone()).await?;
    assert_eq!(kept["state"], "kept");
    assert!(kept["keptAt"].is_number());
    assert_eq!(journal.answer_assumption(keep.clone()).await?, kept);
    assert!(
        journal
            .answer_assumption(command(
                &original,
                Answer::Change {
                    text: "PostgreSQL".into()
                }
            ))
            .await
            .is_err()
    );
    journal
        .finish_assumptions(ticket, Finish::Complete(vec![]))
        .await?;
    drop(journal);
    let journal = Journal::open(directory.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(journal.answer_assumption(keep).await?, kept);
    assert!(
        journal.snapshot().await?["assumptions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        journal
            .clip_detail(original["clipId"].as_str().unwrap().into())
            .await?["assumptions"][0]["state"],
        "kept"
    );
    Ok(())
}

#[tokio::test]
async fn change_records_one_locked_input_atomically_and_retries_it_after_restart() -> Result<()> {
    let (directory, journal, track) = fixture().await?;
    message(&journal, &track, "a", 1, "I assumed SQLite.").await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    let original = assumption(&journal).await?;
    let change = command(
        &original,
        Answer::Change {
            text: "Use PostgreSQL and keep the API unchanged.".into(),
        },
    );
    let changed = journal.answer_assumption(change.clone()).await?;
    assert_eq!(changed["state"], "changed");
    let input = changed["changeInputId"].as_str().unwrap();
    let state = journal.snapshot().await?;
    let words = state["utterances"].as_array().unwrap();
    let word = words.iter().find(|word| word["id"] == input).unwrap();
    assert_eq!(word["target"], json!({"mode":"lock","trackId":track}));
    assert_eq!(word["state"], "staged");
    assert!(
        word["text"]
            .as_str()
            .unwrap()
            .contains("Use PostgreSQL and keep the API unchanged.")
    );
    assert_eq!(journal.answer_assumption(change.clone()).await?, changed);
    drop(journal);
    let journal = Journal::open(directory.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(journal.answer_assumption(change).await?, changed);
    let log = journal
        .editor_log(intercat_daemon::editor_log::Query {
            track_id: track.clone(),
            before: None,
        })
        .await?;
    let changes: Vec<_> = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["scene"] == "assumption-change")
        .collect();
    assert_eq!(changes.len(), 1);
    assert!(
        changes[0]["line"]
            .as_str()
            .unwrap()
            .contains("waiting for A")
    );

    assert_eq!(
        journal.snapshot().await?["utterances"]
            .as_array()
            .unwrap()
            .len(),
        words.len()
    );
    Ok(())
}

#[tokio::test]
async fn interrupted_extraction_retries_and_a_failed_extraction_stops_after_two_attempts()
-> Result<()> {
    let (directory, journal, track) = fixture().await?;
    message(&journal, &track, "a", 1, "I assumed SQLite.").await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(ticket, Finish::Interrupted)
        .await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    drop(journal);
    let journal = Journal::open(directory.path().join("db"), Key::new([7; 32])).await?;
    let resumed = journal.claim_assumptions().await?.unwrap();
    assert_ne!(ticket.request.input, resumed.request.input);
    journal.finish_assumptions(resumed, Finish::Failed).await?;
    assert!(journal.claim_assumptions().await?.is_none());
    Ok(())
}

#[tokio::test]
async fn extraction_rejects_invented_evidence_and_unknown_sources() -> Result<()> {
    let (_directory, journal, track) = fixture().await?;
    message(&journal, &track, "a", 1, "I assumed SQLite.").await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    let mut values = extracted(&ticket);
    values[0].source = "1".into();
    assert_eq!(
        assumptions::parse(&json!({"assumptions":values}).to_string(), &ticket)?.len(),
        1
    );
    for field in ["source", "evidence", "assumed"] {
        let mut value = json!({"assumptions":values});
        value["assumptions"][0][field] = "invented".into();
        assert!(assumptions::parse(&value.to_string(), &ticket).is_err());
    }
    let _window = journal
        .timeline_window(Window {
            tracks: vec![track],
            start: 0,
            end: i64::MAX,
            since: None,
        })
        .await?;
    Ok(())
}

async fn editor(root: &std::path::Path, real: bool) -> Result<intercat_daemon::editor::Manager> {
    use std::os::unix::fs::PermissionsExt;
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let manager = intercat_daemon::editor::Manager::new(
        &project.join("services/editor/src"),
        &root.join("editor.log"),
    )?;
    let config = if real {
        intercat_daemon::editor::Config {
            python: std::env::var("INTERCAT_TEST_EDITOR_PYTHON")?.into(),
            model: std::env::var("INTERCAT_TEST_EDITOR_MODEL")?.into(),
        }
    } else {
        let python = root.join("fixture-python");
        std::fs::write(
            &python,
            format!(
                "#!/usr/bin/python3\n{}",
                std::fs::read_to_string(project.join("tests/fixtures/assumption_editor.py"))?
            ),
        )?;
        std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o700))?;
        let model = root.join("model");
        std::fs::create_dir(&model)?;
        intercat_daemon::editor::Config { python, model }
    };
    manager.configure(config).await?;
    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while !manager.status().model_loaded {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?;
    Ok(manager)
}

async fn wait_assumptions(journal: &Journal, count: usize) -> Result<Value> {
    tokio::time::timeout(std::time::Duration::from_secs(40), async {
        loop {
            let snapshot = journal.snapshot().await?;
            if snapshot["assumptions"].as_array().unwrap().len() == count {
                return Result::Ok(snapshot);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await?
}

#[tokio::test]
async fn the_background_service_extracts_two_workers_in_one_request_without_view_selection()
-> Result<()> {
    let (directory, journal, track) = fixture().await?;
    message(&journal, &track, "a", 1, "I assumed SQLite and continued.").await?;
    message(
        &journal,
        "another-track",
        "b",
        1,
        "I assumed PostgreSQL and continued.",
    )
    .await?;
    let manager = editor(directory.path(), false).await?;
    let service = assumptions::Service::start(journal.clone(), manager.clone());
    let result = wait_assumptions(&journal, 2).await;
    service.shutdown().await;
    manager.stop().await;
    let result = result?;
    assert_eq!(result["assumptions"][0]["text"], "SQLite");
    assert_eq!(result["assumptions"][1]["text"], "PostgreSQL");
    let requests = std::fs::read_to_string(directory.path().join("assumption-generations.jsonl"))?;
    let requests = requests
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        serde_json::from_str::<Vec<Value>>(requests[0]["tail"].as_str().unwrap())?.len(),
        2
    );
    Ok(())
}

#[tokio::test]
#[ignore = "Requires the installed local Glimmer model"]
async fn real_glimmer_extracts_assumptions_and_ignores_explicit_requests_and_waiting_questions()
-> Result<()> {
    let (directory, journal, track) = fixture().await?;
    message(
        &journal,
        &track,
        "assumed",
        1,
        "I assumed SQLite for local storage and continued implementing the cache.",
    )
    .await?;
    message(
        &journal,
        "other-track",
        "requested",
        1,
        "The user requested PostgreSQL. I configured PostgreSQL exactly as requested.",
    )
    .await?;
    message(&journal,"third-track","waiting",1,"Should storage use JSON or YAML? I cannot continue until you choose. Waiting for you: Choose JSON or YAML.").await?;
    let manager = editor(directory.path(), true).await?;
    let service = assumptions::Service::start(journal.clone(), manager.clone());
    let result = wait_assumptions(&journal, 1).await;
    service.shutdown().await;
    manager.stop().await;
    let result = result?;
    assert_eq!(result["assumptions"][0]["trackId"], track);
    assert!(
        result["assumptions"][0]["text"]
            .as_str()
            .unwrap()
            .contains("SQLite")
    );
    assert!(
        result["assumptions"][0]["evidence"]
            .as_str()
            .unwrap()
            .contains("assumed")
    );
    Ok(())
}

#[tokio::test]
async fn a_failed_answer_rolls_back_the_correction_and_retries_the_same_operation() -> Result<()> {
    let (directory, journal, track) = fixture().await?;
    message(&journal, &track, "assumed", 1, "I assumed SQLite.").await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    let original = assumption(&journal).await?;
    let change = command(
        &original,
        Answer::Change {
            text: "Use PostgreSQL.".into(),
        },
    );
    let db = rusqlite::Connection::open(directory.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER reject_assumption_answer BEFORE INSERT ON operations
        WHEN NEW.kind='assumption.respond' BEGIN SELECT RAISE(ABORT,'simulated save failure'); END;")?;
    let before = journal.snapshot().await?;
    assert!(journal.answer_assumption(change.clone()).await.is_err());
    let after = journal.snapshot().await?;
    assert_eq!(before["utterances"], after["utterances"]);
    assert_eq!(before["assumptions"], after["assumptions"]);
    assert_eq!(before["editorEntries"], after["editorEntries"]);
    db.execute_batch("DROP TRIGGER reject_assumption_answer;")?;
    assert_eq!(journal.answer_assumption(change).await?["state"], "changed");
    Ok(())
}

#[tokio::test]
async fn restoring_a_retracted_assumption_creates_a_new_revision() -> Result<()> {
    let (_directory, journal, track) = fixture().await?;
    message(&journal, &track, "message", 1, "I assumed SQLite.").await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    let old = assumption(&journal).await?;
    message(
        &journal,
        &track,
        "message",
        2,
        "The quoted phrase 'I assumed SQLite' did not describe my choice.",
    )
    .await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(ticket, Finish::Complete(vec![]))
        .await?;
    assert!(
        journal.snapshot().await?["assumptions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    message(&journal, &track, "message", 3, "I assumed SQLite.").await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    let restored = assumption(&journal).await?;
    assert_eq!(old["id"], restored["id"]);
    assert!(restored["revision"].as_i64() > old["revision"].as_i64());
    assert!(
        journal
            .answer_assumption(command(&old, Answer::Keep))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn long_reports_are_read_in_full_with_bounded_requests_and_original_source_ids() -> Result<()>
{
    let (directory, journal, track) = fixture().await?;
    let mut text = "I assumed SQLite for the first cache.\n".to_owned();
    for number in 0..2500 {
        text.push_str(&format!(
            "Record {number:04}: routine worker progress without a choice.\n"
        ));
    }
    text.push_str("I assumed PostgreSQL for the final cache.\n");
    message(&journal, &track, "long", 1, &text).await?;
    let manager = editor(directory.path(), false).await?;
    let service = assumptions::Service::start(journal.clone(), manager.clone());
    let result = wait_assumptions(&journal, 2).await;
    service.shutdown().await;
    manager.stop().await;
    let result = result?;
    assert_eq!(
        result["assumptions"][0]["sourceId"],
        result["assumptions"][1]["sourceId"]
    );
    let requests = std::fs::read_to_string(directory.path().join("assumption-generations.jsonl"))?;
    let requests = requests
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert!(requests.len() > 1);
    let mut ranges = Vec::new();
    for request in requests {
        let parts: Vec<Value> = serde_json::from_str(request["tail"].as_str().unwrap())?;
        for part in parts {
            assert_eq!(part["id"], "1");
            let piece = part["text"].as_str().unwrap();
            let start = text.find(piece).unwrap();
            ranges.push(start..start + piece.len());
        }
        let prompt = format!(
            "{}{}\nDescription:\n",
            request["prefix"].as_str().unwrap(),
            request["tail"].as_str().unwrap()
        );
        assert!(prompt.len() / 4 <= 8000);
    }
    ranges.sort_by_key(|range| range.start);
    let mut through = 0;
    for range in ranges {
        assert!(range.start <= through);
        through = through.max(range.end);
    }
    assert_eq!(through, text.len());
    Ok(())
}

#[tokio::test]
async fn output_budget_splits_sources_and_saves_the_complete_batch() -> Result<()> {
    let (directory, journal, track) = fixture().await?;
    for number in 0..5 {
        message(
            &journal,
            &track,
            &format!("message-{number}"),
            1,
            "I assumed SQLite and continued.",
        )
        .await?;
    }
    let manager = editor(directory.path(), false).await?;
    let service = assumptions::Service::start(journal.clone(), manager.clone());
    let result = wait_assumptions(&journal, 5).await;
    service.shutdown().await;
    manager.stop().await;
    assert_eq!(result?["assumptions"].as_array().unwrap().len(), 5);
    let requests = std::fs::read_to_string(directory.path().join("assumption-generations.jsonl"))?;
    assert_eq!(requests.lines().count(), 5);
    Ok(())
}

#[tokio::test]
#[ignore = "Requires the installed local Glimmer model"]
async fn real_glimmer_reads_both_ends_of_a_report_larger_than_its_input_window() -> Result<()> {
    let (directory, journal, track) = fixture().await?;
    let text = format!(
        "I assumed SQLite for the first cache.\n{}I assumed PostgreSQL for the final cache.",
        "A routine check found no new decision.\n".repeat(1200)
    );
    message(&journal, &track, "long-real", 1, &text).await?;
    let manager = editor(directory.path(), true).await?;
    let request = journal
        .screen_editor(
            json!({"tail":json!([{"id":"1","text":text}]).to_string()}),
            intercat_daemon::editor::Priority::Input,
        )
        .await?;
    let tokens = manager
        .count_tokens(
            &request,
            intercat_daemon::editor::Priority::Background,
            None,
        )
        .await?;
    drop(request);
    assert!(
        tokens > 8000,
        "The fixture must exceed the real tokenizer's input limit: {tokens}"
    );
    println!("long report input tokens before extraction: {tokens}");
    let service = assumptions::Service::start(journal.clone(), manager.clone());
    let result = tokio::time::timeout(std::time::Duration::from_secs(120), async {
        loop {
            let snapshot = journal.snapshot().await?;
            if snapshot["assumptions"].as_array().unwrap().len() == 2 {
                return Result::Ok(snapshot);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    if result.is_err() {
        println!(
            "long report saved choices: {}",
            journal.snapshot().await?["assumptions"]
        );
        let db = rusqlite::Connection::open(directory.path().join("db"))?;
        let state: (String, i64) =
            db.query_row("SELECT state,attempts FROM assumption_sources", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
        println!("long report extraction state: {state:?}");
    }
    service.shutdown().await;
    manager.stop().await;
    let result = result??;
    let choices = result["assumptions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value["text"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(choices.iter().any(|value| value.contains("SQLite")));
    assert!(choices.iter().any(|value| value.contains("PostgreSQL")));
    println!(
        "long report input tokens: {tokens}; saved assumptions: {}",
        serde_json::to_string(&choices)?
    );
    Ok(())
}

#[tokio::test]
async fn preemption_discards_completed_parts_until_the_whole_batch_is_retried() -> Result<()> {
    let (directory, journal, track) = fixture().await?;
    for number in 0..5 {
        message(
            &journal,
            &track,
            &format!("message-{number}"),
            1,
            "I assumed SQLite and continued.",
        )
        .await?;
    }
    std::fs::write(directory.path().join("preempt-third"), "")?;
    let manager = editor(directory.path(), false).await?;
    let service = assumptions::Service::start(journal.clone(), manager.clone());
    let result: Result<Value> = async {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !directory.path().join("preempted").exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .map_err(|error| {
            let requests =
                std::fs::read_to_string(directory.path().join("assumption-generations.jsonl"))
                    .ok()
                    .map(|text| text.lines().count());
            format!(
                "Waiting for dummy preemption (requests: {requests:?}, Editor: {}): {error}",
                manager.status().phase
            )
        })?;
        assert!(
            journal.snapshot().await?["assumptions"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        wait_assumptions(&journal, 5).await.map_err(|error| {
            format!("Waiting for the complete retried assumption batch: {error}").into()
        })
    }
    .await;
    service.shutdown().await;
    manager.stop().await;
    assert_eq!(result?["assumptions"].as_array().unwrap().len(), 5);
    Ok(())
}

#[tokio::test]
#[ignore = "Requires the installed local Glimmer model"]
async fn real_glimmer_records_a_numeric_default_without_treating_a_user_choice_as_an_assumption()
-> Result<()> {
    let (directory, journal, track) = fixture().await?;
    message(&journal, &track, "default-rate", 1,
        "The request did not specify a rate limit. I configured the login rate limit to 100 requests per minute per IP and continued implementing the middleware.").await?;
    message(&journal, "requested-track", "requested-rate", 1,
        "The user explicitly requested 10 requests per minute per IP. I configured exactly that limit.").await?;
    message(&journal, "waiting-track", "waiting-rate", 1,
        "Waiting for you: Should the rate limit be 20 or 50 requests per minute? I will wait for your answer.").await?;
    let manager = editor(directory.path(), true).await?;
    let service = assumptions::Service::start(journal.clone(), manager.clone());
    let result = wait_assumptions(&journal, 1).await;
    service.shutdown().await;
    manager.stop().await;
    let result = result?;
    let assumption = &result["assumptions"][0];
    assert_eq!(assumption["trackId"], track);
    assert_eq!(assumption["source"], "default-placement");
    assert!(assumption["text"].as_str().unwrap().contains("100"));
    assert!(assumption["evidence"].as_str().unwrap().contains("100"));
    println!("numeric default: {}", serde_json::to_string(assumption)?);
    Ok(())
}

#[tokio::test]
async fn observation_time_survives_answers_and_restart_and_selects_a_finished_clip_by_its_marker()
-> Result<()> {
    let (directory, journal, track) = fixture().await?;
    message(&journal, &track, "timed", 1, "I assumed SQLite.").await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    let closed = Stamp::now();
    journal.try_msp(track.clone(), format!("session-{track}"), closed.clone(),
        json!({"method":"turn/completed","params":{"sessionId":format!("session-{track}"),"turnId":TURN,"terminal":"completed"}}), None, None)?.await??;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    let item = assumption(&journal).await?;
    let at = item["observedAt"]["timelineMs"].as_i64().unwrap();
    assert!(at > closed.timeline_ms);
    let slice = journal
        .timeline_window(Window {
            tracks: vec![track.clone()],
            start: at,
            end: at,
            since: None,
        })
        .await?;
    assert_eq!(slice["clips"].as_array().unwrap().len(), 1);
    assert_eq!(slice["clips"][0]["assumptionTimes"], json!([at]));
    assert!(slice["clips"][0]["end"].as_i64().unwrap() < at);
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    assert_eq!(
        assumption(&journal).await?["observedAt"],
        item["observedAt"]
    );
    let kept = journal
        .answer_assumption(command(&item, Answer::Keep))
        .await?;
    assert_eq!(kept["observedAt"], item["observedAt"]);
    drop(journal);
    let reopened = Journal::open(directory.path().join("db"), Key::new([7; 32])).await?;
    let detail = reopened.clip_detail(ticket.sources[0].clip.clone()).await?;
    assert_eq!(detail["assumptions"][0]["observedAt"], item["observedAt"]);
    assert_eq!(detail["clip"]["assumptionTimes"], json!([at]));
    Ok(())
}

#[tokio::test]
async fn an_assumption_without_a_recorded_time_does_not_invent_a_timeline_position() -> Result<()> {
    let (directory, journal, track) = fixture().await?;
    message(&journal, &track, "untimed", 1, "I assumed SQLite.").await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    let db = rusqlite::Connection::open(directory.path().join("db"))?;
    db.execute(
        "UPDATE assumptions SET record=json_remove(record,'$.observedAt')",
        [],
    )?;
    let detail = journal.clip_detail(ticket.sources[0].clip.clone()).await?;
    assert_eq!(detail["clip"]["assumptionCount"], 1);
    assert_eq!(detail["clip"]["assumptionTimes"], json!([]));
    assert_eq!(
        detail["assumptions"][0]["observedAt"],
        serde_json::Value::Null
    );
    Ok(())
}

#[tokio::test]
async fn moving_a_source_to_another_clip_discards_its_inflight_extraction() -> Result<()> {
    let (directory, journal, track) = fixture().await?;
    message(&journal, &track, "moved", 1, "I assumed SQLite.").await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    let db = rusqlite::Connection::open(directory.path().join("db"))?;
    db.execute(
        "UPDATE assumption_sources SET clip='another-clip' WHERE id=?1",
        [&ticket.sources[0].id],
    )?;
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    let count: i64 = db.query_row("SELECT count(*) FROM assumptions", [], |row| row.get(0))?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn an_assumed_question_keeps_its_origin_without_answering_a_blocking_question() -> Result<()>
{
    let (directory, journal, track) = fixture().await?;
    let asked = "Which database should this cache use, SQLite or PostgreSQL?";
    let text =
        format!("{asked} I assumed SQLite and continued.\nWaiting for you: Approve deployment?");
    message(&journal, &track, "questions", 1, &text).await?;
    let report = intercat_daemon::reports::Report::new(
        track.clone(),
        format!("session-{track}"),
        TURN.parse()?,
        "questions".into(),
        text.clone(),
    );
    journal.observe_report(report).await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    let mut choices = extracted(&ticket);
    choices[0].question = asked.into();
    journal
        .finish_assumptions(ticket, Finish::Complete(choices))
        .await?;
    let value = assumption(&journal).await?;
    assert_eq!(value["question"], asked);
    assert_eq!(value["source"], "default-placement");
    let question_id = value["questionId"].as_str().unwrap();
    let db = rusqlite::Connection::open(directory.path().join("db"))?;
    let record: String = db.query_row(
        "SELECT record FROM report_questions WHERE id=?1",
        [question_id],
        |row| row.get(0),
    )?;
    let question: intercat_daemon::reports::Question = serde_json::from_str(&record)?;
    assert_eq!(question.text, asked);
    assert_eq!(question.assumed.as_deref(), Some("SQLite"));
    assert_eq!(question.state, "assumed");
    assert_eq!(question.turn_id.to_string(), TURN);
    let updated_text = format!("{text}\nThe cache implementation is ready.");
    message(&journal, &track, "questions", 2, &updated_text).await?;
    journal
        .observe_report(intercat_daemon::reports::Report::new(
            track.clone(),
            format!("session-{track}"),
            TURN.parse()?,
            "questions".into(),
            updated_text,
        ))
        .await?;
    let state: String = db.query_row(
        "SELECT state FROM report_questions WHERE id=?1",
        [question_id],
        |row| row.get(0),
    )?;
    assert_eq!(state, "assumed");
    let state = journal.snapshot().await?;
    assert_eq!(state["reportQuestions"].as_array().unwrap().len(), 1);
    assert_eq!(state["reportQuestions"][0]["text"], "Approve deployment?");
    let kept = journal
        .answer_assumption(command(&value, Answer::Keep))
        .await?;
    journal.try_msp(track.clone(), format!("session-{track}"), Stamp::now(),
        json!({"method":"turn/completed","params":{"sessionId":format!("session-{track}"),"turnId":TURN,"terminal":"completed"}}), None, None)?.await??;
    let detail = journal
        .clip_detail(value["clipId"].as_str().unwrap().into())
        .await?;
    assert_eq!(detail["assumptions"][0]["source"], "question-timeout");
    assert_eq!(detail["assumptions"][0]["state"], "kept");
    assert_eq!(detail["assumptions"][0]["observedAt"], kept["observedAt"]);
    assert_eq!(detail["assumptions"][0]["revision"], kept["revision"]);
    assert_eq!(
        journal.snapshot().await?["reportQuestions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    drop(journal);
    let reopened = Journal::open(directory.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(
        reopened
            .clip_detail(value["clipId"].as_str().unwrap().into())
            .await?["assumptions"][0]["questionId"],
        question_id
    );
    Ok(())
}

#[tokio::test]
async fn appended_and_retracted_question_provenance_preserves_the_answer_and_observation_time()
-> Result<()> {
    let (_directory, journal, track) = fixture().await?;
    let asked = "Which database should this cache use, SQLite or PostgreSQL?";
    message(&journal, &track, "provenance", 1, "I assumed SQLite.").await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    let value = assumption(&journal).await?;
    let kept = journal
        .answer_assumption(command(&value, Answer::Keep))
        .await?;
    message(
        &journal,
        &track,
        "provenance",
        2,
        &format!("I assumed SQLite. {asked}"),
    )
    .await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    let mut choices = extracted(&ticket);
    choices[0].question = asked.into();
    journal
        .finish_assumptions(ticket, Finish::Complete(choices))
        .await?;
    let detail = journal
        .clip_detail(value["clipId"].as_str().unwrap().into())
        .await?;
    let linked = &detail["assumptions"][0];
    assert_eq!(linked["state"], "kept");
    assert_eq!(linked["observedAt"], kept["observedAt"]);
    assert!(linked["questionId"].is_string());
    message(&journal, &track, "provenance", 3, "I assumed SQLite.").await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    let detail = journal
        .clip_detail(value["clipId"].as_str().unwrap().into())
        .await?;
    assert!(detail["assumptions"][0]["questionId"].is_null());
    assert_eq!(detail["assumptions"][0]["state"], "kept");
    assert_eq!(detail["assumptions"][0]["observedAt"], kept["observedAt"]);
    Ok(())
}

#[tokio::test]
async fn question_and_assumption_writes_roll_back_and_retry_together() -> Result<()> {
    let (directory, journal, track) = fixture().await?;
    let asked = "Which database should this cache use, SQLite or PostgreSQL?";
    message(
        &journal,
        &track,
        "atomic",
        1,
        &format!("{asked} I assumed SQLite."),
    )
    .await?;
    journal.try_msp(track.clone(), format!("session-{track}"), Stamp::now(),
        json!({"method":"turn/completed","params":{"sessionId":format!("session-{track}"),"turnId":TURN,"terminal":"completed"}}), None, None)?.await??;
    let ticket = journal.claim_assumptions().await?.unwrap();
    let mut choices = extracted(&ticket);
    choices[0].question = asked.into();
    let db = rusqlite::Connection::open(directory.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER refuse_assumption BEFORE INSERT ON assumptions BEGIN SELECT RAISE(ABORT,'simulated full database'); END;")?;
    assert!(
        journal
            .finish_assumptions(ticket.clone(), Finish::Complete(choices.clone()))
            .await
            .is_err()
    );
    let count: i64 = db.query_row("SELECT count(*) FROM report_questions", [], |row| {
        row.get(0)
    })?;
    assert_eq!(count, 0);
    db.execute_batch("DROP TRIGGER refuse_assumption")?;
    journal
        .finish_assumptions(ticket, Finish::Complete(choices))
        .await?;
    assert!(assumption(&journal).await?["questionId"].is_string());
    assert_eq!(assumption(&journal).await?["source"], "question-timeout");
    Ok(())
}

#[tokio::test]
#[ignore = "Requires the installed local Glimmer model"]
async fn real_glimmer_links_the_question_the_worker_answered_and_keeps_waiting_questions_separate()
-> Result<()> {
    let (directory, journal, track) = fixture().await?;
    let asked = "Which database should this cache use, SQLite or PostgreSQL?";
    message(&journal, &track, "choice", 1, &format!("{asked} I chose SQLite for its zero-configuration setup and continued implementing the cache.\nWaiting for you: May I deploy the cache to production?")).await?;
    let japanese = "保存形式はJSONとYAMLのどちらにしますか？";
    message(&journal, "japanese", "choice", 1, &format!("{japanese} 回答を待たず、JSONを採用して実装を進めました。ユーザーから形式の指定はありません。")).await?;
    message(
        &journal,
        "waiting",
        "wait",
        1,
        "Waiting for you: Choose SQLite or PostgreSQL. I cannot continue until you answer.",
    )
    .await?;
    let manager = editor(directory.path(), true).await?;
    let service = assumptions::Service::start(journal.clone(), manager.clone());
    let result = wait_assumptions(&journal, 2).await;
    println!(
        "question assumption observation: {}",
        journal.snapshot().await?["assumptions"]
    );
    service.shutdown().await;
    manager.stop().await;
    let result = result?;
    let values = result["assumptions"].as_array().unwrap();
    let value = values
        .iter()
        .find(|value| value["trackId"] == track)
        .unwrap();
    assert_eq!(value["trackId"], track);
    assert_eq!(value["question"], asked);
    assert!(value["questionId"].is_string());
    let value = values
        .iter()
        .find(|value| value["trackId"] == "japanese")
        .unwrap();
    assert_eq!(value["question"], japanese);
    assert!(value["questionId"].is_string());
    println!("question assumption: {}", serde_json::to_string(value)?);
    Ok(())
}

#[tokio::test]
async fn an_invalid_long_result_is_split_without_accepting_invented_evidence() -> Result<()> {
    let (directory, journal, track) = fixture().await?;
    let text = format!(
        "I assumed SQLite for the first cache.\n{}I assumed PostgreSQL for the final cache.",
        "Routine worker progress.\n".repeat(1600)
    );
    message(&journal, &track, "invalid-long", 1, &text).await?;
    std::fs::write(directory.path().join("invalid-long"), "")?;
    let manager = editor(directory.path(), false).await?;
    let service = assumptions::Service::start(journal.clone(), manager.clone());
    let result = wait_assumptions(&journal, 2).await;
    service.shutdown().await;
    manager.stop().await;
    let result = result?;
    assert!(
        result["assumptions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["text"] != "Invented")
    );
    let requests = std::fs::read_to_string(directory.path().join("assumption-generations.jsonl"))?;
    assert!(requests.lines().count() > 2);
    Ok(())
}

#[tokio::test]
async fn superseded_extractions_log_the_source_and_cannot_finish_a_new_request() -> Result<()> {
    let (directory, journal, track) = fixture().await?;
    message(
        &journal,
        &track,
        "audit",
        1,
        "I assumed SQLite for the cache.",
    )
    .await?;
    let old = journal.claim_assumptions().await?.unwrap();
    message(
        &journal,
        &track,
        "audit",
        2,
        "I assumed SQLite for the export.",
    )
    .await?;
    let current = journal.claim_assumptions().await?.unwrap();
    assert_ne!(old.request.input, current.request.input);
    journal
        .finish_assumptions(old.clone(), Finish::Complete(extracted(&old)))
        .await?;
    assert!(
        journal.snapshot().await?["assumptions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    journal
        .finish_assumptions(current.clone(), Finish::Complete(extracted(&current)))
        .await?;
    assert_eq!(
        journal.snapshot().await?["assumptions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    journal
        .finish_assumptions(old.clone(), Finish::Complete(vec![]))
        .await?;
    assert_eq!(
        journal.snapshot().await?["assumptions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let db = rusqlite::Connection::open(directory.path().join("db"))?;
    let events = db
        .prepare("SELECT payload FROM events WHERE kind='assumptions.extracted' ORDER BY seq")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let events = events
        .iter()
        .map(|text| serde_json::from_str::<Value>(text))
        .collect::<serde_json::Result<Vec<_>>>()?;
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["request"], old.request.input);
    assert_eq!(
        events[0]["sources"],
        json!([{"source":old.sources[0].id,"clip":old.sources[0].clip,"state":"stale"}])
    );
    assert_eq!(events[1]["sources"][0]["state"], "applied");
    assert_eq!(events[2]["sources"][0]["state"], "stale");
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

#[cfg(feature = "fault-injection")]
#[tokio::test]
async fn stale_extraction_logging_rolls_back_and_survives_restart() -> Result<()> {
    use intercat_daemon::fault_injection::Request as Fault;
    let (directory, journal, track) = fixture().await?;
    message(
        &journal,
        &track,
        "audit",
        1,
        "I assumed SQLite for the cache.",
    )
    .await?;
    let old = journal.claim_assumptions().await?.unwrap();
    message(
        &journal,
        &track,
        "audit",
        2,
        "The user requested PostgreSQL for the cache.",
    )
    .await?;
    journal.test_fault(Fault::Fill).await?;
    assert!(
        journal
            .finish_assumptions(old.clone(), Finish::Complete(extracted(&old)))
            .await
            .is_err()
    );
    journal.test_fault(Fault::Restore).await?;
    let db = rusqlite::Connection::open(directory.path().join("db"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='assumptions.extracted'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    drop(journal);
    let journal = Journal::open(directory.path().join("db"), Key::new([7; 32])).await?;
    let current = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(old.clone(), Finish::Complete(extracted(&old)))
        .await?;
    journal
        .finish_assumptions(current, Finish::Complete(vec![]))
        .await?;
    assert!(journal.claim_assumptions().await?.is_none());
    assert!(
        journal.snapshot().await?["assumptions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let state: String = db.query_row("SELECT json_extract(payload,'$.sources[0].state') FROM events WHERE kind='assumptions.extracted' AND json_extract(payload,'$.request')=?1", [&old.request.input], |r| r.get(0))?;
    assert_eq!(state, "stale");
    Ok(())
}

#[tokio::test]
async fn assumption_speech_is_emitted_once_for_valid_evidence_and_not_for_stale_extraction()
-> Result<()> {
    let (_directory, journal, track) = fixture().await?;
    message(
        &journal,
        &track,
        "one",
        1,
        "I assumed SQLite and continued.",
    )
    .await?;
    let ticket = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    journal
        .finish_assumptions(ticket.clone(), Finish::Complete(extracted(&ticket)))
        .await?;
    message(
        &journal,
        &track,
        "one",
        2,
        "I assumed SQLite and continued. Still working.",
    )
    .await?;
    let updated = journal.claim_assumptions().await?.unwrap();
    journal
        .finish_assumptions(updated.clone(), Finish::Complete(extracted(&updated)))
        .await?;
    let log = journal
        .editor_log(intercat_daemon::editor_log::Query {
            track_id: track.clone(),
            before: None,
        })
        .await?;
    let spoken: Vec<_> = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["scene"] == "assumed")
        .collect();
    assert_eq!(spoken.len(), 1);
    assert!(spoken[0]["line"].as_str().unwrap().contains("SQLite"));
    message(&journal, &track, "two", 1, "I assumed SQLite.").await?;
    let stale = journal.claim_assumptions().await?.unwrap();
    message(&journal, &track, "two", 2, "The user requested PostgreSQL.").await?;
    journal
        .finish_assumptions(stale.clone(), Finish::Complete(extracted(&stale)))
        .await?;
    assert_eq!(
        journal
            .editor_log(intercat_daemon::editor_log::Query {
                track_id: track,
                before: None
            })
            .await?,
        log
    );
    Ok(())
}
