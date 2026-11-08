use intercat_daemon::{
    Result,
    checkpoint::{self, Checkpoint},
    decision::{Decision, Kind, Outcome, Target},
    integration::{self, Action, Command as IntegrationCommand, Job, RowState, Service, Status},
    journal::Journal,
    preparation::{Preparation, Workspace},
    project,
};
use intercat_git::{ObjectId, Repository};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use uuid::Uuid;

#[path = "support/commit_exclusions.rs"]
mod exclusions;

fn inherit_dummy_checkpoint(
    f: &Fixture,
    child: usize,
    parent: usize,
    checkpoint: &Checkpoint,
) -> Result<()> {
    use intercat_daemon::lineage::{Basis, Point, include};
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    let raw: String = db.query_row(
        "SELECT record FROM checkpoint_lineage WHERE track=?1 AND sha=?2",
        rusqlite::params![f.workspaces[parent].track_id, checkpoint.sha],
        |row| row.get(0),
    )?;
    let point: Point = serde_json::from_str(&raw)?;
    let raw: String = db.query_row(
        "SELECT record FROM track_lineage WHERE track=?1",
        [&f.workspaces[child].track_id],
        |row| row.get(0),
    )?;
    let mut basis: Basis = serde_json::from_str(&raw)?;
    for (track, mark) in &point.inherits {
        include(&mut basis.inherits, track, mark)?;
    }
    include(&mut basis.inherits, &point.track, &point.inherited())?;
    db.execute(
        "UPDATE track_lineage SET record=?2 WHERE track=?1",
        rusqlite::params![f.workspaces[child].track_id, serde_json::to_string(&basis)?],
    )?;
    Ok(())
}

fn record_simulated_integration(
    f: &Fixture,
    tracks: &[intercat_daemon::integration::Track],
) -> Result<()> {
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    for track in tracks {
        let Some(checkpoint) = &track.checkpoint else {
            continue;
        };
        let raw: String = db.query_row(
            "SELECT record FROM checkpoint_lineage WHERE track=?1 AND sha=?2",
            rusqlite::params![track.id, checkpoint.sha],
            |row| row.get(0),
        )?;
        let point: intercat_daemon::lineage::Point = serde_json::from_str(&raw)?;
        let mut basis = track.basis.clone();
        basis.base = point.sha.clone();
        basis.base_inherits = point.inherits.clone();
        db.execute(
            "UPDATE track_lineage SET record=?2 WHERE track=?1",
            rusqlite::params![track.id, serde_json::to_string(&basis)?],
        )?;
        db.execute(
            "INSERT INTO integrated_lineage VALUES(?1,?2)",
            rusqlite::params![track.id, serde_json::to_string(&point.inherited())?],
        )?;
    }
    Ok(())
}

#[tokio::test]
async fn commit_cannot_capture_half_of_an_inherited_checkpoint_update() -> Result<()> {
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "saved child work\n")?;
    let checkpoint = f.save(0, None).await?;
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    let operation = Uuid::now_v7().to_string();
    db.execute(
        "INSERT INTO operations VALUES(?1,'workspace.apply-inheritance',?2,'dispatched')",
        rusqlite::params![operation,json!({"operation":operation,"dependencies":{"track":f.workspaces[0].track_id,"input":Uuid::now_v7(),"points":[]},"revision":1,"requestDigest":"dummy","session":"dummy","workspace":f.workspaces[0],"previous":null,"before":checkpoint.sha,"after":checkpoint.sha,"beforeBasis":{"base":f.workspaces[0].base,"baseInherits":{},"inherits":{}},"basis":{"base":f.workspaces[0].base,"baseInherits":{},"inherits":{}},"checkpointTurn":Uuid::now_v7(),"expectedBase":null}).to_string()],
    )?;
    let error = f.open().await.unwrap_err();
    assert!(error.to_string().contains("inherited file update"));
    db.execute(
        "UPDATE operations SET state='complete' WHERE id=?1",
        [operation],
    )?;
    assert_eq!(
        f.open().await?.plan.as_ref().unwrap().rows[0].state,
        RowState::Included
    );
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn inherited_line_edit_integrates_in_order_and_persists_the_number_boundary() -> Result<()> {
    let f = Fixture::new(2).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "200\n")?;
    let parent = f.save(0, None).await?;
    inherit_dummy_checkpoint(&f, 1, 0, &parent)?;
    fs::write(f.workspaces[1].path.join("file.txt"), "300\n")?;
    f.save(1, None).await?;
    let open = f.open().await?;
    assert_eq!(
        open.plan.as_ref().unwrap().rows[1].state,
        RowState::Included
    );
    f.action(Action::Prepare {
        id: open.id,
        revision: open.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let candidate = checked.candidate.as_ref().unwrap();
    assert_eq!(
        git(
            &f.source,
            &["show", &format!("{}:file.txt", candidate.head)]
        ),
        "300"
    );
    f.action(commit_action(&checked)).await?;
    assert_eq!(finished(&f.journal).await?.status, Status::Done);
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    let count: i64 = db.query_row("SELECT count(*) FROM integrated_lineage", [], |row| {
        row.get(0)
    })?;
    assert_eq!(count, 2);
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn inherited_checkpoint_does_not_restore_a_file_deleted_by_a_newer_parent_commit()
-> Result<()> {
    let f = Fixture::new(2).await?;
    fs::write(f.workspaces[0].path.join("old.txt"), "old parent file\n")?;
    let a1 = f.save(0, None).await?;
    inherit_dummy_checkpoint(&f, 1, 0, &a1)?;
    fs::write(f.workspaces[1].path.join("old.txt"), "old parent file\n")?;
    fs::write(f.workspaces[1].path.join("child.txt"), "child work\n")?;
    f.save(1, None).await?;
    fs::remove_file(f.workspaces[0].path.join("old.txt"))?;
    f.save(0, Some(a1)).await?;
    let open = f
        .action(Action::Open {
            tracks: Some(vec![f.workspaces[0].track_id.clone()]),
        })
        .await?;
    f.action(Action::Prepare {
        id: open.id,
        revision: open.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    f.action(commit_action(&checked)).await?;
    assert_eq!(finished(&f.journal).await?.status, Status::Done);
    let open = f
        .action(Action::Open {
            tracks: Some(vec![f.workspaces[1].track_id.clone()]),
        })
        .await?;
    f.action(Action::Prepare {
        id: open.id,
        revision: open.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let tree = Repository::open(&f.source)?
        .saved_files(checked.candidate.as_ref().unwrap().head.parse()?)?;
    assert!(!tree.contains_key("old.txt"));
    assert!(tree.contains_key("child.txt"));
    f.source_unchanged()?;
    Ok(())
}

fn selected_edit(job: &Job, text: &str) -> intercat_daemon::repair_run::Request {
    use intercat_daemon::repair_run::{Purpose, Request, Selection, Side};
    let candidate = job.candidate.as_ref().unwrap();
    let commit = &candidate.commits[0];
    let change = commit
        .changes
        .iter()
        .find(|change| change.path == "file.txt")
        .unwrap();
    Request {
        op_id: Uuid::now_v7(),
        job_id: job.id,
        revision: job.revision,
        track: commit.track.clone(),
        purpose: Purpose::Edit {
            text: text.into(),
            selection: Selection {
                tree: candidate.tree.clone(),
                path: change.path.clone(),
                side: Side::After,
                object: change.after.as_ref().unwrap().object.clone(),
                start_line: 1,
                end_line: 1,
            },
        },
    }
}

async fn checked_for_edit(f: &Fixture) -> Result<Job> {
    fs::write(f.workspaces[0].path.join("file.txt"), "first\n")?;
    f.save(0, None).await?;
    let opened = f.open().await?;
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked);
    Ok(checked)
}

async fn repair_with_saved_deletion(
    f: &Fixture,
) -> Result<(
    intercat_daemon::repair_run::Run,
    intercat_daemon::repair::Workspace,
    intercat_daemon::deletion::Snapshot,
)> {
    use intercat_daemon::{
        deletion, repair,
        repair_run::{Event, Outcome},
    };
    let checked = checked_for_edit(f).await?;
    let mut run = f
        .journal
        .repair_start(selected_edit(&checked, "Remove the temporary file."))
        .await?;
    run = f
        .journal
        .repair_transition(Uuid::now_v7(), run.id, run.revision, Event::Prepare)
        .await?;
    let (_, input, settings) = f.journal.repair_input(run.id).await?;
    let parent = f
        ._root
        .path()
        .join("integration")
        .join(input.id.to_string());
    let source = parent.join(input.candidate.as_ref().unwrap().generation.to_string());
    let workspace = repair::Workspace::prepare(
        &input,
        &source,
        &parent.join(format!("repair-{}", run.id)),
        &run.track,
        &settings,
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    run = f
        .journal
        .repair_transition(Uuid::now_v7(), run.id, run.revision, Event::WorkspaceReady)
        .await?;
    fs::write(
        workspace.checkout.workdir.join("temporary.txt"),
        "recover this private edit\n",
    )?;
    fs::write(
        workspace.checkout.workdir.join("unrelated.txt"),
        "keep this edit\n",
    )?;
    let files = workspace.snapshot_workspace(run.id);
    let request = json!({"sessionId":"repair-session","approvalId":"delete-approval","itemId":"delete-item","currentRequirementId":{"sourceIndex":0},"subject":{"kind":"shell","command":"rm temporary.txt","stages":[{"argv":["rm","temporary.txt"],"argvComplete":true}]}});
    let saved = deletion::prepare(
        files.clone(),
        Some(run.id),
        request.clone(),
        files.base.clone(),
        f.journal.clone(),
    )
    .await?;
    assert!(saved.complete);
    assert!(
        f.journal
            .evaluate_approval(
                run.track.clone(),
                files.path.clone(),
                vec![],
                request,
                Some(saved.clone())
            )
            .await?
            .review
            .is_none()
    );
    let runtime = intercat_daemon::repair_service::Runtime::start(
        intercat_daemon::host_memory::Pool::new(),
        f.journal.clone(),
        f._root.path().join("integration"),
        f._root.path().join("data"),
        f._root.path().join("rpc"),
        std::sync::Arc::new(intercat_daemon::scheduler::Capacity::new(0)),
        f.service.clone(),
        f.project_secrets.clone(),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    assert!(
        runtime
            .service
            .restore(Uuid::now_v7(), run.id, run.track.clone(), saved.id)
            .await
            .is_err()
    );
    runtime.shutdown().await;
    run = f
        .journal
        .repair_transition(
            Uuid::now_v7(),
            run.id,
            run.revision,
            Event::HostClosed {
                outcome: Outcome::Stopped,
            },
        )
        .await?;
    fs::remove_file(workspace.checkout.workdir.join("temporary.txt"))?;
    Ok((run, workspace, saved))
}

#[tokio::test]
async fn repair_deletion_restore_is_private_owner_bound_and_idempotent() -> Result<()> {
    use intercat_daemon::deletion;
    let f = Fixture::new(1).await?;
    let (run, workspace, saved) = repair_with_saved_deletion(&f).await?;
    let files = workspace.snapshot_workspace(run.id);
    assert!(
        Repository::open(&f.source)?
            .commit_details(saved.sha.parse()?)
            .is_err()
    );
    assert!(
        deletion::restore(
            files.clone(),
            None,
            saved.clone(),
            Uuid::now_v7(),
            f.journal.clone()
        )
        .await
        .is_err()
    );
    let runtime = intercat_daemon::repair_service::Runtime::start(
        intercat_daemon::host_memory::Pool::new(),
        f.journal.clone(),
        f._root.path().join("integration"),
        f._root.path().join("data"),
        f._root.path().join("rpc"),
        std::sync::Arc::new(intercat_daemon::scheduler::Capacity::new(0)),
        f.service.clone(),
        f.project_secrets.clone(),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    assert!(
        runtime
            .service
            .restore(Uuid::now_v7(), Uuid::now_v7(), run.track.clone(), saved.id)
            .await
            .is_err()
    );
    let before_head = git(&files.path, &["rev-parse", "HEAD"]);
    let index = fs::read(files.path.join(".git/index"))?;
    let operation = Uuid::now_v7();
    let restored = runtime
        .service
        .restore(operation, run.id, run.track.clone(), saved.id)
        .await?;
    assert_eq!(
        fs::read_to_string(files.path.join("temporary.txt"))?,
        "recover this private edit\n"
    );
    assert_eq!(
        fs::read_to_string(files.path.join("unrelated.txt"))?,
        "keep this edit\n"
    );
    assert_eq!(git(&files.path, &["rev-parse", "HEAD"]), before_head);
    assert_eq!(fs::read(files.path.join(".git/index"))?, index);
    fs::write(files.path.join("temporary.txt"), "later edit\n")?;
    assert_eq!(
        runtime
            .service
            .restore(operation, run.id, run.track.clone(), saved.id)
            .await?
            .backup_id,
        restored.backup_id
    );
    assert_eq!(
        fs::read_to_string(files.path.join("temporary.txt"))?,
        "later edit\n"
    );
    let backup = f.journal.deletion(restored.backup_id).await?;
    assert_eq!(backup.repair_run_id, Some(run.id));
    assert!(backup.absent.contains(&"temporary.txt".into()));
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    assert!(deletion::summaries(&db, Some(&run.track), Some("delete-item"), None)?.is_empty());
    assert_eq!(
        deletion::summaries(&db, Some(&run.track), Some("delete-item"), Some(run.id))?.len(),
        2
    );
    assert!(f.journal.repair_input(run.id).await?.0.restoring.is_none());
    runtime.shutdown().await;
    f.service.shutdown().await;
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_deletion_restore_resumes_after_partial_writes_and_keeps_the_retry_backup()
-> Result<()> {
    use intercat_daemon::{
        deletion,
        repair_run::{Command, Control},
    };
    let f = Fixture::new(1).await?;
    let (run, workspace, saved) = repair_with_saved_deletion(&f).await?;
    let operation = Uuid::now_v7();
    let held = f
        .journal
        .repair_begin_restore(operation, run.id, run.track.clone(), saved.id)
        .await?;
    assert!(
        f.journal
            .repair_control(Command {
                op_id: Uuid::now_v7(),
                id: run.id,
                revision: held.revision,
                action: Control::Resume
            })
            .await
            .is_err()
    );
    let files = workspace.snapshot_workspace(run.id);
    let backup = deletion::before_restore(&files, &saved, &f.journal).await?;
    let child = Uuid::new_v5(&operation, b"restore-files");
    f.journal.intent(&child.to_string(), "deletion.restore", json!({"trackId":run.track,"repairRunId":run.id,"snapshotId":saved.id,"backupId":backup.id})).await?;
    fs::write(
        files.path.join("temporary.txt"),
        "partially restored contents\n",
    )?;
    let reopened = Journal::open(
        f._root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let integration = Service::new(reopened.clone(), f._root.path().join("integration"));
    let runtime = intercat_daemon::repair_service::Runtime::start(
        intercat_daemon::host_memory::Pool::new(),
        reopened.clone(),
        f._root.path().join("integration"),
        f._root.path().join("data"),
        f._root.path().join("rpc"),
        std::sync::Arc::new(intercat_daemon::scheduler::Capacity::new(0)),
        integration.clone(),
        empty_project_secrets(&reopened).await?,
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    assert_eq!(
        reopened.snapshot().await?["pendingRestores"][0]["repairRunId"],
        run.id.to_string()
    );
    let result = runtime
        .service
        .restore(operation, run.id, run.track.clone(), saved.id)
        .await?;
    assert_eq!(result.backup_id, backup.id);
    assert_eq!(
        fs::read_to_string(files.path.join("temporary.txt"))?,
        "recover this private edit\n"
    );
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    let snapshots = deletion::summaries(&db, Some(&run.track), Some("delete-item"), Some(run.id))?;
    assert!(snapshots.iter().any(|s| s.purpose == "before-restore"
        && git(&files.path, &["show", &format!("{}:temporary.txt", s.sha)])
            == "partially restored contents"));
    assert!(
        reopened.snapshot().await?["pendingRestores"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(reopened.repair_input(run.id).await?.0.restoring.is_none());
    runtime.shutdown().await;
    integration.shutdown().await;
    f.service.shutdown().await;
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn selected_edit_rebuilds_checked_and_previously_repaired_candidates() -> Result<()> {
    use intercat_daemon::{
        repair, repair_application,
        repair_run::{Event, Outcome, Phase},
    };
    let f = Fixture::new(1).await?;
    let mut checked = checked_for_edit(&f).await?;
    for content in ["second\n", "third\n"] {
        let run = f
            .journal
            .repair_start(selected_edit(&checked, "Change the selected line."))
            .await?;
        let (_, input, settings) = f.journal.repair_input(run.id).await?;
        let parent = f
            ._root
            .path()
            .join("integration")
            .join(input.id.to_string());
        let source = parent.join(input.candidate.as_ref().unwrap().generation.to_string());
        let workspace = repair::Workspace::prepare(
            &input,
            &source,
            &parent.join(format!("repair-{}", run.id)),
            &run.track,
            &settings,
            tokio::sync::watch::channel(false).1,
        )
        .await?;
        fs::write(workspace.checkout.workdir.join("file.txt"), content)?;
        let mut captured = run;
        let command = Uuid::now_v7();
        for event in [
            Event::Prepare,
            Event::WorkspaceReady,
            Event::HostStarted {
                secret_revision: Some(0),
                host: intercat_daemon::process::Identity {
                    pid: 42,
                    started_seconds: 1,
                    started_micros: 1,
                },
                nonce: Uuid::now_v7(),
            },
            Event::SessionReady {
                session: "selected-edit-session".into(),
            },
            Event::ReserveStart { command },
            Event::Started {
                command,
                turn: Uuid::now_v7(),
            },
            Event::HostClosed {
                outcome: Outcome::Completed,
            },
        ] {
            captured = f
                .journal
                .repair_transition(Uuid::now_v7(), captured.id, captured.revision, event)
                .await?;
        }
        let done = repair_application::apply(
            &f.journal,
            &captured,
            std::sync::Arc::new(workspace),
            source,
            parent.join(input.revision.to_string()),
            f._root.path().join("integration/downloads"),
            tokio::sync::watch::channel(false).1,
        )
        .await?;
        assert_eq!(done.phase, Phase::Done);
        checked = serde_json::from_value(f.journal.snapshot().await?["integration"].clone())?;
        assert_eq!(checked.status, Status::Checked);
        let candidate = checked.candidate.as_ref().unwrap();
        assert_eq!(candidate.commits.len(), 1);
        assert_eq!(
            git(
                &f.source,
                &["show", &format!("{}:file.txt", candidate.head)]
            ),
            content.trim()
        );
        assert!(!checked.contributions.is_empty());
    }
    f.service.shutdown().await;
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn selected_edit_rejects_stale_forged_missing_lines_and_secrets_before_dispatch() -> Result<()>
{
    use intercat_daemon::repair_run::Purpose;
    let f = Fixture::new(1).await?;
    let checked = checked_for_edit(&f).await?;
    f.journal
        .set_worker_config(
            Uuid::now_v7(),
            intercat_daemon::runtime::Config {
                source: f._root.path().join("missing"),
                binary: f._root.path().join("missing"),
                credentials: f._root.path().join("missing-auth"),
                path: "/usr/bin:/bin".into(),
            },
        )
        .await?;
    let runtime = intercat_daemon::repair_service::Runtime::start(
        intercat_daemon::host_memory::Pool::new(),
        f.journal.clone(),
        f._root.path().join("integration"),
        f._root.path().join("data"),
        f._root.path().join("rpc"),
        std::sync::Arc::new(intercat_daemon::scheduler::Capacity::new(0)),
        f.service.clone(),
        f.project_secrets.clone(),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    let mut stale = selected_edit(&checked, "Change this line.");
    stale.revision += 1;
    assert!(runtime.service.start(stale).await.is_err());
    for field in ["tree", "object", "path", "lines"] {
        let mut forged = selected_edit(&checked, "Change this line.");
        if let Purpose::Edit { selection, .. } = &mut forged.purpose {
            match field {
                "tree" => selection.tree = "0".repeat(40),
                "object" => selection.object = "0".repeat(40),
                "path" => selection.path = "../outside".into(),
                _ => selection.end_line = 999,
            }
        }
        assert!(runtime.service.start(forged).await.is_err(), "{field}");
    }
    let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    assert!(
        runtime
            .service
            .start(selected_edit(&checked, &format!("Use {token}")))
            .await
            .is_err()
    );
    assert!(
        f.journal.snapshot().await?["repairRuns"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    for table in ["events", "operations"] {
        assert_eq!(
            db.query_row::<i64, _, _>(
                &format!("SELECT count(*) FROM {table} WHERE instr(payload,?1)>0"),
                [token],
                |r| r.get(0)
            )?,
            0
        );
    }
    runtime.shutdown().await;
    f.service.shutdown().await;
    f.source_unchanged()?;
    Ok(())
}

fn commit_action(job: &Job) -> Action {
    let candidate = job.candidate.as_ref().unwrap();
    Action::Commit {
        id: job.id,
        revision: job.revision,
        head: candidate.head.clone(),
        tree: candidate.tree.clone(),
        settings_hash: candidate.settings_hash.clone(),
    }
}

#[tokio::test]
async fn keep_secret_resumes_the_exact_candidate_without_rerunning_hooks() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(1).await?;
    let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    let second = concat!("ghp_", "B7m8Q1nS6y3U0w9C4p2R5t8X1d6Z3v9K2f4J");
    let hook = f.source.join(".git/hooks/pre-commit");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nprintf 'token = {token}\\nother = {second}\\n' > fixture.txt\n/usr/bin/uuidgen > build-id.txt\ngit add fixture.txt build-id.txt\n"
        ),
    )?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
    fs::write(f.workspaces[0].path.join("file.txt"), "A\n")?;
    f.save(0, None).await?;
    let opened = f.open().await?;
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let mut blocked = finished(&f.journal).await?;
    assert_eq!(
        blocked.candidate.as_ref().unwrap().state,
        intercat_daemon::candidate::State::SecretFound
    );
    let pending = blocked
        .candidate
        .as_ref()
        .unwrap()
        .blocked_commit
        .clone()
        .unwrap();
    let diff = f
        .service
        .diff(integration::DiffRequest {
            id: blocked.id,
            revision: blocked.revision,
            track: pending.track.clone(),
            path: "fixture.txt".into(),
        })
        .await?;
    assert!(diff.redacted);
    assert!(!diff.after.contains(token));
    assert!(!diff.after.contains(second));
    assert!(
        f.service
            .diff(integration::DiffRequest {
                id: blocked.id,
                revision: blocked.revision,
                track: pending.track.clone(),
                path: "../outside".into()
            })
            .await
            .is_err()
    );
    let findings = blocked
        .candidate
        .as_ref()
        .unwrap()
        .secret_scans
        .last()
        .unwrap()
        .findings
        .clone();
    assert!(findings.len() >= 2);
    for finding in findings {
        blocked = f
            .action(Action::KeepSecret {
                id: blocked.id,
                revision: blocked.revision,
                finding: finding.id,
            })
            .await?;
    }
    let resumed = finished(&f.journal).await?;
    assert_eq!(resumed.status, Status::Checked, "{:?}", resumed.error);
    let candidate = resumed.candidate.as_ref().unwrap();
    assert_eq!(candidate.commits[0].sha, pending.sha);
    assert_eq!(candidate.tree, pending.tree);
    assert_eq!(candidate.generation, opened.revision);
    assert!(
        f.action(Action::KeepSecret {
            id: blocked.id,
            revision: opened.revision,
            finding: "old".into()
        })
        .await
        .is_err()
    );
    f.action(commit_action(&resumed)).await?;
    assert_eq!(finished(&f.journal).await?.status, Status::Done);
    assert!(
        f.journal.snapshot().await?["editorEntries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["scene"] == "commit"
                && entry["line"] == "Committed. Push when you like.")
    );

    assert!(
        git(
            &f.source,
            &["show", &format!("{}:fixture.txt", candidate.head)]
        )
        .contains(token)
    );
    assert!(!serde_json::to_string(&f.journal.snapshot().await?)?.contains(token));
    assert!(f.open().await?.kept_secrets.is_empty());
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn keeping_a_hook_message_preserves_the_original_commit_and_hides_the_value() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(1).await?;
    let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    let hook = f.source.join(".git/hooks/commit-msg");
    fs::write(
        &hook,
        format!("#!/bin/sh\nprintf '\\n{token}\\n' >> \"$1\"\n"),
    )?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
    fs::write(f.workspaces[0].path.join("file.txt"), "A\n")?;
    f.save(0, None).await?;
    let opened = f.open().await?;
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let blocked = finished(&f.journal).await?;
    let candidate = blocked.candidate.as_ref().unwrap();
    let sha = candidate.blocked_commit.as_ref().unwrap().sha.clone();
    assert!(
        !candidate
            .blocked_commit
            .as_ref()
            .unwrap()
            .message
            .contains(token)
    );
    let finding = &candidate.secret_scans.last().unwrap().findings[0];
    f.service.shutdown().await;
    let accepted = f
        .journal
        .integration_command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::KeepSecret {
                id: blocked.id,
                revision: blocked.revision,
                finding: finding.id.clone(),
            },
        })
        .await?;
    let reopened = Journal::open(
        f._root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let service = Service::new(reopened.clone(), f._root.path().join("integration"));
    service
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Prepare {
                id: accepted.id,
                revision: accepted.revision,
            },
        })
        .await?;
    let resumed = finished(&reopened).await?;
    assert_eq!(resumed.candidate.as_ref().unwrap().head, sha);
    service
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: commit_action(&resumed),
        })
        .await?;
    assert_eq!(finished(&reopened).await?.status, Status::Done);
    assert!(!serde_json::to_string(&reopened.snapshot().await?)?.contains(token));
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn checked_out_session_branch_is_not_moved_under_another_worktree() -> Result<()> {
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "first\n")?;
    let first = f.save(0, None).await?;
    let job = checked(&f).await?;
    f.action(commit_action(&job)).await?;
    let published = finished(&f.journal).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "second\n")?;
    f.save(0, Some(first)).await?;
    let next = checked(&f).await?;
    let external = f._root.path().join("human-checkout");
    git(
        &f.source,
        &[
            "worktree",
            "add",
            external.to_str().unwrap(),
            &format!("intercat/{}", job.slug),
        ],
    );
    fs::write(external.join("file.txt"), "human unsaved\n")?;
    f.action(commit_action(&next)).await?;
    assert_eq!(finished(&f.journal).await?.status, Status::Failed);
    assert_eq!(
        git(&external, &["rev-parse", "HEAD"]),
        published.candidate.unwrap().head
    );
    assert_eq!(
        fs::read_to_string(external.join("file.txt"))?,
        "human unsaved\n"
    );
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unchanged_saved_tracks_advance_their_boundary_without_inventing_commits() -> Result<()> {
    let f = Fixture::new(1).await?;
    let checkpoint = f.save(0, None).await?;
    let job = checked(&f).await?;
    assert!(job.candidate.as_ref().unwrap().commits.is_empty());
    f.action(commit_action(&job)).await?;
    assert_eq!(finished(&f.journal).await?.status, Status::Done);
    let repo = Repository::open(&f.source)?;
    assert_eq!(
        repo.reference_target(&job.plan.as_ref().unwrap().rows[0].base_ref)?,
        Some(checkpoint.sha.parse()?)
    );
    assert!(
        repo.reference_target(&format!("refs/intercat/{}/integration/A/1", job.slug))?
            .is_none()
    );
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_lfs_pointer_cannot_be_published_without_its_validated_object() -> Result<()> {
    let f = Fixture::new(1).await?;
    fs::write(
        f.workspaces[0].path.join("asset.dat"),
        "version https://git-lfs.github.com/spec/v1\noid sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\nsize 123\n",
    )?;
    f.save(0, None).await?;
    let job = checked(&f).await?;
    f.action(commit_action(&job)).await?;
    let failed = finished(&f.journal).await?;
    assert_eq!(failed.status, Status::Failed);
    assert!(failed.error.unwrap().contains("LFS"));
    assert!(
        Repository::open(&f.source)?
            .reference_target(&job.plan.as_ref().unwrap().target_ref)?
            .is_none()
    );
    f.service.shutdown().await;
    Ok(())
}
async fn checked(f: &Fixture) -> Result<Job> {
    let job = f.open().await?;
    f.action(Action::Prepare {
        id: job.id,
        revision: job.revision,
    })
    .await?;
    let built = finished(&f.journal).await?;
    assert_eq!(built.status, Status::Checked, "{:?}", built.error);
    Ok(built)
}

async fn failed_for_refresh(f: &Fixture) -> Result<Job> {
    fs::write(
        f.workspaces[0].path.join("package.json"),
        r#"{"scripts":{"test":"node --check bad.js"}}"#,
    )?;
    fs::write(f.workspaces[0].path.join("bad.js"), "(\n")?;
    f.save(0, None).await?;
    let job = f.open().await?;
    f.action(Action::Prepare {
        id: job.id,
        revision: job.revision,
    })
    .await?;
    let failed = finished(&f.journal).await?;
    assert_eq!(
        failed.candidate.as_ref().unwrap().state,
        intercat_daemon::candidate::State::CheckFailed
    );
    Ok(failed)
}

async fn begin_test_repair(f: &Fixture, job: &Job) -> Result<intercat_daemon::repair_run::Run> {
    f.journal
        .repair_start(intercat_daemon::repair_run::Request {
            op_id: Uuid::now_v7(),
            job_id: job.id,
            revision: job.revision,
            track: job.tracks[0].id.clone(),
            purpose: intercat_daemon::repair_run::Purpose::Check,
        })
        .await
}

#[tokio::test]
async fn refresh_repair_waits_for_stop_and_atomically_replaces_the_candidate_once() -> Result<()> {
    use intercat_daemon::repair_run::{
        Control, Event, Outcome as RepairOutcome, Phase, RefreshRequest,
    };
    let f = Fixture::new(1).await?;
    let failed = failed_for_refresh(&f).await?;
    let mut run = begin_test_repair(&f, &failed).await?;
    for event in [
        Event::Prepare,
        Event::WorkspaceReady,
        Event::HostStarted {
            secret_revision: Some(0),
            host: intercat_daemon::process::Identity {
                pid: 42,
                started_seconds: 1,
                started_micros: 2,
            },
            nonce: Uuid::now_v7(),
        },
        Event::SessionReady {
            session: "refresh-session".into(),
        },
    ] {
        run = f
            .journal
            .repair_transition(Uuid::now_v7(), run.id, run.revision, event)
            .await?;
    }
    let request = RefreshRequest {
        op_id: Uuid::now_v7(),
        job_id: failed.id,
        revision: run.job_revision,
        repair_id: run.id,
    };
    let mut stale = request.clone();
    stale.revision -= 1;
    assert!(f.journal.repair_refresh(stale).await.is_err());
    let stopping = f.journal.repair_refresh(request.clone()).await?;
    assert_eq!(stopping.phase, Phase::Stopping);
    assert!(
        f.journal
            .repair_finish_refresh(request.op_id)
            .await
            .is_err()
    );
    let held = f
        .journal
        .repair_transition(
            Uuid::now_v7(),
            run.id,
            stopping.revision,
            Event::HostClosed {
                outcome: RepairOutcome::Stopped,
            },
        )
        .await?;
    assert!(
        f.journal
            .repair_control(intercat_daemon::repair_run::Command {
                op_id: Uuid::now_v7(),
                id: held.id,
                revision: held.revision,
                action: Control::Resume
            })
            .await
            .is_err()
    );
    fs::write(f.workspaces[0].path.join("bad.js"), "// fixed checkpoint\n")?;
    let fresh = f.save(0, failed.tracks[0].checkpoint.clone()).await?;
    let refreshed = f
        .journal
        .repair_finish_refresh(request.op_id)
        .await?
        .unwrap();
    assert_eq!(refreshed.revision, run.job_revision + 1);
    assert_eq!(
        refreshed.tracks[0].checkpoint.as_ref().unwrap().sha,
        fresh.sha
    );
    assert!(refreshed.candidate.is_none() && refreshed.contributions.is_empty());
    assert_eq!(
        f.journal.repair_refresh(request.clone()).await?.phase,
        Phase::Discarded
    );
    // A restart after capture resumes the same revision, even when another
    // checkpoint becomes available before planning has resumed.
    fs::write(
        f.workspaces[0].path.join("bad.js"),
        "// even newer checkpoint\n",
    )?;
    f.save(0, Some(fresh)).await?;
    let replayed = f
        .journal
        .repair_finish_refresh(request.op_id)
        .await?
        .unwrap();
    assert_eq!(replayed.revision, refreshed.revision);
    assert_eq!(
        serde_json::to_value(&replayed.tracks)?,
        serde_json::to_value(&refreshed.tracks)?
    );
    f.journal.repair_complete_refresh(request.op_id).await?;
    assert!(
        f.journal
            .repair_finish_refresh(request.op_id)
            .await?
            .is_none()
    );
    let current: Job = serde_json::from_value(f.journal.snapshot().await?["integration"].clone())?;
    assert_eq!(current.revision, refreshed.revision);
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn refresh_repair_recovery_preserves_edits_when_capture_fails() -> Result<()> {
    use intercat_daemon::repair_run::{Control, Phase, RefreshRequest};
    let f = Fixture::new(1).await?;
    let failed = failed_for_refresh(&f).await?;
    let run = begin_test_repair(&f, &failed).await?;
    let request = RefreshRequest {
        op_id: Uuid::now_v7(),
        job_id: failed.id,
        revision: run.job_revision,
        repair_id: run.id,
    };
    let held = f.journal.repair_refresh(request.clone()).await?;
    assert_eq!(held.phase, Phase::Held);
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    let original: String = db.query_row(
        "SELECT value FROM settings WHERE key='session'",
        [],
        |row| row.get(0),
    )?;
    let mut other: serde_json::Value = serde_json::from_str(&original)?;
    other["id"] = json!(Uuid::now_v7());
    db.execute(
        "UPDATE settings SET value=?1 WHERE key='session'",
        [other.to_string()],
    )?;
    assert!(
        f.journal
            .repair_finish_refresh(request.op_id)
            .await?
            .is_none()
    );
    let after = f.journal.repair_input(run.id).await?.0;
    assert_eq!(after.phase, Phase::Held);
    assert!(after.refresh.as_ref().unwrap().error.is_some());
    assert_eq!(
        f.journal
            .repair_input(run.id)
            .await?
            .1
            .candidate
            .as_ref()
            .unwrap()
            .head,
        failed.candidate.as_ref().unwrap().head
    );
    db.execute(
        "UPDATE settings SET value=?1 WHERE key='session'",
        [original],
    )?;
    let resumed = f
        .journal
        .repair_control(intercat_daemon::repair_run::Command {
            op_id: Uuid::now_v7(),
            id: run.id,
            revision: after.revision,
            action: Control::Resume,
        })
        .await?;
    assert!(resumed.refresh.is_none());
    f.service.shutdown().await;
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn refresh_repair_background_driver_finishes_without_an_open_window() -> Result<()> {
    use intercat_daemon::repair_run::RefreshRequest;
    let f = Fixture::new(1).await?;
    let failed = failed_for_refresh(&f).await?;
    let run = begin_test_repair(&f, &failed).await?;
    fs::write(f.workspaces[0].path.join("bad.js"), "// fresh checkpoint\n")?;
    f.save(0, failed.tracks[0].checkpoint.clone()).await?;
    let operation = Uuid::now_v7();
    f.journal
        .repair_refresh(RefreshRequest {
            op_id: operation,
            job_id: failed.id,
            revision: run.job_revision,
            repair_id: run.id,
        })
        .await?;
    // Simulate interruption after the new checkpoint was captured but before
    // the integration service planned or started its Check.
    let captured = f.journal.repair_finish_refresh(operation).await?.unwrap();
    assert_eq!(captured.status, Status::Preparing);
    let snapshot = f.journal.snapshot().await?;
    assert_eq!(snapshot["repairRuns"][0]["phase"], "discarded");
    assert_eq!(
        snapshot["repairRuns"][0]["refresh"]["operation"],
        operation.to_string()
    );
    f.service.shutdown().await;
    let reopened = Journal::open(
        f._root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let integration = Service::new(reopened.clone(), f._root.path().join("integration"));
    let runtime = intercat_daemon::repair_service::Runtime::start(
        intercat_daemon::host_memory::Pool::new(),
        reopened.clone(),
        f._root.path().join("integration"),
        f._root.path().join("data"),
        f._root.path().join("rpc"),
        std::sync::Arc::new(intercat_daemon::scheduler::Capacity::new(4)),
        integration.clone(),
        empty_project_secrets(&reopened).await?,
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let job: Job =
                serde_json::from_value(reopened.snapshot().await?["integration"].clone())?;
            if job.revision > run.job_revision && job.status == Status::Checked {
                break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    runtime.shutdown().await;
    integration.shutdown().await;
    assert_eq!(
        result??
            .candidate
            .as_ref()
            .unwrap()
            .check
            .as_ref()
            .unwrap()
            .code,
        Some(0)
    );
    assert!(reopened.snapshot().await?["repairRuns"][0]["refresh"].is_null());
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn interrupted_refresh_does_not_replace_a_newer_review() -> Result<()> {
    use intercat_daemon::repair_run::RefreshRequest;
    let f = Fixture::new(1).await?;
    let failed = failed_for_refresh(&f).await?;
    let run = begin_test_repair(&f, &failed).await?;
    let operation = Uuid::now_v7();
    f.journal
        .repair_refresh(RefreshRequest {
            op_id: operation,
            job_id: failed.id,
            revision: run.job_revision,
            repair_id: run.id,
        })
        .await?;
    let captured = f.journal.repair_finish_refresh(operation).await?.unwrap();
    let newer = f
        .journal
        .integration_command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Refresh {
                id: captured.id,
                revision: captured.revision,
            },
        })
        .await?;
    assert!(f.journal.repair_finish_refresh(operation).await?.is_none());
    let current: Job = serde_json::from_value(f.journal.snapshot().await?["integration"].clone())?;
    assert_eq!(current.revision, newer.revision);
    assert!(f.journal.snapshot().await?["repairRuns"][0]["refresh"].is_null());
    f.service.shutdown().await;
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires the pinned Muse binary and saved authentication; no model turn"]
async fn real_refresh_stops_the_prepared_repair_before_rebuilding() -> Result<()> {
    use intercat_daemon::{
        host,
        repair_run::{Phase, RefreshRequest},
        repair_service,
        runtime::Config,
    };
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(1).await?;
    let failed = failed_for_refresh(&f).await?;
    let binary = host::prepare_binary(
        &PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        &f._root.path().join("muse"),
    )?;
    f.journal
        .set_worker_config(
            Uuid::now_v7(),
            Config {
                source: binary.clone(),
                binary,
                credentials: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?),
                path: std::env::var("PATH")?,
            },
        )
        .await?;
    let rpc = f._root.path().join("rpc");
    fs::create_dir(&rpc)?;
    fs::set_permissions(&rpc, fs::Permissions::from_mode(0o700))?;
    let runtime = repair_service::Runtime::start(
        intercat_daemon::host_memory::Pool::new(),
        f.journal.clone(),
        f._root.path().join("integration"),
        f._root.path().join("data"),
        rpc,
        std::sync::Arc::new(intercat_daemon::scheduler::Capacity::new(0)),
        f.service.clone(),
        f.project_secrets.clone(),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    let run = begin_test_repair(&f, &failed).await?;
    let result = async {
        let ready = tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let run = f.journal.repair_input(run.id).await?.0;
                if run.session_ready {
                    break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(run);
                }
                if run.phase == Phase::Failed {
                    return Err("Repair host preparation failed.".into());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        if ready.start_command.is_some() || ready.turn.is_some() {
            return Err("The capacity gate dispatched a model turn.".into());
        }
        let identity = ready.host.ok_or("Missing prepared repair host.")?;
        fs::write(f.workspaces[0].path.join("bad.js"), "// fresh checkpoint\n")?;
        f.save(0, failed.tracks[0].checkpoint.clone()).await?;
        runtime
            .service
            .refresh(RefreshRequest {
                op_id: Uuid::now_v7(),
                job_id: failed.id,
                revision: ready.job_revision,
                repair_id: ready.id,
            })
            .await?;
        let refreshed = tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let job: Job =
                    serde_json::from_value(f.journal.snapshot().await?["integration"].clone())?;
                if job.revision > ready.job_revision && job.status == Status::Checked {
                    break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        if intercat_daemon::process::is_alive(identity) {
            return Err("The refreshed repair host survived.".into());
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(refreshed)
    }
    .await;
    runtime.shutdown().await;
    f.service.shutdown().await;
    let output = PathBuf::from(std::env::var("INTERCAT_REPAIR_OUTPUT")?);
    fs::create_dir_all(&output)?;
    let database = output.join("refresh-host.sqlite");
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    db.execute("VACUUM INTO ?1", [database.to_string_lossy().as_ref()])?;
    let count: i64 = db.query_row(
        "SELECT count(*) FROM operations WHERE kind='turn.start'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(count, 0);
    let refreshed = result?;
    fs::write(
        output.join("refresh-host.json"),
        serde_json::to_vec_pretty(
            &json!({"job":refreshed,"modelTurns":count,"sourceUnchanged":true,"snapshot":f.journal.snapshot().await?}),
        )?,
    )?;
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn local_dependency_checkpoint_passes_check_and_commit_but_stale_locks_hold_new_work()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let work = &f.workspaces[0].path;
    fs::create_dir_all(work.join("vendor/tiny-dep"))?;
    fs::write(
        work.join("vendor/tiny-dep/package.json"),
        json!({"name":"tiny-dep","version":"1.0.0","main":"index.js"}).to_string(),
    )?;
    fs::write(
        work.join("vendor/tiny-dep/index.js"),
        "module.exports = 42;\n",
    )?;
    fs::write(work.join(".gitignore"), "node_modules/\n")?;
    let manifest = json!({"name":"local-fixture","version":"1.0.0","scripts":{"test":"node --test"},"dependencies":{"tiny-dep":"file:vendor/tiny-dep"}});
    let lock = json!({"name":"local-fixture","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"local-fixture","version":"1.0.0","dependencies":{"tiny-dep":"file:vendor/tiny-dep"}},"node_modules/tiny-dep":{"resolved":"vendor/tiny-dep","link":true},"vendor/tiny-dep":{"version":"1.0.0"}}});
    fs::write(work.join("package.json"), manifest.to_string())?;
    fs::write(work.join("package-lock.json"), lock.to_string())?;
    fs::write(
        work.join("login.cjs"),
        "module.exports = () => require('tiny-dep');\n",
    )?;
    fs::write(
        work.join("login.test.cjs"),
        "const test=require('node:test'); const assert=require('node:assert/strict'); test('login uses local dependency',()=>assert.equal(require('./login.cjs')(),42));\n",
    )?;
    let checkpoint = f.save(0, None).await?;
    let built = checked(&f).await?;
    let candidate = built.candidate.as_ref().unwrap();
    assert_eq!(candidate.commits.len(), 1);
    assert_eq!(candidate.check.as_ref().unwrap().code, Some(0));
    assert!(candidate.dependency_runs.len() >= 2);
    assert!(candidate.dependency_runs.iter().all(|r| r.downloads == 0));
    f.action(commit_action(&built)).await?;
    let done = finished(&f.journal).await?;
    assert_eq!(done.status, Status::Done);
    let repo = Repository::open(&f.source)?;
    let head = repo
        .reference_target(&built.plan.as_ref().unwrap().target_ref)?
        .unwrap();
    assert_eq!(head.to_string(), candidate.head);
    let files = repo.saved_files(head)?;
    assert_eq!(
        repo.saved_bytes(&files["vendor/tiny-dep/index.js"])?,
        b"module.exports = 42;\n"
    );
    assert!(files.keys().all(|p| !p.starts_with("node_modules/")));
    let mut changed = manifest.clone();
    changed["dependencies"]["tiny-dep"] = json!("file:vendor/missing");
    fs::write(work.join("package.json"), changed.to_string())?;
    let next = f.save(0, Some(checkpoint)).await?;
    let opened = f.open().await?;
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let manifest_failure = finished(&f.journal).await?;
    assert_eq!(
        manifest_failure.candidate.as_ref().unwrap().state,
        intercat_daemon::candidate::State::EnvFailed
    );
    assert!(manifest_failure.candidate.as_ref().unwrap().check.is_none());
    fs::write(work.join("package.json"), manifest.to_string())?;
    let mut changed = lock;
    changed["packages"][""]["dependencies"]["tiny-dep"] = json!("file:vendor/missing");
    fs::write(work.join("package-lock.json"), changed.to_string())?;
    f.save(0, Some(next)).await?;
    let refreshed = f
        .action(Action::Refresh {
            id: manifest_failure.id,
            revision: manifest_failure.revision,
        })
        .await?;
    f.action(Action::Prepare {
        id: refreshed.id,
        revision: refreshed.revision,
    })
    .await?;
    let lock_failure = finished(&f.journal).await?;
    assert_eq!(
        lock_failure.candidate.as_ref().unwrap().state,
        intercat_daemon::candidate::State::EnvFailed
    );
    assert!(lock_failure.candidate.as_ref().unwrap().check.is_none());
    assert_eq!(
        repo.reference_target(&built.plan.as_ref().unwrap().target_ref)?,
        Some(head)
    );
    f.source_unchanged()?;
    if let Ok(output) = std::env::var("INTERCAT_LOCAL_DEPENDENCIES_OUTPUT") {
        let output = PathBuf::from(output);
        fs::create_dir_all(&output)?;
        let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
        db.execute(
            "VACUUM INTO ?1",
            [output.join("events.sqlite").to_str().unwrap()],
        )?;
        fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(
                &json!({"built":built,"done":done,"manifestFailure":manifest_failure,"lockFailure":lock_failure,"publishedHead":head.to_string(),"sourceUnchanged":true,"modelTurns":0}),
            )?,
        )?;
    }
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn publishes_reviewed_commits_and_advances_only_the_integrated_tracks() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(2).await?;
    let hook = f.source.join(".git/hooks/pre-commit");
    fs::write(
        &hook,
        "#!/bin/sh\n/usr/bin/uuidgen > build-id.txt\ngit add build-id.txt\n",
    )?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
    fs::write(f.workspaces[0].path.join("file.txt"), "A one\n")?;
    fs::write(f.workspaces[1].path.join("other.txt"), "B one\n")?;
    let a = f.save(0, None).await?;
    let b = f.save(1, None).await?;
    let built = checked(&f).await?;
    let candidate = built.candidate.clone().unwrap();
    let command = IntegrationCommand {
        op_id: Uuid::now_v7(),
        action: commit_action(&built),
    };
    assert_eq!(
        f.service.command(command.clone()).await?.status,
        Status::Committing
    );
    let done = finished(&f.journal).await?;
    assert_eq!(done.status, Status::Done, "{:?}", done.error);
    let repo = Repository::open(&f.source)?;
    assert_eq!(
        repo.reference_target(&built.plan.as_ref().unwrap().target_ref)?,
        Some(candidate.head.parse()?)
    );
    for (track, checkpoint) in [(&built.tracks[0], &a), (&built.tracks[1], &b)] {
        for kind in ["base", "integrated"] {
            assert_eq!(
                repo.reference_target(&format!(
                    "refs/intercat/{}/{kind}/{}",
                    built.slug, track.label
                ))?,
                Some(checkpoint.sha.parse()?)
            );
        }
        let sha = &candidate
            .commits
            .iter()
            .find(|commit| commit.track == track.id)
            .unwrap()
            .sha;
        assert_eq!(
            repo.reference_target(&format!(
                "refs/intercat/{}/integration/{}/1",
                built.slug, track.label
            ))?,
            Some(sha.parse()?)
        );
    }
    assert_eq!(f.service.command(command).await?.status, Status::Done);
    fs::write(f.workspaces[0].path.join("file.txt"), "A two\n")?;
    f.save(0, Some(a)).await?;
    let next = checked(&f).await?;
    assert_ne!(next.id, built.id);
    assert_eq!(next.tracks[0].integration_count, 1);
    assert_eq!(next.candidate.as_ref().unwrap().commits.len(), 1);
    assert_eq!(
        git(
            &f.source,
            &[
                "show",
                &format!("{}:other.txt", next.candidate.as_ref().unwrap().head)
            ]
        ),
        "B one"
    );
    f.action(commit_action(&next)).await?;
    assert_eq!(finished(&f.journal).await?.status, Status::Done);
    assert!(
        repo.reference_target(&format!("refs/intercat/{}/integration/A/2", built.slug))?
            .is_some()
    );
    assert!(
        repo.reference_target(&format!("refs/intercat/{}/integration/B/2", built.slug))?
            .is_none()
    );
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn message_edits_rebuild_the_frozen_candidate_and_invalidate_old_reviews() -> Result<()> {
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "saved\n")?;
    f.save(0, None).await?;
    let first = checked(&f).await?;
    fs::write(
        f.workspaces[0].path.join("file.txt"),
        "not in this Commit\n",
    )?;
    let editing = f
        .action(Action::EditMessage {
            id: first.id,
            revision: first.revision,
            track: first.tracks[0].id.clone(),
            message: "Reviewed subject\n\nReviewed body.".into(),
        })
        .await?;
    assert_eq!(editing.revision, first.revision + 1);
    let rebuilt = finished(&f.journal).await?;
    let candidate = rebuilt.candidate.as_ref().unwrap();
    assert_eq!(candidate.tree, first.candidate.as_ref().unwrap().tree);
    assert_ne!(candidate.head, first.candidate.as_ref().unwrap().head);
    assert!(
        candidate.commits[0]
            .message
            .starts_with("Reviewed subject\n\nReviewed body.")
    );
    assert!(candidate.commits[0].message.contains("Intercat:"));
    assert!(f.action(commit_action(&first)).await.is_err());
    let mut wrong = commit_action(&rebuilt);
    if let Action::Commit { tree, .. } = &mut wrong {
        *tree = f.head.clone();
    }
    assert!(f.action(wrong).await.is_err());
    f.action(commit_action(&rebuilt)).await?;
    assert_eq!(finished(&f.journal).await?.status, Status::Done);
    assert_eq!(
        git(
            &f.source,
            &["show", &format!("{}:file.txt", candidate.head)]
        ),
        "saved"
    );
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn secret_messages_and_unchecked_candidates_cannot_be_published() -> Result<()> {
    let f = Fixture::new(1).await?;
    let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    fs::write(f.workspaces[0].path.join("file.txt"), "saved\n")?;
    f.save(0, None).await?;
    let job = checked(&f).await?;
    assert!(
        f.action(Action::EditMessage {
            id: job.id,
            revision: job.revision,
            track: job.tracks[0].id.clone(),
            message: token.into()
        })
        .await
        .is_err()
    );
    assert!(!serde_json::to_string(&f.journal.snapshot().await?)?.contains(token));
    let mut invalid = job.clone();
    invalid.candidate.as_mut().unwrap().secret_scans.clear();
    assert!(intercat_daemon::publication::intent(&invalid, Uuid::now_v7()).is_err());
    let mut obsolete = job.clone();
    obsolete.candidate.as_mut().unwrap().secret_scans[0]
        .binding
        .ruleset = "obsolete rules".into();
    assert!(intercat_daemon::publication::intent(&obsolete, Uuid::now_v7()).is_err());
    let mut request = commit_action(&job);
    if let Action::Commit { settings_hash, .. } = &mut request {
        settings_hash.push('0');
    }
    assert!(f.action(request).await.is_err());
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn obsolete_secret_rules_require_refresh_before_commit() -> Result<()> {
    use sha2::{Digest, Sha256};
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "saved\n")?;
    f.save(0, None).await?;
    let mut job = checked(&f).await?;
    let settings = f
        .journal
        .integration_settings(job.id, job.revision)
        .await?
        .unwrap();
    // Reproduce the old Check signature, before it included the rule policy.
    let old_hash = Sha256::digest(serde_json::to_vec(&settings)?)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert_ne!(settings.hash()?, old_hash);
    let candidate = job.candidate.as_mut().unwrap();
    candidate.settings_hash = old_hash.clone();
    candidate.check.as_mut().unwrap().settings_hash = old_hash;
    for scan in &mut candidate.secret_scans {
        scan.binding.ruleset = "previous policy".into();
    }
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    db.execute(
        "UPDATE integration_jobs SET record=?2 WHERE id=?1",
        rusqlite::params![job.id.to_string(), serde_json::to_string(&job)?],
    )?;
    let error = f.action(commit_action(&job)).await.unwrap_err().to_string();
    assert!(error.contains("Secret detection rules changed"), "{error}");
    assert_eq!(
        Repository::open(&f.source)?
            .reference_target(&job.plan.as_ref().unwrap().target_ref)?
            .map(|id| id.to_string()),
        job.plan.as_ref().unwrap().expected_target
    );
    let refreshed = f
        .action(Action::Refresh {
            id: job.id,
            revision: job.revision,
        })
        .await?;
    f.action(Action::Prepare {
        id: refreshed.id,
        revision: refreshed.revision,
    })
    .await?;
    let rebuilt = finished(&f.journal).await?;
    assert_eq!(rebuilt.status, Status::Checked, "{:?}", rebuilt.error);
    assert!(
        rebuilt
            .candidate
            .as_ref()
            .unwrap()
            .secret_scans
            .iter()
            .all(|scan| scan.binding.ruleset == intercat_daemon::secret_scan::ruleset())
    );
    f.action(commit_action(&rebuilt)).await?;
    assert_eq!(finished(&f.journal).await?.status, Status::Done);
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn foreign_branch_change_prevents_publication_without_advancing_track_markers() -> Result<()>
{
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "saved\n")?;
    f.save(0, None).await?;
    let job = checked(&f).await?;
    let repo = Repository::open(&f.source)?;
    let plan = job.plan.as_ref().unwrap();
    let foreign =
        repo.checkpoint_commit(repo.tree_id(f.head.parse()?)?, f.head.parse()?, "foreign")?;
    repo.sync_objects(&[foreign])?;
    repo.compare_and_swap(&plan.target_ref, None, foreign)?;
    f.action(commit_action(&job)).await?;
    let failed = finished(&f.journal).await?;
    assert_eq!(failed.status, Status::Failed, "{:?}", failed.error);
    assert_eq!(failed.revision, job.revision + 1);
    assert!(failed.publication.is_none());
    assert_eq!(repo.reference_target(&plan.target_ref)?, Some(foreign));
    assert!(repo.reference_target(&plan.rows[0].base_ref)?.is_none());
    assert!(
        repo.reference_target(&plan.rows[0].integrated_ref)?
            .is_none()
    );
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn two_clients_cannot_refresh_while_a_saved_commit_waits_to_publish() -> Result<()> {
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "saved\n")?;
    f.save(0, None).await?;
    let checked = checked(&f).await?;
    f.service.shutdown().await;
    // Hold publication at the real durable intent/ref boundary for five seconds.
    let pending = f
        .journal
        .integration_command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: commit_action(&checked),
        })
        .await?;
    let other = Service::new(f.journal.clone(), f._root.path().join("second-client"));
    for _ in 0..2 {
        for client in [&f.service, &other] {
            assert!(
                client
                    .command(IntegrationCommand {
                        op_id: Uuid::now_v7(),
                        action: Action::Refresh {
                            id: pending.id,
                            revision: pending.revision
                        },
                    })
                    .await
                    .is_err()
            );
            assert_eq!(
                f.journal.snapshot().await?["integration"]["status"],
                "committing"
            );
        }
        tokio::time::sleep(Duration::from_millis(2500)).await;
    }
    other.recover().await?;
    let done = finished(&f.journal).await?;
    assert_eq!(done.status, Status::Done);
    assert_eq!(
        done.candidate.unwrap().head,
        checked.candidate.unwrap().head
    );
    f.source_unchanged()?;
    other.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn recovery_completes_every_durable_ref_prefix_without_rebuilding_commits() -> Result<()> {
    // branch, 4 base/integrated refs, 2 history refs, operation ref = 8 writes.
    for prefix in 0..=8 {
        let f = Fixture::new(2).await?;
        fs::write(f.workspaces[0].path.join("file.txt"), "A\n")?;
        fs::write(f.workspaces[1].path.join("other.txt"), "B\n")?;
        f.save(0, None).await?;
        f.save(1, None).await?;
        let job = checked(&f).await?;
        f.service.shutdown().await;
        let pending = f
            .journal
            .integration_command(IntegrationCommand {
                op_id: Uuid::now_v7(),
                action: commit_action(&job),
            })
            .await?;
        let publication = pending.publication.as_ref().unwrap();
        assert_eq!(publication.refs.len(), 8);
        let repo = Repository::open(&f.source)?;
        for change in publication.refs.iter().take(prefix) {
            repo.compare_and_swap(
                &change.name,
                change.old.as_ref().map(|id| id.parse()).transpose()?,
                change.new.parse()?,
            )?;
            repo.sync_ref(&change.name)?;
        }
        assert!(
            f.action(Action::Discard {
                id: job.id,
                revision: job.revision
            })
            .await
            .is_err()
        );
        let reopened = Journal::open(
            f._root.path().join("events.sqlite"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        let service = Service::new(reopened.clone(), f._root.path().join("resume"));
        service.recover().await?;
        let done: Job = serde_json::from_value(reopened.snapshot().await?["integration"].clone())?;
        assert_eq!(
            done.status,
            Status::Done,
            "prefix {prefix}: {:?}",
            done.error
        );
        assert_eq!(done.candidate.unwrap().head, job.candidate.unwrap().head);
        for change in &publication.refs {
            assert_eq!(
                repo.reference_target(&change.name)?,
                Some(change.new.parse()?)
            );
        }
        service.recover().await?;
        f.source_unchanged()?;
        service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn recovery_keeps_descendant_branches_and_waits_for_foreign_track_refs() -> Result<()> {
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "A\n")?;
    f.save(0, None).await?;
    let job = checked(&f).await?;
    f.service.shutdown().await;
    let pending = f
        .journal
        .integration_command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: commit_action(&job),
        })
        .await?;
    let publication = pending.publication.as_ref().unwrap();
    let repo = Repository::open(&f.source)?;
    let head: ObjectId = job.candidate.as_ref().unwrap().head.parse()?;
    let descendant = repo.checkpoint_commit(repo.tree_id(head)?, head, "external followup")?;
    repo.sync_objects(&[descendant])?;
    repo.compare_and_swap(&publication.refs[0].name, None, descendant)?;
    let blocked_ref = &publication.refs[1];
    repo.compare_and_swap(&blocked_ref.name, None, descendant)?;
    let reopened = Journal::open(
        f._root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let service = Service::new(reopened.clone(), f._root.path().join("resume"));
    service.recover().await?;
    let blocked: Job = serde_json::from_value(reopened.snapshot().await?["integration"].clone())?;
    assert_eq!(blocked.status, Status::CommitBlocked);
    assert!(
        service
            .command(IntegrationCommand {
                op_id: Uuid::now_v7(),
                action: Action::Refresh {
                    id: job.id,
                    revision: job.revision
                }
            })
            .await
            .is_err()
    );
    assert_eq!(
        repo.reference_target(&publication.refs[0].name)?,
        Some(descendant)
    );
    repo.delete_ref(&blocked_ref.name, descendant)?;
    service
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::RetryCommit {
                id: job.id,
                revision: job.revision,
            },
        })
        .await?;
    let done = finished(&reopened).await?;
    assert_eq!(done.status, Status::Done, "{:?}", done.error);
    assert_eq!(
        repo.reference_target(&publication.refs[0].name)?,
        Some(descendant)
    );
    service.shutdown().await;
    f.source_unchanged()?;
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}
async fn empty_project_secrets(
    journal: &Journal,
) -> Result<intercat_daemon::project_secrets::Store> {
    use intercat_daemon::{
        project_secrets::{Load, Prepare},
        secrets::Key,
    };
    let id = journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let catalog = journal
        .prepare_project_secrets(Prepare {
            op_id: Uuid::now_v7(),
            project_id: id.clone(),
            revision: 0,
            fingerprints: vec![],
            key_check: Key::new([7; 32])
                .check()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    let store = intercat_daemon::project_secrets::Store::default();
    store.publish(
        journal
            .activate_project_secrets(Load {
                project_id: id,
                revision: 0,
                entries: vec![],
            })
            .await?,
    );
    Ok(store)
}
struct Fixture {
    _root: tempfile::TempDir,
    source: PathBuf,
    journal: Journal,
    service: Service,
    project_secrets: intercat_daemon::project_secrets::Store,
    workspaces: Vec<Workspace>,
    index: Vec<u8>,
    head: String,
}
impl Fixture {
    async fn new(count: usize) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        fs::create_dir(&source)?;
        git(&source, &["init", "-q", "-b", "main"]);
        fs::write(source.join("file.txt"), "base\n")?;
        fs::write(source.join("other.txt"), "other\n")?;
        git(&source, &["add", "."]);
        git(
            &source,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "base",
            ],
        );
        let head = git(&source, &["rev-parse", "HEAD"]);
        let index = fs::read(source.join(".git/index"))?;
        let journal = Journal::open(
            root.path().join("events.sqlite"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        journal
            .configure_project(
                Uuid::now_v7(),
                project::inspect(
                    uuid::Uuid::now_v7(),
                    "Dummy project".into(),
                    vec![source.clone()],
                )
                .await?,
            )
            .await?;
        let project_secrets = empty_project_secrets(&journal).await?;
        for i in 0..count {
            journal
                .stage_utterance(
                    Uuid::now_v7(),
                    format!("Track {i}"),
                    Target::New,
                    None,
                    Vec::new(),
                )
                .await?;
            let input = journal.claim_input().await?.unwrap();
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
                .await?;
        }
        let preparation = Preparation::start(&root.path().join("workspaces"), journal.clone())?;
        let workspaces = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let snapshot = journal.snapshot().await?;
                let workspaces: Vec<Workspace> =
                    serde_json::from_value(snapshot["workspaces"].clone())?;
                assert!(
                    !workspaces.iter().any(|w| w.state == "failed"),
                    "{snapshot}"
                );
                if workspaces.len() == count && workspaces.iter().all(|w| w.state == "ready") {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(workspaces);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        preparation.shutdown().await;
        let service = Service::new(journal.clone(), root.path().join("integration"));
        Ok(Self {
            _root: root,
            source,
            journal,
            service,
            project_secrets,
            workspaces,
            index,
            head,
        })
    }
    async fn save(&self, track: usize, previous: Option<Checkpoint>) -> Result<Checkpoint> {
        checkpoint::save(
            self.workspaces[track].clone(),
            Uuid::now_v7(),
            previous,
            false,
            self.journal.clone(),
        )
        .await
    }
    async fn action(&self, action: Action) -> Result<Job> {
        let job = self
            .service
            .command(IntegrationCommand {
                op_id: Uuid::now_v7(),
                action,
            })
            .await?;
        // These existing merge tests choose the displayed order. Dedicated
        // proposal tests below drive the unconfirmed state directly.
        if job.status == Status::Ordering {
            self.service
                .command(IntegrationCommand {
                    op_id: Uuid::now_v7(),
                    action: Action::Order {
                        id: job.id,
                        revision: job.revision,
                        tracks: job.order_review.as_ref().unwrap().order.clone(),
                    },
                })
                .await
        } else {
            Ok(job)
        }
    }
    async fn open(&self) -> Result<Job> {
        self.action(Action::Open { tracks: None }).await
    }
    fn source_unchanged(&self) -> Result<()> {
        assert_eq!(fs::read(self.source.join(".git/index"))?, self.index);
        assert_eq!(git(&self.source, &["rev-parse", "HEAD"]), self.head);
        assert_eq!(fs::read_to_string(self.source.join("file.txt"))?, "base\n");
        Ok(())
    }
}

#[tokio::test]
async fn freezes_saved_turns_across_windows_and_refresh_is_explicit() -> Result<()> {
    let f = Fixture::new(2).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "saved one\n")?;
    let first = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("file.txt"),
        "unsaved in-flight write\n",
    )?;
    let opened = f.open().await?;
    assert_eq!(opened.status, Status::Open);
    let plan = opened.plan.as_ref().unwrap();
    assert_eq!(plan.expected_target, None);
    assert_eq!(plan.rows[1].state, RowState::NoCheckpoint);
    assert_eq!(
        git(
            &f.source,
            &["show", &format!("{}:file.txt", plan.tree.as_ref().unwrap())]
        ),
        "saved one"
    );
    assert_eq!(opened.tracks[0].checkpoint.as_ref().unwrap().sha, first.sha);
    let second = f.save(0, Some(first)).await?;
    let other_window = Service::new(f.journal.clone(), f._root.path().join("integration"))
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Open {
                tracks: Some(vec![f.workspaces[0].track_id.clone()]),
            },
        })
        .await?;
    assert_eq!(other_window.id, opened.id);
    assert_eq!(other_window.revision, opened.revision);
    assert_ne!(
        other_window.tracks[0].checkpoint.as_ref().unwrap().sha,
        second.sha
    );
    assert_eq!(
        f.journal.snapshot().await?["integration"]["finishedSinceOpen"],
        1
    );
    let refreshed = f
        .action(Action::Refresh {
            id: opened.id,
            revision: opened.revision,
        })
        .await?;
    assert_eq!(refreshed.revision, opened.revision + 1);
    assert_eq!(
        refreshed.tracks[0].checkpoint.as_ref().unwrap().sha,
        second.sha
    );
    assert_eq!(
        f.journal.snapshot().await?["integration"]["finishedSinceOpen"],
        0
    );
    assert!(
        f.journal
            .integration_planned(opened.id, opened.revision, Ok(plan.clone()))
            .await
            .is_err()
    );
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn conflicts_stop_later_rows_and_excluding_them_preserves_all_worker_files() -> Result<()> {
    let f = Fixture::new(3).await?;
    for (index, contents) in ["worker A\n", "worker B\n"].into_iter().enumerate() {
        fs::write(f.workspaces[index].path.join("file.txt"), contents)?;
        f.save(index, None).await?;
    }
    fs::write(f.workspaces[2].path.join("third.txt"), "worker C\n")?;
    f.save(2, None).await?;
    let opened = f.open().await?;
    assert_eq!(opened.status, Status::MergeConflict);
    let rows = &opened.plan.as_ref().unwrap().rows;
    assert_eq!(rows[0].state, RowState::Included);
    assert_eq!(rows[1].state, RowState::Conflict);
    assert_eq!(rows[1].conflicts[0].path, "file.txt");
    assert_eq!(
        rows[1].conflicts[0].with_tracks,
        vec![f.workspaces[0].track_id.clone()]
    );
    assert_eq!(rows[2].state, RowState::Waiting);
    assert!(opened.plan.as_ref().unwrap().tree.is_none());
    let resolved = f
        .action(Action::Select {
            id: opened.id,
            revision: opened.revision,
            track: f.workspaces[1].track_id.clone(),
            included: false,
        })
        .await?;
    assert_eq!(resolved.status, Status::Open);
    assert_eq!(
        resolved.plan.as_ref().unwrap().rows[1].state,
        RowState::Excluded
    );
    let tree = resolved.plan.as_ref().unwrap().tree.as_ref().unwrap();
    assert_eq!(
        git(&f.source, &["show", &format!("{tree}:file.txt")]),
        "worker A"
    );
    assert_eq!(
        git(&f.source, &["show", &format!("{tree}:third.txt")]),
        "worker C"
    );
    assert_eq!(
        fs::read_to_string(f.workspaces[1].path.join("file.txt"))?,
        "worker B\n"
    );
    assert!(!f.workspaces[0].path.join("third.txt").exists());
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn selection_retains_frozen_refs_and_refresh_reads_the_new_boundaries() -> Result<()> {
    let f = Fixture::new(2).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "worker A\n")?;
    let first = f.save(0, None).await?;
    let opened = f.open().await?;
    let plan = opened.plan.as_ref().unwrap();
    let repo = Repository::open(&f.source)?;
    let checkpoint: ObjectId = first.sha.parse()?;
    let tree = repo.tree_id(checkpoint)?;
    let advanced = repo.checkpoint_commit(tree, f.head.parse()?, "simulated prior integration")?;
    repo.sync_objects(&[advanced])?;
    for (reference, next) in [
        (&plan.target_ref, advanced),
        (&plan.rows[0].base_ref, checkpoint),
        (&plan.rows[0].integrated_ref, checkpoint),
    ] {
        repo.compare_and_swap(reference, None, next)?;
        repo.sync_ref(reference)?;
    }
    record_simulated_integration(&f, &opened.tracks)?;
    let selected = f
        .action(Action::Select {
            id: opened.id,
            revision: opened.revision,
            track: f.workspaces[0].track_id.clone(),
            included: true,
        })
        .await?;
    assert_eq!(
        selected.plan.as_ref().unwrap().target_head,
        opened.plan.as_ref().unwrap().target_head
    );
    assert_eq!(selected.plan.as_ref().unwrap().rows[0].base, f.head);
    let refreshed = f
        .action(Action::Refresh {
            id: opened.id,
            revision: selected.revision,
        })
        .await?;
    let plan = refreshed.plan.as_ref().unwrap();
    assert_eq!(plan.expected_target, Some(advanced.to_string()));
    assert_eq!(plan.rows[0].expected_base, Some(first.sha.clone()));
    assert_eq!(plan.rows[0].expected_integrated, Some(first.sha));
    assert_eq!(plan.rows[0].state, RowState::NoChanges);
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn commands_are_idempotent_and_stale_windows_cannot_discard_or_select() -> Result<()> {
    let f = Fixture::new(1).await?;
    f.save(0, None).await?;
    let command = IntegrationCommand {
        op_id: Uuid::now_v7(),
        action: Action::Open { tracks: None },
    };
    let opened = f.service.command(command.clone()).await?;
    let replay = f.service.command(command.clone()).await?;
    assert_eq!((replay.id, replay.revision), (opened.id, opened.revision));
    assert!(
        f.service
            .command(IntegrationCommand {
                op_id: command.op_id,
                action: Action::Open {
                    tracks: Some(vec![])
                }
            })
            .await
            .is_err()
    );
    let refreshed = f
        .action(Action::Refresh {
            id: opened.id,
            revision: opened.revision,
        })
        .await?;
    assert!(
        f.action(Action::Discard {
            id: opened.id,
            revision: opened.revision
        })
        .await
        .is_err()
    );
    assert!(
        f.action(Action::Select {
            id: opened.id,
            revision: opened.revision,
            track: f.workspaces[0].track_id.clone(),
            included: false
        })
        .await
        .is_err()
    );
    let dropped = f
        .action(Action::Discard {
            id: opened.id,
            revision: refreshed.revision,
        })
        .await?;
    assert_eq!(dropped.status, Status::Discarded);
    assert!(f.journal.snapshot().await?["integration"].is_null());
    assert_ne!(f.open().await?.id, opened.id);
    assert_eq!(
        Repository::open(&f.source)?.reference_target(&opened.plan.unwrap().target_ref)?,
        None
    );
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn resumes_frozen_preparation_after_restart_without_adopting_a_new_checkpoint() -> Result<()>
{
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "old frozen\n")?;
    let first = f.save(0, None).await?;
    let command = IntegrationCommand {
        op_id: Uuid::now_v7(),
        action: Action::Open { tracks: None },
    };
    let pending = f.journal.integration_command(command.clone()).await?;
    assert_eq!(pending.status, Status::Preparing);
    let frozen = integration::freeze(&pending)?;
    let pending = f
        .journal
        .integration_frozen(pending.id, pending.revision, frozen)
        .await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "after crash\n")?;
    let second = f.save(0, Some(first.clone())).await?;
    let repo = Repository::open(&f.source)?;
    let old = pending.plan.as_ref().unwrap();
    let later = repo.checkpoint_commit(
        repo.tree_id(second.sha.parse()?)?,
        f.head.parse()?,
        "another completed integration",
    )?;
    repo.sync_objects(&[later])?;
    repo.compare_and_swap(&old.target_ref, None, later)?;
    repo.sync_ref(&old.target_ref)?;
    repo.compare_and_swap(&old.rows[0].base_ref, None, second.sha.parse()?)?;
    repo.sync_ref(&old.rows[0].base_ref)?;
    // A new connection/coordinator exercises persisted inputs, not any state
    // held by the original service. It models a crash before the merge result.
    let reopened = Journal::open(
        f._root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let resumed = Service::new(reopened, f._root.path().join("integration-reopened"))
        .command(command)
        .await?;
    assert_eq!(resumed.id, pending.id);
    assert_eq!(
        resumed.tracks[0].checkpoint.as_ref().unwrap().sha,
        first.sha
    );
    assert_eq!(resumed.plan.as_ref().unwrap().expected_target, None);
    assert_eq!(resumed.plan.as_ref().unwrap().rows[0].base, f.head);
    assert_eq!(
        git(
            &f.source,
            &[
                "show",
                &format!("{}:file.txt", resumed.plan.unwrap().tree.unwrap())
            ]
        ),
        "old frozen"
    );
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn a_second_integration_only_carries_new_work_and_keeps_other_tracks_changes() -> Result<()> {
    let f = Fixture::new(2).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "A first\n")?;
    let first_a = f.save(0, None).await?;
    fs::write(f.workspaces[1].path.join("other.txt"), "B first\n")?;
    f.save(1, None).await?;
    let first = f.open().await?;
    let plan = first.plan.as_ref().unwrap();
    let repo = Repository::open(&f.source)?;
    let committed = repo.checkpoint_commit(
        plan.tree.as_ref().unwrap().parse()?,
        f.head.parse()?,
        "simulated candidate",
    )?;
    repo.sync_objects(&[committed])?;
    repo.compare_and_swap(&plan.target_ref, None, committed)?;
    repo.sync_ref(&plan.target_ref)?;
    for (track, row) in first.tracks.iter().zip(&plan.rows) {
        for reference in [&row.base_ref, &row.integrated_ref] {
            repo.compare_and_swap(
                reference,
                None,
                track.checkpoint.as_ref().unwrap().sha.parse()?,
            )?;
            repo.sync_ref(reference)?;
        }
    }
    record_simulated_integration(&f, &first.tracks)?;
    f.action(Action::Discard {
        id: first.id,
        revision: first.revision,
    })
    .await?;
    fs::write(f.workspaces[0].path.join("new.txt"), "A second\n")?;
    f.save(0, Some(first_a)).await?;
    let next = f.open().await?;
    let plan = next.plan.as_ref().unwrap();
    assert_eq!(
        plan.rows[0]
            .changes
            .iter()
            .map(|c| c.path.as_str())
            .collect::<Vec<_>>(),
        vec!["new.txt"]
    );
    assert_eq!(plan.rows[1].state, RowState::NoChanges);
    let tree = plan.tree.as_ref().unwrap();
    assert_eq!(
        git(&f.source, &["show", &format!("{tree}:other.txt")]),
        "B first"
    );
    assert_eq!(
        git(&f.source, &["show", &format!("{tree}:new.txt")]),
        "A second"
    );
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn authenticated_rpc_clients_share_the_job_and_reject_stale_actions() -> Result<()> {
    use intercat_daemon::{
        editor,
        lock::DaemonLock,
        rpc::{Server, Token},
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixStream,
    };
    async fn call(
        stream: &mut BufReader<UnixStream>,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
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
        tokio::time::timeout(Duration::from_secs(10), stream.read_line(&mut line)).await??;
        Ok(serde_json::from_str(&line)?)
    }
    let f = Fixture::new(1).await?;
    f.save(0, None).await?;
    let directory = tempfile::Builder::new()
        .prefix("ic-int-rpc-")
        .tempdir_in("/private/tmp")?;
    let _lock = DaemonLock::acquire(directory.path())?.unwrap();
    let token = "23".repeat(32);
    let server = Server::bind(
        directory.path(),
        Token::parse(&token)?,
        f.journal.clone(),
        editor::Manager::new(directory.path(), &directory.path().join("editor.log"))?,
        &f._root.path().join("workspaces"),
    )?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let mut one = BufReader::new(UnixStream::connect(directory.path().join("rpc.sock")).await?);
    let mut two = BufReader::new(UnixStream::connect(directory.path().join("rpc.sock")).await?);
    let open = || json!({"opId":Uuid::now_v7(),"action":{"action":"open","tracks":null}});
    assert_eq!(
        call(&mut one, "integration.command", open()).await?["error"]["code"],
        -32001
    );
    // An unauthenticated request closes that connection.
    one = BufReader::new(UnixStream::connect(directory.path().join("rpc.sock")).await?);
    for stream in [&mut one, &mut two] {
        assert_eq!(
            call(stream, "rpc.authenticate", json!({"token":token})).await?["result"]["protocol"],
            1
        );
    }
    let mut locked = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    let transaction = locked.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let waiting = f.journal.try_append("flow-test", json!({}))?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let health = tokio::time::timeout(
        Duration::from_millis(250),
        call(&mut one, "system.outputFlows", json!({})),
    )
    .await??;
    assert_eq!(health["result"], json!([]));
    transaction.commit()?;
    waiting.await??;
    let (first, second) = tokio::join!(
        call(&mut one, "integration.command", open()),
        call(&mut two, "integration.command", open())
    );
    let first = first?;
    let second = second?;
    assert!(first["error"].is_null(), "{first}");
    assert_eq!(first["result"]["id"], second["result"]["id"]);
    assert_eq!(first["result"]["revision"], second["result"]["revision"]);
    let id = first["result"]["id"].clone();
    let revision = first["result"]["revision"].clone();
    let refreshed = call(
        &mut one,
        "integration.command",
        json!({"opId":Uuid::now_v7(),"action":{"action":"refresh","id":id,"revision":revision}}),
    )
    .await?;
    assert_eq!(refreshed["result"]["revision"], 2);
    let stale = call(
        &mut two,
        "integration.command",
        json!({"opId":Uuid::now_v7(),"action":{"action":"discard","id":id,"revision":revision}}),
    )
    .await?;
    assert_eq!(stale["error"]["code"], -32000);
    assert!(
        stale["error"]["message"]
            .as_str()
            .unwrap()
            .contains("review again")
    );
    let prepared = call(
        &mut one,
        "integration.command",
        json!({"opId":Uuid::now_v7(),"action":{"action":"prepare","id":id,"revision":2}}),
    )
    .await?;
    assert_eq!(prepared["result"]["status"], "building", "{prepared}");
    let built = finished(&f.journal).await?;
    assert_eq!(built.status, Status::Checked, "{:?}", built.error);
    let status = call(&mut two, "system.status", json!({})).await?;
    assert_eq!(
        status["result"]["integration"]["candidate"]["head"],
        built.candidate.as_ref().unwrap().head
    );
    let candidate = built.candidate.as_ref().unwrap();
    let publish = json!({"opId":Uuid::now_v7(),"action":{"action":"commit","id":built.id,"revision":built.revision,"head":candidate.head,"tree":candidate.tree,"settingsHash":candidate.settings_hash}});
    let accepted = call(&mut one, "integration.command", publish.clone()).await?;
    assert_eq!(accepted["result"]["status"], "committing", "{accepted}");
    assert_eq!(finished(&f.journal).await?.status, Status::Done);
    let replay = call(&mut two, "integration.command", publish).await?;
    assert_eq!(replay["result"]["status"], "done", "{replay}");
    drop((one, two));
    let _ = stop.send(());
    task.await??;
    f.source_unchanged()?;
    Ok(())
}

async fn finished(journal: &Journal) -> Result<Job> {
    // A candidate can perform multiple secret scans, each bounded at 35 s.
    // Let those operations return their own failure instead of cancelling the
    // fixture first when macOS delays a fresh scanner process at startup.
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            let job: Job =
                serde_json::from_value(journal.snapshot().await?["integration"].clone())?;
            if !matches!(
                job.status,
                Status::Building | Status::Staging | Status::Committing
            ) {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

#[tokio::test]
async fn builds_and_persists_a_candidate_without_rerunning_hooks_or_exposing_private_settings()
-> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "candidate\n")?;
    fs::write(
        f.workspaces[0].path.join("package.json"),
        r#"{"scripts":{"test":"test $(cat file.txt) = candidate && echo checked"}}"#,
    )?;
    f.save(0, None).await?;
    let hook = f.source.join(".git/hooks/pre-commit");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf x >> hook-count.txt\ngit add hook-count.txt\n",
    )?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
    git(
        &f.source,
        &[
            "config",
            "filter.private.clean",
            "do-not-expose-this-setting",
        ],
    );
    let opened = f.open().await?;
    let hash = f
        .journal
        .integration_settings(opened.id, opened.revision)
        .await?
        .unwrap()
        .hash()?;
    assert!(
        !f.journal
            .snapshot()
            .await?
            .to_string()
            .contains("do-not-expose-this-setting")
    );
    git(&f.source, &["config", "core.autocrlf", "true"]);
    let started = f
        .action(Action::Prepare {
            id: opened.id,
            revision: opened.revision,
        })
        .await?;
    assert_eq!(started.status, Status::Building);
    let built = finished(&f.journal).await?;
    assert_eq!(built.status, Status::Checked, "{:?}", built.error);
    let candidate = built.candidate.unwrap();
    assert_eq!(candidate.settings_hash, hash);
    let staging = format!("refs/intercat/{}/staging", opened.slug);
    assert_eq!(
        Repository::open(&f.source)?
            .reference_target(&staging)?
            .unwrap()
            .to_string(),
        candidate.head
    );
    git(&f.source, &["prune", "--expire", "now"]);
    assert_eq!(
        git(
            &f.source,
            &["show", &format!("{}:hook-count.txt", candidate.head)]
        ),
        "x"
    );
    assert!(candidate.check.as_ref().unwrap().stdout.contains("checked"));
    fs::write(&hook, "#!/bin/sh\nexit 42\n")?;
    let repeated = f
        .action(Action::Prepare {
            id: opened.id,
            revision: opened.revision,
        })
        .await?;
    assert_eq!(repeated.candidate.unwrap().head, candidate.head);
    f.service.shutdown().await;
    let reopened = Journal::open(
        f._root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    assert_eq!(
        reopened.snapshot().await?["integration"]["candidate"]["head"],
        candidate.head
    );
    f.action(Action::Discard {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    assert!(
        Repository::open(&f.source)?
            .reference_target(&staging)?
            .is_none()
    );
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn refresh_cancels_and_awaits_the_check_but_a_stale_request_does_not() -> Result<()> {
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "candidate\n")?;
    fs::write(f.workspaces[0].path.join(".gitignore"), ".check-pid\n")?;
    fs::write(
        f.workspaces[0].path.join("package.json"),
        r#"{"scripts":{"test":"sleep 20 & echo $! > .check-pid; wait"}}"#,
    )?;
    f.save(0, None).await?;
    let opened = f.open().await?;
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let pid_path = f
        ._root
        .path()
        .join("integration")
        .join(opened.id.to_string())
        .join(opened.revision.to_string())
        .join("repo/.check-pid");
    let pid: u32 = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(text) = fs::read_to_string(&pid_path)
                && let Ok(pid) = text.trim().parse()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let identity = intercat_daemon::process::identity(pid)?;
    assert!(
        f.action(Action::Refresh {
            id: opened.id,
            revision: opened.revision - 1
        })
        .await
        .is_err()
    );
    assert!(intercat_daemon::process::is_alive(identity));
    let refreshed = tokio::time::timeout(
        Duration::from_secs(5),
        f.action(Action::Refresh {
            id: opened.id,
            revision: opened.revision,
        }),
    )
    .await??;
    assert_eq!(refreshed.revision, opened.revision + 1);
    assert!(refreshed.candidate.is_none());
    assert!(
        Repository::open(&f.source)?
            .reference_target(&format!("refs/intercat/{}/staging", opened.slug))?
            .is_none()
    );
    assert!(!intercat_daemon::process::is_alive(identity));
    f.service.shutdown().await;
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn an_interrupted_build_waits_for_explicit_prepare_and_uses_a_new_revision() -> Result<()> {
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "candidate\n")?;
    f.save(0, None).await?;
    let opened = f.open().await?;
    f.journal
        .integration_command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Prepare {
                id: opened.id,
                revision: opened.revision,
            },
        })
        .await?;
    let reopened = Journal::open(
        f._root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let interrupted: Job =
        serde_json::from_value(reopened.snapshot().await?["integration"].clone())?;
    assert_eq!(interrupted.status, Status::Failed);
    assert!(interrupted.candidate.is_none());
    let service = Service::new(
        reopened.clone(),
        f._root.path().join("recovered-integration"),
    );
    let retry = service
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Prepare {
                id: opened.id,
                revision: opened.revision,
            },
        })
        .await?;
    assert_eq!(retry.revision, opened.revision + 1);
    let built = finished(&reopened).await?;
    assert_eq!(built.status, Status::Checked, "{:?}", built.error);
    service.shutdown().await;
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn a_changed_staging_ref_is_not_overwritten_by_the_candidate_builder() -> Result<()> {
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "candidate\n")?;
    f.save(0, None).await?;
    let opened = f.open().await?;
    let repo = Repository::open(&f.source)?;
    let reference = format!("refs/intercat/{}/staging", opened.slug);
    let foreign = repo.checkpoint_commit(
        repo.tree_id(f.head.parse()?)?,
        f.head.parse()?,
        "another staging writer",
    )?;
    repo.sync_objects(&[foreign])?;
    repo.compare_and_swap(&reference, None, foreign)?;
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let failed = finished(&f.journal).await?;
    assert_eq!(failed.status, Status::Failed);
    assert_eq!(repo.reference_target(&reference)?, Some(foreign));
    f.service.shutdown().await;
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn skipping_outside_hooks_is_an_explicit_new_revision_with_a_new_settings_hash() -> Result<()>
{
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "candidate\n")?;
    f.save(0, None).await?;
    let outside = f._root.path().join("outside-hooks");
    fs::create_dir(&outside)?;
    let hook = outside.join("pre-commit");
    fs::write(&hook, "#!/bin/sh\nexit 42\n")?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
    git(
        &f.source,
        &["config", "core.hooksPath", outside.to_str().unwrap()],
    );
    let opened = f.open().await?;
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let blocked = finished(&f.journal).await?;
    assert_eq!(
        blocked.candidate.as_ref().unwrap().state,
        intercat_daemon::candidate::State::HooksOutside
    );
    let old_hash = blocked.candidate.unwrap().settings_hash;
    let skipping = f
        .action(Action::SkipHooks {
            id: opened.id,
            revision: opened.revision,
        })
        .await?;
    assert_eq!(skipping.revision, opened.revision + 1);
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let candidate = checked.candidate.unwrap();
    assert!(candidate.hooks_skipped);
    assert_ne!(candidate.settings_hash, old_hash);
    f.service.shutdown().await;
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn secret_scan_is_persisted_without_its_value_and_cannot_become_checked_on_reopen()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    fs::write(
        f.workspaces[0].path.join("fixture.txt"),
        format!("token = '{token}'\n"),
    )?;
    f.save(0, None).await?;
    let job = f.open().await?;
    f.action(Action::Prepare {
        id: job.id,
        revision: job.revision,
    })
    .await?;
    let failed = finished(&f.journal).await?;
    assert_eq!(failed.status, Status::Failed);
    let candidate = failed.candidate.unwrap();
    assert_eq!(
        candidate.state,
        intercat_daemon::candidate::State::SecretFound
    );
    assert!(candidate.check.is_none());
    assert!(!candidate.secret_scans[0].findings.is_empty());
    assert!(!serde_json::to_string(&f.journal.snapshot().await?)?.contains(token));
    f.service.shutdown().await;
    let reopened = Journal::open(
        f._root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let snapshot = reopened.snapshot().await?;
    let persisted: Job = serde_json::from_value(snapshot["integration"].clone())?;
    assert_eq!(persisted.status, Status::Failed);
    assert_eq!(
        persisted.candidate.unwrap().secret_scans,
        candidate.secret_scans
    );
    assert!(!serde_json::to_string(&snapshot)?.contains(token));
    f.source_unchanged()?;
    Ok(())
}

async fn recorded_assumption(f: &Fixture, track: usize, turn: Uuid, item: &str) -> Result<()> {
    use intercat_daemon::assumptions::{Extracted, Finish};
    let track = &f.workspaces[track].track_id;
    let session = format!("assumption-session-{track}");
    f.journal
        .try_msp(
            track.clone(),
            session.clone(),
            intercat_daemon::clock::Stamp::now(),
            json!({"method":"item/updated","params":{"sessionId":session,"item":{
            "itemId":item,"turnId":turn,"kind":"agentMessage","revision":1,
            "status":"completed","text":"I assumed SQLite for local storage."}}}),
            None,
            None,
        )?
        .await??;
    let ticket = f.journal.claim_assumptions().await?.unwrap();
    let result = ticket
        .sources
        .iter()
        .map(|source| Extracted {
            source: source.id.clone(),
            question: "Which database should store the cache?".into(),
            options: ["SQLite".into(), "PostgreSQL".into()],
            assumed: "SQLite".into(),
            evidence: "I assumed SQLite".into(),
        })
        .collect();
    f.journal
        .finish_assumptions(ticket, Finish::Complete(result))
        .await?;
    Ok(())
}

async fn checked_assumptions(f: &Fixture) -> Result<Job> {
    let job = f.open().await?;
    f.action(Action::Prepare {
        id: job.id,
        revision: job.revision,
    })
    .await?;
    assert_eq!(finished(&f.journal).await?.status, Status::Checked);
    f.open().await
}

#[tokio::test]
async fn commit_assumptions_follow_frozen_clips_and_are_kept_only_for_published_tracks()
-> Result<()> {
    let f = Fixture::new(2).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "first\n")?;
    fs::write(f.workspaces[1].path.join("other.txt"), "second\n")?;
    for track in 0..2 {
        let saved = f.save(track, None).await?;
        recorded_assumption(&f, track, saved.turn_id, "saved").await?;
    }
    recorded_assumption(&f, 0, Uuid::now_v7(), "still-running").await?;
    let checked = checked_assumptions(&f).await?;
    let view = f.journal.snapshot().await?["integration"].clone();
    assert_eq!(
        view["assumptions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|value| value["included"] == true)
            .count(),
        2
    );
    assert_eq!(
        view["assumptions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|value| value["included"] == false)
            .count(),
        1
    );
    assert!(
        checked
            .candidate
            .as_ref()
            .unwrap()
            .commits
            .iter()
            .all(|commit| commit.message.contains("Assumptions kept\n")
                && commit.message.matches("Which database").count() == 1)
    );
    let selected = f
        .action(Action::Select {
            id: checked.id,
            revision: checked.revision,
            track: f.workspaces[1].track_id.clone(),
            included: false,
        })
        .await?;
    f.action(Action::Prepare {
        id: selected.id,
        revision: selected.revision,
    })
    .await?;
    assert_eq!(finished(&f.journal).await?.status, Status::Checked);
    let checked = f.open().await?;
    assert_eq!(checked.candidate.as_ref().unwrap().commits.len(), 1);
    let chosen = checked
        .assumptions
        .iter()
        .find(|review| review.eligible && review.assumption.track_id == f.workspaces[0].track_id)
        .unwrap();
    let command = IntegrationCommand {
        op_id: Uuid::now_v7(),
        action: Action::KeepAssumption {
            id: checked.id,
            revision: checked.revision,
            assumption: chosen.assumption.id.parse()?,
        },
    };
    let kept = f.service.command(command.clone()).await?;
    assert_eq!(
        kept.candidate.as_ref().unwrap().head,
        checked.candidate.as_ref().unwrap().head
    );
    assert_eq!(f.service.command(command).await?.revision, kept.revision);
    assert!(
        f.action(Action::Commit {
            id: checked.id,
            revision: checked.revision,
            head: checked.candidate.as_ref().unwrap().head.clone(),
            tree: checked.candidate.as_ref().unwrap().tree.clone(),
            settings_hash: checked.candidate.as_ref().unwrap().settings_hash.clone()
        })
        .await
        .is_err()
    );
    let candidate = kept.candidate.as_ref().unwrap();
    f.action(Action::Commit {
        id: kept.id,
        revision: kept.revision,
        head: candidate.head.clone(),
        tree: candidate.tree.clone(),
        settings_hash: candidate.settings_hash.clone(),
    })
    .await?;
    assert_eq!(finished(&f.journal).await?.status, Status::Done);
    let open = f.journal.snapshot().await?["assumptions"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(open.len(), 2);
    assert!(open.iter().any(|value| {
        value["sourceId"]
            .as_str()
            .unwrap()
            .contains("still-running")
    }));
    assert!(
        open.iter()
            .any(|value| value["trackId"] == f.workspaces[1].track_id)
    );
    let next = f.open().await?;
    assert_eq!(next.assumptions.len(), 2);
    f.service.shutdown().await;
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn late_or_changed_assumptions_require_a_new_review_and_keep_rolls_back_with_the_job()
-> Result<()> {
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "first\n")?;
    let saved = f.save(0, None).await?;
    recorded_assumption(&f, 0, saved.turn_id, "first").await?;
    let old = checked_assumptions(&f).await?;
    recorded_assumption(&f, 0, saved.turn_id, "late").await?;
    assert_eq!(
        f.journal.snapshot().await?["integration"]["assumptionsChanged"],
        true
    );
    let candidate = old.candidate.as_ref().unwrap();
    assert!(
        f.action(Action::Commit {
            id: old.id,
            revision: old.revision,
            head: candidate.head.clone(),
            tree: candidate.tree.clone(),
            settings_hash: candidate.settings_hash.clone()
        })
        .await
        .is_err()
    );
    let refreshed = f
        .action(Action::Refresh {
            id: old.id,
            revision: old.revision,
        })
        .await?;
    f.action(Action::Prepare {
        id: refreshed.id,
        revision: refreshed.revision,
    })
    .await?;
    finished(&f.journal).await?;
    let current = f.open().await?;
    assert_eq!(current.assumptions.len(), 2);
    let operation = IntegrationCommand {
        op_id: Uuid::now_v7(),
        action: Action::KeepAssumption {
            id: current.id,
            revision: current.revision,
            assumption: current.assumptions[0].assumption.id.parse()?,
        },
    };
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    db.execute_batch("CREATE TRIGGER fail_commit_answer BEFORE INSERT ON operations WHEN NEW.kind='integration.command' BEGIN SELECT RAISE(FAIL,'test write failure'); END;")?;
    assert!(f.service.command(operation.clone()).await.is_err());
    assert_eq!(
        f.journal.snapshot().await?["assumptions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        f.open().await.err().unwrap().to_string(),
        "test write failure"
    );
    db.execute_batch("DROP TRIGGER fail_commit_answer")?;
    let kept = f.service.command(operation).await?;
    assert_eq!(kept.revision, current.revision + 1);
    let reopened = Journal::open(
        f._root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    assert_eq!(
        reopened.snapshot().await?["integration"]["assumptions"][0]["state"],
        "kept"
    );
    let other = &kept.assumptions[1].assumption;
    f.journal
        .answer_assumption(intercat_daemon::assumptions::Command {
            op_id: Uuid::now_v7(),
            assumption_id: other.id.parse()?,
            revision: other.revision,
            answer: intercat_daemon::assumptions::Answer::Keep,
        })
        .await?;
    assert_eq!(
        f.journal.snapshot().await?["integration"]["assumptionsChanged"],
        true
    );
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn changing_a_commit_assumption_repairs_the_reviewed_tree_without_refresh() -> Result<()> {
    use intercat_daemon::{
        repair, repair_application,
        repair_run::{Event, Outcome, Phase, Purpose, Request},
    };
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "SQLite\n")?;
    let saved = f.save(0, None).await?;
    recorded_assumption(&f, 0, saved.turn_id, "saved").await?;
    let checked = checked_assumptions(&f).await?;
    let assumption = &checked.assumptions[0].assumption;
    let request = Request {
        op_id: Uuid::now_v7(),
        job_id: checked.id,
        revision: checked.revision,
        track: assumption.track_id.clone(),
        purpose: Purpose::Assumption {
            id: assumption.id.parse()?,
            revision: assumption.revision,
            text: "Use PostgreSQL.".into(),
        },
    };
    let run = f.journal.repair_start(request.clone()).await?;
    assert_eq!(f.journal.repair_start(request).await?.id, run.id);
    let log = f
        .journal
        .editor_log(intercat_daemon::editor_log::Query {
            track_id: assumption.track_id.clone(),
            before: None,
        })
        .await?;
    let changes: Vec<_> = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["scene"] == "assumption-change")
        .collect();
    assert_eq!(changes.len(), 1);
    assert!(
        changes[0]["line"]
            .as_str()
            .unwrap()
            .contains("Commit waits")
    );

    assert!(
        f.journal.snapshot().await?["assumptions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let (_, input, settings) = f.journal.repair_input(run.id).await?;
    let parent = f
        ._root
        .path()
        .join("integration")
        .join(input.id.to_string());
    let source = parent.join(input.candidate.as_ref().unwrap().generation.to_string());
    let workspace = repair::Workspace::prepare(
        &input,
        &source,
        &parent.join(format!("repair-{}", run.id)),
        &run.track,
        &settings,
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    fs::write(workspace.checkout.workdir.join("file.txt"), "PostgreSQL\n")?;
    let mut captured = run;
    let command = Uuid::now_v7();
    for event in [
        Event::Prepare,
        Event::WorkspaceReady,
        Event::HostStarted {
            secret_revision: Some(0),
            host: intercat_daemon::process::Identity {
                pid: 42,
                started_seconds: 1,
                started_micros: 1,
            },
            nonce: Uuid::now_v7(),
        },
        Event::SessionReady {
            session: "assumption-fix".into(),
        },
        Event::ReserveStart { command },
        Event::Started {
            command,
            turn: Uuid::now_v7(),
        },
        Event::HostClosed {
            outcome: Outcome::Completed,
        },
    ] {
        captured = f
            .journal
            .repair_transition(Uuid::now_v7(), captured.id, captured.revision, event)
            .await?;
    }
    let done = repair_application::apply(
        &f.journal,
        &captured,
        std::sync::Arc::new(workspace),
        source,
        parent.join(input.revision.to_string()),
        f._root.path().join("integration/downloads"),
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    assert_eq!(done.phase, Phase::Done);
    let repaired = f.open().await?;
    assert_eq!(repaired.id, checked.id);
    assert_eq!(repaired.status, Status::Checked);
    let candidate = repaired.candidate.as_ref().unwrap();
    assert_eq!(
        git(
            &f.source,
            &["show", &format!("{}:file.txt", candidate.head)]
        ),
        "PostgreSQL"
    );
    assert!(!candidate.commits[0].message.contains("Assumptions kept"));
    assert_eq!(
        fs::read_to_string(f.workspaces[0].path.join("file.txt"))?,
        "SQLite\n"
    );
    assert_eq!(repaired.assumptions[0].assumption.state, "changed");
    f.action(Action::Refresh {
        id: repaired.id,
        revision: repaired.revision,
    })
    .await?;
    assert_eq!(
        f.journal.snapshot().await?["assumptions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    f.service.shutdown().await;
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn commit_waits_for_assumption_reading_and_can_retry_an_exhausted_extraction() -> Result<()> {
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "new\n")?;
    let saved = f.save(0, None).await?;
    let track = &f.workspaces[0].track_id;
    f.journal
        .try_msp(
            track.clone(),
            "reading-session".into(),
            intercat_daemon::clock::Stamp::now(),
            json!({"method":"item/updated","params":{"sessionId":"reading-session","item":{
            "itemId":"pending","turnId":saved.turn_id,"kind":"agentMessage","revision":1,
            "status":"completed","text":"Inspecting the cache implementation."}}}),
            None,
            None,
        )?
        .await??;
    let checked = checked_assumptions(&f).await?;
    assert_eq!(
        f.journal.snapshot().await?["integration"]["assumptionsPending"],
        1
    );
    let candidate = checked.candidate.as_ref().unwrap();
    assert!(
        f.action(Action::Commit {
            id: checked.id,
            revision: checked.revision,
            head: candidate.head.clone(),
            tree: candidate.tree.clone(),
            settings_hash: candidate.settings_hash.clone()
        })
        .await
        .is_err()
    );
    for _ in 0..2 {
        let ticket = f.journal.claim_assumptions().await?.unwrap();
        f.journal
            .finish_assumptions(ticket, intercat_daemon::assumptions::Finish::Failed)
            .await?;
    }
    assert_eq!(
        f.journal.snapshot().await?["integration"]["assumptionsFailed"],
        1
    );
    let retry = f
        .action(Action::RetryAssumptions {
            id: checked.id,
            revision: checked.revision,
        })
        .await?;
    assert_eq!(retry.assumptions_failed, 0);
    assert_eq!(retry.assumptions_pending, 1);
    let ticket = f.journal.claim_assumptions().await?.unwrap();
    f.journal
        .finish_assumptions(
            ticket,
            intercat_daemon::assumptions::Finish::Complete(vec![]),
        )
        .await?;
    let ready = f.open().await?;
    assert_eq!(ready.assumptions_pending, 0);
    let candidate = ready.candidate.as_ref().unwrap();
    f.action(Action::Commit {
        id: ready.id,
        revision: ready.revision,
        head: candidate.head.clone(),
        tree: candidate.tree.clone(),
        settings_hash: candidate.settings_hash.clone(),
    })
    .await?;
    assert_eq!(finished(&f.journal).await?.status, Status::Done);
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn check_environment_selection_is_frozen_and_recheck_keeps_the_saved_tracks() -> Result<()> {
    use intercat_daemon::check_environment::Change;
    let f = Fixture::new(1).await?;
    let file = f.source.join(".env.test");
    fs::write(&file, "CHECK_VALUE=original-check-value\n")?;
    let choose = Change {
        op_id: Uuid::now_v7(),
        revision: 0,
        file: Some(file.clone()),
    };
    let selected = f.journal.change_check_environment(choose.clone()).await?;
    assert_eq!(selected.revision, 1);
    fs::remove_file(&file)?;
    assert_eq!(f.journal.change_check_environment(choose).await?, selected);
    fs::write(&file, "CHECK_VALUE=original-check-value\n")?;
    let checked = checked_for_edit(&f).await?;
    let original_settings = f
        .journal
        .integration_settings(checked.id, checked.revision)
        .await?
        .unwrap();
    assert_eq!(
        original_settings.environment_values()?["CHECK_VALUE"],
        "original-check-value"
    );
    let command = Change {
        op_id: Uuid::now_v7(),
        revision: 1,
        file: None,
    };
    f.journal.change_check_environment(command.clone()).await?;
    assert_eq!(
        f.journal.change_check_environment(command).await?.revision,
        2
    );
    assert_eq!(
        f.journal.snapshot().await?["integration"]["settingsChanged"],
        true
    );
    assert_eq!(
        f.journal
            .integration_settings(checked.id, checked.revision)
            .await?
            .unwrap()
            .hash()?,
        original_settings.hash()?
    );
    fs::write(f.workspaces[0].path.join("file.txt"), "later work\n")?;
    f.save(0, checked.tracks[0].checkpoint.clone()).await?;
    let request = IntegrationCommand {
        op_id: Uuid::now_v7(),
        action: Action::Recheck {
            id: checked.id,
            revision: checked.revision,
        },
    };
    f.service.command(request.clone()).await?;
    let updated = finished(&f.journal).await?;
    assert_eq!(updated.status, Status::Checked);
    assert_eq!(updated.revision, checked.revision + 1);
    assert_eq!(
        updated.candidate.as_ref().unwrap().tree,
        checked.candidate.as_ref().unwrap().tree
    );
    assert_eq!(
        updated.tracks[0].checkpoint.as_ref().unwrap().sha,
        checked.tracks[0].checkpoint.as_ref().unwrap().sha
    );
    assert!(
        f.journal
            .integration_settings(updated.id, updated.revision)
            .await?
            .unwrap()
            .environment_values()?
            .is_empty()
    );
    assert_eq!(f.service.command(request).await?.revision, updated.revision);
    assert_eq!(
        f.journal.snapshot().await?["integration"]["settingsChanged"],
        false
    );
    f.service.shutdown().await;
    let reopened = Journal::open(
        f._root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    assert_eq!(reopened.check_environment().await?.revision, 2);
    assert!(reopened.check_environment().await?.file.is_none());
    Ok(())
}
#[tokio::test]
async fn commit_order_proposal_freezes_inputs_and_replays_the_confirmed_order() -> Result<()> {
    let f = Fixture::new(2).await?;
    for index in 0..2 {
        fs::write(
            f.workspaces[index].path.join("file.txt"),
            if index == 0 { "base\nA\n" } else { "B\nbase\n" },
        )?;
        f.save(index, None).await?;
    }
    let open = IntegrationCommand {
        op_id: Uuid::now_v7(),
        action: Action::Open { tracks: None },
    };
    let proposed = f.service.command(open.clone()).await?;
    assert_eq!(proposed.status, Status::Ordering);
    assert!(proposed.candidate.is_none());
    assert!(proposed.plan.as_ref().unwrap().tree.is_none());
    let review = proposed.order_review.as_ref().unwrap();
    assert_eq!(review.overlaps.len(), 1);
    assert_eq!(review.overlaps[0].path, "file.txt");
    assert!(!review.confirmed);
    assert!(
        f.service
            .command(IntegrationCommand {
                op_id: Uuid::now_v7(),
                action: Action::Open {
                    tracks: Some(vec![review.order[0].clone()]),
                }
            })
            .await
            .is_err()
    );
    let mut reverse = review.order.clone();
    reverse.reverse();
    // New in-flight edits must not enter the captured order or preview.
    fs::write(f.workspaces[0].path.join("file.txt"), "not saved\n")?;
    assert!(
        f.service
            .command(IntegrationCommand {
                op_id: Uuid::now_v7(),
                action: Action::Prepare {
                    id: proposed.id,
                    revision: proposed.revision,
                }
            })
            .await
            .is_err()
    );
    let confirm = IntegrationCommand {
        op_id: Uuid::now_v7(),
        action: Action::Order {
            id: proposed.id,
            revision: proposed.revision,
            tracks: reverse.clone(),
        },
    };
    let ordered = f.service.command(confirm.clone()).await?;
    assert_eq!(ordered.status, Status::Open);
    assert_eq!(ordered.plan.as_ref().unwrap().rows[0].track, reverse[0]);
    assert_eq!(ordered.tracks[0].id, reverse[0]);
    assert!(ordered.order_review.as_ref().unwrap().confirmed);
    assert_eq!(
        git(
            &f.source,
            &[
                "show",
                &format!(
                    "{}:file.txt",
                    ordered.plan.as_ref().unwrap().tree.as_ref().unwrap()
                )
            ]
        ),
        "B\nbase\nA"
    );
    let repeated = f.service.command(confirm).await?;
    assert_eq!(repeated.revision, ordered.revision);
    let reloaded = Journal::open(
        f._root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let reopened = Service::new(reloaded, f._root.path().join("reopened-review"))
        .command(open)
        .await?;
    assert_eq!(reopened.revision, ordered.revision);
    assert_eq!(reopened.tracks[0].id, reverse[0]);
    assert_eq!(
        f.journal.snapshot().await?["integration"]["orderReview"]["confirmed"],
        true
    );
    f.action(Action::Prepare {
        id: ordered.id,
        revision: ordered.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    assert_eq!(
        checked
            .candidate
            .as_ref()
            .unwrap()
            .commits
            .iter()
            .map(|commit| &commit.track)
            .collect::<Vec<_>>(),
        reverse.iter().collect::<Vec<_>>()
    );
    let refreshed = f
        .service
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Refresh {
                id: ordered.id,
                revision: ordered.revision,
            },
        })
        .await?;
    assert_eq!(refreshed.order_review.as_ref().unwrap().order, reverse);
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn commit_order_proposal_rejects_stale_and_invalid_orders_and_refreshes_explicitly()
-> Result<()> {
    let f = Fixture::new(2).await?;
    let mut checkpoints = Vec::new();
    for index in 0..2 {
        fs::write(f.workspaces[index].path.join("file.txt"), "shared change\n")?;
        checkpoints.push(f.save(index, None).await?);
    }
    let proposed = f
        .service
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Open { tracks: None },
        })
        .await?;
    let order = proposed.order_review.as_ref().unwrap().order.clone();
    for invalid in [
        vec![],
        vec![order[0].clone(), order[0].clone()],
        vec![order[0].clone(), Uuid::now_v7().to_string()],
    ] {
        assert!(
            f.service
                .command(IntegrationCommand {
                    op_id: Uuid::now_v7(),
                    action: Action::Order {
                        id: proposed.id,
                        revision: proposed.revision,
                        tracks: invalid,
                    }
                })
                .await
                .is_err()
        );
    }
    fs::write(f.workspaces[1].path.join("file.txt"), "base\n")?;
    f.save(1, Some(checkpoints.pop().unwrap())).await?;
    let same = f
        .service
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Open { tracks: None },
        })
        .await?;
    assert_eq!(same.status, Status::Ordering);
    assert_eq!(same.revision, proposed.revision);
    let refreshed = f
        .service
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Refresh {
                id: proposed.id,
                revision: proposed.revision,
            },
        })
        .await?;
    assert_eq!(refreshed.status, Status::Open);
    assert!(refreshed.order_review.as_ref().unwrap().overlaps.is_empty());
    assert!(
        f.service
            .command(IntegrationCommand {
                op_id: Uuid::now_v7(),
                action: Action::Order {
                    id: proposed.id,
                    revision: proposed.revision,
                    tracks: order,
                }
            })
            .await
            .is_err()
    );
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn commit_order_uses_saved_changes_and_excludes_already_integrated_paths() -> Result<()> {
    let f = Fixture::new(2).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "shared change\n")?;
    let saved = f.save(0, None).await?;
    fs::write(f.workspaces[1].path.join("other.txt"), "different change\n")?;
    let other = f.save(1, None).await?;
    fs::write(f.workspaces[1].path.join("file.txt"), "in-flight overlap\n")?;
    let opened = f
        .service
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Open { tracks: None },
        })
        .await?;
    assert_eq!(opened.status, Status::Open);
    assert!(opened.order_review.as_ref().unwrap().overlaps.is_empty());
    fs::write(f.workspaces[1].path.join("file.txt"), "shared change\n")?;
    f.save(1, Some(other)).await?;
    let mut inspected = f
        .service
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Refresh {
                id: opened.id,
                revision: opened.revision,
            },
        })
        .await?;
    assert_eq!(inspected.status, Status::Ordering);
    inspected.plan.as_mut().unwrap().rows[0].expected_integrated = Some(saved.sha);
    let review = intercat_daemon::integration_order::inspect(&inspected)?;
    assert!(review.overlaps.is_empty());
    f.source_unchanged()?;
    Ok(())
}

#[tokio::test]
async fn commit_order_write_failure_keeps_the_job_unmerged_and_can_retry() -> Result<()> {
    let f = Fixture::new(2).await?;
    for index in 0..2 {
        fs::write(f.workspaces[index].path.join("file.txt"), "shared change\n")?;
        f.save(index, None).await?;
    }
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    db.execute_batch("CREATE TRIGGER fail_order BEFORE INSERT ON integration_order_reviews BEGIN SELECT RAISE(ABORT,'order storage full'); END;")?;
    let request = IntegrationCommand {
        op_id: Uuid::now_v7(),
        action: Action::Open { tracks: None },
    };
    assert!(f.service.command(request.clone()).await.is_err());
    let snapshot = f.journal.snapshot().await?;
    assert_eq!(snapshot["integration"]["status"], "preparing");
    assert!(snapshot["integration"]["plan"]["tree"].is_null());
    assert!(snapshot["integration"]["candidate"].is_null());
    db.execute_batch("DROP TRIGGER fail_order;")?;
    let proposed = f.service.command(request).await?;
    assert_eq!(proposed.status, Status::Ordering);
    let confirm = IntegrationCommand {
        op_id: Uuid::now_v7(),
        action: Action::Order {
            id: proposed.id,
            revision: proposed.revision,
            tracks: proposed
                .order_review
                .as_ref()
                .unwrap()
                .order
                .iter()
                .rev()
                .cloned()
                .collect(),
        },
    };
    db.execute_batch("CREATE TRIGGER fail_order BEFORE INSERT ON integration_order_reviews BEGIN SELECT RAISE(ABORT,'order storage full'); END;")?;
    assert!(f.service.command(confirm.clone()).await.is_err());
    assert_eq!(
        f.journal.snapshot().await?["integration"]["revision"],
        proposed.revision
    );
    db.execute_batch("DROP TRIGGER fail_order;")?;
    assert_eq!(f.service.command(confirm).await?.status, Status::Open);
    f.source_unchanged()?;
    Ok(())
}
