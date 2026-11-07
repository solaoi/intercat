use intercat_daemon::{
    journal::Journal,
    lock::DaemonLock,
    rpc::{Server, Token},
};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

async fn call(stream: &mut BufReader<UnixStream>, method: &str, params: Value) -> Value {
    stream
        .get_mut()
        .write_all(
            format!(
                "{}\n",
                json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        stream.read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    serde_json::from_str(&line).unwrap()
}
async fn client(path: &std::path::Path) -> BufReader<UnixStream> {
    BufReader::new(UnixStream::connect(path).await.unwrap())
}

#[tokio::test]
async fn global_pause_holds_inputs_and_replays_never_reverse_a_newer_control() {
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let _lock = DaemonLock::acquire(root.path()).unwrap().unwrap();
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    let token = "a5".repeat(32);
    let pause = uuid::Uuid::now_v7();
    let resume = uuid::Uuid::now_v7();
    let input = uuid::Uuid::now_v7();
    let editor =
        intercat_daemon::editor::Manager::new(root.path(), &root.path().join("editor.log"))
            .unwrap();
    for restart in 0..2 {
        let server = Server::bind(
            root.path(),
            Token::parse(&token).unwrap(),
            journal.clone(),
            editor.clone(),
            &root.path().join("workspaces"),
        )
        .unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server.serve(async {
            let _ = stopped.await;
        }));
        let mut stream = client(&root.path().join("rpc.sock")).await;
        assert_eq!(
            call(&mut stream, "rpc.authenticate", json!({"token":token})).await["result"]["protocol"],
            1
        );
        if restart == 1 {
            let status = call(&mut stream, "system.status", json!({})).await;
            assert_eq!(status["result"]["dispatchPaused"], true);
            assert_eq!(status["result"]["utterances"][0]["state"], "held");
            call(
                &mut stream,
                "system.pause",
                json!({"opId":uuid::Uuid::now_v7(),"paused":false}),
            )
            .await;
        }
        if restart == 0 {
            assert_eq!(
                call(
                    &mut stream,
                    "system.pause",
                    json!({"opId":pause,"paused":true})
                )
                .await["result"]["paused"],
                true
            );
            assert_eq!(call(&mut stream,"utterance.create",json!({"opId":input,"text":"A held request","target":{"mode":"new"},"selectedTrackId":null,"attachmentIds":[]})).await["result"]["state"],"held");
            assert_eq!(
                call(
                    &mut stream,
                    "worker.start",
                    json!({"opId":uuid::Uuid::now_v7(),"trackId":"unused"})
                )
                .await["error"]["code"],
                -32005
            );
            assert_eq!(
                call(
                    &mut stream,
                    "system.pause",
                    json!({"opId":resume,"paused":false})
                )
                .await["result"]["paused"],
                false
            );
        }
        assert_eq!(
            call(
                &mut stream,
                "system.pause",
                json!({"opId":pause,"paused":true})
            )
            .await["result"]["paused"],
            false
        );
        let status = call(&mut stream, "system.status", json!({})).await;
        assert_eq!(status["result"]["dispatchPaused"], false);
        assert_eq!(status["result"]["utterances"][0]["state"], "held");
        assert!(
            call(
                &mut stream,
                "system.pause",
                json!({"opId":pause,"paused":false})
            )
            .await
            .get("error")
            .is_some()
        );
        if restart == 0 {
            assert_eq!(
                call(
                    &mut stream,
                    "system.pause",
                    json!({"opId":uuid::Uuid::now_v7(),"paused":true})
                )
                .await["result"]["paused"],
                true
            );
        }
        stop.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn shutdown_acknowledges_before_closing_the_authenticated_connection() {
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let _lock = DaemonLock::acquire(root.path()).unwrap().unwrap();
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    let token = "a6".repeat(32);
    let server = Server::bind(
        root.path(),
        Token::parse(&token).unwrap(),
        journal.clone(),
        intercat_daemon::editor::Manager::new(root.path(), &root.path().join("editor.log"))
            .unwrap(),
        &root.path().join("workspaces"),
    )
    .unwrap();
    let task = tokio::spawn(server.serve(std::future::pending()));
    let mut stream = client(&root.path().join("rpc.sock")).await;
    call(&mut stream, "rpc.authenticate", json!({"token":token})).await;
    assert_eq!(
        call(
            &mut stream,
            "system.shutdown",
            json!({"opId":uuid::Uuid::now_v7(),"paused":true})
        )
        .await["result"]["paused"],
        true
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(journal.snapshot().await.unwrap()["dispatchPaused"], true);
}

#[tokio::test]
async fn fingerprint_registration_is_authenticated_idempotent_and_absent_from_snapshots() {
    let root = tempfile::Builder::new()
        .prefix("ic-fingerprints-")
        .tempdir_in("/private/tmp")
        .unwrap();
    let _lock = DaemonLock::acquire(root.path()).unwrap().unwrap();
    let key = intercat_daemon::secrets::Key::new([7; 32]);
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let secret = "synthetic RPC credential";
    let params = json!({"opId":uuid::Uuid::now_v7(),"trackId":"track","sessionId":"session","name":"TOKEN","byteLen":secret.len(),"tag":hex(&key.tag(secret.as_bytes())),"keyCheck":hex(&key.check())});
    let journal = Journal::open(root.path().join("events.sqlite"), key)
        .await
        .unwrap();
    let db = rusqlite::Connection::open(root.path().join("events.sqlite")).unwrap();
    db.execute(
        "INSERT INTO msp_positions VALUES('session','track',NULL)",
        [],
    )
    .unwrap();
    let token = "36".repeat(32);
    let server = Server::bind(
        root.path(),
        Token::parse(&token).unwrap(),
        journal.clone(),
        intercat_daemon::editor::Manager::new(root.path(), &root.path().join("editor.log"))
            .unwrap(),
        &root.path().join("workspaces"),
    )
    .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let path = root.path().join("rpc.sock");
    assert_eq!(
        call(
            &mut client(&path).await,
            "secret.registerFingerprint",
            params.clone()
        )
        .await["error"]["code"],
        -32001
    );
    let mut stream = client(&path).await;
    call(&mut stream, "rpc.authenticate", json!({"token":token})).await;
    for _ in 0..2 {
        assert_eq!(
            call(&mut stream, "secret.registerFingerprint", params.clone()).await["result"],
            serde_json::to_value(journal.fingerprint_catalog().await.unwrap()).unwrap()
        );
    }
    let mut wrong = params.clone();
    wrong["tag"] = json!("00".repeat(32));
    assert_eq!(
        call(&mut stream, "secret.registerFingerprint", wrong).await["error"]["code"],
        -32002
    );
    let mut raw = params.clone();
    raw["value"] = json!(secret);
    assert_eq!(
        call(&mut stream, "secret.registerFingerprint", raw).await["error"]["code"],
        -32602
    );
    let status = call(&mut stream, "system.status", json!({}))
        .await
        .to_string();
    assert!(!status.contains(secret));
    assert!(!status.contains(params["tag"].as_str().unwrap()));
    assert_eq!(
        db.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM secret_fingerprints", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        journal
            .session_mask("track", "session")
            .await
            .unwrap()
            .redact(secret),
        "<secret:TOKEN>"
    );
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn authenticates_before_any_method_and_persists_effect_once_for_multiple_clients() {
    let directory = tempfile::Builder::new()
        .prefix("ic-rpc-")
        .tempdir_in("/private/tmp")
        .unwrap();
    let _lock = DaemonLock::acquire(directory.path()).unwrap().unwrap();
    let token = "35".repeat(32);
    let database = directory.path().join("events.sqlite");
    let journal = Journal::open(&database, intercat_daemon::secrets::Key::new([7; 32]))
        .await
        .unwrap();
    let server = Server::bind(
        directory.path(),
        Token::parse(&token).unwrap(),
        journal.clone(),
        intercat_daemon::editor::Manager::new(
            directory.path(),
            &directory.path().join("editor.log"),
        )
        .unwrap(),
        &directory.path().join("workspaces"),
    )
    .unwrap();
    let path = directory.path().join("rpc.sock");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let rejected = call(&mut client(&path).await, "system.status", json!({})).await;
    assert_eq!(rejected["error"]["code"], -32001);
    let wrong = call(
        &mut client(&path).await,
        "rpc.authenticate",
        json!({"token":"00".repeat(32)}),
    )
    .await;
    assert_eq!(wrong["error"]["code"], -32001);
    let mut one = client(&path).await;
    let mut two = client(&path).await;
    for stream in [&mut one, &mut two] {
        assert_eq!(
            call(stream, "rpc.authenticate", json!({"token":token})).await["result"]["protocol"],
            1
        );
    }
    let op = uuid::Uuid::now_v7();
    let params = json!({"opId":op,"text":"Keep this local until the editor is ready.","target":{"mode":"none"},"selectedTrackId":null,"attachmentIds":[]});
    let (first, second) = tokio::join!(
        call(&mut one, "utterance.create", params.clone()),
        call(&mut two, "utterance.create", params)
    );
    assert!(first.get("error").is_none());
    assert_eq!(first, second);
    let mismatch = call(
        &mut one,
        "utterance.create",
        json!({"opId":op,"text":"different","target":{"mode":"none"},"selectedTrackId":null,"attachmentIds":[]}),
    )
    .await;
    assert_eq!(mismatch["error"]["code"], -32002);
    let state = call(&mut two, "system.status", json!({})).await;
    assert_eq!(state["result"]["utterances"].as_array().unwrap().len(), 1);
    let recorded: i64 = rusqlite::Connection::open(&database)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM events WHERE kind='utterance.staged' AND json_extract(payload,'$.id')=?1",
            [op.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(recorded, 1);
    assert_eq!(
        call(
            &mut one,
            "utterance.create",
            json!({"opId":"00000000-0000-4000-8000-000000000000","text":"old uuid","target":{"mode":"none"},"selectedTrackId":null,"attachmentIds":[]})
        )
        .await["error"]["code"],
        -32602
    );
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(!path.exists());
    drop(one);
    drop(two);
    drop(journal);
    let recovered = Journal::open(&database, intercat_daemon::secrets::Key::new([7; 32]))
        .await
        .unwrap();
    let mut persisted = state["result"].clone();
    persisted.as_object_mut().unwrap().remove("editor");
    persisted.as_object_mut().unwrap().remove("input");
    persisted.as_object_mut().unwrap().remove("preparation");
    persisted.as_object_mut().unwrap().remove("outputFlows");
    let mut actual = recovered.snapshot().await.unwrap();
    assert!(
        actual["timeline"]["clock"]["monoMs"].as_i64().unwrap()
            >= persisted["timeline"]["clock"]["monoMs"].as_i64().unwrap()
    );
    actual["timeline"].as_object_mut().unwrap().remove("clock");
    persisted["timeline"]
        .as_object_mut()
        .unwrap()
        .remove("clock");
    assert_eq!(actual, persisted);
    for name in ["events.sqlite", "events.sqlite-wal"] {
        if let Ok(bytes) = fs::read(directory.path().join(name)) {
            assert!(
                !bytes
                    .windows(token.len())
                    .any(|window| window == token.as_bytes())
            );
        }
    }
}

#[tokio::test]
async fn refuses_symlink_socket_and_oversized_unauthenticated_frames() {
    let directory = tempfile::Builder::new()
        .prefix("ic-rpc-")
        .tempdir_in("/private/tmp")
        .unwrap();
    let _lock = DaemonLock::acquire(directory.path()).unwrap().unwrap();
    let journal = Journal::open(
        directory.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    let path = directory.path().join("rpc.sock");
    std::os::unix::fs::symlink("events.sqlite", &path).unwrap();
    let editor = intercat_daemon::editor::Manager::new(
        directory.path(),
        &directory.path().join("editor.log"),
    )
    .unwrap();
    assert!(
        Server::bind(
            directory.path(),
            Token::parse(&"12".repeat(32)).unwrap(),
            journal.clone(),
            editor.clone(),
            &directory.path().join("workspaces"),
        )
        .is_err()
    );
    fs::remove_file(&path).unwrap();
    let server = Server::bind(
        directory.path(),
        Token::parse(&"12".repeat(32)).unwrap(),
        journal,
        editor,
        &directory.path().join("workspaces"),
    )
    .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let mut stream = client(&path).await;
    let _ = stream.get_mut().write_all(&vec![b'x'; 513 * 1024]).await;
    let mut line = String::new();
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        stream.read_line(&mut line),
    )
    .await
    .unwrap();
    assert!(read.is_err() || read.unwrap() == 0);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn timeline_windows_are_authenticated_and_do_not_depend_on_status_clip_limits() {
    let root = tempfile::tempdir_in("/private/tmp").unwrap();
    let _lock = DaemonLock::acquire(root.path()).unwrap().unwrap();
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    journal.try_msp("track".into(),"session".into(),intercat_daemon::clock::Stamp::now(),json!({"method":"item/updated","params":{"sessionId":"session","item":{"itemId":"message","turnId":"turn","kind":"userMessage","revision":1,"status":"completed"}}}),None,None).unwrap().await.unwrap().unwrap();
    let token = "37".repeat(32);
    let server = Server::bind(
        root.path(),
        Token::parse(&token).unwrap(),
        journal,
        intercat_daemon::editor::Manager::new(root.path(), &root.path().join("editor.log"))
            .unwrap(),
        &root.path().join("workspaces"),
    )
    .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let path = root.path().join("rpc.sock");
    let request = json!({"tracks":["track"],"start":0,"end":9007199254740991i64,"since":null});
    assert_eq!(
        call(&mut client(&path).await, "timeline.window", request.clone()).await["error"]["code"],
        -32001
    );
    for method in ["branch.prepare", "branch.activate"] {
        assert_eq!(
            call(&mut client(&path).await, method, json!({})).await["error"]["code"],
            -32001
        );
    }
    let mut stream = client(&path).await;
    call(&mut stream, "rpc.authenticate", json!({"token":token})).await;
    let first = call(&mut stream, "timeline.window", request.clone()).await;
    assert_eq!(call(&mut stream,"branch.prepare",json!({"opId":uuid::Uuid::now_v7(),"parentTrackId":"track","turnId":uuid::Uuid::now_v7()})).await["error"]["code"],-32002);
    assert_eq!(first["result"]["clips"].as_array().unwrap().len(), 1);
    assert_eq!(first["result"]["marks"].as_array().unwrap().len(), 1);
    assert!(
        call(&mut stream, "system.status", json!({})).await["result"]["timeline"]
            .get("clips")
            .is_none()
    );
    assert!(
        call(&mut stream, "system.status", json!({})).await["result"]["timeline"]
            .get("marks")
            .is_none()
    );
    let mut next = request;
    next["since"] = first["result"]["revision"].clone();
    assert_eq!(
        call(&mut stream, "timeline.window", next.clone()).await["result"]["clips"],
        json!([])
    );
    next["unexpected"] = json!(true);
    assert_eq!(
        call(&mut stream, "timeline.window", next).await["error"]["code"],
        -32602
    );
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn correction_resume_requires_authentication_and_stays_held_without_its_worker() {
    use intercat_daemon::{
        decision::{Decision, Kind, Outcome, Resolution, Target},
        runtime::WorkerState,
    };
    let root = tempfile::Builder::new()
        .prefix("ic-redirect-")
        .tempdir_in("/private/tmp")
        .unwrap();
    let _lock = DaemonLock::acquire(root.path()).unwrap().unwrap();
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    let db = rusqlite::Connection::open(root.path().join("events.sqlite")).unwrap();
    db.execute(
        "INSERT INTO tracks VALUES('track','A','Fixture','working')",
        [],
    )
    .unwrap();
    let mut worker = WorkerState {
        track_id: "track".into(),
        phase: "working".into(),
        session_id: Some("session".into()),
        secret_revision: None,
        identity: None,
        detail: None,
        active_turn: Some(uuid::Uuid::now_v7()),
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    journal.worker_state(worker.clone()).await.unwrap();
    journal
        .stage_utterance(
            uuid::Uuid::now_v7(),
            "Change direction".into(),
            Target::Lock("track".into()),
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
                kind: Kind::Trim,
                track_id: Some("track".into()),
                boundary: None,
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await
        .unwrap();
    let checkpoint = intercat_daemon::checkpoint::Checkpoint {
        turn_id: worker.active_turn.unwrap(),
        sha: "a".repeat(40),
        reference: "refs/intercat/test/A".into(),
        unmerged_paths: vec![],
        live_processes: false,
    };
    let operation = uuid::Uuid::new_v5(&checkpoint.turn_id, b"checkpoint");
    journal
        .intent(
            &operation.to_string(),
            "checkpoint.save",
            json!({"trackId":worker.track_id,"checkpoint":checkpoint}),
        )
        .await
        .unwrap();
    journal
        .finish(&operation.to_string(), "dispatched")
        .await
        .unwrap();
    journal
        .finish(&operation.to_string(), "complete")
        .await
        .unwrap();
    worker.phase = "held".into();
    worker.active_turn = None;
    journal.worker_state(worker).await.unwrap();
    let token = "39".repeat(32);
    let server = Server::bind(
        root.path(),
        Token::parse(&token).unwrap(),
        journal.clone(),
        intercat_daemon::editor::Manager::new(root.path(), &root.path().join("editor.log"))
            .unwrap(),
        &root.path().join("workspaces"),
    )
    .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let path = root.path().join("rpc.sock");
    let params = json!({"opId":uuid::Uuid::now_v7(),"utteranceId":input.id,"revision":input.revision,"resolution":Resolution::Retry { target: Target::Lock("track".into()) }});
    assert_eq!(
        call(
            &mut client(&path).await,
            "utterance.resolve",
            params.clone()
        )
        .await["error"]["code"],
        -32001
    );
    let mut stream = client(&path).await;
    call(&mut stream, "rpc.authenticate", json!({"token":token})).await;
    assert_eq!(
        call(&mut stream, "utterance.resolve", params.clone()).await["error"]["code"],
        -32005
    );
    let replay = call(&mut stream, "utterance.resolve", params).await;
    assert_eq!(replay["result"]["state"], "held");
    assert_eq!(
        journal.snapshot().await.unwrap()["redirects"][0]["phase"],
        "held"
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM operations WHERE kind='turn.start'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    let _ = stop.send(());
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn file_recovery_cancellation_is_authenticated_durable_and_idempotent() {
    let root = tempfile::Builder::new()
        .prefix("ic-cancel-rpc-")
        .tempdir_in("/private/tmp")
        .unwrap();
    let _lock = DaemonLock::acquire(root.path()).unwrap().unwrap();
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    let db = rusqlite::Connection::open(root.path().join("events.sqlite")).unwrap();
    db.execute(
        "INSERT INTO tracks VALUES('track','A','Keep current files','waiting')",
        [],
    )
    .unwrap();
    let workspace = root.path().join("worktree");
    fs::create_dir(&workspace).unwrap();
    fs::write(workspace.join("kept.txt"), "unfinished dummy edit").unwrap();
    journal
        .workspace_result(intercat_daemon::preparation::Workspace {
            attachment_roots: Vec::new(),
            track_id: "track".into(),
            path: workspace.clone(),
            reference: "refs/intercat/test/A".into(),
            base: "0".repeat(40),
            state: "ready".into(),
            error: None,
        })
        .await
        .unwrap();
    journal
        .worker_state(intercat_daemon::runtime::WorkerState {
            track_id: "track".into(),
            phase: "after-connection-loss".into(),
            session_id: Some("old-session".into()),
            secret_revision: None,
            identity: None,
            detail: Some("Interrupted".into()),
            active_turn: None,
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        })
        .await
        .unwrap();
    let token = "27".repeat(32);
    let server = Server::bind(
        root.path(),
        Token::parse(&token).unwrap(),
        journal.clone(),
        intercat_daemon::editor::Manager::new(root.path(), &root.path().join("editor.log"))
            .unwrap(),
        &root.path().join("workspaces"),
    )
    .unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let path = root.path().join("rpc.sock");
    let params = json!({"opId":uuid::Uuid::now_v7(),"recoveryId":uuid::Uuid::now_v7(),"trackId":"track","sessionId":"old-session"});
    assert_eq!(
        call(
            &mut client(&path).await,
            "worker.pickup.cancel",
            params.clone()
        )
        .await["error"]["code"],
        -32001
    );
    assert!(
        journal.snapshot().await.unwrap()["pickups"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let mut stream = client(&path).await;
    call(&mut stream, "rpc.authenticate", json!({"token":token})).await;
    let mut invalid = params.clone();
    invalid["unexpected"] = json!(true);
    assert_eq!(
        call(&mut stream, "worker.pickup.cancel", invalid).await["error"]["code"],
        -32602
    );
    let first = call(&mut stream, "worker.pickup.cancel", params.clone()).await;
    assert_eq!(first["result"]["recovery"]["phase"], "cancelled");
    assert_eq!(
        call(&mut stream, "worker.pickup.cancel", params).await["result"],
        first["result"]
    );
    assert_eq!(
        fs::read_to_string(workspace.join("kept.txt")).unwrap(),
        "unfinished dummy edit"
    );
    let status = journal.snapshot().await.unwrap();
    assert_eq!(status["workers"][0]["sessionId"], "old-session");
    assert_eq!(status["workers"][0]["phase"], "after-connection-loss");
    assert_eq!(db.query_row("SELECT COUNT(*) FROM operations WHERE kind='worker.pickup.cancel' AND state='complete'", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}
