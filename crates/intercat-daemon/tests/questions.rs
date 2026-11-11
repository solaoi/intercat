use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Target},
    editor::Manager,
    execution::{self, Context, Port, WorkerIo},
    journal::Journal,
    msp::Connection,
    process::{self, Tracker},
    questions::{Prompt, Response},
    runtime::WorkerState,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
    sync::watch,
};
use uuid::Uuid;
#[path = "../src/test_host.rs"]
mod test_host;
fn prompt() -> Value {
    json!({"sessionId":"session","userInputId":"prompt","turnId":"turn","itemId":"item","toolCallId":"tool-call","toolName":"request_user_input","viewCursor":"1","questions":[
        {"id":"one","header":"Choice","question":"Choose one","selection":{"mode":"single"},"options":[{"label":"First"},{"label":"Second"}]},
        {"id":"many","header":"Options","question":"Choose two","selection":{"mode":"multiple","minSelections":2,"maxSelections":2},"options":[{"label":"Red"},{"label":"Green"},{"label":"Blue"}]},
        {"id":"free","header":"Explain","question":"Anything else?","selection":{"mode":"single"},"options":[{"label":"Yes"},{"label":"No"}]}
    ]})
}
fn answers() -> Value {
    json!({"action":"answer","answers":[{"questionId":"one","selectedLabel":"First"},{"questionId":"many","selectedLabels":["Red","Blue"]},{"questionId":"free","freeText":"A dummy explanation","note":"Dummy note"}]})
}
#[test]
fn validates_all_questions_exact_labels_counts_and_unicode_lengths() -> Result<()> {
    let question = Prompt::parse(prompt())?;
    let response: Response = serde_json::from_value(answers())?;
    question.validate(&response)?;
    let mut cases = vec![];
    let mut value = answers();
    value["answers"][0]["selectedLabel"] = "Invented".into();
    cases.push(value);
    let mut value = answers();
    value["answers"][1]["selectedLabels"] = json!(["Red"]);
    cases.push(value);
    let mut value = answers();
    value["answers"][1]["selectedLabels"] = json!(["Red", "Red"]);
    cases.push(value);
    let mut value = answers();
    value["answers"][0]["freeText"] = "Mixed representations".into();
    cases.push(value);
    let mut value = answers();
    value["answers"].as_array_mut().unwrap().pop();
    cases.push(value);
    let mut value = answers();
    value["answers"][2]["freeText"] = "あ".repeat(501).into();
    cases.push(value);
    for invalid in cases {
        assert!(
            question
                .validate(&serde_json::from_value(invalid)?)
                .is_err()
        );
    }
    let mut value = answers();
    value["answers"][2]["freeText"] = "あ".repeat(500).into();
    question.validate(&serde_json::from_value(value)?)?;
    question.validate(&Response::Explain {
        content: "This needs a different framing.".into(),
    })?;
    let mut repeated = prompt();
    repeated["viewCursor"] = "later".into();
    assert_eq!(question.revision, Prompt::parse(repeated)?.revision);
    let mut changed = prompt();
    changed["questions"][0]["question"] = "Changed".into();
    assert_ne!(question.revision, Prompt::parse(changed)?.revision);
    Ok(())
}
struct Harness {
    _host: test_host::IdleTestHost,
    _root: tempfile::TempDir,
    journal: Journal,
    port: Port,
    incoming: BufReader<DuplexStream>,
    outgoing: DuplexStream,
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<()>>,
    capacity: Arc<intercat_daemon::scheduler::Capacity>,
    _admission: tokio::sync::SemaphorePermit<'static>,
}

#[tokio::test]
async fn a_recovered_answer_waits_for_host_resolution_and_cannot_be_sent_again() -> Result<()> {
    let request = approval("recovered", &["echo", "test"]);
    let recovery = intercat_daemon::reconcile::State {
        decided_approvals: std::collections::BTreeSet::from([
            intercat_daemon::reconcile::approval_key("recovered", &request["currentRequirementId"]),
        ]),
        ..Default::default()
    };
    let mut h = Harness::build(false, Some(recovery)).await?;
    h.send(json!({"method":"approval/request","params":request}))
        .await?;
    for _ in 0..100 {
        if h.state().await?["approvals"][0]["id"] == "recovered" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let state = h.state().await?;
    assert_eq!(state["approvals"][0]["canAllow"], false);
    assert_eq!(state["approvals"][0]["canDeny"], false);
    assert!(
        h.port
            .answer(
                Uuid::now_v7(),
                "recovered".into(),
                request["currentRequirementId"].clone(),
                intercat_daemon::approval::Response::Allow
            )
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), h.read())
            .await
            .is_err()
    );
    h.send(json!({"method":"approval/resolved","params":{"sessionId":"session","approvalId":"recovered"}})).await?;
    for _ in 0..100 {
        if h.state().await?["approvals"].as_array().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    h.send(json!({"method":"approval/request","params":request}))
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(h.state().await?["approvals"].as_array().unwrap().is_empty());
    h.close().await
}

fn approval(id: &str, argv: &[&str]) -> Value {
    json!({"sessionId":"session","approvalId":id,"turnId":"turn","itemId":format!("item-{id}"),"rawArgs":"{}","subject":{"kind":"shell","command":argv.join(" "),"stages":[{"argv":argv,"argvComplete":true}]},"currentRequirementId":{"approvalId":id,"sourceIndex":0},"availableChoices":[{"scope":"once","decision":"approved","choiceId":"yes"},{"scope":"once","decision":"denied","choiceId":"no"}]})
}

#[tokio::test]
async fn question_responses_reuse_the_held_slot_while_other_work_waits() -> Result<()> {
    for method in [
        "userInput/answer",
        "userInput/clarify",
        "userInput/interrupt",
    ] {
        let turn = Uuid::now_v7();
        let mut h = Harness::build(
            false,
            Some(intercat_daemon::reconcile::State {
                resume_turn: Some(turn),
                ..Default::default()
            }),
        )
        .await?;
        h.capacity.set_limit(1, 1);
        let mut request = prompt();
        request["turnId"] = json!(turn);
        h.send(json!({"method":"userInput/request","params":request}))
            .await?;
        assert_eq!(h.read().await?["method"], "userInput/engaged");
        let shown = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let Some(value) = h.state().await?["prompts"]
                    .as_array()
                    .and_then(|values| values.first())
                {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(value.clone());
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        let response = match method {
            "userInput/answer" => serde_json::from_value(answers())?,
            "userInput/clarify" => Response::Explain {
                content: "Dummy clarification.".into(),
            },
            _ => Response::Stop { answers: vec![] },
        };
        let mut other = h.capacity.request(
            "later-work".into(),
            intercat_daemon::scheduler::Priority::User,
        );
        assert!(other.try_acquire().is_none());
        let port = h.port.clone();
        let id = Uuid::now_v7();
        let answering = tokio::spawn(async move {
            port.respond(
                id,
                "prompt".into(),
                shown["revision"].as_str().unwrap().into(),
                response,
            )
            .await
        });
        let sent = h.read().await?;
        assert_eq!(sent["method"], method);
        assert_eq!(h.capacity.snapshot().active, 1);
        assert!(other.try_acquire().is_none());
        if method != "userInput/interrupt" {
            h.send(json!({"id":sent["id"],"result":{"commandId":id,"userInputId":"prompt","status":"accepted"}})).await?;
        }
        answering.await??;
        assert_eq!(h.capacity.snapshot().active, 1);
        assert!(other.try_acquire().is_none());
        let capacity = h.capacity.clone();
        h.close().await?;
        assert_eq!(capacity.snapshot().active, 0);
        assert!(other.try_acquire().is_some());
    }
    Ok(())
}

#[tokio::test]
async fn approval_responses_reuse_the_held_slot_while_other_work_waits() -> Result<()> {
    for choice in ["allow", "deny", "reply"] {
        let turn = Uuid::now_v7();
        let mut h = Harness::build(
            false,
            Some(intercat_daemon::reconcile::State {
                resume_turn: Some(turn),
                ..Default::default()
            }),
        )
        .await?;
        h.capacity.set_limit(1, 1);
        h.journal
            .set_stops(
                Uuid::now_v7(),
                intercat_daemon::policy::Stops::EveryApproval,
            )
            .await?;
        let mut request = approval("held-approval", &["node", "--test"]);
        request["turnId"] = json!(turn);
        request["availableChoices"][1]["acceptsFeedback"] = json!(true);
        h.send(json!({"method":"approval/request","params":request}))
            .await?;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while h.state().await?["approvals"].as_array().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await??;
        let response = match choice {
            "allow" => intercat_daemon::approval::Response::Allow,
            "deny" => intercat_daemon::approval::Response::Deny,
            _ => intercat_daemon::approval::Response::Reply {
                text: "Dummy denial with clarification.".into(),
                mode: intercat_daemon::approval::ReplyMode::Feedback,
            },
        };
        let mut other = h.capacity.request(
            "later-work".into(),
            intercat_daemon::scheduler::Priority::User,
        );
        assert!(other.try_acquire().is_none());
        let port = h.port.clone();
        let id = Uuid::now_v7();
        let answering = tokio::spawn(async move {
            port.answer(
                id,
                "held-approval".into(),
                json!({"approvalId":"held-approval","sourceIndex":0}),
                response,
            )
            .await
        });
        let sent = h.read().await?;
        assert_eq!(sent["method"], "approval/decide");
        assert_eq!(
            sent["params"]["choiceId"],
            if choice == "allow" { "yes" } else { "no" }
        );
        assert_eq!(h.capacity.snapshot().active, 1);
        assert!(other.try_acquire().is_none());
        h.send(json!({"id":sent["id"],"result":{"commandId":id,"approvalId":"held-approval","status":"accepted","terminal":true}})).await?;
        answering.await??;
        assert_eq!(h.capacity.snapshot().active, 1);
        assert!(other.try_acquire().is_none());
        let capacity = h.capacity.clone();
        h.close().await?;
        assert_eq!(capacity.snapshot().active, 0);
        assert!(other.try_acquire().is_some());
    }
    Ok(())
}

#[tokio::test]
async fn timed_out_questions_and_approvals_keep_the_running_turn_slot() -> Result<()> {
    for question in [true, false] {
        let turn = Uuid::now_v7();
        let mut h = Harness::build(
            false,
            Some(intercat_daemon::reconcile::State {
                resume_turn: Some(turn),
                ..Default::default()
            }),
        )
        .await?;
        h.capacity.set_limit(1, 1);
        if question {
            let mut request = prompt();
            request["turnId"] = json!(turn);
            request["autoResolutionMs"] = json!(25);
            h.send(json!({"method":"userInput/request","params":request}))
                .await?;
            assert_eq!(h.read().await?["method"], "userInput/engaged");
        } else {
            h.journal
                .set_stops(
                    Uuid::now_v7(),
                    intercat_daemon::policy::Stops::EveryApproval,
                )
                .await?;
            let mut request = approval("expired-approval", &["node", "--test"]);
            request["turnId"] = json!(turn);
            h.send(json!({"method":"approval/request","params":request}))
                .await?;
        }
        let field = if question { "prompts" } else { "approvals" };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while h.state().await?[field].as_array().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await??;
        let mut other = h.capacity.request(
            "later-work".into(),
            intercat_daemon::scheduler::Priority::User,
        );
        assert!(other.try_acquire().is_none());
        if question {
            h.send(json!({"method":"userInput/settled","params":{"sessionId":"session","userInputId":"prompt","outcome":"timedOut","answers":[]}})).await?;
        } else {
            h.send(json!({"method":"approval/resolved","params":{"sessionId":"session","approvalId":"expired-approval","decision":"timedOut"}})).await?;
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !h.state().await?[field].as_array().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await??;
        assert_eq!(h.capacity.snapshot().active, 1);
        assert!(other.try_acquire().is_none());
        h.close().await?;
        assert!(other.try_acquire().is_some());
    }
    Ok(())
}

#[tokio::test]
async fn approval_feedback_is_a_durable_denial_even_when_the_text_asks_to_allow() -> Result<()> {
    use intercat_daemon::approval::{ReplyMode, Response as ApprovalResponse};
    let mut h = Harness::new().await?;
    let mut request = approval("feedback", &["git", "push", "origin", "main"]);
    request["availableChoices"].as_array_mut().unwrap().push(json!({"scope":"once","decision":"abort","choiceId":"with-feedback","acceptsFeedback":true}));
    h.send(json!({"method":"approval/request","params":request}))
        .await?;
    for _ in 0..100 {
        if h.state().await?["approvals"][0]["replyMode"] == "feedback" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(h.state().await?["approvals"][0]["replyMode"], "feedback");
    let lock = rusqlite::Connection::open(h._root.path().join("events.sqlite"))?;
    lock.execute_batch("BEGIN IMMEDIATE")?;
    let id = Uuid::now_v7();
    let response = ApprovalResponse::Reply {
        text: "Yes, run it".into(),
        mode: ReplyMode::Feedback,
    };
    let port = h.port.clone();
    let requirement = request["currentRequirementId"].clone();
    let sent = response.clone();
    let answer =
        tokio::spawn(async move { port.answer(id, "feedback".into(), requirement, sent).await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), h.read())
            .await
            .is_err()
    );
    lock.execute_batch("ROLLBACK")?;
    let command = h.read().await?;
    assert_eq!(command["method"], "approval/decide");
    assert_eq!(command["params"]["choiceId"], "with-feedback");
    assert_eq!(command["params"]["feedback"], "Yes, run it");
    assert!(command["params"].get("response").is_none());
    assert_eq!(h.journal.operation(id).await?.unwrap()["state"], "prepared");
    h.send(json!({"id":command["id"],"result":{"commandId":id,"approvalId":"feedback","status":"accepted","terminal":true}})).await?;
    answer.await??;
    for _ in 0..100 {
        if h.journal.operation(id).await?.unwrap()["state"] == "complete" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        intercat_daemon::approval::replay(
            &h.journal,
            id,
            &intercat_daemon::secret_input::Owner::Work
                .key(h.state().await?["trackId"].as_str().unwrap()),
            "feedback",
            &request["currentRequirementId"],
            &response
        )
        .await?
    );
    h.close().await
}

#[tokio::test]
async fn approval_without_feedback_interrupts_before_waiting_for_the_writer() -> Result<()> {
    use intercat_daemon::approval::{ReplyMode, Response as ApprovalResponse};
    let turn = Uuid::now_v7();
    let recovery = intercat_daemon::reconcile::State {
        resume_turn: Some(turn),
        ..Default::default()
    };
    let mut h = Harness::build(false, Some(recovery)).await?;
    let mut request = approval("restart", &["git", "push", "origin", "main"]);
    request["turnId"] = json!(turn);
    h.send(json!({"method":"approval/request","params":request}))
        .await?;
    for _ in 0..100 {
        if h.state().await?["approvals"][0]["id"] == "restart" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let lock = rusqlite::Connection::open(h._root.path().join("events.sqlite"))?;
    lock.execute_batch("BEGIN IMMEDIATE")?;
    let id = Uuid::now_v7();
    let port = h.port.clone();
    let requirement = request["currentRequirementId"].clone();
    let answer = tokio::spawn(async move {
        port.answer(
            id,
            "restart".into(),
            requirement,
            ApprovalResponse::Reply {
                text: "Explain before continuing.".into(),
                mode: ReplyMode::Restart,
            },
        )
        .await
    });
    let interrupted = h.read().await?;
    assert_eq!(interrupted["method"], "turn/interrupt");
    assert_eq!(interrupted["params"]["turnId"], turn.to_string());
    assert!(!answer.is_finished());
    lock.execute_batch("ROLLBACK")?;
    answer.await??;
    assert_eq!(
        h.journal.operation(id).await?.unwrap()["kind"],
        "approval.reply"
    );
    let input = h
        .journal
        .read_utterance(Uuid::new_v5(&id, b"approval-reply").to_string())
        .await?;
    assert_eq!(input["state"], "redirecting");
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), h.read())
            .await
            .is_err()
    );
    h.close().await
}

#[tokio::test]
async fn a_boundary_hold_keeps_current_approval_actionable() -> Result<()> {
    let mut h = Harness::new().await?;
    let request = approval("boundary-push", &["git", "push", "origin", "HEAD:probe"]);
    h.send(json!({"method":"approval/request","params":request}))
        .await?;
    for _ in 0..100 {
        if h.state().await?["approvals"][0]["id"] == "boundary-push" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    h.port.hold(Uuid::now_v7())?;
    let answer = h.port.clone();
    let requirement = request["currentRequirementId"].clone();
    let mut reply = tokio::spawn(async move {
        answer
            .answer(
                Uuid::now_v7(),
                "boundary-push".into(),
                requirement,
                intercat_daemon::approval::Response::Deny,
            )
            .await
    });
    let sent = tokio::select! {
        sent = h.read() => sent?,
        early = &mut reply => return Err(format!("Approval ended before its wire request: {early:?}").into()),
    };
    assert_eq!(sent["method"], "approval/decide");
    assert_eq!(sent["params"]["choiceId"], "no");
    h.send(json!({"id":sent["id"],"result":{"commandId":sent["params"]["commandId"],"approvalId":"boundary-push","status":"accepted","terminal":true}})).await?;
    reply.await??;
    h.close().await
}

#[tokio::test]
async fn a_stale_captured_stop_does_not_interrupt_a_newer_active_turn() -> Result<()> {
    let active = Uuid::now_v7();
    let recovery = intercat_daemon::reconcile::State {
        resume_turn: Some(active),
        ..Default::default()
    };
    let mut h = Harness::build(false, Some(recovery)).await?;
    let old = Uuid::now_v7();
    h.port.stop_captured(Uuid::now_v7(), Some(old))?;
    let first = h.read().await?;
    assert_eq!(first["method"], "turn/interrupt");
    assert_eq!(first["params"]["turnId"], old.to_string());
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), h.read())
            .await
            .is_err()
    );
    assert_eq!(h.state().await?["activeTurn"], active.to_string());
    h.close().await
}

#[tokio::test]
async fn deletion_permission_waits_for_a_durable_snapshot_and_manual_permission_recaptures()
-> Result<()> {
    let mut h = Harness::build(true, None).await?;
    let path = h._root.path().canonicalize()?.join("worker");
    std::fs::write(path.join("keep"), "first edit\n")?;
    h.send(json!({"method":"approval/request","params":approval("delete-one",&["rm","keep"])}))
        .await?;
    let command = h.read().await?;
    assert_eq!(command["method"], "approval/decide");
    assert_eq!(command["params"]["choiceId"], "yes");
    let db = rusqlite::Connection::open(h._root.path().join("events.sqlite"))?;
    let snapshot_id:String=db.query_row("SELECT json_extract(record,'$.deletionSnapshotId') FROM approval_policies WHERE approval='delete-one'",[],|r|r.get(0))?;
    let saved = h.journal.deletion(snapshot_id.parse()?).await?;
    assert!(saved.complete);
    let policy_seq: i64 = db.query_row(
        "SELECT max(seq) FROM events WHERE kind='policy.evaluated'",
        [],
        |r| r.get(0),
    )?;
    let saved_seq: i64 = db.query_row(
        "SELECT max(seq) FROM events WHERE kind='deletion.saved'",
        [],
        |r| r.get(0),
    )?;
    assert!(saved_seq < policy_seq);
    std::fs::remove_file(path.join("keep"))?;
    h.send(json!({"id":command["id"],"result":{"commandId":command["params"]["commandId"],"approvalId":command["params"]["approvalId"],"status":"accepted","terminal":true}})).await?;
    h.port.restore(Uuid::now_v7(), saved.id).await?;
    assert_eq!(std::fs::read_to_string(path.join("keep"))?, "first edit\n");
    assert_eq!(h.state().await?["phase"], "held");

    h.journal
        .set_stops(
            Uuid::now_v7(),
            intercat_daemon::policy::Stops::EveryApproval,
        )
        .await?;
    h.port.start(Uuid::now_v7()).await?;
    let request = approval("delete-two", &["rm", "keep"]);
    h.send(json!({"method":"approval/request","params":request}))
        .await?;
    for _ in 0..200 {
        if h.state().await?["approvals"][0]["id"] == "delete-two" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        h.state().await?["approvals"][0]["deletion"]["complete"],
        true
    );
    std::fs::write(path.join("keep"), "edit while waiting\n")?;
    let port = h.port.clone();
    let id = Uuid::now_v7();
    let answer = tokio::spawn(async move {
        port.answer(
            id,
            "delete-two".into(),
            request["currentRequirementId"].clone(),
            intercat_daemon::approval::Response::Allow,
        )
        .await
    });
    let command = h.read().await?;
    let latest:String=db.query_row("SELECT json_extract(payload,'$.snapshotId') FROM events WHERE kind='deletion.prepared' AND json_extract(payload,'$.commandId')=?1",[id.to_string()],|r|r.get(0))?;
    let latest = h.journal.deletion(latest.parse()?).await?;
    let repo = intercat_git::Repository::open(&path)?;
    let files = repo.saved_files(latest.sha.parse()?)?;
    assert_eq!(repo.saved_bytes(&files["keep"])?, b"edit while waiting\n");
    h.send(json!({"id":command["id"],"result":{"commandId":command["params"]["commandId"],"approvalId":command["params"]["approvalId"],"status":"accepted","terminal":true}})).await?;
    answer.await??;
    h.close().await
}

#[tokio::test]
async fn approval_policy_is_saved_before_send_and_cannot_bypass_an_unsupported_action() -> Result<()>
{
    use intercat_daemon::policy::Stops;
    // Stop targets the known active turn, including while the journal is locked.
    // An idle fixture has no turn to interrupt.
    let turn = Uuid::now_v7();
    let mut h = Harness::build(
        false,
        Some(intercat_daemon::reconcile::State {
            resume_turn: Some(turn),
            ..Default::default()
        }),
    )
    .await?;
    let active_approval = |id: &str, argv: &[&str]| {
        let mut value = approval(id, argv);
        value["turnId"] = json!(turn);
        value
    };
    h.journal
        .set_stops(Uuid::now_v7(), Stops::EveryApproval)
        .await?;
    let first = active_approval("one", &["node", "--test"]);
    h.send(json!({"method":"approval/request","params":first}))
        .await?;
    for _ in 0..100 {
        if h.state().await?["approvals"].as_array().unwrap().len() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        h.state().await?["approvals"][0]["reason"]["kind"],
        "approval"
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(40), h.incoming.fill_buf())
            .await
            .is_err()
    );
    let port = h.port.clone();
    let id = Uuid::now_v7();
    let requirement = first["currentRequirementId"].clone();
    let answer = tokio::spawn(async move {
        port.answer(
            id,
            "one".into(),
            requirement,
            intercat_daemon::approval::Response::Allow,
        )
        .await
    });
    let command = h
        .read()
        .await
        .map_err(|error| format!("first explicit approval: {error}"))?;
    assert_eq!(command["method"], "approval/decide");
    assert_eq!(command["params"]["choiceId"], "yes");
    assert_eq!(h.journal.operation(id).await?.unwrap()["state"], "prepared");
    h.send(json!({"id":command["id"],"result":{"commandId":command["params"]["commandId"],"approvalId":command["params"]["approvalId"],"status":"accepted","terminal":true}})).await?;
    answer.await??;

    h.journal
        .set_stops(Uuid::now_v7(), Stops::IrreversibleOnly)
        .await?;
    h.send(json!({"method":"approval/request","params":active_approval("two",&["node","--test"])}))
        .await?;
    let command = h
        .read()
        .await
        .map_err(|error| format!("automatic approval: {error}"))?;
    assert_eq!(command["params"]["choiceId"], "yes");
    h.send(json!({"id":command["id"],"result":{"commandId":command["params"]["commandId"],"approvalId":command["params"]["approvalId"],"status":"accepted","terminal":true}})).await?;

    let third = active_approval("three", &["crontab", "jobs.txt"]);
    h.send(json!({"method":"approval/request","params":third}))
        .await?;
    for _ in 0..100 {
        if h.state().await?["approvals"][0]["id"] == "three" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(h.state().await?["approvals"][0]["canAllow"], false);
    assert!(
        h.port
            .answer(
                Uuid::now_v7(),
                "three".into(),
                third["currentRequirementId"].clone(),
                intercat_daemon::approval::Response::Allow
            )
            .await
            .is_err()
    );
    let port = h.port.clone();
    let requirement = third["currentRequirementId"].clone();
    let answer = tokio::spawn(async move {
        port.answer(
            Uuid::now_v7(),
            "three".into(),
            requirement,
            intercat_daemon::approval::Response::Deny,
        )
        .await
    });
    let command = h
        .read()
        .await
        .map_err(|error| format!("unsupported-action denial: {error}"))?;
    assert_eq!(command["params"]["choiceId"], "no");
    h.send(json!({"id":command["id"],"result":{"commandId":command["params"]["commandId"],"approvalId":command["params"]["approvalId"],"status":"accepted","terminal":true}})).await?;
    answer.await??;

    let db = rusqlite::Connection::open(h._root.path().join("events.sqlite"))?;
    assert_eq!(
        db.query_row(
            "SELECT json_extract(record,'$.stops') FROM approval_policies WHERE approval='one'",
            [],
            |r| r.get::<_, String>(0)
        )?,
        "every-approval"
    );
    db.execute_batch("BEGIN IMMEDIATE")?;
    h.send(
        json!({"method":"approval/request","params":active_approval("four",&["node","--test"])}),
    )
    .await?;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    h.port.stop(Uuid::now_v7())?;
    assert_eq!(
        h.read()
            .await
            .map_err(|error| format!("Stop with locked journal: {error}"))?["method"],
        "turn/interrupt"
    );
    db.execute_batch("COMMIT")?;
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    while let Ok(buffer) =
        tokio::time::timeout(std::time::Duration::from_millis(60), h.incoming.fill_buf()).await
    {
        if buffer?.is_empty() {
            break;
        }
        assert_eq!(h.read().await?["method"], "turn/interrupt");
    }
    h.close().await
}
impl Harness {
    async fn new() -> Result<Self> {
        Self::build(false, None).await
    }
    async fn build(
        with_files: bool,
        recovery: Option<intercat_daemon::reconcile::State>,
    ) -> Result<Self> {
        let admission = test_host::FILE_FIXTURES.acquire().await?;
        let host = test_host::IdleTestHost::new()?;
        let root = tempfile::tempdir()?;
        let path = if with_files {
            root.path().canonicalize()?.join("worker")
        } else {
            root.path().to_owned()
        };
        let mut base = String::new();
        if with_files {
            std::fs::create_dir(&path)?;
            let git = |args: &[&str]| -> String {
                let output = std::process::Command::new("/usr/bin/git")
                    .current_dir(&path)
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .args(args)
                    .output()
                    .unwrap();
                assert!(output.status.success());
                String::from_utf8(output.stdout).unwrap().trim().into()
            };
            git(&["init", "-q", "-b", "main"]);
            std::fs::write(path.join("keep"), "base\n")?;
            git(&["add", "."]);
            git(&[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "base",
            ]);
            base = git(&["rev-parse", "HEAD"]);
        }
        let journal = Journal::open(
            root.path().join("events.sqlite"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        journal
            .stage_utterance(
                Uuid::now_v7(),
                "Dummy track".into(),
                Target::New,
                None,
                Vec::new(),
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
                    boundary: Some("when-told".into()),
                    command: None,
                    confidence: 1.,
                    inference_ms: 1.,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await?;
        let track = journal.snapshot().await?["tracks"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        if with_files {
            journal
                .workspace_result(intercat_daemon::preparation::Workspace {
                    denied_roots: Vec::new(),
                    attachment_roots: Vec::new(),
                    track_id: track.clone(),
                    path: path.clone(),
                    reference: "refs/intercat/test/track".into(),
                    base,
                    state: "ready".into(),
                    error: None,
                })
                .await?;
        }
        let mut state = WorkerState {
            secret_revision: None,
            track_id: track.clone(),
            phase: "ready".into(),
            session_id: Some("session".into()),
            identity: None,
            detail: None,
            active_turn: recovery.as_ref().and_then(|state| state.resume_turn),
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        };
        journal.worker_state(state.clone()).await?;
        let (read, outgoing) = tokio::io::duplex(65536);
        let (write, incoming) = tokio::io::duplex(65536);
        let mut connection = Connection::new(read, write);
        let (port, commands) = Port::new(std::sync::Arc::new(
            intercat_daemon::dispatch_control::Gate::new(false),
        ));
        port.attach(connection.client.clone(), "session".into(), None);
        let editor = Manager::new(root.path(), &root.path().join("editor.log"))?;
        let capacity = Arc::new(intercat_daemon::scheduler::Capacity::new(4));
        let mut context = Context {
            project_secrets: intercat_daemon::project_secrets::Store::default(),
            port: port.clone(),
            commands,
            editor,
            capacity: capacity.clone(),
        };
        let (stop, mut stopped) = watch::channel(false);
        let record = journal.clone();
        let identity = host.identity()?;
        let tracker = Arc::new(Mutex::new(Tracker::new(
            Uuid::now_v7().to_string(),
            identity,
        )?));
        let task = tokio::spawn(async move {
            let mut continuity = execution::Continuity::default();
            let result = execution::run(
                WorkerIo {
                    connection: &mut connection,
                    tracker,
                    identity,
                    recovery,
                    continuity: &mut continuity,
                },
                &record,
                &path,
                &mut state,
                &mut context,
                &mut stopped,
            )
            .await;
            connection.close();
            let settled = continuity.settle().await;
            result?;
            settled
        });
        Ok(Self {
            _admission: admission,
            _host: host,
            _root: root,
            journal,
            port,
            incoming: BufReader::new(incoming),
            outgoing,
            stop,
            task,
            capacity,
        })
    }
    async fn send(&mut self, mut value: Value) -> Result<()> {
        value["jsonrpc"] = json!("2.0");
        self.outgoing
            .write_all(format!("{value}\n").as_bytes())
            .await?;
        Ok(())
    }
    async fn read(&mut self) -> Result<Value> {
        // Cold local privacy initialization can exceed two seconds. Tests of
        // Stop and engagement latency keep their own shorter outer deadlines.
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.incoming.read_line(&mut line),
        )
        .await??;
        Ok(serde_json::from_str(&line)?)
    }
    async fn state(&self) -> Result<Value> {
        Ok(self.journal.snapshot().await?["workers"][0].clone())
    }
    async fn observe(&mut self) -> Result<Prompt> {
        self.send(json!({"jsonrpc":"2.0","id":100,"method":"userInput/request","params":prompt()}))
            .await?;
        assert_eq!(self.read().await?["method"], "userInput/engaged");
        let receipt = self.read().await?;
        assert_eq!(receipt["id"], 100);
        assert_eq!(receipt["result"], json!({}));
        for _ in 0..100 {
            let state = self.state().await?;
            if state["prompts"].as_array().is_some_and(|p| !p.is_empty()) {
                return Ok(serde_json::from_value(state["prompts"][0].clone())?);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        Err(std::io::Error::other("Prompt did not become visible").into())
    }
    async fn close(self) -> Result<()> {
        // A stopped connection may have completed before this cleanup runs.
        let _ = self.stop.send(true);
        self.task.await??;
        Ok(())
    }
}
#[tokio::test]
async fn prompt_answer_is_durable_before_send_and_settlement_wins_over_late_ack() -> Result<()> {
    let mut h = Harness::new().await?;
    let p = h.observe().await?;
    assert!(h.port.start(Uuid::now_v7()).await.is_err());
    assert!(
        h.port
            .respond(
                Uuid::now_v7(),
                "prompt".into(),
                "old revision".into(),
                serde_json::from_value(answers())?
            )
            .await
            .is_err()
    );
    let id = Uuid::now_v7();
    let port = h.port.clone();
    let rev = p.revision.clone();
    let task = tokio::spawn(async move {
        port.respond(
            id,
            "prompt".into(),
            rev,
            serde_json::from_value(answers()).unwrap(),
        )
        .await
    });
    let command = h.read().await?;
    assert_eq!(command["method"], "userInput/answer");
    assert_eq!(command["params"]["answers"], answers()["answers"]);
    assert_eq!(h.journal.operation(id).await?.unwrap()["state"], "prepared");
    h.send(json!({"jsonrpc":"2.0","method":"userInput/settled","params":{"sessionId":"session","userInputId":"prompt","outcome":"answered"}})).await?;
    h.send(json!({"jsonrpc":"2.0","id":command["id"],"result":{"commandId":id,"userInputId":"prompt","status":"accepted"}})).await?;
    task.await??;
    assert_eq!(h.journal.operation(id).await?.unwrap()["state"], "complete");
    assert!(h.state().await?["prompts"].as_array().unwrap().is_empty());
    assert!(
        h.port
            .respond(
                Uuid::now_v7(),
                "prompt".into(),
                p.revision,
                serde_json::from_value(answers())?
            )
            .await
            .is_err()
    );
    h.send(json!({"jsonrpc":"2.0","method":"userInput/requested","params":prompt()}))
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    assert!(h.state().await?["prompts"].as_array().unwrap().is_empty());
    h.close().await
}

#[tokio::test]
async fn engagement_does_not_wait_for_the_sqlite_writer() -> Result<()> {
    let mut h = Harness::new().await?;
    let lock = rusqlite::Connection::open(h._root.path().join("events.sqlite"))?;
    lock.execute_batch("BEGIN IMMEDIATE")?;
    h.send(json!({"jsonrpc":"2.0","id":100,"method":"userInput/request","params":prompt()}))
        .await?;
    let note = tokio::time::timeout(std::time::Duration::from_millis(300), h.read()).await??;
    assert_eq!(note["method"], "userInput/engaged");
    assert_eq!(h.read().await?["id"], 100);
    lock.execute_batch("ROLLBACK")?;
    h.close().await
}
#[tokio::test]
async fn partial_answers_stop_the_prompt_even_while_sqlite_is_locked() -> Result<()> {
    let mut h = Harness::new().await?;
    let p = h.observe().await?;
    let lock = rusqlite::Connection::open(h._root.path().join("events.sqlite"))?;
    lock.execute_batch("BEGIN IMMEDIATE")?;
    let port = h.port.clone();
    let task = tokio::spawn(async move {
        port.respond(
            Uuid::now_v7(),
            "prompt".into(),
            p.revision,
            serde_json::from_value(
                json!({"action":"stop","answers":[{"questionId":"one","selectedLabel":"Second"}]}),
            )
            .unwrap(),
        )
        .await
    });
    let command = tokio::time::timeout(std::time::Duration::from_millis(300), h.read()).await??;
    assert_eq!(command["method"], "userInput/interrupt");
    assert_eq!(
        command["params"]["answers"],
        json!([{"questionId":"one","selectedLabel":"Second"}])
    );
    lock.execute_batch("ROLLBACK")?;
    task.await??;
    h.close().await
}

#[tokio::test]
async fn a_persisted_unfinished_restore_holds_new_work_and_reuses_the_same_operation() -> Result<()>
{
    let h = Harness::build(true, None).await?;
    let track = h.state().await?["trackId"].as_str().unwrap().to_owned();
    let workspace = h.journal.workspace(track.clone()).await?;
    std::fs::write(workspace.path.join("keep"), "saved\n")?;
    let saved = intercat_daemon::deletion::prepare(
        workspace.clone(),
        None,
        approval("restore", &["rm", "keep"]),
        workspace.base.clone(),
        h.journal.clone(),
    )
    .await?;
    std::fs::write(workspace.path.join("keep"), "before restore\n")?;
    let backup = intercat_daemon::deletion::before_restore(&workspace, &saved, &h.journal).await?;
    let id = Uuid::now_v7();
    h.journal
        .intent(
            &id.to_string(),
            "deletion.restore",
            json!({"trackId":track,"snapshotId":saved.id,"backupId":backup.id}),
        )
        .await?;
    for _ in 0..200 {
        if h.state().await?["phase"] == "needs-recovery" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(h.state().await?["phase"], "needs-recovery");
    assert_eq!(
        h.journal.snapshot().await?["pendingRestores"][0]["operationId"],
        id.to_string()
    );
    assert!(h.port.start(Uuid::now_v7()).await.is_err());
    assert!(h.port.restore(Uuid::now_v7(), saved.id).await.is_err());
    h.port.restore(id, saved.id).await?;
    assert_eq!(
        std::fs::read_to_string(workspace.path.join("keep"))?,
        "saved\n"
    );
    assert!(
        h.journal.snapshot().await?["pendingRestores"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(h.state().await?["phase"], "held");
    h.close().await
}

#[tokio::test]
async fn approval_presentation_overtakes_bulk_output_in_the_execution_loop() -> Result<()> {
    let mut h = Harness::build(false, None).await?;
    h.journal
        .set_stops(
            Uuid::now_v7(),
            intercat_daemon::policy::Stops::EveryApproval,
        )
        .await?;
    let database = h._root.path().join("events.sqlite");
    let mut lock = rusqlite::Connection::open(&database)?;
    let transaction = lock.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for ordinal in 0..7 {
        h.send(json!({"method":"item/updated","params":{"sessionId":"session","item":{"itemId":format!("bulk-{ordinal}"),"kind":"agentMessage","text":"x".repeat(600_000),"turnId":"bulk-turn","messagePhase":"commentary"}}})).await?;
    }
    h.send(json!({"method":"approval/request","params":approval("priority-review",&["node","--test"])})).await?;
    transaction.commit()?;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if h.state().await?["approvals"]
                .as_array()
                .is_some_and(|cards| cards.iter().any(|card| card["id"] == "priority-review"))
            {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await??;
    let db = rusqlite::Connection::open(&database)?;
    let approval:i64=db.query_row("SELECT MIN(seq) FROM events WHERE kind='msp.applied' AND json_extract(payload,'$.params.approvalId')='priority-review'",[],|row|row.get(0))?;
    let output:Option<i64>=db.query_row("SELECT MIN(seq) FROM events WHERE kind='msp.applied' AND json_extract(payload,'$.params.item.itemId') LIKE 'bulk-%'",[],|row|row.get(0))?;
    assert!(
        output.is_none_or(|output| approval < output),
        "Approval must be processed before the queued large bodies"
    );
    let received=db.prepare("SELECT coalesce(json_extract(payload,'$.params.item.itemId'),json_extract(payload,'$.params.approvalId')) FROM events WHERE kind='msp.received' AND (json_extract(payload,'$.params.item.itemId') LIKE 'bulk-%' OR json_extract(payload,'$.params.approvalId')='priority-review') ORDER BY seq")?.query_map([],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    assert_eq!(
        received,
        (0..7)
            .map(|n| format!("bulk-{n}"))
            .chain(std::iter::once("priority-review".into()))
            .collect::<Vec<_>>()
    );
    h.close().await
}

async fn secret_question(
    h: &mut Harness,
    turn: Uuid,
) -> Result<intercat_daemon::secret_input::Request> {
    let mut question = prompt();
    question["turnId"] = json!(turn);
    h.send(json!({"id":100,"method":"userInput/request","params":question}))
        .await?;
    assert_eq!(h.read().await?["method"], "userInput/engaged");
    assert_eq!(h.read().await?["id"], 100);
    let question = Prompt::parse(question)?;
    for _ in 0..100 {
        if !h.state().await?["prompts"].as_array().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let track = h.state().await?["trackId"].as_str().unwrap().to_owned();
    let key = intercat_daemon::secrets::Key::new([7; 32]);
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let value = "dummy-secret-for-question-test";
    let catalog = h
        .journal
        .register_fingerprint(intercat_daemon::secrets::Registration {
            op_id: Uuid::now_v7(),
            track_id: track.clone(),
            session_id: "session".into(),
            name: "NPM_TOKEN".into(),
            byte_len: value.len(),
            tag: hex(&key.tag(value.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    h.journal.confirm_fingerprints(catalog.current).await?;
    Ok(intercat_daemon::secret_input::Request {
        owner: intercat_daemon::secret_input::Owner::Work,
        op_id: Uuid::now_v7(),
        track_id: track,
        session_id: "session".into(),
        turn_id: turn,
        question: intercat_daemon::secret_input::Question::Msp {
            id: "prompt".into(),
            revision: question.revision,
        },
        name: "NPM_TOKEN".into(),
        value: value.into(),
    })
}

#[tokio::test]
async fn secret_restart_waits_for_cancelled_turn_question_settlement_and_saved_files() -> Result<()>
{
    let turn = Uuid::now_v7();
    let mut h = Harness::build(
        true,
        Some(intercat_daemon::reconcile::State {
            active_turn: Some(turn),
            resume_turn: Some(turn),
            ..Default::default()
        }),
    )
    .await?;
    let request = secret_question(&mut h, turn).await?;
    let id = request.op_id;
    let install = h.journal.prepare_secret(request).await?.unwrap();
    let port = h.port.clone();
    let accepting = tokio::spawn(async move { port.install_secret(install).await });
    let mut interrupted = vec![
        h.read().await?["method"].clone(),
        h.read().await?["method"].clone(),
    ];
    interrupted.sort_by_key(|v| v.as_str().unwrap().to_owned());
    assert_eq!(
        interrupted,
        vec![json!("turn/interrupt"), json!("userInput/interrupt")]
    );
    accepting.await??;
    assert!(!h.task.is_finished());
    let track = h.state().await?["trackId"].as_str().unwrap().to_owned();
    let workspace = h.journal.workspace(track).await?;
    std::fs::write(workspace.path.join("keep"), "saved before secret restart\n")?;
    h.send(json!({"method":"turn/completed","params":{"sessionId":"session","turnId":turn,"terminal":"cancelled"}})).await?;
    for _ in 0..200 {
        if !h.state().await?["checkpoint"].is_null() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!h.state().await?["checkpoint"].is_null());
    assert!(
        !h.task.is_finished(),
        "The open question must settle before host restart"
    );
    h.send(json!({"method":"userInput/settled","params":{"sessionId":"session","userInputId":"prompt","outcome":"interrupted"}})).await?;
    for _ in 0..200 {
        if h.task.is_finished() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(h.task.is_finished());
    assert_eq!(h.state().await?["phase"], "installing-secret");
    assert!(!h.state().await?["checkpointPending"].as_bool().unwrap());
    assert_eq!(
        h.journal.snapshot().await?["secretInputs"][0]["opId"],
        id.to_string()
    );
    assert!(h.port.start(Uuid::now_v7()).await.is_err());
    h.task.await??;
    Ok(())
}

#[tokio::test]
async fn stop_during_secret_acceptance_cancels_without_waiting_for_the_writer() -> Result<()> {
    let turn = Uuid::now_v7();
    let mut h = Harness::build(
        false,
        Some(intercat_daemon::reconcile::State {
            active_turn: Some(turn),
            resume_turn: Some(turn),
            ..Default::default()
        }),
    )
    .await?;
    let request = secret_question(&mut h, turn).await?;
    let install = h.journal.prepare_secret(request).await?.unwrap();
    let lock = rusqlite::Connection::open(h._root.path().join("events.sqlite"))?;
    lock.execute_batch("BEGIN IMMEDIATE")?;
    let port = h.port.clone();
    let accepting = tokio::spawn(async move { port.install_secret(install).await });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    h.port.stop(Uuid::now_v7())?;
    let interrupt = tokio::time::timeout(std::time::Duration::from_millis(300), h.read()).await??;
    assert!(matches!(
        interrupt["method"].as_str(),
        Some("turn/interrupt" | "userInput/interrupt")
    ));
    lock.execute_batch("ROLLBACK")?;
    assert!(accepting.await?.is_err());
    for _ in 0..100 {
        if h.journal.snapshot().await?["secretInputs"][0]["state"] == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        h.journal.snapshot().await?["secretInputs"][0]["state"],
        "failed"
    );
    h.close().await
}

#[tokio::test]
async fn question_stop_discards_an_already_accepted_secret() -> Result<()> {
    let turn = Uuid::now_v7();
    let mut h = Harness::build(
        false,
        Some(intercat_daemon::reconcile::State {
            active_turn: Some(turn),
            resume_turn: Some(turn),
            ..Default::default()
        }),
    )
    .await?;
    let request = secret_question(&mut h, turn).await?;
    let intercat_daemon::secret_input::Question::Msp { revision, .. } = &request.question else {
        unreachable!()
    };
    let revision = revision.clone();
    let install = h.journal.prepare_secret(request).await?.unwrap();
    let port = h.port.clone();
    let accepting = tokio::spawn(async move { port.install_secret(install).await });
    h.read().await?;
    h.read().await?;
    accepting.await??;
    h.port
        .respond(
            Uuid::now_v7(),
            "prompt".into(),
            revision,
            Response::Stop { answers: vec![] },
        )
        .await?;
    for _ in 0..100 {
        if h.journal.snapshot().await?["secretInputs"][0]["state"] == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        h.journal.snapshot().await?["secretInputs"][0]["state"],
        "failed"
    );
    assert!(!h.task.is_finished());
    h.close().await
}

#[tokio::test]
async fn a_stopped_report_can_resume_a_secret_without_an_active_msp_prompt() -> Result<()> {
    let h = Harness::new().await?;
    let track = h.state().await?["trackId"].as_str().unwrap().to_owned();
    rusqlite::Connection::open(h._root.path().join("events.sqlite"))?.execute(
        "INSERT INTO msp_positions VALUES('session',?1,NULL)",
        [&track],
    )?;
    rusqlite::Connection::open(h._root.path().join("events.sqlite"))?
        .execute("UPDATE utterances SET state='cut' WHERE state='placed'", [])?;
    let turn = Uuid::now_v7();
    h.journal
        .observe_report(intercat_daemon::reports::Report::new(
            track.clone(),
            "session".into(),
            turn,
            "report".into(),
            "Waiting for you: Provide a token.".into(),
        ))
        .await?;
    let snapshot = h.journal.snapshot().await?;
    let question = &snapshot["reportQuestions"][0];
    let key = intercat_daemon::secrets::Key::new([7; 32]);
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let value = "dummy-report-token";
    let catalog = h
        .journal
        .register_fingerprint(intercat_daemon::secrets::Registration {
            op_id: Uuid::now_v7(),
            track_id: track.clone(),
            session_id: "session".into(),
            name: "TOKEN".into(),
            byte_len: value.len(),
            tag: hex(&key.tag(value.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    h.journal.confirm_fingerprints(catalog.current).await?;
    let install = h
        .journal
        .prepare_secret(intercat_daemon::secret_input::Request {
            owner: intercat_daemon::secret_input::Owner::Work,
            op_id: Uuid::now_v7(),
            track_id: track,
            session_id: "session".into(),
            turn_id: turn,
            question: intercat_daemon::secret_input::Question::Report {
                id: question["id"].as_str().unwrap().into(),
                revision: question["revision"].as_i64().unwrap(),
            },
            name: "TOKEN".into(),
            value: value.into(),
        })
        .await?
        .unwrap();
    h.port.stop(Uuid::now_v7())?;
    for _ in 0..100 {
        if h.state().await?["phase"] == "held" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    h.port.start(Uuid::now_v7()).await?;
    assert!(!h.task.is_finished());
    h.port.install_secret(install).await?;
    for _ in 0..100 {
        if h.task.is_finished() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(h.task.is_finished());
    assert_eq!(h.state().await?["phase"], "installing-secret");
    h.task.await??;
    Ok(())
}
