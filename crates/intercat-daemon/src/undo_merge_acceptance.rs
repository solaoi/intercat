use super::*;
use crate::repair_run::{Owner, Phase as RunPhase, Purpose};

fn start_repairs(f: &Fixture, paused: bool) -> crate::repair_service::Runtime {
    crate::repair_service::Runtime::start(
        crate::host_memory::Pool::new(),
        f.journal.clone(),
        f.root.path().join("integration"),
        f.root.path().into(),
        f.root.path().join("rpc.sock"),
        Arc::new(crate::scheduler::Capacity::new(4)),
        f.review.clone(),
        crate::project_secrets::Store::default(),
        Arc::new(crate::dispatch_control::Gate::new(paused)),
    )
}

async fn conflicted_undo() -> Result<(Fixture, Job)> {
    let f = Fixture::new(4).await?;
    let fork = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own retired A edit\n",
    )?;
    let first = f.save(0, Some(fork.clone())).await?;
    f.publish(vec![f.workspaces[0].track_id.clone()]).await?;
    fs::write(
        f.workspaces[0].path.join("independent.txt"),
        "Own later A file\n",
    )?;
    let last = f.save(0, Some(first)).await?;
    f.publish(vec![f.workspaces[0].track_id.clone()]).await?;
    f.inherit(2, 0, &last)?;
    fs::write(
        f.workspaces[2].path.join("parent.txt"),
        "Own later B edit\n",
    )?;
    f.save(2, None).await?;
    f.publish(vec![f.workspaces[2].track_id.clone()]).await?;
    f.branch(&fork)?;
    fs::write(
        f.workspaces[1].path.join("alternative.txt"),
        "Own chosen alternative\n",
    )?;
    f.save(1, None).await?;
    let opened = f
        .action(Action::Open {
            tracks: Some(vec![f.workspaces[1].track_id.clone()]),
        })
        .await?;
    let chosen = f
        .action(Action::ChoosePath {
            id: opened.id,
            revision: opened.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    assert_eq!(chosen.status, Status::MergeConflict);
    f.action(Action::Prepare {
        id: chosen.id,
        revision: chosen.revision,
    })
    .await?;
    let conflicted = f.settled(chosen.id).await?;
    assert_eq!(
        conflicted.status,
        Status::MergeConflict,
        "{:?}",
        conflicted.error
    );
    Ok((f, conflicted))
}

#[tokio::test]
#[ignore = "native Muse echo transport for an app-owned Undo; dummy auth only, no paid provider"]
async fn a_native_undo_closes_its_host_before_check_and_releases_its_files() -> Result<()> {
    use crate::repair_run::Event;
    let (f, job) = conflicted_undo().await?;
    let request = crate::undo_merge::automatic_request(&job)?.unwrap();
    let queued = f.journal.repair_start(request).await?;
    let mut run = f
        .journal
        .repair_transition(Uuid::now_v7(), queued.id, queued.revision, Event::Prepare)
        .await?;
    let root = f
        .root
        .path()
        .join("integration")
        .join(job.id.to_string())
        .join(format!("repair-{}", run.id));
    let (_, source) = f.journal.repair_source(run.id).await?;
    let workspace = Arc::new(
        crate::run_workspace::Workspace::open_source(
            source,
            &root,
            &run,
            tokio::sync::watch::channel(false).1,
        )
        .await?,
    );
    fs::write(
        workspace.checkout().workdir.join("parent.txt"),
        "Own later B edit\n",
    )?;
    workspace
        .checkout()
        .git(
            &["add".into(), "--".into(), "parent.txt".into()],
            true,
            tokio::sync::watch::channel(false).1,
        )
        .await?
        .require_success("Stage own dummy Undo resolution")?;
    let binary = crate::host::prepare_binary(
        &PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        &f.root.path().join("muse"),
    )?;
    let credentials = f.root.path().join("own-dummy-auth.json");
    fs::write(&credentials, r#"{"schema_version":1}"#)?;
    let config = crate::runtime::Config {
        source: binary.clone(),
        binary,
        credentials,
        path: std::env::var("PATH")?,
    };
    let memory = crate::host_memory::Pool::fixed(crate::host_memory::Sample {
        free_bytes: 8 * 1024 * 1024 * 1024,
        pressured: false,
    });
    let secrets = crate::project_secrets::Store::default();
    let manifest = f.journal.snapshot().await?["projectSecrets"].clone();
    secrets.publish(
        f.journal
            .activate_project_secrets(crate::project_secrets::Load {
                project_id: manifest["projectId"]
                    .as_str()
                    .ok_or("Missing own dummy project")?
                    .into(),
                revision: manifest["revision"]
                    .as_u64()
                    .ok_or("Missing own dummy secret revision")?,
                entries: vec![],
            })
            .await?,
    );
    let host_directory = f.root.path().join("repair-hosts").join(run.id.to_string());
    let rpc = f.root.path().join("own-rpc");
    fs::create_dir(&rpc)?;
    let mut hosted = crate::host::OFFLINE_ECHO
        .scope(
            (),
            crate::repair_host::open(
                &f.journal,
                &mut run,
                &workspace,
                crate::repair_host::Launch {
                    memory: memory.clone(),
                    worker: &config,
                    directory: &host_directory,
                    rpc: &rpc,
                    project_secrets: secrets.clone(),
                    one_time: None,
                },
            ),
        )
        .await?;
    let identity = hosted.identity();
    let tracker = hosted.host_mut().tracker.clone();
    struct Guard(Arc<Mutex<crate::process::Tracker>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Ok(mut tracker) = self.0.lock() {
                let _ = tracker.stop_confirmed();
            }
        }
    }
    let _guard = Guard(tracker);
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let echo: i64 = db.query_row("SELECT count(*) FROM events WHERE kind='worker.session' AND json_extract(payload,'$.result.session.providerId')='echo'",[],|row|row.get(0))?;
    assert!(echo > 0, "Never send the dummy turn to a paid provider");
    let capacity = Arc::new(crate::scheduler::Capacity::new(1));
    let (_, runner) = crate::repair_execution::channel(&mut hosted, &run)?;
    let captured = crate::host::OFFLINE_ECHO
        .scope(
            (),
            runner.execute(
                hosted,
                f.journal.clone(),
                &workspace,
                crate::repair_execution::Task {
                    instruction: "INTERCAT_OWN_UNDO_ECHO".into(),
                    capacity: capacity.clone(),
                    shutdown: tokio::sync::watch::channel(false).1,
                    project_secrets: secrets,
                },
            ),
        )
        .await?;
    assert_eq!(captured.phase, RunPhase::Captured);
    assert!(!crate::process::is_alive(identity));
    assert_eq!(capacity.snapshot().active, 0);
    assert_eq!(memory.snapshot().hosts, 0);
    let done = crate::repair_application::apply_source(
        &f.journal,
        &f.review,
        &captured,
        workspace,
        f.root.path().join("integration"),
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    assert_eq!(done.phase, RunPhase::Done);
    let checked = f.settled(job.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    assert_eq!(
        checked
            .candidate
            .as_ref()
            .unwrap()
            .undo
            .as_ref()
            .unwrap()
            .author,
        "Intercat"
    );
    assert!(checked.contributions.is_empty());
    let monitor = crate::repair_process_service::Monitor::start(
        f.journal.clone(),
        f.root.path().join("integration"),
        f.root.path().into(),
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if f.journal.repair_run(run.id).await?.remaining.is_none() {
                break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    monitor.shutdown().await;
    assert!(!root.exists());
    assert!(!host_directory.exists());
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    assert_eq!(
        fs::read_to_string(f.workspaces[2].path.join("parent.txt"))?,
        "Own later B edit\n"
    );
    let output = PathBuf::from(std::env::var("INTERCAT_M4_UNDO_OUTPUT")?);
    fs::create_dir_all(&output)?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    fs::write(
        output.join("verification.json"),
        serde_json::to_vec_pretty(
            &json!({"verified":true,"schema":crate::journal::SCHEMA_VERSION,"provider":"echo","paidTurns":0,"modelResolution":false,"stagedDummyResolution":true,"owner":"undo","hostClosedBeforeCheck":true,"capacityReleased":true,"memoryLeaseReleased":true,"candidateChecked":true,"workerAttributionEmpty":true,"privateFilesRemoved":true,"sourceUnchanged":true,"echoSessions":echo}),
        )?,
    )?;
    f.review.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_app_owned_undo_resumes_its_manual_result_and_checks_without_a_window() -> Result<()> {
    let (mut f, conflicted) = conflicted_undo().await?;
    let chosen = conflicted.clone();
    let conflict = conflicted
        .candidate
        .as_ref()
        .unwrap()
        .undo_conflict
        .as_ref()
        .unwrap();
    assert_eq!((conflict.applied.len(), conflict.remaining.len()), (1, 1));
    assert_eq!(conflict.track, f.workspaces[0].track_id);
    assert!(
        !conflicted
            .tracks
            .iter()
            .find(|t| t.id == conflict.track)
            .unwrap()
            .included
    );
    let before = f
        .workspaces
        .iter()
        .map(|w| fs::read(w.path.join("parent.txt")))
        .collect::<std::io::Result<Vec<_>>>()?;
    let auto = crate::undo_merge::automatic_request(&conflicted)?.unwrap();
    let public: Job = serde_json::from_value(
        f.journal.snapshot().await?["integrations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|job| job["id"] == conflicted.id.to_string())
            .unwrap()
            .clone(),
    )?;
    assert_eq!(
        crate::undo_merge::automatic_request(&public)?
            .unwrap()
            .op_id,
        auto.op_id
    );
    assert_eq!(
        auto.owner,
        Owner::Undo {
            id: chosen.id,
            revision: chosen.revision
        }
    );
    let mut stale = auto.clone();
    stale.op_id = Uuid::now_v7();
    stale.track = f.workspaces[2].track_id.clone();
    assert!(f.journal.merge_start(stale).await.is_err());
    let queued = f.journal.repair_start(auto.clone()).await?;
    assert_eq!(queued.phase, RunPhase::Queued);
    let held = f
        .journal
        .repair_control(crate::repair_run::Command {
            op_id: Uuid::now_v7(),
            id: queued.id,
            revision: queued.revision,
            action: crate::repair_run::Control::Stop,
        })
        .await?;
    assert_eq!(held.phase, RunPhase::Held);
    let discarded = f
        .journal
        .repair_control(crate::repair_run::Command {
            op_id: Uuid::now_v7(),
            id: held.id,
            revision: held.revision,
            action: crate::repair_run::Control::Discard,
        })
        .await?;
    assert_eq!(discarded.phase, RunPhase::Discarded);
    assert!(discarded.host.is_none());
    assert!(
        f.journal.worker_config().await?.is_none(),
        "this test must never launch a model host"
    );
    let runtime = start_repairs(&f, true);
    let request = crate::repair_run::Request {
        op_id: Uuid::now_v7(),
        ..auto
    };
    let run = runtime.service.start_manual(request.clone()).await?;
    assert_eq!(runtime.service.start_manual(request).await?.id, run.id);
    assert_eq!(run.phase, RunPhase::Manual);
    assert_eq!(run.purpose, Purpose::Merge);
    let view = runtime
        .service
        .read_manual(crate::merge_manual::Read {
            id: run.id,
            revision: run.revision,
            path: Some("parent.txt".into()),
        })
        .await?;
    let captured = runtime
        .service
        .apply_manual(crate::merge_manual::Apply {
            op_id: Uuid::now_v7(),
            id: run.id,
            revision: run.revision,
            files: vec![crate::merge_manual::Edit::Text {
                path: "parent.txt".into(),
                version: view.file.version,
                text: "Own later B edit\n".into(),
            }],
        })
        .await?;
    let (_, source) = f.journal.repair_source(run.id).await?;
    let merge_root = f
        .root
        .path()
        .join("integration")
        .join(chosen.id.to_string())
        .join(format!("repair-{}", run.id));
    let workspace = crate::run_workspace::Workspace::open_source(
        source,
        &merge_root,
        &captured,
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    let crate::run_workspace::Workspace::Undo(workspace) = workspace else {
        panic!("Undo must own its private merge")
    };
    let source_repo = intercat_git::Repository::open(&f.source)?;
    assert_ne!(
        source_repo
            .tree_id(workspace.input.plan.parent.parse()?)?
            .to_string(),
        workspace.input.conflict.target,
        "the conflict target follows an already reversed record"
    );
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    db.execute_batch("CREATE TRIGGER own_undo_result_fault BEFORE UPDATE ON integration_jobs WHEN json_extract(NEW.record,'$.revision')>json_extract(OLD.record,'$.revision') BEGIN SELECT RAISE(ABORT,'Own dummy undo result storage failure'); END;")?;
    assert!(
        crate::undo_merge_apply::apply(
            &f.journal,
            &captured,
            &workspace,
            tokio::sync::watch::channel(false).1
        )
        .await
        .is_err()
    );
    let result = Uuid::new_v5(&run.id, b"apply-undo-merge");
    assert_eq!(
        f.journal.operation(result).await?.unwrap()["state"],
        "prepared"
    );
    assert_eq!(
        f.journal.repair_run(run.id).await?.phase,
        RunPhase::Applying
    );
    assert_eq!(
        f.journal.integration_job(chosen.id).await?.revision,
        chosen.revision
    );
    for (index, w) in f.workspaces.iter().enumerate() {
        assert_eq!(fs::read(w.path.join("parent.txt"))?, before[index]);
    }
    db.execute_batch("DROP TRIGGER own_undo_result_fault;")?;
    // Reopen the journal and both coordinators. The persisted solution must
    // resume from NeedsRecovery without a window, model configuration or auth.
    runtime.shutdown().await;
    f.review.shutdown().await;
    f.journal = Journal::open(
        f.root.path().join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    assert_eq!(
        f.journal.repair_run(run.id).await?.phase,
        RunPhase::NeedsRecovery
    );
    assert!(f.journal.worker_config().await?.is_none());
    f.review = Service::new(f.journal.clone(), f.root.path().join("integration"));
    db.execute_batch("CREATE TRIGGER own_undo_check_fault BEFORE UPDATE ON integration_jobs WHEN json_extract(NEW.record,'$.status')='building' BEGIN SELECT RAISE(ABORT,'Own dummy Check dispatch failure'); END;")?;
    let runtime = start_repairs(&f, false);
    f.journal.pause_dispatch(Uuid::now_v7(), false).await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if f.journal.repair_run(run.id).await?.phase == RunPhase::Done
                && f.journal
                    .operation(result)
                    .await?
                    .is_some_and(|op| op["state"] == "dispatched")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await??;
    runtime.shutdown().await;
    f.review.shutdown().await;
    assert!(
        !merge_root.exists(),
        "a dispatched result must not require the cleaned merge files"
    );
    db.execute_batch("DROP TRIGGER own_undo_check_fault;")?;
    f.journal = Journal::open(
        f.root.path().join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    assert_eq!(
        f.journal.operation(result).await?.unwrap()["state"],
        "dispatched"
    );
    assert!(f.journal.worker_config().await?.is_none());
    f.review = Service::new(f.journal.clone(), f.root.path().join("integration"));
    let runtime = start_repairs(&f, false);
    f.journal.pause_dispatch(Uuid::now_v7(), false).await?;
    let checked = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let job = f.journal.integration_job(chosen.id).await?;
            if job.revision > chosen.revision
                && job
                    .candidate
                    .as_ref()
                    .is_some_and(|c| c.revision == job.revision)
                && matches!(
                    job.status,
                    Status::Checked | Status::Failed | Status::MergeConflict
                )
            {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    assert_eq!(checked.revision, chosen.revision + 1);
    assert_eq!(checked.undo.as_ref().unwrap().resolutions.len(), 1);
    assert_eq!(
        f.journal.operation(result).await?.unwrap()["state"],
        "complete"
    );
    let candidate = checked.candidate.as_ref().unwrap();
    let undo = candidate.undo.as_ref().unwrap();
    assert_eq!(undo.author, "Intercat");
    assert_eq!(undo.records, checked.undo.as_ref().unwrap().ids());
    assert_eq!(undo.records.len(), 2);
    assert_eq!(candidate.commits.len(), 1);
    assert_eq!(candidate.commits[0].track, f.workspaces[1].track_id);
    assert_eq!(candidate.commits[0].parent, undo.sha);
    let undo_diff = f
        .review
        .diff(crate::integration::DiffRequest {
            id: checked.id,
            revision: checked.revision,
            owner: crate::integration::DiffOwner::Undo,
            path: "independent.txt".into(),
        })
        .await?;
    assert_eq!(undo_diff.before, "Own later A file\n");
    assert_eq!(undo_diff.after, "");
    assert!(!undo_diff.redacted);
    assert!(
        f.review
            .diff(crate::integration::DiffRequest {
                id: checked.id,
                revision: checked.revision,
                owner: crate::integration::DiffOwner::Undo,
                path: "alternative.txt".into(),
            })
            .await
            .is_err(),
        "a worker-only path does not belong to the undo commit"
    );
    assert!(
        f.review
            .diff(crate::integration::DiffRequest {
                id: checked.id,
                revision: checked.revision - 1,
                owner: crate::integration::DiffOwner::Undo,
                path: "independent.txt".into(),
            })
            .await
            .is_err(),
        "stale undo previews must be refused"
    );
    assert!(!undo.message.contains("since checkpoint"));
    let files = source_repo.tree_files(candidate.tree.parse()?)?;
    assert!(!files.contains_key("independent.txt"));
    assert!(files.contains_key("alternative.txt"));
    assert_eq!(
        source_repo.saved_bytes(&files["parent.txt"])?,
        b"Own later B edit\n"
    );
    let done = f.journal.repair_run(run.id).await?;
    assert_eq!(done.phase, RunPhase::Done);
    assert!(done.host.is_none() && done.session.is_none() && done.start_command.is_none());
    assert_eq!(
        db.query_row("SELECT count(*) FROM integration_cancelled", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    runtime.shutdown().await;
    f.review.finish_undo_merge_result(result).await?;
    let committing = f
        .journal
        .integration_command(ReviewCommand {
            op_id: Uuid::now_v7(),
            action: commit(&checked),
        })
        .await?;
    let entries: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    for entry in entries.into_iter().filter(|e| e.job == committing.id) {
        f.journal
            .change_path_retirement(crate::path_retirement::Change::Freeze {
                id: entry.id,
                session: None,
                checkpoint: None,
            })
            .await?;
        let entries: Vec<crate::path_retirement::Entry> =
            serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
        let saved = entries
            .iter()
            .find(|e| e.id == entry.id)
            .unwrap()
            .saved
            .as_ref()
            .unwrap();
        source_repo.compare_and_swap(&saved.reference, None, saved.sha.parse()?)?;
        source_repo.sync_ref(&saved.reference)?;
        f.journal
            .change_path_retirement(crate::path_retirement::Change::Ready(entry.id))
            .await?;
    }
    let published = crate::publication::publish(
        committing,
        f.journal.clone(),
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    assert_eq!(published.status, Status::Done, "{:?}", published.error);
    assert_eq!(
        db.query_row("SELECT count(*) FROM integration_cancelled", [], |r| r
            .get::<_, i64>(0))?,
        2
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='undo.merge.applied'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    for (index, w) in f.workspaces.iter().enumerate() {
        assert_eq!(fs::read(w.path.join("parent.txt"))?, before[index]);
    }
    f.review.shutdown().await;
    Ok(())
}
