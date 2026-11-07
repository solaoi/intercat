use intercat_daemon::{
    Result,
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Target},
    input_origin::{self, DiffRequest, Identity, Phase},
    journal::Journal,
    runtime::WorkerState,
    secrets::Key,
};
use rusqlite::Connection;
use serde_json::json;
use uuid::Uuid;

struct Fixture {
    root: tempfile::TempDir,
    journal: Journal,
    input: Uuid,
    command: Uuid,
    turn: Uuid,
    track: String,
    worker: WorkerState,
}

impl Fixture {
    async fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        let input = Uuid::now_v7();
        journal
            .stage_utterance(
                input,
                "The original request".into(),
                Target::None,
                None,
                vec![],
            )
            .await?;
        let pending = journal.claim_input().await?.unwrap();
        journal
            .apply_input(
                input.to_string(),
                pending.revision,
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
        let track = journal.read_utterance(input.to_string()).await?["decision"]["trackId"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(journal.read_input_origin(input).await?.is_none());
        let (command, turn) = (Uuid::now_v7(), Uuid::now_v7());
        journal
            .reserve_turn(
                command,
                input.to_string(),
                1,
                track.clone(),
                json!({"sessionId":"source-session","commandId":command}),
            )
            .await?;
        journal.try_msp(track.clone(),"source-session".into(),Stamp::now(),json!({"method":"item/updated","params":{"sessionId":"source-session","item":{"kind":"userMessage","itemId":"source-input","commandId":command,"turnId":turn,"text":"The original request","revision":1,"status":"completed"}}}),None,None)?.await??;
        let worker = WorkerState {
            track_id: track.clone(),
            phase: "working".into(),
            session_id: Some("source-session".into()),
            secret_revision: None,
            identity: None,
            detail: None,
            active_turn: Some(turn),
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        };
        journal.worker_state(worker.clone()).await?;
        Ok(Self {
            root,
            journal,
            input,
            command,
            turn,
            track,
            worker,
        })
    }
    fn identity(&self) -> Identity {
        Identity {
            utterance_id: self.input,
            session_id: "source-session".into(),
            turn_id: self.turn,
        }
    }
    async fn complete(&mut self) -> Result<()> {
        self.journal.try_msp(self.track.clone(),"source-session".into(),Stamp::now(),json!({"method":"turn/completed","params":{"sessionId":"source-session","turnId":self.turn,"status":"completed"}}),None,None)?.await??;
        self.worker.active_turn = None;
        self.worker.phase = "checkpoint".into();
        self.worker.checkpoint_pending = true;
        self.journal.worker_state(self.worker.clone()).await
    }
    async fn checkpoint(&mut self) -> Result<String> {
        use intercat_git::Repository;
        let path = self.root.path().join("objects");
        Repository::init_isolated(&path)?;
        let repo = Repository::open(&path)?;
        let before = repo.write_tree(&[("file.txt", repo.write_blob(b"original\n")?)])?;
        let signature = gix::actor::Signature {
            name: "Test".into(),
            email: "test@example.test".into(),
            time: gix::date::Time::now_local_or_utc(),
        };
        let base = repo.write_commit(before, &[], "base", signature)?;
        let after = repo.write_tree(&[
            ("file.txt", repo.write_blob(b"original request change\n")?),
            ("binary.bin", repo.write_blob(&[0, 1, 2])?),
            ("large.txt", repo.write_blob(&vec![b'x'; 1024 * 1024 + 1])?),
            (
                "secret.txt",
                repo.write_blob(
                    concat!("token = ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H\n").as_bytes(),
                )?,
            ),
        ])?;
        let sha = repo.checkpoint_commit(after, base, "original turn")?;
        self.journal
            .workspace_result(intercat_daemon::preparation::Workspace {
                track_id: self.track.clone(),
                path,
                reference: "refs/intercat/test/tracks/A".into(),
                base: base.to_string(),
                state: "ready".into(),
                error: None,
                attachment_roots: vec![],
            })
            .await?;
        let checkpoint = intercat_daemon::checkpoint::Checkpoint {
            turn_id: self.turn,
            sha: sha.to_string(),
            reference: format!("refs/intercat/test/tracks/A-checkpoints/{}", self.turn),
            unmerged_paths: vec![],
            live_processes: false,
        };
        let operation = Uuid::new_v5(&self.turn, b"checkpoint");
        self.journal
            .intent(
                &operation.to_string(),
                "checkpoint.save",
                json!({"trackId":self.track,"checkpoint":checkpoint}),
            )
            .await?;
        self.journal
            .finish(&operation.to_string(), "dispatched")
            .await?;
        self.journal
            .finish(&operation.to_string(), "complete")
            .await?;
        self.worker.phase = "ready".into();
        self.worker.checkpoint_pending = false;
        self.worker.checkpoint = Some(checkpoint);
        self.journal.worker_state(self.worker.clone()).await?;
        Ok(sha.to_string())
    }
}

#[tokio::test]
async fn the_original_turn_identity_comes_from_its_receipt_and_tracks_running_and_finished_states()
-> Result<()> {
    let mut f = Fixture::new().await?;
    let origin = f.journal.read_input_origin(f.input).await?.unwrap();
    assert_ne!(f.command, f.turn);
    assert_eq!(origin.identity, f.identity());
    assert_eq!(origin.phase, Phase::Running);
    f.worker.phase = "stopping".into();
    f.journal.worker_state(f.worker.clone()).await?;
    assert_eq!(
        f.journal.read_input_origin(f.input).await?.unwrap().phase,
        Phase::Stopping
    );
    f.complete().await?;
    assert_eq!(
        f.journal.read_input_origin(f.input).await?.unwrap().phase,
        Phase::Saving
    );
    let sha = f.checkpoint().await?;
    let origin = f.journal.read_input_origin(f.input).await?.unwrap();
    assert_eq!(origin.phase, Phase::Finished);
    assert_eq!(origin.checkpoint.as_deref(), Some(sha.as_str()));
    f.worker.active_turn = Some(Uuid::now_v7());
    f.worker.phase = "working".into();
    f.journal.worker_state(f.worker.clone()).await?;
    let moved = f.journal.read_input_origin(f.input).await?.unwrap();
    assert_eq!(moved.phase, Phase::MovedOn);
    assert_eq!(moved.identity, origin.identity);
    assert_eq!(moved.checkpoint, origin.checkpoint);
    Ok(())
}

#[tokio::test]
async fn inherited_turns_and_a_new_session_do_not_replace_the_receiving_session() -> Result<()> {
    let mut f = Fixture::new().await?;
    let db = Connection::open(f.root.path().join("db"))?;
    db.execute("INSERT INTO attachment_turns SELECT 'inherited-session',command,turn,ordinal,1 FROM attachment_turns WHERE session='source-session'",[])?;
    f.worker.session_id = Some("inherited-session".into());
    f.journal.worker_state(f.worker.clone()).await?;
    let origin = f.journal.read_input_origin(f.input).await?.unwrap();
    assert_eq!(origin.identity, f.identity());
    assert_eq!(origin.phase, Phase::MovedOn);
    let missing = Uuid::now_v7();
    f.journal
        .stage_utterance(
            missing,
            "Unlinked request".into(),
            Target::None,
            None,
            vec![],
        )
        .await?;
    assert!(f.journal.read_input_origin(missing).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn changes_read_only_the_original_checkpoint_and_enforce_the_file_list() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.complete().await?;
    let sha = f.checkpoint().await?;
    let changes = input_origin::changes(&f.journal, f.identity()).await?;
    assert_eq!(
        changes.files,
        vec!["binary.bin", "file.txt", "large.txt", "secret.txt"]
    );
    let diff = input_origin::diff(
        &f.journal,
        DiffRequest {
            identity: f.identity(),
            checkpoint: sha.clone(),
            path: "file.txt".into(),
        },
    )
    .await?;
    assert_eq!(diff.before, "original\n");
    assert_eq!(diff.after, "original request change\n");
    let binary = input_origin::diff(
        &f.journal,
        DiffRequest {
            identity: f.identity(),
            checkpoint: sha.clone(),
            path: "binary.bin".into(),
        },
    )
    .await?;
    assert!(binary.binary);
    let large = input_origin::diff(
        &f.journal,
        DiffRequest {
            identity: f.identity(),
            checkpoint: sha.clone(),
            path: "large.txt".into(),
        },
    )
    .await?;
    assert!(large.too_large);
    let secret = input_origin::diff(
        &f.journal,
        DiffRequest {
            identity: f.identity(),
            checkpoint: sha.clone(),
            path: "secret.txt".into(),
        },
    )
    .await?;
    assert!(secret.redacted);
    assert!(
        !secret
            .after
            .contains(concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H"))
    );
    assert!(
        input_origin::diff(
            &f.journal,
            DiffRequest {
                identity: f.identity(),
                checkpoint: sha.clone(),
                path: "../outside".into()
            }
        )
        .await
        .is_err()
    );
    assert!(
        input_origin::diff(
            &f.journal,
            DiffRequest {
                identity: f.identity(),
                checkpoint: "bad".into(),
                path: "file.txt".into()
            }
        )
        .await
        .is_err()
    );
    let forged = Identity {
        turn_id: Uuid::now_v7(),
        ..f.identity()
    };
    assert!(input_origin::changes(&f.journal, forged).await.is_err());
    f.worker.checkpoint.as_mut().unwrap().turn_id = Uuid::now_v7();
    f.journal.worker_state(f.worker.clone()).await?;
    assert_eq!(
        f.journal.read_input_origin(f.input).await?.unwrap().phase,
        Phase::MovedOn
    );
    let old = input_origin::diff(
        &f.journal,
        DiffRequest {
            identity: f.identity(),
            checkpoint: sha,
            path: "file.txt".into(),
        },
    )
    .await?;
    assert_eq!(old.after, "original request change\n");
    Ok(())
}

#[tokio::test]
async fn original_changes_survive_reopening_the_journal() -> Result<()> {
    let mut f = Fixture::new().await?;
    f.complete().await?;
    let sha = f.checkpoint().await?;
    let identity = f.identity();
    let path = f.root.path().join("db");
    drop(f.journal);
    let journal = Journal::open(path, Key::new([7; 32])).await?;
    let changes = input_origin::changes(&journal, identity).await?;
    assert_eq!(changes.checkpoint, sha);
    Ok(())
}

#[tokio::test]
async fn authenticated_rpc_reads_the_fixed_turn_and_rejects_a_completed_stop() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let mut f = Fixture::new().await?;
    f.complete().await?;
    let sha = f.checkpoint().await?;
    let _lock = intercat_daemon::lock::DaemonLock::acquire(f.root.path())?.unwrap();
    let token = "a5".repeat(32);
    let server = intercat_daemon::rpc::Server::bind(
        f.root.path(),
        intercat_daemon::rpc::Token::parse(&token)?,
        f.journal.clone(),
        intercat_daemon::editor::Manager::new(f.root.path(), &f.root.path().join("editor.log"))?,
        &f.root.path().join("workspaces"),
    )?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let mut stream =
        BufReader::new(tokio::net::UnixStream::connect(f.root.path().join("rpc.sock")).await?);
    for (method, params, accepted) in [
        ("rpc.authenticate", json!({"token":token}), true),
        ("utterance.origin", json!({"utteranceId":f.input}), true),
        (
            "utterance.changes",
            serde_json::to_value(f.identity())?,
            true,
        ),
        (
            "utterance.diff",
            json!({"identity":f.identity(),"checkpoint":sha,"path":"file.txt"}),
            true,
        ),
        (
            "utterance.diff",
            json!({"identity":f.identity(),"checkpoint":sha,"path":"../elsewhere"}),
            false,
        ),
        (
            "utterance.stop",
            json!({"opId":Uuid::now_v7(),"trackId":f.track,"identity":f.identity()}),
            false,
        ),
        (
            "utterance.changes",
            json!({"utteranceId":f.input,"sessionId":"wrong","turnId":f.turn}),
            false,
        ),
    ] {
        stream
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await?;
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_line(&mut line),
        )
        .await??;
        let response: serde_json::Value = serde_json::from_str(&line)?;
        assert_eq!(response.get("result").is_some(), accepted, "{response}");
        if method == "utterance.diff" && accepted {
            assert_eq!(response["result"]["after"], "original request change\n");
        }
    }
    stop.send(()).unwrap();
    task.await??;
    Ok(())
}
