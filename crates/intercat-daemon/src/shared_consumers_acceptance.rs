//! Two real worker workspaces consume one immutable shared-source solution.
use super::*;

#[tokio::test]
async fn changing_the_ancestor_choice_reprojects_the_shared_source_without_reusing_the_old_view()
-> Result<()> {
    let (f, job, _request, shared, _later) = nested(false, 1).await?;
    f.reparent(3, 2, &job).await?;
    let first = f.action(Action::Open { tracks: None }).await?;
    assert!(f.workspaces[3].path.join("chosen-a.txt").exists());
    assert!(!f.workspaces[3].path.join("retired-a.txt").exists());
    let changed = f
        .action(Action::ChoosePath {
            id: first.id,
            revision: first.revision,
            track: f.workspaces[0].track_id.clone(),
        })
        .await?;
    assert_eq!(changed.status, Status::Choosing);
    let updated = f.reparent(3, 2, &changed).await?;
    assert!(updated.path_state.as_ref().unwrap().ready());
    assert!(!f.workspaces[3].path.join("chosen-a.txt").exists());
    assert!(f.workspaces[3].path.join("retired-a.txt").exists());
    assert!(f.workspaces[3].path.join("shared-b.txt").exists());
    assert!(f.workspaces[3].path.join("chosen-b.txt").exists());
    assert!(!f.workspaces[3].path.join("later-b.txt").exists());
    let ready = f.action(Action::Open { tracks: None }).await?;
    f.action(Action::Prepare {
        id: ready.id,
        revision: ready.revision,
    })
    .await?;
    let checked = f.settled(ready.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let done = publish_unhosted_replacement(&f, &checked).await?;
    let repo = intercat_git::Repository::open(&f.source)?;
    let head = repo
        .reference_target(&done.plan.as_ref().unwrap().target_ref)?
        .unwrap();
    let files = repo.tree_files(repo.tree_id(head)?)?;
    for path in ["retired-a.txt", "shared-b.txt", "chosen-b.txt"] {
        assert!(files.contains_key(path), "{path}");
    }
    for path in ["chosen-a.txt", "later-b.txt"] {
        assert!(!files.contains_key(path), "{path}");
    }
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let mut query = db.prepare("SELECT record FROM shared_contexts ORDER BY rowid")?;
    let contexts = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let contexts = contexts
        .iter()
        .map(|raw| serde_json::from_str::<crate::shared_context_store::Record>(raw))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(contexts.len(), 2);
    assert_ne!(contexts[0].id, contexts[1].id);
    for context in &contexts {
        assert_eq!(context.input.projection.source.sha, shared.sha);
    }
    assert!(contexts[1].input.projection.previous.is_some());
    assert_eq!(
        contexts[1].input.projection.from.track,
        f.workspaces[1].track_id
    );
    assert_eq!(
        contexts[1].input.projection.onto.track,
        f.workspaces[0].track_id
    );
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    f.review.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn two_frozen_consumer_file_updates_can_finish_in_the_opposite_order() -> Result<()> {
    let (f, job, first, _shared, _later) = nested(false, 2).await?;
    let second = Request {
        op_id: Uuid::now_v7(),
        track_id: f.workspaces[4].track_id.clone(),
        source: first.source.clone(),
    };
    let mut applications = vec![];
    for (index, request) in [(3, first), (4, second)] {
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
        let applied = f
            .apply_files(index, &request)
            .await?
            .ok_or("Missing actual consumer update")?;
        applications.push((index, request, claimed, applied));
    }
    assert_eq!(
        f.journal.integration_job(job.id).await?.revision,
        job.revision
    );
    for (index, request, claimed, applied) in applications.into_iter().rev() {
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
            .await?
            .ok_or("The original consumer request must finish")?;
    }
    assert_eq!(
        f.journal.integration_job(job.id).await?.revision,
        job.revision + 2
    );
    let ready = f.action(Action::Open { tracks: None }).await?;
    f.action(Action::Prepare {
        id: ready.id,
        revision: ready.revision,
    })
    .await?;
    let checked = f.settled(ready.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    publish_unhosted_replacement(&f, &checked).await?;
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    f.review.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn one_shared_solution_resumes_both_consumers_across_review_advances() -> Result<()> {
    let (f, job, first, shared, _later) = nested(true, 2).await?;
    let second = Request {
        op_id: Uuid::now_v7(),
        track_id: f.workspaces[4].track_id.clone(),
        source: first.source.clone(),
    };
    let a = queue_shared(&f, &first).await?;
    let b = queue_shared(&f, &second).await?;
    let summary = a
        .shared_input
        .as_ref()
        .ok_or("Missing first shared input")?;
    assert_eq!(summary.id, b.shared_input.as_ref().unwrap().id);
    assert_eq!(summary.revision, b.shared_input.as_ref().unwrap().revision);
    let runtime = start_runtime(&f, true);
    let request = crate::repair_run::Request {
        op_id: Uuid::now_v7(),
        owner: Owner::Shared {
            id: summary.id,
            revision: summary.revision,
        },
        track: summary.track.clone(),
        purpose: Purpose::Merge,
    };
    let run = runtime.service.start_manual(request.clone()).await?;
    assert_eq!(
        runtime.service.start_manual(request.clone()).await?.id,
        run.id
    );
    assert!(
        runtime
            .service
            .start_manual(crate::repair_run::Request {
                op_id: Uuid::now_v7(),
                ..request
            })
            .await
            .is_err(),
        "Two consumers must not start two owners of the same conflict"
    );
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
                text: "Own chosen A with shared B for both consumers\n".into(),
            }],
        })
        .await?;
    let (_, source) = f.journal.repair_source(run.id).await?;
    let root = f
        .root
        .path()
        .join("integration")
        .join(summary.id.to_string())
        .join(format!("repair-{}", run.id));
    let workspace = Arc::new(
        crate::run_workspace::Workspace::open_source(
            source,
            &root,
            &captured,
            tokio::sync::watch::channel(false).1,
        )
        .await?,
    );
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
    for request in [&first, &second] {
        let snapshot = f.journal.snapshot().await?;
        let entry = snapshot["realignments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["request"]["opId"] == request.op_id.to_string())
            .unwrap();
        assert_eq!(entry["phase"], "queued");
        assert!(entry["sharedInput"].is_null());
    }
    resume_shared(&f, &first).await?;
    assert_eq!(
        f.journal.integration_job(job.id).await?.revision,
        job.revision + 1
    );
    let stale_new = Request {
        op_id: Uuid::now_v7(),
        ..second.clone()
    };
    assert!(
        f.journal.realign_context(stale_new).await.is_err(),
        "A stale window cannot submit a new request using a sibling's permission"
    );
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let event: i64 = db.query_row(
        "SELECT seq FROM events WHERE kind='replacement.realigned' AND json_extract(payload,'$.job')=?1",
        [job.id.to_string()], |row|row.get(0))?;
    db.execute(
        "UPDATE events SET kind='own.unrelated.review.change' WHERE seq=?1",
        [event],
    )?;
    assert!(
        f.journal.realign_context(second.clone()).await.is_err(),
        "A missing sibling-completion record must keep the old request invalid"
    );
    db.execute(
        "UPDATE events SET kind='replacement.realigned' WHERE seq=?1",
        [event],
    )?;
    resume_shared(&f, &second).await?;
    let updated = f.action(Action::Open { tracks: None }).await?;
    f.action(Action::Prepare {
        id: updated.id,
        revision: updated.revision,
    })
    .await?;
    let checked = f.settled(job.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let done = publish_unhosted_replacement(&f, &checked).await?;
    let repo = intercat_git::Repository::open(&f.source)?;
    let head = repo
        .reference_target(&done.plan.as_ref().unwrap().target_ref)?
        .unwrap();
    let files = repo.tree_files(repo.tree_id(head)?)?;
    for path in [
        "shared-a.txt",
        "chosen-a.txt",
        "shared-b.txt",
        "chosen-b.txt",
        "consumer-4.txt",
    ] {
        assert!(files.contains_key(path), "{path}");
    }
    for path in ["retired-a.txt", "later-b.txt"] {
        assert!(!files.contains_key(path), "{path}");
    }
    for index in [3, 4] {
        assert_eq!(
            fs::read_to_string(f.workspaces[index].path.join("parent.txt"))?,
            "Own chosen A with shared B for both consumers\n"
        );
    }
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let count: i64 = db.query_row(
        "SELECT count(*) FROM repair_runs WHERE job=?1",
        [summary.id.to_string()],
        |row| row.get(0),
    )?;
    assert_eq!(count, 1);
    let state = f.journal.shared_context(summary.id).await?;
    assert_eq!(state.phase, Phase::Ready);
    assert_eq!(state.solutions.len(), 1);
    assert_eq!(state.input.projection.source.sha, shared.sha);
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    runtime.shutdown().await;
    f.review.shutdown().await;
    Ok(())
}
