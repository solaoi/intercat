use super::*;
use crate::{plans::State, secrets::Key};
use std::time::{Duration, Instant};

#[tokio::test]
#[ignore = "offline pinned-host test with a real 60-second deadline; INTERCAT_TEST_MUSE_BINARY required"]
async fn offline_stalled_planner_times_out_reaps_its_host_and_releases_its_slot() -> Result<()> {
    stalled(false).await
}

#[tokio::test]
#[ignore = "offline pinned-host stop test; INTERCAT_TEST_MUSE_BINARY required"]
async fn offline_stalled_planner_stops_without_waiting_for_the_deadline_or_restarting() -> Result<()>
{
    stalled(true).await
}

async fn stalled(stop: bool) -> Result<()> {
    let root = tempfile::tempdir()?;
    let repository = root.path().join("dummy");
    std::fs::create_dir(&repository)?;
    gix::init(&repository)?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    journal
        .configure_project(
            Uuid::now_v7(),
            crate::project::inspect(Uuid::now_v7(), "Dummy deadline".into(), vec![repository])
                .await?,
        )
        .await?;
    journal
        .stage_document(crate::request_sources::Submission {
            op_id: Uuid::now_v7(),
            text: "Plan one dummy task.".into(),
            target: crate::decision::Target::New,
            selected_track_id: None,
            attachment_ids: vec![],
            sources: None,
            execute_allowed: false,
            plan_item: None,
        })
        .await?;
    let plan = journal.plans().await?.pop().unwrap();
    let credentials = root.path().join("dummy-auth.json");
    std::fs::write(&credentials, "{}")?;
    let binary: PathBuf = std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into();
    let config = crate::runtime::Config {
        source: binary.clone(),
        binary,
        credentials,
        path: "/usr/bin:/bin:/usr/sbin:/sbin".into(),
    };
    let rpc = root.path().join("rpc");
    std::fs::create_dir(&rpc)?;
    let pool = Arc::new(Capacity::new(1));
    let (halt, stopping) = watch::channel(false);
    let probe = Arc::new(crate::planner::OutputProbe::default());
    let started = Instant::now();
    let task = tokio::spawn(crate::host::OFFLINE_ECHO.scope(
        (),
        crate::planner::SUPPRESS_OUTPUT.scope(
            probe.clone(),
            generate(
                journal.clone(),
                root.path().into(),
                rpc,
                pool.clone(),
                crate::host_memory::Pool::new(),
                Arc::new(crate::dispatch_control::Gate::new(false)),
                config,
                plan.clone(),
                stopping,
            ),
        ),
    ));
    let host_directory = root
        .path()
        .join("planner-hosts")
        .join(plan.generation.id.to_string());
    let identity = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(record) =
                crate::process_ledger::read(&host_directory.join(crate::process_ledger::NAME))
                && let Some(identity) = record.root
            {
                break identity;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !probe.accepted.load(std::sync::atomic::Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let waiting_since = started;
    assert_eq!(crate::process::identity(identity.pid)?, identity);
    let settings: serde_json::Value = serde_json::from_slice(&std::fs::read(
        host_directory.join("config/muse/settings.json"),
    )?)?;
    assert_eq!(settings["provider"], "echo");
    assert_eq!(pool.snapshot().active, 1);
    let waiting_pool = pool.clone();
    let waiting = tokio::spawn(async move {
        waiting_pool
            .request("next-dummy-plan".into(), Priority::Plan)
            .acquire()
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!waiting.is_finished());
    let stop_started = Instant::now();
    if stop {
        journal.pause_dispatch(Uuid::now_v7(), true).await?;
        halt.send_replace(true);
    }
    tokio::time::timeout(Duration::from_secs(70), task).await???;
    let elapsed = waiting_since.elapsed();
    let permit = tokio::time::timeout(Duration::from_secs(2), waiting).await??;
    drop(permit);
    let finished = journal.plans().await?.pop().unwrap();
    if let Ok(output) = std::env::var("INTERCAT_M3_TIMEOUT_OUTPUT") {
        std::fs::create_dir_all(&output)?;
        std::fs::write(
            PathBuf::from(output).join(if stop {
                "stop-observed.json"
            } else {
                "timeout-observed.json"
            }),
            serde_json::to_vec_pretty(&serde_json::json!({
                "elapsedMs":elapsed.as_millis(),"hostPresent":host_directory.exists(),"hostAlive":crate::process::identity(identity.pid).is_ok_and(|current| current==identity),
                "activeSlots":pool.snapshot().active,"discardedFrames":probe.discarded.load(std::sync::atomic::Ordering::Relaxed),"state":finished,
            }))?,
        )?;
    }
    assert_eq!(pool.snapshot().active, 0);
    if !stop {
        assert!(probe.discarded.load(std::sync::atomic::Ordering::Relaxed) > 0);
    }
    assert!(!crate::process::identity(identity.pid).is_ok_and(|current| current == identity));
    assert!(!host_directory.exists());
    assert_eq!(
        std::fs::read_dir(root.path().join("planner-workspaces"))?.count(),
        0
    );
    if stop {
        assert!(stop_started.elapsed() < Duration::from_secs(6));
        assert_eq!(finished.state, State::Held);
        assert_eq!(finished.generation.state, GenerationState::Failed);
    } else {
        assert!(elapsed >= Duration::from_secs(60) && elapsed < Duration::from_secs(70));
        assert_eq!(finished.state, State::Draft);
        assert_eq!(finished.generation.state, GenerationState::TimedOut);
    }
    assert!(
        journal.snapshot().await?["workers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    drop(journal);
    let reopened = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(
        reopened.plans().await?[0].generation.state,
        finished.generation.state
    );
    assert!(reopened.claim_input().await?.is_none());
    if let Ok(output) = std::env::var("INTERCAT_M3_TIMEOUT_OUTPUT") {
        std::fs::create_dir_all(&output)?;
        std::fs::write(
            PathBuf::from(output).join(if stop { "stop.json" } else { "timeout.json" }),
            serde_json::to_vec_pretty(&serde_json::json!({
                "offlineEcho":true,"outputDiscarded":true,"echoTurns":1,"modelCalls":0,"stopped":stop,"elapsedMs":elapsed.as_millis(),
                "hostReaped":true,"snapshotsRemoved":true,"nextSlotAdmitted":true,"activeSlots":0,"restartSentNothing":true,"state":finished,
            }))?,
        )?;
    }
    Ok(())
}
