use intercat_daemon::{
    decision::{Decision, Kind, Outcome, Target},
    execution::Port,
    journal::Journal,
    msp::Connection,
};
use serde_json::json;
use uuid::Uuid;
#[tokio::test]
async fn stop_reaches_the_priority_pipe_without_waiting_for_control_consumption() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let (read_client, _write_server) = tokio::io::duplex(4096);
    let (write_client, read_server) = tokio::io::duplex(4096);
    let connection = Connection::new(read_client, write_client);
    let (port, mut controls) = Port::new(std::sync::Arc::new(
        intercat_daemon::dispatch_control::Gate::new(false),
    ));
    let turn = Uuid::now_v7();
    port.attach(connection.client.clone(), "session".into(), Some(turn));
    port.stop(Uuid::now_v7()).unwrap();
    assert!(port.stopped.load(std::sync::atomic::Ordering::Acquire));
    let mut reader = BufReader::new(read_server);
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        reader.read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap()["method"],
        "turn/interrupt"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap()["params"]["turnId"],
        turn.to_string()
    );
    assert!(matches!(
        controls.try_recv().unwrap(),
        intercat_daemon::execution::Command::Stop { .. }
    ));
}
#[tokio::test]
async fn a_cut_request_cannot_reserve_a_turn_and_a_started_request_cannot_be_cut() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    let first = Uuid::now_v7();
    journal
        .stage_utterance(first, "Dummy".into(), Target::New, None, Vec::new())
        .await
        .unwrap();
    let input = journal.claim_input().await.unwrap().unwrap();
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
        .await
        .unwrap();
    let track = journal.snapshot().await.unwrap()["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    journal
        .cut_input(Uuid::now_v7(), input.id.clone())
        .await
        .unwrap();
    assert!(
        journal
            .reserve_turn(
                Uuid::now_v7(),
                input.id,
                input.revision,
                track.clone(),
                json!({"sessionId":"session"}),
                None,
            )
            .await
            .is_err()
    );
    let next = Uuid::now_v7();
    journal
        .stage_utterance(
            next,
            "Second".into(),
            Target::Lock(track.clone()),
            None,
            Vec::new(),
        )
        .await
        .unwrap();
    let input = journal.claim_input().await.unwrap().unwrap();
    journal
        .apply_input(
            input.id.clone(),
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(track.clone()),
                boundary: None,
                command: None,
                confidence: 1.0,
                inference_ms: 1.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await
        .unwrap();
    journal
        .reserve_turn(
            Uuid::now_v7(),
            input.id.clone(),
            input.revision,
            track,
            json!({"sessionId":"session"}),
            None,
        )
        .await
        .unwrap();
    assert!(journal.cut_input(Uuid::now_v7(), input.id).await.is_err());
}

#[tokio::test]
async fn boundary_hold_never_interrupts_the_running_host() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let (read_client, _write_server) = tokio::io::duplex(4096);
    let (write_client, read_server) = tokio::io::duplex(4096);
    let connection = Connection::new(read_client, write_client);
    let (port, mut controls) = Port::new(std::sync::Arc::new(
        intercat_daemon::dispatch_control::Gate::new(false),
    ));
    port.attach(connection.client.clone(), "session".into(), None);
    port.hold(Uuid::now_v7()).unwrap();
    assert!(!port.stopped.load(std::sync::atomic::Ordering::Acquire));
    assert!(matches!(
        controls.try_recv().unwrap(),
        intercat_daemon::execution::Command::Hold { .. }
    ));
    let mut reader = BufReader::new(read_server);
    let mut line = String::new();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            reader.read_line(&mut line)
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn immediate_stop_sends_only_the_captured_turn_and_an_empty_capture_sends_nothing() {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let (read_client, _write_server) = tokio::io::duplex(4096);
    let (write_client, read_server) = tokio::io::duplex(4096);
    let connection = Connection::new(read_client, write_client);
    let (port, _controls) = Port::new(std::sync::Arc::new(
        intercat_daemon::dispatch_control::Gate::new(false),
    ));
    port.attach(connection.client.clone(), "session".into(), None);
    let turn = Uuid::now_v7();
    port.stop_captured(Uuid::now_v7(), Some(turn)).unwrap();
    let mut reader = BufReader::new(read_server);
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        reader.read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(frame["method"], "turn/interrupt");
    assert_eq!(frame["params"]["turnId"], turn.to_string());
    port.stop_captured(Uuid::now_v7(), None).unwrap();
    line.clear();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            reader.read_line(&mut line)
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn boundary_hold_survives_reopen_and_start_releases_only_the_chosen_input() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("events.sqlite");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32]))
        .await
        .unwrap();
    let mut track = None;
    let mut ids = Vec::new();
    for index in 0..2 {
        let id = Uuid::now_v7();
        ids.push(id.to_string());
        journal
            .stage_utterance(
                id,
                format!("Held input {index}"),
                track.clone().map_or(Target::New, Target::Lock),
                None,
                vec![],
            )
            .await
            .unwrap();
        let input = journal.claim_input().await.unwrap().unwrap();
        journal
            .apply_input(
                input.id,
                input.revision,
                Outcome::Classified(Decision {
                    kind: if track.is_none() {
                        Kind::Fork
                    } else {
                        Kind::Extend
                    },
                    track_id: track.clone(),
                    boundary: Some("when-told".into()),
                    command: None,
                    confidence: 1.,
                    inference_ms: 1.,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await
            .unwrap();
        track = Some(
            journal.snapshot().await.unwrap()["tracks"][0]["id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    let pending = Uuid::now_v7();
    journal
        .stage_utterance(
            pending,
            "Still being classified".into(),
            Target::Lock(track.clone().unwrap()),
            track.clone(),
            vec![],
        )
        .await
        .unwrap();
    let reading = journal.claim_input().await.unwrap().unwrap();
    let hold = Uuid::now_v7();
    journal
        .hold_inputs(hold, track.clone().unwrap())
        .await
        .unwrap();
    journal
        .hold_inputs(hold, track.clone().unwrap())
        .await
        .unwrap();
    journal
        .apply_input(
            reading.id,
            reading.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: track,
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await
        .unwrap();
    drop(journal);
    let journal = Journal::open(path, intercat_daemon::secrets::Key::new([7; 32]))
        .await
        .unwrap();
    let held = journal.snapshot().await.unwrap();
    assert!(
        held["utterances"]
            .as_array()
            .unwrap()
            .iter()
            .all(|word| word["state"] == "held")
    );
    let second = &held["utterances"][1];
    let start = Uuid::now_v7();
    let result = journal
        .start_held_input(start, ids[1].clone(), second["revision"].as_i64().unwrap())
        .await
        .unwrap();
    assert_eq!(result["state"], "placed");
    assert_eq!(
        journal
            .start_held_input(start, ids[1].clone(), second["revision"].as_i64().unwrap())
            .await
            .unwrap(),
        result
    );
    assert!(
        journal
            .start_held_input(start, ids[0].clone(), second["revision"].as_i64().unwrap())
            .await
            .is_err()
    );
    assert!(
        journal
            .start_held_input(
                Uuid::now_v7(),
                ids[1].clone(),
                second["revision"].as_i64().unwrap()
            )
            .await
            .is_err()
    );
    assert_eq!(
        journal.snapshot().await.unwrap()["utterances"][0]["state"],
        "held"
    );
    let track = held["tracks"][0]["id"].as_str().unwrap().to_owned();
    // Starting one input preserves the worker-wide hold. The resume projection
    // allows its host to reopen while only that explicitly chosen input runs.
    let snapshot = journal.snapshot().await.unwrap();
    assert_eq!(snapshot["localResumes"][&track], json!(hold));
    assert_eq!(snapshot["workerHolds"][&track], json!(hold));
    assert_eq!(
        journal.read_utterance(ids[0].clone()).await.unwrap()["dispatchRunnable"],
        false
    );
    assert_eq!(
        journal.read_utterance(ids[1].clone()).await.unwrap()["dispatchRunnable"],
        true
    );
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "A new user request".into(),
            Target::Lock(track.clone()),
            Some(track.clone()),
            vec![],
        )
        .await
        .unwrap();
    let fresh = journal.claim_input().await.unwrap().unwrap();
    journal
        .apply_input(
            fresh.id,
            fresh.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(track.clone()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        journal.snapshot().await.unwrap()["localResumes"][&track],
        json!(hold)
    );
    journal.hold_inputs(Uuid::now_v7(), track).await.unwrap();
    assert_eq!(journal.snapshot().await.unwrap()["localResumes"], json!({}));
}

#[tokio::test]
async fn track_completion_distinguishes_cancelled_work_and_survives_shutdown() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Status test".into(),
            Target::New,
            None,
            vec![],
        )
        .await
        .unwrap();
    let input = journal.claim_input().await.unwrap().unwrap();
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
        .await
        .unwrap();
    let track = journal.snapshot().await.unwrap()["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let turn = Uuid::now_v7();
    let mut worker = intercat_daemon::runtime::WorkerState {
        track_id: track.clone(),
        phase: "held".into(),
        session_id: None,
        secret_revision: None,
        identity: None,
        detail: None,
        active_turn: None,
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    worker.session_id = Some("status-session".into());
    worker.checkpoint = Some(intercat_daemon::checkpoint::Checkpoint {
        turn_id: turn,
        sha: "a".repeat(40),
        reference: "refs/test/status".into(),
        unmerged_paths: vec![],
        live_processes: false,
    });
    for (terminal, expected) in [("cancelled", "stopped"), ("completed", "done")] {
        journal.try_msp(track.clone(),"status-session".into(),intercat_daemon::clock::Stamp::now(),json!({"jsonrpc":"2.0","method":"turn/completed","params":{"sessionId":"status-session","turnId":turn,"terminal":terminal}}),None,None).unwrap().await.unwrap().unwrap();
        worker.phase = "held".into();
        journal.worker_state(worker.clone()).await.unwrap();
        assert_eq!(
            journal.snapshot().await.unwrap()["tracks"][0]["status"],
            expected
        );
        worker.phase = "stopped".into();
        journal.worker_state(worker.clone()).await.unwrap();
        assert_eq!(
            journal.snapshot().await.unwrap()["tracks"][0]["status"],
            expected
        );
    }
    worker.phase = "working".into();
    worker.active_turn = Some(Uuid::now_v7());
    journal.worker_state(worker).await.unwrap();
    assert_eq!(
        journal.snapshot().await.unwrap()["tracks"][0]["status"],
        "working"
    );
}
