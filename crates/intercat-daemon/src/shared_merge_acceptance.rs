use super::*;
use crate::repair_run::{Owner, Phase as RunPhase, Purpose};
#[path = "shared_consumers_acceptance.rs"]
mod consumers;
#[path = "shared_resources_acceptance.rs"]
mod resources;
#[path = "shared_sdk_acceptance.rs"]
mod sdk;

fn start_runtime(f: &Fixture, paused: bool) -> crate::repair_service::Runtime {
    crate::repair_service::Runtime::start(
        crate::host_memory::Pool::new(),
        f.journal.clone(),
        f.root.path().join("integration"),
        f.root.path().into(),
        f.root.path().join("own-shared-rpc.sock"),
        Arc::new(crate::scheduler::Capacity::new(4)),
        f.review.clone(),
        crate::project_secrets::Store::default(),
        Arc::new(crate::dispatch_control::Gate::new(paused)),
    )
}

async fn queue_shared(f: &Fixture, request: &Request) -> Result<crate::realign::Job> {
    let index = f
        .workspaces
        .iter()
        .position(|workspace| workspace.track_id == request.track_id)
        .ok_or("Missing own consumer workspace")?;
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
    let error = f
        .apply_files(index, request)
        .await
        .err()
        .ok_or("The own shared merge should block before worker files change")?;
    assert!(
        error.is::<crate::shared_context_service::Required>(),
        "{error}"
    );
    f.journal
        .change_realign_queue(Queue::Finish {
            id: request.op_id,
            revision: claimed.revision,
            phase: QueuePhase::NeedsMerge,
            error: None,
            inspection: None,
            consume_stop: false,
        })
        .await?
        .ok_or_else(|| "Missing shared queue state".into())
}

async fn resume_shared(f: &Fixture, request: &Request) -> Result<()> {
    let index = f
        .workspaces
        .iter()
        .position(|workspace| workspace.track_id == request.track_id)
        .ok_or("Missing own consumer workspace")?;
    let root_job: crate::realign::Job = serde_json::from_value(
        f.journal.snapshot().await?["realignments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["request"]["opId"] == request.op_id.to_string())
            .unwrap()
            .clone(),
    )?;
    assert_eq!(root_job.phase, QueuePhase::Queued);
    assert!(root_job.resolutions.is_empty());
    let claimed = f
        .journal
        .change_realign_queue(Queue::Claim {
            id: request.op_id,
            revision: root_job.revision,
        })
        .await?
        .unwrap();
    let applied = f
        .apply_files(index, request)
        .await?
        .ok_or("Missing shared file application")?;
    f.worker(index, applied.checkpoint).await?;
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
    Ok(())
}

#[tokio::test]
async fn a_shared_merge_has_its_own_run_and_recovers_before_resuming_the_same_reparent()
-> Result<()> {
    let (mut f, job, request, shared, _later) = nested(true, 1).await?;
    let original = fs::read(f.workspaces[2].path.join("parent.txt"))?;
    let child_before = fs::read(f.workspaces[3].path.join("parent.txt"))?;
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
    let error = f
        .apply_files(3, &request)
        .await
        .err()
        .ok_or("The own shared merge should block before worker files change")?;
    assert!(
        error.is::<crate::shared_context_service::Required>(),
        "{error}"
    );
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
    assert!(pending.merge_input.is_none());
    assert!(pending.resolutions.is_empty());
    let summary = pending
        .shared_input
        .as_ref()
        .ok_or("Missing shared conflict owner")?;
    assert_eq!(summary.track, f.workspaces[2].track_id);
    assert_eq!(summary.checkpoint, 1);
    let frozen = crate::shared_merge::read(&f.journal, summary).await?;
    assert_eq!(frozen.record.input.projection.source.sha, shared.sha);
    let manual = crate::repair_run::Request {
        op_id: Uuid::now_v7(),
        owner: Owner::Shared {
            id: summary.id,
            revision: summary.revision,
        },
        track: summary.track.clone(),
        purpose: Purpose::Merge,
    };
    let runtime = start_runtime(&f, true);
    let run = runtime.service.start_manual(manual.clone()).await?;
    assert_eq!(run.phase, RunPhase::Manual);
    assert_eq!(runtime.service.start_manual(manual).await?.id, run.id);
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
                text: "Own chosen A plus shared B resolution\n".into(),
            }],
        })
        .await?;
    assert_eq!(captured.phase, RunPhase::Captured);
    let (_, source) = f.journal.repair_source(run.id).await?;
    let merge_root = f
        .root
        .path()
        .join("integration")
        .join(summary.id.to_string())
        .join(format!("repair-{}", run.id));
    let workspace = crate::run_workspace::Workspace::open_source(
        source,
        &merge_root,
        &captured,
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    let crate::run_workspace::Workspace::Shared(workspace) = workspace else {
        panic!("The shared source must own its workspace");
    };
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    db.execute_batch("CREATE TRIGGER own_shared_resolution_fault BEFORE UPDATE ON shared_contexts WHEN json_extract(NEW.record,'$.phase')='pending' BEGIN SELECT RAISE(ABORT,'Own shared resolution admission failure'); END;")?;
    assert!(
        crate::shared_merge_apply::apply(
            &f.journal,
            &captured,
            &workspace,
            tokio::sync::watch::channel(false).1
        )
        .await
        .is_err()
    );
    let result = Uuid::new_v5(&run.id, b"apply-shared-merge");
    assert_eq!(
        f.journal.operation(result).await?.unwrap()["state"],
        "prepared"
    );
    assert_eq!(
        f.journal.repair_run(run.id).await?.phase,
        RunPhase::Applying
    );
    assert_eq!(
        f.journal.shared_context(summary.id).await?.phase,
        Phase::NeedsMerge
    );
    assert_eq!(
        fs::read(f.workspaces[3].path.join("parent.txt"))?,
        child_before
    );
    assert_eq!(fs::read(f.workspaces[2].path.join("parent.txt"))?, original);
    db.execute_batch("DROP TRIGGER own_shared_resolution_fault;")?;
    runtime.shutdown().await;
    f.review.shutdown().await;
    f.journal = Journal::open(
        f.root.path().join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    f.review = Service::new(f.journal.clone(), f.root.path().join("integration"));
    assert_eq!(
        f.journal.repair_run(run.id).await?.phase,
        RunPhase::NeedsRecovery
    );
    assert!(f.journal.worker_config().await?.is_none());
    let resumed = start_runtime(&f, false);
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let run = f.journal.repair_run(run.id).await?;
            if run.phase == RunPhase::Done && run.remaining.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await??;
    let record = f.journal.shared_context(summary.id).await?;
    assert_eq!(record.phase, Phase::Pending);
    assert_eq!(record.solutions.len(), 1);
    assert!(record.computed.is_none());
    assert_eq!(
        f.journal.operation(result).await?.unwrap()["state"],
        "complete"
    );
    assert!(
        !merge_root.exists(),
        "The completed shared Run may release its private files"
    );
    let replay = crate::shared_merge_apply::apply(
        &f.journal,
        &captured,
        &workspace,
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    assert_eq!(replay.phase, RunPhase::Done);
    let root: crate::realign::Job = serde_json::from_value(
        f.journal.snapshot().await?["realignments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|root| root["request"]["opId"] == request.op_id.to_string())
            .unwrap()
            .clone(),
    )?;
    assert_eq!(root.phase, QueuePhase::Queued);
    assert!(root.shared_input.is_none());
    assert!(
        root.resolutions.is_empty(),
        "A shared solution must not enter the descendant's resolution scope"
    );
    assert_eq!(
        f.journal.integration_job(job.id).await?.revision,
        job.revision
    );
    assert_eq!(
        fs::read(f.workspaces[3].path.join("parent.txt"))?,
        child_before
    );
    let claimed = f
        .journal
        .change_realign_queue(Queue::Claim {
            id: request.op_id,
            revision: root.revision,
        })
        .await?
        .unwrap();
    let applied = f
        .apply_files(3, &request)
        .await?
        .ok_or("Missing resumed shared reparent")?;
    f.worker(3, applied.checkpoint).await?;
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
    assert_eq!(
        fs::read_to_string(f.workspaces[3].path.join("parent.txt"))?,
        "Own chosen A plus shared B resolution\n"
    );
    assert_eq!(fs::read(f.workspaces[2].path.join("parent.txt"))?, original);
    assert!(!f.workspaces[3].path.join("retired-a.txt").exists());
    assert!(!f.workspaces[3].path.join("later-b.txt").exists());
    let prepared = f.action(Action::Open { tracks: None }).await?;
    f.action(Action::Prepare {
        id: prepared.id,
        revision: prepared.revision,
    })
    .await?;
    let checked = f.settled(prepared.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    resumed.shutdown().await;
    f.review.shutdown().await;
    Ok(())
}
