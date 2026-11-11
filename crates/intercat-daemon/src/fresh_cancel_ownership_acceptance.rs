//! Cancellation keeps dispatch fenced until owned work is confirmed stopped.
use super::*;

async fn preparing(f: &Fixture, multiple: bool) -> Result<crate::fresh::Job> {
    let files = snapshot(f).await?;
    if multiple {
        let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
        let mut source: WorkerState = serde_json::from_str(&db.query_row(
            "SELECT record FROM workers WHERE track_id=?1",
            [&files.request.track_id],
            |row| row.get::<_, String>(0),
        )?)?;
        source.output =
            Some("Own bounded dummy history; no user files or credentials.\n".repeat(1600));
        f.journal.worker_state(source).await?;
    }
    let job = f
        .journal
        .fresh(Change::PrepareSummary {
            id: files.request.op_id,
            revision: files.revision,
            changes: crate::fresh_summary::changes(&files).await?,
        })
        .await?;
    f.journal
        .summary_plan(crate::handoff::Source {
            id: job.summary_id.unwrap(),
            identity: crate::handoff::Identity {
                track_id: job.request.track_id.clone(),
                session_id: job.request.session_id.clone(),
            },
            context: job.summary_input.clone().unwrap(),
        })
        .await?;
    Ok(job)
}

#[tokio::test]
async fn a_saved_summary_cancellation_rejects_late_results_and_finishes_after_restart() -> Result<()>
{
    let f = Fixture::new(1).await?;
    let job = preparing(&f, false).await?;
    let queued = future(&f).await?;
    let op = Uuid::now_v7();
    let change = || Change::Cancel {
        op_id: op,
        id: job.request.op_id,
        revision: job.revision,
    };
    let pending = f.journal.fresh(change()).await?;
    assert_eq!(pending.phase, Phase::Cancelling);
    assert_eq!(f.journal.fresh(change()).await?.revision, pending.revision);
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert!(crate::fresh_store::holds(&db, &job.request.track_id)?);
    assert_eq!(f.journal.operation(op).await?.unwrap()["state"], "prepared");
    assert!(
        crate::fresh_store::retire(&db, &job.request.track_id, &job.request.session_id).is_err()
    );
    assert!(
        f.journal
            .fresh(Change::Summarized {
                id: job.request.op_id,
                revision: job.revision,
                document: document(&job),
            })
            .await
            .is_err()
    );
    let id = queued["id"].as_str().unwrap().to_owned();
    assert_eq!(f.journal.read_utterance(id.clone()).await?["state"], "held");
    let Fixture { root, journal, .. } = f;
    drop(journal);
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    let restored = journal.fresh_job(job.request.op_id).await?.unwrap();
    assert_eq!(restored.phase, Phase::Cancelling);
    let editor_dir = root.path().join("own-editor");
    fs::create_dir(&editor_dir)?;
    let editor = crate::editor::Manager::new(&editor_dir, &root.path().join("own-editor.log"))?;
    let runtime = crate::runtime::Runtime::start(
        crate::host_memory::Pool::fixed(crate::host_memory::Sample {
            free_bytes: 8 * 1024 * 1024 * 1024,
            pressured: false,
        }),
        root.path().to_owned(),
        root.path().join("rpc"),
        journal.clone(),
        editor.clone(),
        Arc::new(crate::scheduler::Capacity::new(1)),
        Arc::new(crate::dispatch_control::Gate::new(false)),
    );
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = journal.fresh_job(job.request.op_id).await?.unwrap();
            if current.phase == Phase::Cancelled {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(current);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    runtime.shutdown().await;
    editor.stop().await;
    let done = completed??;
    assert_eq!(done.phase, Phase::Cancelled);
    assert!(!crate::fresh_store::holds(&db, &job.request.track_id)?);
    assert_eq!(journal.operation(op).await?.unwrap()["state"], "complete");
    assert_eq!(journal.read_utterance(id).await?["state"], "held");
    assert_eq!(journal.fresh(change()).await?.revision, done.revision);
    assert_eq!(
        journal
            .fresh(Change::Cancelled {
                id: job.request.op_id,
                revision: pending.revision
            })
            .await?
            .revision,
        done.revision
    );
    assert!(!crate::continuation::closed(&db, &job.request.track_id)?);
    Ok(())
}

#[tokio::test]
async fn cancelling_waits_for_capture_ownership_while_journal_stop_stays_available() -> Result<()> {
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let scope = f.journal.capture_scope(job.workspace.clone()).await?;
    let pending = f
        .journal
        .fresh(Change::Cancel {
            op_id: Uuid::now_v7(),
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    assert_eq!(pending.phase, Phase::Cancelling);
    let journal = f.journal.clone();
    let data = f.root.path().to_owned();
    let mut finishing =
        tokio::spawn(async move { crate::fresh_cancel::complete(&data, &journal, &pending).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(80), &mut finishing)
            .await
            .is_err()
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        f.journal.pause_dispatch(Uuid::now_v7(), true),
    )
    .await??;
    assert_eq!(
        f.journal.fresh_job(job.request.op_id).await?.unwrap().phase,
        Phase::Cancelling
    );
    drop(scope);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), finishing)
            .await???
            .phase,
        Phase::Cancelled
    );
    Ok(())
}

#[tokio::test]
async fn a_bad_ledger_blocks_completion_but_does_not_skip_another_owned_host() -> Result<()> {
    let f = Fixture::new(1).await?;
    let job = preparing(&f, true).await?;
    let tasks = f.journal.summary_tasks(job.summary_id.unwrap()).await?;
    assert!(tasks.len() >= 2);
    let workspace = f
        .root
        .path()
        .join("summary-workspaces")
        .join(job.summary_id.unwrap().to_string());
    fs::create_dir_all(&workspace)?;
    let bad_dir = f
        .root
        .path()
        .join("summary-hosts")
        .join(tasks[0].to_string())
        .join(Uuid::now_v7().to_string());
    fs::create_dir_all(&bad_dir)?;
    fs::set_permissions(
        &bad_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )?;
    let mut bad =
        crate::process::Tracker::prepare(&bad_dir, &workspace, Uuid::now_v7().to_string())?;
    fs::write(
        bad_dir.join(crate::process_ledger::NAME),
        "Own damaged record",
    )?;
    let own_dir = f
        .root
        .path()
        .join("summary-hosts")
        .join(tasks[1].to_string())
        .join(Uuid::now_v7().to_string());
    fs::create_dir_all(&own_dir)?;
    fs::set_permissions(
        &own_dir,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )?;
    let nonce = Uuid::now_v7().to_string();
    let mut tracker = crate::process::Tracker::prepare(&own_dir, &workspace, nonce.clone())?;
    let mut child = tokio::process::Command::new("/bin/sleep")
        .arg("60")
        .env_clear()
        .env("INTERCAT_HOST", nonce)
        .current_dir(&workspace)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let identity = crate::process::identity(child.id().unwrap())?;
    tracker.attach_process_group(identity)?;
    let pending = f
        .journal
        .fresh(Change::Cancel {
            op_id: Uuid::now_v7(),
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    let error = crate::fresh_cancel::complete(f.root.path(), &f.journal, &pending)
        .await
        .unwrap_err();
    tokio::time::timeout(Duration::from_secs(3), child.wait()).await??;
    assert!(!crate::process::is_alive(identity));
    let failed = f
        .journal
        .fresh(Change::Failed {
            id: pending.request.op_id,
            revision: pending.revision,
            error: error.to_string(),
        })
        .await?;
    assert_eq!(failed.phase, Phase::Cancelling);
    assert!(failed.error.is_some());
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert!(crate::fresh_store::holds(&db, &job.request.track_id)?);
    assert!(
        f.journal
            .fresh(Change::Retry {
                op_id: Uuid::now_v7(),
                id: job.request.op_id,
                revision: failed.revision
            })
            .await
            .is_err()
    );
    // No launch occurred for this record; its in-memory owner can restore it.
    bad.cancel_launch()?;
    assert_eq!(
        crate::fresh_cancel::complete(f.root.path(), &f.journal, &failed)
            .await?
            .phase,
        Phase::Cancelled
    );
    Ok(())
}

#[tokio::test]
async fn cancelling_file_capture_or_publication_rejects_its_late_callback_without_changing_worker_files()
-> Result<()> {
    for publishing in [false, true] {
        let f = Fixture::new(1).await?;
        let released = release_without_a_host(&f).await?;
        fs::write(
            released.workspace.path.join("own-late.txt"),
            "Own current bytes\n",
        )?;
        let mut job = begin_files(&f, released).await?;
        if publishing {
            job = crate::fresh_files::capture(&f.journal, &job).await?;
            assert_eq!(job.phase, Phase::PublishingFiles);
        } else {
            assert_eq!(job.phase, Phase::CapturingFiles);
        }
        let head = git(&job.workspace.path, &["rev-parse", "HEAD"])?;
        let index = PathBuf::from(git(
            &job.workspace.path,
            &["rev-parse", "--absolute-git-dir"],
        )?)
        .join("index");
        let index_bytes = fs::read(&index)?;
        let pending = f
            .journal
            .fresh(Change::Cancel {
                op_id: Uuid::now_v7(),
                id: job.request.op_id,
                revision: job.revision,
            })
            .await?;
        assert_eq!(pending.phase, Phase::Cancelling);
        let late = if publishing {
            crate::fresh_files::publish(&f.journal, &job).await
        } else {
            crate::fresh_files::capture(&f.journal, &job).await
        };
        assert!(late.is_err());
        assert_eq!(
            crate::fresh_cancel::complete(f.root.path(), &f.journal, &pending)
                .await?
                .phase,
            Phase::Cancelled
        );
        assert_eq!(git(&job.workspace.path, &["rev-parse", "HEAD"])?, head);
        assert_eq!(fs::read(index)?, index_bytes);
        assert_eq!(
            fs::read_to_string(job.workspace.path.join("own-late.txt"))?,
            "Own current bytes\n"
        );
        let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
        assert!(!crate::continuation::closed(&db, &job.request.track_id)?);
    }
    Ok(())
}
