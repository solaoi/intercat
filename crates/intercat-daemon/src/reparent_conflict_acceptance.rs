use super::*;
use crate::{
    realign::Phase as QueuePhase,
    realign_queue::Change as Queue,
    repair_run::{Event, Owner, Phase as RunPhase, Purpose},
};

#[tokio::test]
async fn a_reparent_conflict_replays_its_saved_manual_result_before_updating_worker_files()
-> Result<()> {
    let f = Fixture::new(4).await?;
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own fork context\n",
    )?;
    let fork = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own retired parent\n",
    )?;
    let old = f.save(0, Some(fork.clone())).await?;
    f.publish(vec![f.workspaces[0].track_id.clone()]).await?;
    f.inherit(2, 0, &old)?;
    fs::write(f.workspaces[2].path.join("parent.txt"), "Own B edit\n")?;
    let child = f.save(2, None).await?;
    f.worker(2, child).await?;
    f.branch(&fork)?;
    fs::write(f.workspaces[1].path.join("parent.txt"), "Own alternative\n")?;
    let alternative = f.save(1, None).await?;
    let opened = f.action(Action::Open { tracks: None }).await?;
    let chosen = f
        .action(Action::ChoosePath {
            id: opened.id,
            revision: opened.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    let request = Request {
        op_id: Uuid::now_v7(),
        track_id: f.workspaces[2].track_id.clone(),
        source: Source::Reparent {
            job_id: chosen.id,
            revision: chosen.revision,
            parent: f.workspaces[0].track_id.clone(),
        },
    };
    let queued = f
        .journal
        .change_realign_queue(Queue::Request(request.clone()))
        .await?
        .unwrap();
    let claimed = f
        .journal
        .change_realign_queue(Queue::Claim {
            id: request.op_id,
            revision: queued.revision,
        })
        .await?
        .unwrap();
    let Err(error) = f.apply_files(2, &request).await else {
        panic!("B's edit must conflict")
    };
    assert!(error.is::<crate::realign_merge::Required>(), "{error}");
    let pending = f
        .journal
        .change_realign_queue(Queue::Finish {
            id: request.op_id,
            revision: claimed.revision,
            phase: QueuePhase::NeedsMerge,
            error: None,
            inspection: None,
            consume_stop: false,
        })
        .await?
        .unwrap();
    let frozen =
        crate::realign_merge::ensure_ready(&f.journal, pending.merge_input.unwrap()).await?;
    assert!(frozen.plan.reparent.is_some());
    assert_eq!(frozen.plan.target, alternative.sha);
    assert_eq!(
        fs::read_to_string(f.workspaces[2].path.join("parent.txt"))?,
        "Own B edit\n"
    );
    assert_eq!(
        f.journal.integration_job(chosen.id).await?.revision,
        chosen.revision
    );
    assert!(
        f.action(Action::ChoosePath {
            id: chosen.id,
            revision: chosen.revision,
            track: f.workspaces[0].track_id.clone(),
        })
        .await
        .is_err(),
        "the queued merge owns its replacement review"
    );

    let manual_request = crate::repair_run::Request {
        op_id: Uuid::now_v7(),
        owner: Owner::Realign {
            id: request.op_id,
            revision: pending.revision,
        },
        track: request.track_id.clone(),
        purpose: Purpose::Merge,
    };
    let run = f.journal.merge_start(manual_request.clone()).await?;
    assert_eq!(f.journal.merge_start(manual_request).await?.id, run.id);
    let (_, source) = f.journal.repair_source(run.id).await?;
    let root = f
        .root
        .path()
        .join("integration")
        .join(request.op_id.to_string())
        .join(format!("repair-{}", run.id));
    let workspace = crate::run_workspace::Workspace::open_source(
        source,
        &root,
        &run,
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    let run = f
        .journal
        .repair_transition(Uuid::now_v7(), run.id, run.revision, Event::WorkspaceReady)
        .await?;
    let view = crate::merge_editor::view(workspace.merge_files()?, &run, Some("parent.txt"))?;
    assert_eq!(view.paths, ["parent.txt"]);
    let apply = crate::merge_manual::Apply {
        op_id: Uuid::now_v7(),
        id: run.id,
        revision: run.revision,
        files: vec![crate::merge_manual::Edit::Text {
            path: "parent.txt".into(),
            version: view.file.version,
            text: "Own resolved B on the alternative\n".into(),
        }],
    };
    let snapshot = crate::merge_editor::save(workspace.merge_files()?, &apply)?;
    let captured = f
        .journal
        .repair_transition(
            apply.op_id,
            run.id,
            run.revision,
            Event::CaptureManual {
                snapshot,
                cleanup: Box::new(crate::run_processes::Remaining {
                    nonce: apply.op_id,
                    host: None,
                    directory: f
                        .root
                        .path()
                        .canonicalize()?
                        .join("repair-hosts")
                        .join(run.id.to_string()),
                    workspace: workspace.checkout().workdir.clone(),
                    label: "B".into(),
                    session_name: "Own reparent conflict".into(),
                    created_at: crate::journal::timestamp(),
                    revision: 0,
                    state: crate::run_processes::State::Cleaning,
                    error: None,
                }),
            },
        )
        .await?;
    assert_eq!(captured.phase, RunPhase::Captured);
    let crate::run_workspace::Workspace::Realign(workspace) = workspace else {
        panic!("The manual files must remain owned by the realignment")
    };
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    db.execute_batch("CREATE TRIGGER own_reparent_merge_fault BEFORE UPDATE ON realign_jobs WHEN json_extract(NEW.record,'$.revision')>json_extract(OLD.record,'$.revision') BEGIN SELECT RAISE(ABORT,'Own dummy merge result write failure'); END;")?;
    assert!(
        crate::realign_merge_apply::apply(
            &f.journal,
            &captured,
            &workspace,
            tokio::sync::watch::channel(false).1,
        )
        .await
        .is_err()
    );
    let result = Uuid::new_v5(&run.id, b"apply-realignment-merge");
    assert_eq!(
        f.journal.operation(result).await?.unwrap()["state"],
        "prepared"
    );
    let applying = f.journal.repair_run(run.id).await?;
    assert_eq!(applying.phase, RunPhase::Applying);
    assert_eq!(
        fs::read_to_string(f.workspaces[2].path.join("parent.txt"))?,
        "Own B edit\n"
    );
    db.execute_batch("DROP TRIGGER own_reparent_merge_fault;")?;
    let (_, source) = f.journal.repair_source(run.id).await?;
    let resumed = crate::run_workspace::Workspace::open_source(
        source,
        &root,
        &applying,
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    let crate::run_workspace::Workspace::Realign(resumed) = resumed else {
        panic!("The saved resolution must resume its own workspace")
    };
    let done = crate::realign_merge_apply::apply(
        &f.journal,
        &applying,
        &resumed,
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    assert_eq!(done.phase, RunPhase::Done);
    assert_eq!(
        f.journal.operation(result).await?.unwrap()["state"],
        "complete"
    );
    let requeued: crate::realign::Job = serde_json::from_value(
        f.journal.snapshot().await?["realignments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|job| job["request"]["opId"] == request.op_id.to_string())
            .unwrap()
            .clone(),
    )?;
    assert_eq!(requeued.phase, QueuePhase::Queued);
    assert_eq!(requeued.resolutions.len(), 1);
    assert_eq!(requeued.request.op_id, request.op_id);
    let monitor = crate::repair_process_service::Monitor::start(
        f.journal.clone(),
        f.root.path().join("integration"),
        f.root.path().into(),
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        while f.journal.repair_run(run.id).await?.remaining.is_some() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await??;
    monitor.shutdown().await;
    assert!(
        !root.exists(),
        "the completed isolated merge is cleaned without touching B"
    );
    assert_eq!(
        crate::realign_merge_apply::apply(
            &f.journal,
            &captured,
            &workspace,
            tokio::sync::watch::channel(false).1,
        )
        .await?
        .phase,
        RunPhase::Done,
        "the lost acknowledgement replays even after cleanup"
    );
    let claimed = f
        .journal
        .change_realign_queue(Queue::Claim {
            id: request.op_id,
            revision: requeued.revision,
        })
        .await?
        .unwrap();
    let applied = f
        .apply_files(2, &request)
        .await?
        .ok_or("Missing resolved file application")?;
    f.worker(2, applied.checkpoint).await?;
    f.journal
        .change_realign(crate::realign_store::Change::Complete(applied.operation))
        .await?;
    f.journal
        .change_realign_queue(Queue::Finish {
            id: request.op_id,
            revision: claimed.revision,
            phase: QueuePhase::Done,
            error: None,
            inspection: None,
            consume_stop: false,
        })
        .await?;
    assert!(f.apply_files(2, &request).await?.is_none());
    assert_eq!(
        fs::read_to_string(f.workspaces[2].path.join("parent.txt"))?,
        "Own resolved B on the alternative\n"
    );
    let completed = f.journal.integration_job(chosen.id).await?;
    assert_eq!(completed.revision, chosen.revision + 1);
    let ready = f.action(Action::Open { tracks: None }).await?;
    assert_eq!(
        ready.revision,
        completed.revision + 1,
        "Open captures the new review after the single reparent revision"
    );
    f.action(Action::Prepare {
        id: ready.id,
        revision: ready.revision,
    })
    .await?;
    let checked = f.settled(ready.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let repo = intercat_git::Repository::open(&f.source)?;
    let files = repo.tree_files(checked.candidate.as_ref().unwrap().tree.parse()?)?;
    assert_eq!(
        repo.saved_bytes(&files["parent.txt"])?,
        b"Own resolved B on the alternative\n"
    );
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE kind='realign.merge.applied'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    f.review.shutdown().await;
    Ok(())
}
