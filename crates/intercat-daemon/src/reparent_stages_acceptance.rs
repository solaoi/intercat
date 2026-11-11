//! Exercise the parent-composition and integrated-base conflicts through the
//! same saved queue, isolated editor, file application and final Commit Check.
use super::*;
use crate::{
    realign::{ConflictStage, Phase as QueuePhase, Resolution},
    realign_queue::Change as Queue,
    repair_run::{Owner, Phase as RunPhase, Purpose},
};

fn runtime(f: &Fixture) -> crate::repair_service::Runtime {
    crate::repair_service::Runtime::start(
        crate::host_memory::Pool::new(),
        f.journal.clone(),
        f.root.path().join("integration"),
        f.root.path().into(),
        f.root.path().join("own-rpc.sock"),
        Arc::new(crate::scheduler::Capacity::new(4)),
        f.review.clone(),
        crate::project_secrets::Store::default(),
        Arc::new(crate::dispatch_control::Gate::new(true)),
    )
}

async fn manual(
    f: &Fixture,
    runtime: &crate::repair_service::Runtime,
    owner: Owner,
    track: String,
    path: &str,
    text: &str,
) -> Result<crate::repair_run::Run> {
    let request = crate::repair_run::Request {
        op_id: Uuid::now_v7(),
        owner,
        track,
        purpose: Purpose::Merge,
    };
    let run = runtime.service.start_manual(request.clone()).await?;
    assert_eq!(runtime.service.start_manual(request).await?.id, run.id);
    let view = runtime
        .service
        .read_manual(crate::merge_manual::Read {
            id: run.id,
            revision: run.revision,
            path: Some(path.into()),
        })
        .await?;
    assert!(view.paths.iter().any(|p| p == path));
    let captured = runtime
        .service
        .apply_manual(crate::merge_manual::Apply {
            op_id: Uuid::now_v7(),
            id: run.id,
            revision: run.revision,
            files: vec![crate::merge_manual::Edit::Text {
                path: path.into(),
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
        .join(run.owner.id().to_string())
        .join(format!("repair-{}", run.id));
    let workspace = crate::run_workspace::Workspace::open_source(
        source,
        &root,
        &captured,
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    let done = crate::repair_application::apply_source(
        &f.journal,
        &f.review,
        &captured,
        Arc::new(workspace),
        f.root.path().join("integration"),
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    assert_eq!(done.phase, RunPhase::Done);
    assert!(done.host.is_none());
    Ok(done)
}

async fn reparent(
    f: &Fixture,
    runtime: &crate::repair_service::Runtime,
    job: &Job,
    stage: ConflictStage,
    path: &str,
    before: &str,
    solution: &str,
) -> Result<Job> {
    let request = Request {
        op_id: Uuid::now_v7(),
        track_id: f.workspaces[2].track_id.clone(),
        source: Source::Reparent {
            job_id: job.id,
            revision: job.revision,
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
    let error = f
        .apply_files(2, &request)
        .await
        .err()
        .ok_or("The requested stage must conflict")?;
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
    assert!(
        matches!(&frozen.plan.resolution,Resolution::Conflict {stage:actual,paths,..} if *actual==stage&&paths.iter().any(|p|p==path)),
        "{:?}",
        frozen.plan.resolution
    );
    assert_eq!(fs::read_to_string(f.workspaces[2].path.join(path))?, before);
    manual(
        f,
        runtime,
        Owner::Realign {
            id: request.op_id,
            revision: pending.revision,
        },
        request.track_id.clone(),
        path,
        solution,
    )
    .await?;
    assert_eq!(
        fs::read_to_string(f.workspaces[2].path.join(path))?,
        before,
        "a saved isolated choice does not yet update B"
    );
    assert_eq!(
        f.journal.integration_job(job.id).await?.revision,
        job.revision
    );
    let queued: crate::realign::Job = serde_json::from_value(
        f.journal.snapshot().await?["realignments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["request"]["opId"] == request.op_id.to_string())
            .unwrap()
            .clone(),
    )?;
    assert_eq!(queued.phase, QueuePhase::Queued);
    assert_eq!(queued.resolutions.len(), 1);
    let claimed = f
        .journal
        .change_realign_queue(Queue::Claim {
            id: request.op_id,
            revision: queued.revision,
        })
        .await?
        .unwrap();
    let applied = f
        .apply_files(2, &request)
        .await?
        .ok_or("Missing stage-resolved file application")?;
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
    let updated = f.journal.integration_job(job.id).await?;
    assert_eq!(updated.revision, job.revision + 1);
    Ok(updated)
}

async fn checked(
    f: &Fixture,
    runtime: &crate::repair_service::Runtime,
    job: &Job,
    path: &str,
    solution: &str,
) -> Result<Job> {
    let opened = f.action(Action::Open { tracks: None }).await?;
    assert_eq!(opened.id, job.id);
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let conflict = f.settled(opened.id).await?;
    assert_eq!(conflict.status, Status::Failed, "{:?}", conflict.error);
    assert_eq!(
        conflict.candidate.as_ref().unwrap().state,
        crate::candidate::State::Conflict
    );
    assert!(
        conflict
            .plan
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .any(|row| row.track == f.workspaces[1].track_id
                && row.state == crate::integration::RowState::Conflict)
    );
    manual(
        f,
        runtime,
        Owner::Commit {
            id: conflict.id,
            revision: conflict.revision,
        },
        f.workspaces[1].track_id.clone(),
        path,
        solution,
    )
    .await?;
    let checked = f.journal.integration_job(conflict.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    assert!(f.journal.worker_config().await?.is_none());
    Ok(checked)
}

#[tokio::test]
async fn conflicting_new_parents_resume_the_same_reparent_then_check_with_the_later_publication()
-> Result<()> {
    let f = Fixture::new(4).await?;
    let fork = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("retired.txt"),
        "Own retired A file\n",
    )?;
    let old = f.save(0, Some(fork.clone())).await?;
    f.publish(vec![f.workspaces[0].track_id.clone()]).await?;
    fs::write(
        f.workspaces[3].path.join("parent.txt"),
        "Own C parent choice\n",
    )?;
    let independent = f.save(3, None).await?;
    f.publish(vec![f.workspaces[3].track_id.clone()]).await?;
    f.inherit(2, 0, &old)?;
    fs::write(
        f.workspaces[2].path.join("parent.txt"),
        "Own C parent choice\n",
    )?;
    fs::write(f.workspaces[2].path.join("pending.txt"), "Own B pending\n")?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let point = crate::lineage_store::point(&db, &f.workspaces[3].track_id, &independent.sha)?;
    let mut basis = crate::lineage_store::basis(&db, &f.workspaces[2].track_id)?;
    basis.parents.insert(point.track.clone(), point.inherited());
    crate::lineage::include(&mut basis.inherits, &point.track, &point.inherited())?;
    crate::lineage_store::set_basis(&db, &f.workspaces[2].track_id, &basis)?;
    let child = f.save(2, None).await?;
    f.worker(2, child).await?;
    f.branch(&fork)?;
    fs::write(
        f.workspaces[1].path.join("parent.txt"),
        "Own alternative parent choice\n",
    )?;
    f.save(1, None).await?;
    let opened = f.action(Action::Open { tracks: None }).await?;
    let chosen = f
        .action(Action::ChoosePath {
            id: opened.id,
            revision: opened.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    let runtime = runtime(&f);
    let changed = reparent(
        &f,
        &runtime,
        &chosen,
        ConflictStage::Parents,
        "parent.txt",
        "Own C parent choice\n",
        "Own C parent choice\n",
    )
    .await?;
    assert_eq!(
        fs::read_to_string(f.workspaces[2].path.join("parent.txt"))?,
        "Own C parent choice\n"
    );
    assert!(!f.workspaces[2].path.join("retired.txt").exists());
    let result = checked(
        &f,
        &runtime,
        &changed,
        "parent.txt",
        "Own C parent choice\n",
    )
    .await?;
    let repo = intercat_git::Repository::open(&f.source)?;
    let files = repo.tree_files(result.candidate.as_ref().unwrap().tree.parse()?)?;
    assert_eq!(
        repo.saved_bytes(&files["parent.txt"])?,
        b"Own C parent choice\n"
    );
    assert!(files.contains_key("pending.txt"));
    assert!(!files.contains_key("retired.txt"));
    runtime.shutdown().await;
    f.review.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_integrated_base_conflict_keeps_the_workers_current_revert_through_check() -> Result<()>
{
    let f = Fixture::new(4).await?;
    let fork = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("retired.txt"),
        "Own retired A file\n",
    )?;
    let old = f.save(0, Some(fork.clone())).await?;
    f.publish(vec![f.workspaces[0].track_id.clone()]).await?;
    f.inherit(2, 0, &old)?;
    fs::write(
        f.workspaces[2].path.join("parent.txt"),
        "Own committed B edit\n",
    )?;
    let integrated = f.save(2, None).await?;
    f.publish(vec![f.workspaces[2].track_id.clone()]).await?;
    fs::write(
        f.workspaces[2].path.join("parent.txt"),
        "Own initial base\n",
    )?;
    fs::write(f.workspaces[2].path.join("pending.txt"), "Own B pending\n")?;
    let current = f.save(2, Some(integrated)).await?;
    f.worker(2, current).await?;
    f.branch(&fork)?;
    fs::write(
        f.workspaces[1].path.join("parent.txt"),
        "Own alternative parent choice\n",
    )?;
    f.save(1, None).await?;
    let opened = f.action(Action::Open { tracks: None }).await?;
    let chosen = f
        .action(Action::ChoosePath {
            id: opened.id,
            revision: opened.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    let runtime = runtime(&f);
    let changed = reparent(
        &f,
        &runtime,
        &chosen,
        ConflictStage::Baseline,
        "parent.txt",
        "Own initial base\n",
        "Own committed B edit\n",
    )
    .await?;
    assert_eq!(
        fs::read_to_string(f.workspaces[2].path.join("parent.txt"))?,
        "Own alternative parent choice\n"
    );
    let result = checked(
        &f,
        &runtime,
        &changed,
        "parent.txt",
        "Own alternative parent choice\n",
    )
    .await?;
    let repo = intercat_git::Repository::open(&f.source)?;
    let files = repo.tree_files(result.candidate.as_ref().unwrap().tree.parse()?)?;
    assert_eq!(
        repo.saved_bytes(&files["parent.txt"])?,
        b"Own alternative parent choice\n"
    );
    assert!(files.contains_key("pending.txt"));
    assert!(!files.contains_key("retired.txt"));
    runtime.shutdown().await;
    f.review.shutdown().await;
    Ok(())
}
