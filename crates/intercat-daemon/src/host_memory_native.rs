//! Native host lifecycle with synthetic credentials and the offline echo provider.
use super::*;
use crate::host_memory::{Pool, Sample};
use std::time::Duration;

async fn phase(journal: &Journal, track: &str, expected: &str) -> Result<WorkerState> {
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            if let Some(value) = journal.snapshot().await?["workers"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|worker| worker["trackId"] == track)
            {
                let state: WorkerState = serde_json::from_value(value.clone())?;
                if state.phase == expected {
                    return Ok(state);
                }
                if ["failed", "needs-recovery", "lost"].contains(&state.phase.as_str()) {
                    return Err(format!("Unexpected {}: {:?}", state.phase, state.detail).into());
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

#[tokio::test]
#[ignore = "native offline echo: memory waiting, idle eviction and durable resume"]
async fn idle_host_releases_memory_and_resumes_the_same_session() -> Result<()> {
    let (root, runner, _, service, _, _) = reconnect_fixture("MEMORY_DUMMY").await?;
    let journal = runner.journal.clone();
    let track = runner.state.track_id.clone();
    let binary = PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?).canonicalize()?;
    assert_eq!(host::hash(&binary)?, host::BINARY_SHA256);
    let credentials = root.path().join("dummy-auth.json");
    std::fs::write(&credentials, r#"{"schema_version":1}"#)?;
    let config = Config {
        source: binary.clone(),
        binary,
        credentials,
        path: std::env::var("PATH")?,
    };
    let source = root.path().join("dummy-project");
    std::fs::create_dir(&source)?;
    native_git(&source, &["init", "-q", "-b", "main"])?;
    std::fs::write(
        source.join("README.txt"),
        "Generated memory lifecycle fixture.\n",
    )?;
    native_git(&source, &["add", "."])?;
    native_git(&source, &["commit", "-qm", "fixture"])?;
    journal
        .configure_project(
            Uuid::now_v7(),
            crate::project::inspect(Uuid::now_v7(), "Memory fixture".into(), vec![source]).await?,
        )
        .await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let project = journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    let key = crate::secrets::Key::new([7; 32]);
    let catalog = journal
        .prepare_project_secrets(crate::project_secrets::Prepare {
            op_id: Uuid::now_v7(),
            project_id: project.clone(),
            revision: 0,
            fingerprints: vec![],
            key_check: key.check().iter().map(|b| format!("{b:02x}")).collect(),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    service
        .activate_project_secrets(crate::project_secrets::Load {
            project_id: project,
            revision: 0,
            entries: vec![],
        })
        .await?;
    let preparation =
        crate::preparation::Preparation::start(&root.path().join("workspaces"), journal.clone())?;
    let workspace = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if let Some(value) = journal.snapshot().await?["workspaces"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|w| w["trackId"] == track && w["state"] == "ready")
            {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(value.clone());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    preparation.shutdown().await;
    let editor = native_editor(root.path()).await?;
    let memory = Pool::fixed(Sample {
        free_bytes: 4 * 1024 * 1024 * 1024,
        pressured: false,
    });
    let (shutdown, stopping) = watch::channel(false);
    let launch = |previous| {
        let (port, commands) =
            crate::execution::Port::new(Arc::new(crate::dispatch_control::Gate::new(false)));
        let launch = Launch {
            host_memory: memory.clone(),
            config: config.clone(),
            data: root.path().into(),
            rpc: root.path().join("rpc"),
            journal: journal.clone(),
            workspace: workspace.clone(),
            previous,
            stopping: stopping.clone(),
            execution: crate::execution::Context {
                project_secrets: service.project_secrets(),
                port: port.clone(),
                commands,
                editor: editor.clone(),
                capacity: Arc::new(crate::scheduler::Capacity::new(1)),
            },
        };
        (
            port,
            tokio::spawn(host::OFFLINE_ECHO.scope((), run_host(launch))),
        )
    };
    let (first_port, first_task) = launch(None);
    let result = async {
        let first = phase(&journal, &track, "ready").await?;
        memory.set_sample(Sample {
            free_bytes: 0,
            pressured: true,
        });
        // The first sample can precede the actor's idle boundary.
        let mut other = memory.request("new-host".into());
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                assert!(other.try_acquire().is_none());
                if journal.snapshot().await?["workers"][0]["phase"] == "sleeping" {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await??;
        drop(other);
        let sleeping = phase(&journal, &track, "sleeping").await?;
        assert_eq!(sleeping.session_id, first.session_id);
        assert!(sleeping.identity.is_none());
        assert!(!crate::process::is_alive(first.identity.unwrap()));
        // Waiting consumes no host and Stop records the original operation.
        let (waiting_port, waiting_task) = launch(Some(sleeping.clone()));
        phase(&journal, &track, "waiting-memory").await?;
        assert_eq!(memory.snapshot().hosts, 0);
        let stop = Uuid::now_v7();
        waiting_port.stop(stop)?;
        tokio::time::timeout(Duration::from_secs(10), waiting_task).await??;
        assert_eq!(
            journal.operation(stop).await?.unwrap()["kind"],
            "worker.hold"
        );
        assert!(memory.snapshot().waiting.is_empty());
        // Explicitly reopen the retained session after memory returns.
        memory.set_sample(Sample {
            free_bytes: 4 * 1024 * 1024 * 1024,
            pressured: false,
        });
        let (last_port, last_task) = launch(Some(sleeping));
        let resumed = phase(&journal, &track, "ready").await?;
        assert_eq!(resumed.session_id, first.session_id);
        assert_ne!(resumed.identity, first.identity);
        let db = rusqlite::Connection::open(root.path().join("db"))?;
        let turns: i64 = db.query_row(
            "SELECT count(*) FROM operations WHERE kind='turn.start'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(turns, 0);
        shutdown.send_replace(true);
        let _ = last_port;
        tokio::time::timeout(Duration::from_secs(10), last_task).await??;
        assert_eq!(memory.snapshot().hosts, 0);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    shutdown.send_replace(true);
    let _ = first_port;
    let _ = tokio::time::timeout(Duration::from_secs(10), first_task).await;
    editor.stop().await;
    result
}
