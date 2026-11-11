use super::*;
use crate::path_restore::{Phase as RestorePhase, Request as RestoreRequest, Stage};
use crate::repair_run::{Owner, Phase as RunPhase, Purpose};

#[path = "restore_native_acceptance.rs"]
mod native;
#[path = "restore_sdk_acceptance.rs"]
mod sdk;

#[tokio::test]
async fn a_retirement_does_not_choose_stop_here_to_clear_an_unsaved_checkpoint() -> Result<()> {
    let f = Fixture::new(1).await?;
    let track = f.workspaces[0].track_id.clone();
    let state = WorkerState {
        track_id: track.clone(),
        phase: "failed".into(),
        session_id: None,
        secret_revision: Some(0),
        identity: None,
        detail: Some("Own interrupted checkpoint".into()),
        active_turn: None,
        approvals: vec![],
        output: None,
        checkpoint_pending: true,
        checkpoint: None,
        prompts: vec![],
    };
    f.journal.worker_state(state.clone()).await?;
    let editor =
        crate::editor::Manager::new(f.root.path(), &f.root.path().join("unused-editor.log"))?;
    let runtime = crate::runtime::Runtime::start(
        crate::host_memory::Pool::new(),
        f.root.path().into(),
        f.root.path().join("unused-rpc"),
        f.journal.clone(),
        editor,
        Arc::new(crate::scheduler::Capacity::new(1)),
        Arc::new(crate::dispatch_control::Gate::new(false)),
    );
    let id = Uuid::now_v7();
    let outcome = runtime.service.stop_path_boundary(id, &track).await;
    let snapshot = f.journal.snapshot().await?;
    runtime.shutdown().await;
    assert!(
        outcome
            .err()
            .unwrap()
            .to_string()
            .contains("final checkpoint")
    );
    let mut expected = serde_json::to_value(state)?;
    expected["pairedInstruction"] = serde_json::Value::Null;
    assert_eq!(snapshot["workers"][0], expected);
    assert!(f.journal.operation(id).await?.is_none());
    Ok(())
}

async fn conflicted_restore() -> Result<(Fixture, RestoreRequest)> {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(4).await?;
    let fork = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own worker before publication\n",
    )?;
    f.save(0, Some(fork.clone())).await?;
    // A real private pre-commit hook changes the published delta. The worker
    // remains at its original checkpoint, as it does before a realignment.
    let hook = f.source.join(".git/hooks/pre-commit");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf 'Own published hook edit\\n' > parent.txt\ngit add parent.txt\n",
    )?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
    let first = f.publish(vec![f.workspaces[0].track_id.clone()]).await?;
    fs::remove_file(hook)?;
    assert_eq!(
        fs::read_to_string(f.workspaces[0].path.join("parent.txt"))?,
        "Own worker before publication\n"
    );
    let repo = intercat_git::Repository::open(&f.source)?;
    assert_eq!(
        git(
            &f.source,
            &[
                "show",
                &format!("{}:parent.txt", first.candidate.as_ref().unwrap().head)
            ]
        )?,
        "Own published hook edit"
    );
    f.branch(&fork)?;
    fs::write(
        f.workspaces[1].path.join("parent.txt"),
        "Own chosen branch\n",
    )?;
    f.save(1, None).await?;
    let open = f
        .action(Action::Open {
            tracks: Some(vec![f.workspaces[1].track_id.clone()]),
        })
        .await?;
    let chosen = f
        .action(Action::ChoosePath {
            id: open.id,
            revision: open.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    f.action(Action::Prepare {
        id: chosen.id,
        revision: chosen.revision,
    })
    .await?;
    let checked = f.settled(chosen.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    assert_eq!(checked.undo.as_ref().unwrap().records.len(), 1);
    let committing = f
        .journal
        .integration_command(ReviewCommand {
            op_id: Uuid::now_v7(),
            action: commit(&checked),
        })
        .await?;
    assert_eq!(committing.status, Status::Committing);
    let entries: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.track, f.workspaces[0].track_id);
    f.journal
        .change_path_retirement(crate::path_retirement::Change::Freeze {
            id: entry.id,
            session: None,
            checkpoint: None,
        })
        .await?;
    let entry: crate::path_retirement::Entry =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"][0].clone())?;
    let saved = entry.saved.as_ref().unwrap();
    repo.compare_and_swap(&saved.reference, None, saved.sha.parse()?)?;
    repo.sync_ref(&saved.reference)?;
    f.journal
        .change_path_retirement(crate::path_retirement::Change::Ready(entry.id))
        .await?;
    f.review.recover().await?;
    let published = f.settled(checked.id).await?;
    assert_eq!(published.status, Status::Done, "{:?}", published.error);
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own pending worker edit\n",
    )?;
    let request = RestoreRequest {
        op_id: Uuid::now_v7(),
        retirement_id: entry.id,
    };
    f.journal.restore_path(request.clone()).await?;
    Ok((f, request))
}
async fn advance(
    f: &Fixture,
    request: &RestoreRequest,
) -> Result<Option<crate::path_restore_apply::Applied>> {
    let gate = Arc::new(crate::dispatch_control::Gate::new(false));
    for _ in 0..4 {
        let boundary = crate::path_restore_apply::Boundary::Offline {
            guard: crate::inheritance_apply::Guard::Recovery {
                generation: Arc::new(AtomicU64::new(0)),
                expected: 0,
                epoch: gate.epoch(),
                gate: gate.clone(),
            },
            directory: f.root.path().join("never-hosted"),
            workspace: f.workspaces[0].path.clone(),
            expected: None,
        };
        match crate::path_restore_apply::apply(&f.journal, request.clone(), boundary).await {
            Ok(applied) => return Ok(applied),
            Err(error) if error.is::<crate::path_restore::MergeRequired>() => return Err(error),
            Err(error) => {
                let Some(required) =
                    error.downcast_ref::<crate::workspace_access::ConfirmationRequired>()
                else {
                    return Err(error);
                };
                // This fixture owns every temporary file being confirmed.
                f.journal
                    .approve_path_restore_access(crate::path_restore::AccessConsent {
                        op_id: Uuid::now_v7(),
                        request: request.clone(),
                        fingerprint: required.inspection.fingerprint.clone(),
                    })
                    .await?;
            }
        }
    }
    Err("The own Restore access boundary did not settle".into())
}
fn runtime(f: &Fixture, paused: bool) -> crate::repair_service::Runtime {
    crate::repair_service::Runtime::start(
        crate::host_memory::Pool::new(),
        f.journal.clone(),
        f.root.path().join("integration"),
        f.root.path().into(),
        f.root.path().join("own-restore-rpc.sock"),
        Arc::new(crate::scheduler::Capacity::new(4)),
        f.review.clone(),
        crate::project_secrets::Store::default(),
        Arc::new(crate::dispatch_control::Gate::new(paused)),
    )
}
#[tokio::test]
async fn restore_controls_keep_the_frozen_request_and_stop_only_the_retired_paths_processes()
-> Result<()> {
    use crate::path_restore::{Control, ControlAction};
    use crate::path_restore_store::Change;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;
    struct OwnChild(std::process::Child);
    impl Drop for OwnChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let (f, request) = conflicted_restore().await?;
    let queued = f.journal.restore_path(request.clone()).await?;
    let hold = Control {
        op_id: Uuid::now_v7(),
        id: request.op_id,
        revision: queued.revision,
        action: ControlAction::Hold,
    };
    let held = f
        .journal
        .change_path_restore(Change::Control(hold.clone()))
        .await?;
    assert_eq!(held.phase, RestorePhase::Held);
    let replay = f
        .journal
        .change_path_restore(Change::Control(hold.clone()))
        .await?;
    assert_eq!(replay.revision, held.revision);
    assert_eq!(replay.request, request);
    let stale = Control {
        op_id: Uuid::now_v7(),
        action: ControlAction::Retry,
        ..hold
    };
    assert!(
        f.journal
            .change_path_restore(Change::Control(stale))
            .await
            .is_err()
    );
    // A driver finishing an earlier attempt must not overwrite a later Hold.
    let late = f
        .journal
        .change_path_restore(Change::Progress {
            request: request.clone(),
            phase: RestorePhase::WaitingForWorker,
            error: None,
            inspection: None,
        })
        .await?;
    assert_eq!(late.phase, RestorePhase::Held);
    assert_eq!(late.revision, held.revision);
    let before = f.journal.snapshot().await?;
    let mut own = OwnChild(
        Command::new("/bin/sleep")
            .arg("600")
            .process_group(0)
            .spawn()?,
    );
    let other = OwnChild(
        Command::new("/bin/sleep")
            .arg("600")
            .process_group(0)
            .spawn()?,
    );
    let own_identity = crate::process::identity(own.0.id())?;
    let other_identity = crate::process::identity(other.0.id())?;
    let track = &f.workspaces[0].track_id;
    let directory = f
        .root
        .path()
        .join("hosts")
        .join(track)
        .join("own-process-test");
    fs::create_dir_all(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let mut tracker = crate::process::Tracker::prepare(
        &directory,
        &f.workspaces[0].path,
        Uuid::now_v7().to_string(),
    )?;
    tracker.attach_process_group(own_identity)?;
    tracker.refresh()?;
    let editor =
        crate::editor::Manager::new(f.root.path(), &f.root.path().join("unused-editor.log"))?;
    let runtime = crate::runtime::Runtime::start(
        crate::host_memory::Pool::new(),
        f.root.path().into(),
        f.root.path().join("unused-rpc"),
        f.journal.clone(),
        editor,
        Arc::new(crate::scheduler::Capacity::new(1)),
        Arc::new(crate::dispatch_control::Gate::new(false)),
    );
    let service = &runtime.service;
    let info = service.aside_processes(request.retirement_id).await?;
    assert_eq!(info.groups, Some(1));
    assert!(service.apply_path_restore(request.clone()).await.is_err());
    let stop = crate::aside_processes::Request {
        op_id: Uuid::now_v7(),
        retirement_id: request.retirement_id,
    };
    let queued_stop = service.stop_aside_processes(stop.clone()).await?;
    assert_eq!(queued_stop.record.track, *track);
    // The actual no-window driver resumes the saved process Stop.
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let value = service.stop_aside_processes(stop.clone()).await?;
            if value.state == "complete" {
                break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await??;
    assert!(!crate::process::is_alive(own_identity));
    own.0.wait()?;
    assert!(crate::process::is_alive(other_identity));
    assert_eq!(
        service.aside_processes(request.retirement_id).await?.groups,
        Some(0)
    );
    let after = f.journal.snapshot().await?;
    assert_eq!(before["workers"], after["workers"]);
    assert_eq!(before["utterances"], after["utterances"]);
    assert_eq!(before["pathRetirements"], after["pathRetirements"]);
    let held = f.journal.restore_path(request.clone()).await?;
    assert_eq!(held.phase, RestorePhase::Held);
    runtime.shutdown().await;
    let waiting = f
        .journal
        .change_path_restore(Change::Control(Control {
            op_id: Uuid::now_v7(),
            id: request.op_id,
            revision: held.revision,
            action: ControlAction::Retry,
        }))
        .await?;
    assert_eq!(waiting.request, request);
    assert_eq!(waiting.phase, RestorePhase::Queued);
    let waiting = f
        .journal
        .change_path_restore(Change::Progress {
            request: request.clone(),
            phase: RestorePhase::WaitingForProcesses,
            error: None,
            inspection: None,
        })
        .await?;
    let keep = f
        .journal
        .change_path_restore(Change::Control(Control {
            op_id: Uuid::now_v7(),
            id: request.op_id,
            revision: waiting.revision,
            action: ControlAction::KeepProcesses,
        }))
        .await?;
    assert!(keep.keep_processes);
    assert_eq!(keep.phase, RestorePhase::WaitingForProcesses);
    assert_eq!(keep.request, request);
    Ok(())
}
#[tokio::test]
async fn a_restore_merge_keeps_each_stage_and_recovers_before_applying_the_original_worker()
-> Result<()> {
    let (mut f, request) = conflicted_restore().await?;
    let operation = crate::path_restore::file_operation(&request);
    let original = fs::read(f.workspaces[0].path.join("parent.txt"))?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let cancellations: i64 =
        db.query_row("SELECT count(*) FROM integration_cancelled", [], |row| {
            row.get(0)
        })?;
    let mut prior = None;
    for (stage, text) in [
        (Stage::Undo, "Own initial base\n"),
        (Stage::Files, "Own restored worker and chosen branch\n"),
    ] {
        let error = advance(&f, &request)
            .await
            .err()
            .ok_or("Expected a saved Restore conflict")?;
        assert!(error.is::<crate::path_restore::MergeRequired>(), "{error}");
        let pending = f.journal.restore_path(request.clone()).await?;
        assert_eq!(pending.phase, RestorePhase::NeedsMerge);
        let summary = pending.merge_input.as_ref().unwrap();
        assert_eq!(summary.stage, stage);
        assert_eq!(summary.paths, ["parent.txt"]);
        let frozen = crate::restore_merge::read(&f.journal, summary).await?;
        if let Some((old, old_run)) = &prior {
            assert_ne!(frozen.id, *old);
            let (_, source) = f.journal.repair_source(*old_run).await?;
            let crate::run_input::Source::Restore(saved) = source else {
                panic!("Original conflict owner must remain readable");
            };
            assert_eq!(saved.id, *old);
            assert!(
                f.journal
                    .repair_start(crate::repair_run::Request {
                        op_id: Uuid::now_v7(),
                        owner: Owner::Restore {
                            id: summary.id,
                            revision: summary.revision - 1
                        },
                        track: summary.track.clone(),
                        purpose: Purpose::Merge
                    })
                    .await
                    .is_err()
            );
        }
        let service = runtime(&f, true);
        let run = service
            .service
            .start_manual(crate::repair_run::Request {
                op_id: Uuid::now_v7(),
                owner: Owner::Restore {
                    id: summary.id,
                    revision: summary.revision,
                },
                track: summary.track.clone(),
                purpose: Purpose::Merge,
            })
            .await?;
        let view = service
            .service
            .read_manual(crate::merge_manual::Read {
                id: run.id,
                revision: run.revision,
                path: Some("parent.txt".into()),
            })
            .await?;
        let captured = service
            .service
            .apply_manual(crate::merge_manual::Apply {
                op_id: Uuid::now_v7(),
                id: run.id,
                revision: run.revision,
                files: vec![crate::merge_manual::Edit::Text {
                    path: "parent.txt".into(),
                    version: view.file.version,
                    text: text.into(),
                }],
            })
            .await?;
        assert_eq!(captured.phase, RunPhase::Captured);
        let (_, source) = f.journal.repair_source(run.id).await?;
        let root = f
            .root
            .path()
            .join("integration")
            .join(summary.id.to_string())
            .join(format!("repair-{}", run.id));
        let workspace = crate::run_workspace::Workspace::open_source(
            source,
            &root,
            &captured,
            tokio::sync::watch::channel(false).1,
        )
        .await?;
        let crate::run_workspace::Workspace::Restore(workspace) = workspace else {
            panic!("Restore must own the private workspace");
        };
        // Refuse admission after import and planning. A fresh writer/repair
        // runtime must resume the saved continuation without another model.
        db.execute_batch("CREATE TRIGGER own_restore_merge_fault BEFORE UPDATE ON operations WHEN NEW.kind='workspace.restorePath' AND OLD.state='prepared' BEGIN SELECT RAISE(ABORT,'Own Restore merge admission failure'); END;")?;
        assert!(
            crate::restore_merge_apply::apply(
                &f.journal,
                &captured,
                &workspace,
                tokio::sync::watch::channel(false).1
            )
            .await
            .is_err()
        );
        let result = Uuid::new_v5(&run.id, b"apply-restore-merge");
        let intent = f.journal.operation(result).await?.unwrap();
        assert_eq!(intent["state"], "dispatched");
        assert_eq!(
            f.journal.operation(operation).await?.unwrap()["state"],
            "prepared"
        );
        assert_eq!(fs::read(f.workspaces[0].path.join("parent.txt"))?, original);
        db.execute_batch("DROP TRIGGER own_restore_merge_fault;")?;
        service.shutdown().await;
        f.review.shutdown().await;
        f.journal = Journal::open(
            f.root.path().join("events.sqlite"),
            crate::secrets::Key::new([7; 32]),
        )
        .await?;
        f.review = Service::new(f.journal.clone(), f.root.path().join("integration"));
        assert!(f.journal.worker_config().await?.is_none());
        let resumed = runtime(&f, false);
        let done = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let run = f.journal.repair_run(run.id).await?;
                if run.phase == RunPhase::Done && run.remaining.is_none() {
                    break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        resumed.shutdown().await;
        done??;
        assert_eq!(
            f.journal.operation(result).await?.unwrap()["state"],
            "complete"
        );
        assert_eq!(
            f.journal.operation(result).await?.unwrap()["payload"],
            intent["payload"]
        );
        assert_eq!(fs::read(f.workspaces[0].path.join("parent.txt"))?, original);
        assert!(crate::path_retirement::holds(
            &db,
            &f.workspaces[0].track_id
        )?);
        prior = Some((frozen.id, run.id));
    }
    let applied = advance(&f, &request)
        .await?
        .ok_or("Missing completed Restore files")?;
    f.journal
        .change_path_restore(crate::path_restore_store::Change::Complete(
            applied.operation,
        ))
        .await?;
    assert_eq!(
        f.journal.restore_path(request.clone()).await?.phase,
        RestorePhase::Done
    );
    assert_eq!(
        fs::read_to_string(f.workspaces[0].path.join("parent.txt"))?,
        "Own restored worker and chosen branch\n"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM integration_cancelled", [], |row| row
            .get::<_, i64>(
            0
        ))?,
        cancellations
    );
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    f.review.shutdown().await;
    Ok(())
}
