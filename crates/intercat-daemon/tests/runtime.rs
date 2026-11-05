use intercat_daemon::{
    decision::{Decision, Kind, Outcome, Target},
    journal::{Admission, Journal},
    runtime::{Config, WorkerState},
};
use serde_json::json;
use std::fs;
use uuid::Uuid;

#[tokio::test]
async fn unverified_executable_is_rejected_before_it_can_run() {
    let root = tempfile::tempdir().unwrap();
    let program = root.path().join("muse");
    let marker = root.path().join("executed");
    fs::write(
        &program,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
    let credentials = root.path().join("auth.json");
    fs::write(&credentials, "{}").unwrap();
    assert!(
        Config::prepare(program, credentials, root.path().join("data"))
            .await
            .is_err()
    );
    assert!(!marker.exists());
}

#[tokio::test]
async fn worker_retry_is_idempotent_and_preserves_its_session() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    journal
        .stage_utterance(Uuid::now_v7(), "Wait".into(), Target::New, None, Vec::new())
        .await
        .unwrap();
    let word = journal.claim_input().await.unwrap().unwrap();
    journal
        .apply_input(
            word.id,
            word.revision,
            Outcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: Some("when-told".into()),
                command: None,
                confidence: 1.0,
                inference_ms: 1.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await
        .unwrap();
    let track = journal.snapshot().await.unwrap()["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut state = WorkerState {
        secret_revision: None,
        track_id: track.clone(),
        phase: "failed".into(),
        session_id: Some("retained-session".into()),
        identity: None,
        detail: Some("transient failure".into()),
        active_turn: Some(Uuid::now_v7()),
        approvals: vec![],
        output: None,
        checkpoint_pending: true,
        checkpoint: None,
        prompts: vec![],
    };
    journal.worker_state(state.clone()).await.unwrap();
    let retry = Uuid::now_v7();
    assert_eq!(
        journal.retry_worker(retry, track.clone()).await.unwrap(),
        Admission::New
    );
    assert_eq!(
        journal.retry_worker(retry, track.clone()).await.unwrap(),
        Admission::Replay("complete".into())
    );
    let worker = &journal.snapshot().await.unwrap()["workers"][0];
    assert_eq!(worker["sessionId"], "retained-session");
    assert_eq!(worker["phase"], "retry");
    assert_eq!(worker["activeTurn"], state.active_turn.unwrap().to_string());
    assert_eq!(worker["checkpointPending"], true);
    state.phase = "ready".into();
    journal.worker_state(state.clone()).await.unwrap();
    assert!(
        journal
            .retry_worker(Uuid::now_v7(), track.clone())
            .await
            .is_err()
    );
    state.phase = "needs-recovery".into();
    journal.worker_state(state).await.unwrap();
    assert!(journal.retry_worker(Uuid::now_v7(), track).await.is_err());
}
