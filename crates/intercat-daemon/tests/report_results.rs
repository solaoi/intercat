use intercat_daemon::{
    Result,
    checkpoint::Checkpoint,
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    runtime::WorkerState,
    secrets::Key,
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal, WorkerState)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Dummy result task".into(),
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
    let state = WorkerState {
        track_id: track,
        phase: "working".into(),
        session_id: Some("result-session".into()),
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
    journal.worker_state(state.clone()).await?;
    Ok((root, journal, state))
}

async fn put(journal: &Journal, worker: &WorkerState, frame: Value) -> Result<()> {
    journal
        .try_msp(
            worker.track_id.clone(),
            worker.session_id.clone().unwrap(),
            Stamp::now(),
            frame,
            None,
            None,
        )?
        .await??;
    Ok(())
}

async fn report(
    journal: &Journal,
    worker: &WorkerState,
    item: &str,
    revision: i64,
    text: &str,
    phase: &str,
) -> Result<()> {
    put(journal, worker, json!({"method":"item/completed","params":{
        "sessionId":worker.session_id,"item":{"itemId":item,"turnId":worker.active_turn,
        "kind":"agentMessage","messagePhase":phase,"revision":revision,"status":"completed","text":text}
    }})).await
}

async fn finish(journal: &Journal, worker: &mut WorkerState, terminal: &str) -> Result<()> {
    let turn = worker.active_turn.unwrap();
    put(journal, worker, json!({"method":"turn/completed","params":{"sessionId":worker.session_id,"turnId":turn,"terminal":terminal}})).await?;
    worker.active_turn = None;
    worker.phase = "checkpoint".into();
    worker.checkpoint_pending = true;
    journal.worker_state(worker.clone()).await?;
    assert!(journal.snapshot().await?["tracks"][0]["completion"].is_null());
    worker.phase = "ready".into();
    worker.checkpoint_pending = false;
    worker.checkpoint = Some(Checkpoint {
        turn_id: turn,
        sha: "a".repeat(40),
        reference: "refs/intercat/dummy".into(),
        unmerged_paths: vec![],
        live_processes: false,
    });
    journal.worker_state(worker.clone()).await
}

#[tokio::test]
async fn saved_final_result_has_exact_source_and_survives_reopen() -> Result<()> {
    for kind in ["done", "partial", "failed"] {
        let (root, journal, mut worker) = fixture().await?;
        let turn = worker.active_turn.unwrap();
        report(
            &journal,
            &worker,
            "final",
            1,
            &format!("Dummy report.\nResult: {kind} — 確認した結果です。"),
            "finalAnswer",
        )
        .await?;
        assert!(journal.snapshot().await?["tracks"][0]["completion"].is_null());
        finish(&journal, &mut worker, "completed").await?;
        let completion = journal.snapshot().await?["tracks"][0]["completion"].clone();
        assert_eq!(completion["sessionId"], "result-session");
        assert_eq!(completion["turnId"], turn.to_string());
        assert_eq!(
            completion["result"],
            json!({"kind":kind,"reason":"確認した結果です。","itemId":"final","revision":1})
        );
        drop(journal);
        let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
        assert_eq!(
            journal.snapshot().await?["tracks"][0]["completion"],
            completion
        );
    }
    Ok(())
}

#[tokio::test]
async fn latest_final_revision_controls_result_and_new_work_hides_old_result() -> Result<()> {
    let (_root, journal, mut worker) = fixture().await?;
    report(
        &journal,
        &worker,
        "final",
        1,
        "Result: done — initial",
        "finalAnswer",
    )
    .await?;
    let turn = worker.active_turn.unwrap();
    finish(&journal, &mut worker, "completed").await?;
    let mut source = worker.clone();
    source.active_turn = Some(turn);
    report(
        &journal,
        &source,
        "final",
        2,
        "Result: failed — corrected",
        "finalAnswer",
    )
    .await?;
    report(
        &journal,
        &source,
        "final",
        1,
        "Result: done — stale",
        "finalAnswer",
    )
    .await?;
    assert_eq!(
        journal.snapshot().await?["tracks"][0]["completion"]["result"]["kind"],
        "failed"
    );
    report(
        &journal,
        &source,
        "comment",
        1,
        "Result: done — not a final answer",
        "commentary",
    )
    .await?;
    assert_eq!(
        journal.snapshot().await?["tracks"][0]["completion"]["result"]["kind"],
        "failed"
    );
    worker.active_turn = Some(Uuid::now_v7());
    worker.phase = "working".into();
    journal.worker_state(worker.clone()).await?;
    assert!(journal.snapshot().await?["tracks"][0]["completion"].is_null());
    finish(&journal, &mut worker, "completed").await?;
    assert!(
        journal.snapshot().await?["tracks"][0]["completion"]["result"].is_null(),
        "do not use a previous turn's result"
    );
    Ok(())
}

#[tokio::test]
async fn missing_or_superseded_final_footer_stays_unclear_and_cancelled_is_not_done() -> Result<()>
{
    let (_root, journal, mut worker) = fixture().await?;
    report(
        &journal,
        &worker,
        "first-final",
        1,
        "Result: done — initial",
        "finalAnswer",
    )
    .await?;
    report(
        &journal,
        &worker,
        "last-final",
        1,
        "The last report has no result footer.",
        "finalAnswer",
    )
    .await?;
    finish(&journal, &mut worker, "completed").await?;
    assert!(journal.snapshot().await?["tracks"][0]["completion"]["result"].is_null());
    worker.active_turn = Some(Uuid::now_v7());
    worker.phase = "working".into();
    journal.worker_state(worker.clone()).await?;
    report(
        &journal,
        &worker,
        "cancelled-final",
        1,
        "Result: done — this must not turn a stop into success",
        "finalAnswer",
    )
    .await?;
    finish(&journal, &mut worker, "cancelled").await?;
    assert_eq!(journal.snapshot().await?["tracks"][0]["status"], "stopped");
    assert!(journal.snapshot().await?["tracks"][0]["completion"].is_null());
    Ok(())
}

#[tokio::test]
async fn truncated_retracted_or_incomplete_reports_never_show_success() -> Result<()> {
    for variant in 0..3 {
        let (_root, journal, mut worker) = fixture().await?;
        let mut frame = json!({"method":"item/completed","params":{
            "sessionId":worker.session_id,"item":{"itemId":"unreliable-final","turnId":worker.active_turn,
            "kind":"agentMessage","messagePhase":"finalAnswer","revision":1,"status":"completed",
            "text":"Result: done — this footer must not be trusted"}
        }});
        match variant {
            0 => frame["params"]["item"]["retracted"] = json!(true),
            1 => frame["_intercatTruncated"] = json!(["/params/item/text"]),
            _ => {
                frame["method"] = json!("item/updated");
                frame["params"]["item"]["status"] = json!("inProgress");
            }
        }
        put(&journal, &worker, frame).await?;
        finish(&journal, &mut worker, "completed").await?;
        assert!(
            journal.snapshot().await?["tracks"][0]["completion"]["result"].is_null(),
            "variant {variant}"
        );
    }
    Ok(())
}
