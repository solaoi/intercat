use super::*;
use crate::host_memory::{Pool, Sample};
use std::time::Duration;

#[tokio::test]
async fn active_checkpoint_question_and_shell_hosts_remain_alive_under_pressure() -> Result<()> {
    for protected in ["active", "checkpoint", "question", "shell"] {
        let root = tempfile::tempdir()?;
        let journal = crate::journal::Journal::open(
            root.path().join("db"),
            crate::secrets::Key::new([7; 32]),
        )
        .await?;
        journal
            .stage_utterance(
                Uuid::now_v7(),
                "Dummy memory test".into(),
                crate::decision::Target::New,
                None,
                vec![],
            )
            .await?;
        let input = journal.claim_input().await?.unwrap();
        journal
            .apply_input(
                input.id,
                input.revision,
                crate::decision::Outcome::Classified(crate::decision::Decision {
                    kind: crate::decision::Kind::Fork,
                    track_id: None,
                    boundary: Some("when-told".into()),
                    command: None,
                    confidence: 1.0,
                    inference_ms: 1.0,
                    timed_out: false,
                    scores: serde_json::json!({}),
                }),
            )
            .await?;
        let track = journal.snapshot().await?["tracks"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        if protected == "question" {
            journal
                .observe_report(crate::reports::Report::new(
                    track.clone(),
                    "session".into(),
                    Uuid::now_v7(),
                    "report".into(),
                    "Waiting for you: Choose a dummy label.".into(),
                ))
                .await?;
        }
        let mut child = if protected == "shell" {
            Some(
                tokio::process::Command::new("/bin/sleep")
                    .arg("30")
                    .kill_on_drop(true)
                    .spawn()?,
            )
        } else {
            None
        };
        let identity = crate::process::identity(std::process::id())?;
        let tracker = Arc::new(Mutex::new(crate::process::Tracker::new(
            "memory-protection".into(),
            identity,
        )?));
        let memory = Pool::fixed(Sample {
            free_bytes: 4 * 1024 * 1024 * 1024,
            pressured: false,
        });
        let lease = memory.request("track".into()).try_acquire().unwrap();
        lease.attach(identity);
        let (reader, responses) = tokio::io::duplex(65536);
        let mut connection = crate::msp::Connection::new(reader, tokio::io::sink());
        let (port, commands) = Port::new(Arc::new(crate::dispatch_control::Gate::new(false)));
        let editor = crate::editor::Manager::new(root.path(), &root.path().join("editor.log"))?;
        let mut context = Context {
            project_secrets: crate::project_secrets::Store::default(),
            port,
            commands,
            editor,
            capacity: Arc::new(crate::scheduler::Capacity::new(1)),
        };
        let mut state = crate::runtime::WorkerState {
            track_id: track.clone(),
            phase: "ready".into(),
            session_id: Some("session".into()),
            secret_revision: None,
            identity: Some(identity),
            detail: None,
            active_turn: (protected == "active").then(Uuid::now_v7),
            approvals: vec![],
            output: None,
            checkpoint_pending: protected == "checkpoint",
            checkpoint: None,
            prompts: vec![],
        };
        journal.worker_state(state.clone()).await?;
        let (_halt, mut stopping) = tokio::sync::watch::channel(false);
        let mut continuity = Continuity {
            host_memory: Some(lease),
            ..Continuity::default()
        };
        memory.set_sample(Sample {
            free_bytes: 0,
            pressured: true,
        });
        let mut waiting = memory.request("new-host".into());
        let controller = async {
            for _ in 0..7 {
                assert!(waiting.try_acquire().is_none());
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            drop(responses);
        };
        let runner = run(
            WorkerIo {
                connection: &mut connection,
                tracker,
                identity,
                recovery: None,
                continuity: &mut continuity,
            },
            &journal,
            root.path(),
            &mut state,
            &mut context,
            &mut stopping,
        );
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(runner, controller)
        })
        .await?;
        if let Some(child) = child.as_mut() {
            child.kill().await?;
            child.wait().await?;
        }
        let error = result.expect_err("closing the mock transport must end execution");
        assert!(
            error.is::<crate::msp::ConnectionLost>(),
            "{protected} was reclaimed: {error}"
        );
        continuity.settle().await?;
    }
    Ok(())
}
