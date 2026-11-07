use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Request, Target},
    edit_hold::{Action, Boundary, Edit, Request as EditRequest},
    journal::Journal,
    secrets::Key,
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    Ok((root, journal))
}

async fn track(journal: &Journal, text: &str) -> Result<(String, String)> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, text.into(), Target::New, None, vec![])
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id.clone(),
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: Some("when-told".into()),
                command: None,
                confidence: 1.0,
                inference_ms: 0.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let snapshot = journal.snapshot().await?;
    let word = snapshot["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|word| word["id"] == input.id)
        .unwrap();
    Ok((
        input.id,
        word["decision"]["trackId"].as_str().unwrap().to_owned(),
    ))
}

async fn next(journal: &Journal, text: &str) -> Result<intercat_daemon::decision::PendingInput> {
    journal
        .stage_utterance(Uuid::now_v7(), text.into(), Target::None, None, vec![])
        .await?;
    Ok(journal.claim_input().await?.unwrap())
}

#[tokio::test]
async fn normal_input_gets_eight_decisions_and_three_prior_utterances_without_future_text()
-> Result<()> {
    let (root, journal) = fixture().await?;
    for index in 0..10 {
        track(&journal, &format!("Task {index}")).await?;
    }
    let current = Uuid::now_v7();
    journal
        .stage_utterance(
            current,
            "Current request".into(),
            Target::None,
            None,
            vec![],
        )
        .await?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Future request".into(),
            Target::None,
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    assert_eq!(input.id, current.to_string());
    assert_eq!(input.history.decisions.len(), 8);
    assert_eq!(input.history.decisions[0]["title"], "Task 2");
    assert_eq!(input.history.decisions[7]["title"], "Task 9");
    assert_eq!(
        input
            .history
            .utterances
            .iter()
            .map(|row| row["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["Task 7", "Task 8", "Task 9"]
    );
    assert!(!format!("{:?}", input.history).contains("Future request"));
    journal.cut_input(Uuid::now_v7(), input.id).await?;
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let restored = journal.claim_input().await?.unwrap();
    assert_eq!(restored.history.decisions.len(), 8);
    assert_eq!(
        restored.history.utterances.last().unwrap()["text"],
        "Task 9"
    );
    Ok(())
}

#[tokio::test]
async fn read_as_keeps_the_replaced_assignment_beside_the_current_decision() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let (input, _) = track(&journal, "Original instruction").await?;
    let (_, destination) = track(&journal, "Destination task").await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let opened = journal
        .edit_input(EditRequest {
            owner,
            action: Action::Begin {
                token,
                input: input.clone(),
                revision: 1,
            },
        })
        .await?;
    journal
        .edit_input(EditRequest {
            owner,
            action: Action::End {
                token,
                revision: opened["revision"].as_i64().unwrap(),
                edit: Edit::ReadAs {
                    track: destination,
                    boundary: Boundary::WhenTold,
                    turn: None,
                },
            },
        })
        .await?;
    let pending = next(&journal, "Continue the corrected assignment").await?;
    let row = pending
        .history
        .decisions
        .iter()
        .find(|row| row["title"] == "Original instruction")
        .unwrap();
    assert_eq!(row["corrected"], true);
    assert_eq!(row["decision"]["worker"], "B");
    assert_eq!(row["decision"]["boundary"], "when-told");
    assert_eq!(row["state"], "placed");
    assert_eq!(row["previous"]["worker"], "A");
    assert_eq!(row["previous"]["kind"], "fork");
    journal.cut_input(Uuid::now_v7(), pending.id).await?;
    let snapshot = journal.snapshot().await?;
    let current = snapshot["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|word| word["id"] == input)
        .unwrap();
    journal
        .start_held_input(Uuid::now_v7(), input, current["revision"].as_i64().unwrap())
        .await?;
    let pending = next(&journal, "Continue after starting that request").await?;
    let row = pending
        .history
        .decisions
        .iter()
        .find(|row| row["title"] == "Original instruction")
        .unwrap();
    assert_eq!(row["decision"]["boundary"], "now");
    assert_eq!(row["corrected"], true);
    assert_eq!(row["previous"]["worker"], "A");
    Ok(())
}

#[tokio::test]
async fn waiting_context_includes_questions_and_permissions_without_worker_logs() -> Result<()> {
    use intercat_daemon::{
        execution::Approval, policy::Reason, questions::Prompt, runtime::WorkerState,
    };
    let (root, journal) = fixture().await?;
    let (_, worker) = track(&journal, "Storage work").await?;
    let prompt = Prompt::parse(
        json!({"sessionId":"session","userInputId":"prompt","turnId":"turn","itemId":"item","toolCallId":"tool","toolName":"request_user_input","viewCursor":"1","questions":[{"id":"one","header":"Storage","question":"Which store?","selection":{"mode":"single"},"options":[{"label":"SQLite"},{"label":"PostgreSQL"}]}]}),
    )?;
    journal
        .worker_state(WorkerState {
            track_id: worker.clone(),
            phase: "failed".into(),
            session_id: Some("session".into()),
            secret_revision: None,
            identity: None,
            detail: Some("raw-worker-log-sentinel".into()),
            active_turn: None,
            approvals: vec![Approval {
                id: "approval".into(),
                subject: "Run the command".into(),
                requirement: json!({"raw":"raw-requirement-sentinel"}),
                reason: Reason {
                    kind: "shell".into(),
                    message: "raw-reason-sentinel".into(),
                },
                can_allow: true,
                can_deny: true,
                reply_mode: None,
                deletion: None,
            }],
            output: Some("raw-worker-log-sentinel".into()),
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![prompt],
        })
        .await?;
    let question = intercat_daemon::reports::Question {
        id: "report-question".into(),
        report_id: "report".into(),
        track_id: worker,
        session_id: "session".into(),
        turn_id: Uuid::now_v7(),
        revision: 1,
        kind: "explicit".into(),
        text: "Choose the output format".into(),
        state: "open".into(),
        assumed: None,
        reply_utterance_id: None,
    };
    rusqlite::Connection::open(root.path().join("db"))?.execute(
        "INSERT INTO report_questions VALUES(?1,?2,?3,?4,?5)",
        rusqlite::params![
            question.id,
            question.report_id,
            question.track_id,
            question.state,
            serde_json::to_string(&question)?
        ],
    )?;
    let pending = next(&journal, "Use SQLite").await?;
    let cards = &pending.history.cards;
    for kind in ["report-question", "question", "approval", "worker"] {
        assert!(cards.iter().any(|card| card["kind"] == kind));
    }
    assert_eq!(
        cards
            .iter()
            .find(|card| card["kind"] == "question")
            .unwrap()["choices"],
        json!(["SQLite", "PostgreSQL"])
    );
    let mut request = Request::new(&pending.text, pending.tracks, pending.target, None)?;
    let mut tail: Value = serde_json::from_str(request.wire["tail"].as_str().unwrap())?;
    pending.history.apply(&mut tail);
    request.wire["tail"] = tail.to_string().into();
    let serialized = request.wire.to_string();
    assert!(!serialized.contains("raw-worker-log-sentinel"));
    assert!(!serialized.contains("raw-requirement-sentinel"));
    assert!(!serialized.contains("raw-reason-sentinel"));
    Ok(())
}

#[tokio::test]
async fn context_titles_are_screened_before_any_shortening() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let secret = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    track(&journal, &format!("{} token={secret}", "x".repeat(20))).await?;
    let pending = next(&journal, "Continue safely").await?;
    let mut request = Request::new(&pending.text, pending.tracks, pending.target, None)?;
    let mut tail: Value = serde_json::from_str(request.wire["tail"].as_str().unwrap())?;
    pending.history.apply(&mut tail);
    request.wire["tail"] = tail.to_string().into();
    let safe = journal
        .screen_editor(request.wire, intercat_daemon::editor::Priority::Input)
        .await?;
    let tail: Value = serde_json::from_str(safe.params()["tail"].as_str().unwrap())?;
    assert_eq!(
        tail["recentDecisions"][0]["title"],
        format!("{} token=<redacted:api-key#1>", "x".repeat(20))
    );
    assert_eq!(
        tail["previousUtterances"][0]["text"],
        format!("{} token=<redacted:api-key#1>", "x".repeat(20))
    );
    assert!(!tail.to_string().contains("ghp_"));
    Ok(())
}

#[tokio::test]
async fn running_and_planned_titles_follow_the_current_lane_without_worker_text() -> Result<()> {
    use intercat_daemon::{
        clip_summaries::{Finish, Summary, View},
        clock::Stamp,
    };
    let (_root, journal) = fixture().await?;
    let (_, worker) = track(&journal, "Queued task").await?;
    journal
        .observe_timeline(
            Uuid::now_v7(),
            1,
            Some(View {
                track_id: worker.clone(),
                repair_run_id: None,
            }),
        )
        .await?;
    journal.try_msp(worker.clone(),"session".into(),Stamp::now(),json!({"method":"item/updated","params":{"sessionId":"session","item":{
        "itemId":"reasoning","turnId":"turn","kind":"reasoning","revision":1,"status":"inProgress","text":"raw-thinking-sentinel"}}}),None,None)?.await??;
    let ticket = journal.claim_clip_summary().await?.unwrap();
    journal
        .finish_clip_summary(
            ticket,
            Finish::Complete(Summary {
                title: "Retry analysis".into(),
                summary: "Examining retry counters.".into(),
            }),
        )
        .await?;
    let pending = next(&journal, "Continue that work").await?;
    let candidate = pending
        .tracks
        .iter()
        .find(|candidate| candidate.id == worker)
        .unwrap();
    assert_eq!(candidate.running.as_deref(), Some("Retry analysis"));
    assert_eq!(candidate.planned.len(), 1);
    assert_eq!(candidate.planned[0].title, "Queued task");
    assert_eq!(candidate.planned[0].boundary.as_deref(), Some("when-told"));
    let request = Request::new(&pending.text, pending.tracks, pending.target, None)?;
    assert!(!request.wire.to_string().contains("raw-thinking-sentinel"));
    Ok(())
}

#[tokio::test]
#[ignore = "local Glimmer model and Python, no paid API"]
async fn real_glimmer_uses_a_corrected_assignment_instead_of_the_browsed_worker() -> Result<()> {
    use intercat_daemon::{
        editor::{Config, Manager},
        input::Processor,
    };
    use std::{sync::Arc, time::Duration};
    use tokio::sync::Notify;
    let (root, journal) = fixture().await?;
    let (_, a) = track(&journal, "Arithmetic tests").await?;
    let (_, b) = track(&journal, "Documentation").await?;
    let request = next(&journal, "Add a zero-case arithmetic test").await?;
    journal
        .apply_input(
            request.id.clone(),
            request.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(a.clone()),
                boundary: Some("when-told".into()),
                command: None,
                confidence: 1.0,
                inference_ms: 0.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let opened = journal
        .edit_input(EditRequest {
            owner,
            action: Action::Begin {
                token,
                input: request.id,
                revision: 1,
            },
        })
        .await?;
    journal
        .edit_input(EditRequest {
            owner,
            action: Action::End {
                token,
                revision: opened["revision"].as_i64().unwrap(),
                edit: Edit::ReadAs {
                    track: b.clone(),
                    boundary: Boundary::WhenTold,
                    turn: None,
                },
            },
        })
        .await?;
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let editor = Manager::new(
        &project.join("services/editor/src"),
        &root.path().join("editor.log"),
    )?;
    editor
        .configure(Config {
            python: std::env::var("INTERCAT_TEST_EDITOR_PYTHON")?.into(),
            model: std::env::var("INTERCAT_TEST_EDITOR_MODEL")?.into(),
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(60), async {
        while !editor.status().model_loaded {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let wake = Arc::new(Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone()).await?;
    let mut results = Vec::new();
    for text in [
        "Add another case to that same worker.",
        "Add the next case to the worker I just reassigned the request to.",
    ] {
        let current = Uuid::now_v7();
        journal
            .stage_utterance(current, text.into(), Target::None, Some(a.clone()), vec![])
            .await?;
        wake.notify_one();
        let word = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let snapshot = journal.snapshot().await?;
                let word = snapshot["utterances"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|word| word["id"] == current.to_string())
                    .unwrap();
                if word["state"] == "placed" || word["state"] == "asked" {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(word.clone());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        let stored: Option<String> = rusqlite::Connection::open(root.path().join("db"))?
            .query_row(
                "SELECT decision FROM utterances WHERE id=?1",
                [current.to_string()],
                |row| row.get(0),
            )?;
        let stored = stored
            .map(|value| serde_json::from_str::<Value>(&value))
            .transpose()?;
        results.push(json!({"word":word,"decision":stored}));
        journal
            .cut_input(Uuid::now_v7(), current.to_string())
            .await?;
    }
    processor.shutdown().await?;
    editor.stop().await;
    if let Ok(path) = std::env::var("INTERCAT_CONTEXT_OUTPUT") {
        std::fs::write(path, serde_json::to_string_pretty(&results)?)?;
    }
    for result in &results {
        let word = &result["word"];
        assert_eq!(word["decision"]["trackId"], b, "{word}");
        assert_eq!(word["decision"]["kind"], "extend", "{word}");
        let scores = &result["decision"]["scores"];
        let operation = scores["stageConfidence"]["decision"].as_f64().unwrap();
        let lane = scores["stageConfidence"]["lane"].as_f64().unwrap();
        assert!(operation >= 0.3 && lane >= 0.3, "{result}");
        // The bundled temperature can require confirmation even when the
        // corrected destination is the highest-scoring candidate.
        let review = if operation < 0.55 {
            Some("operation")
        } else if lane < 0.55 {
            Some("lane")
        } else {
            None
        };
        if let Some(stage) = review {
            assert_eq!(word["state"], "asked", "{word}");
            assert_eq!(word["review"]["stage"], stage, "{word}");
        } else {
            assert_eq!(word["state"], "placed", "{word}");
            assert!(word["review"].is_null(), "{word}");
        }
        assert!(result["decision"]["scores"]["tailTokens"].as_u64().unwrap() <= 500);
        assert_eq!(
            result["decision"]["scores"]["tailHash"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
    }
    Ok(())
}
