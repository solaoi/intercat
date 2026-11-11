//! Persistent cancellation fences, including inputs materialized after the hold.
use super::*;

async fn old_plan(f: &Fixture, track: &str) -> Result<Uuid> {
    use crate::plans::{GenerationState, Item, ItemState, State};
    let id = Uuid::now_v7();
    f.journal
        .stage_utterance(
            id,
            "Own plan requested before cancellation".into(),
            Target::Lock(track.into()),
            None,
            vec![],
        )
        .await?;
    let input = f.journal.claim_input().await?.unwrap();
    f.journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Plan,
                track_id: Some(track.into()),
                boundary: None,
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    crate::clock::register(&db)?;
    let mut plan = crate::plan_store::source(&db, &id.to_string())?.unwrap();
    plan.summary = "Own held future plan".into();
    plan.state = State::Running;
    plan.generation.state = GenerationState::Ready;
    plan.items.push(Item {
        plan_item_id: Uuid::now_v7(),
        track_ref: track.into(),
        folder_id: crate::folders::track(&db, track)?.id,
        order: 0,
        title: "Own generated later".into(),
        instruction: "Own future plan input".into(),
        constraints: vec![],
        done_when: "Own dummy output exists".into(),
        after: vec![],
        skip_order: vec![],
        start_requested: false,
        state: ItemState::Planned,
        utterance_id: None,
        detail: None,
    });
    crate::plan_store::save(&db, &plan, "own.plan-fixture")?;
    Ok(plan.id)
}
#[tokio::test]
async fn cancelled_fresh_holds_later_plan_inputs_but_allows_only_new_words_or_explicit_plan_start()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let job = release_without_a_host(&f).await?;
    let queued = future(&f).await?;
    let plan_id = old_plan(&f, &job.request.track_id).await?;
    let cancel = Uuid::now_v7();
    f.journal
        .fresh(Change::Cancel {
            op_id: cancel,
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    let held = f
        .journal
        .read_utterance(queued["id"].as_str().unwrap().into())
        .await?;
    assert_eq!(held["state"], "held");
    assert_eq!(held["dispatchRunnable"], false);
    f.journal.advance_plans().await?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let plan = crate::plan_store::read(&db, plan_id)?;
    let input = plan.items[0].utterance_id.unwrap().to_string();
    let generated = f.journal.read_utterance(input.clone()).await?;
    assert_eq!(generated["state"], "held");
    assert_eq!(generated["dispatchRunnable"], false);
    // New materialization alone never releases the local hold, nor does each
    // scheduler tick churn the revision of the same held plan input.
    for _ in 0..3 {
        f.journal.advance_plans().await?;
    }
    assert_eq!(
        f.journal.read_utterance(input.clone()).await?["revision"],
        generated["revision"]
    );
    assert!(
        f.journal.snapshot().await?["localResumes"]
            .get(&job.request.track_id)
            .is_none()
    );
    let newer = future(&f).await?;
    assert_eq!(newer["state"], "placed");
    assert_eq!(newer["dispatchRunnable"], true);
    assert_eq!(
        f.journal.read_utterance(input.clone()).await?["state"],
        "held"
    );
    let held_plan = f
        .journal
        .plan_command(crate::plan_store::Command {
            op_id: Uuid::now_v7(),
            plan_id,
            revision: plan.rev,
            action: crate::plan_store::Action::Hold,
        })
        .await?;
    let reviewed = f
        .journal
        .plan_command(crate::plan_store::Command {
            op_id: Uuid::now_v7(),
            plan_id,
            revision: held_plan.rev,
            action: crate::plan_store::Action::Review,
        })
        .await?;
    let started = f
        .journal
        .plan_command(crate::plan_store::Command {
            op_id: Uuid::now_v7(),
            plan_id,
            revision: reviewed.rev,
            action: crate::plan_store::Action::Start,
        })
        .await?;
    assert_eq!(started.state, crate::plans::State::Running);
    f.journal.advance_plans().await?;
    let approved = f.journal.read_utterance(input).await?;
    assert_eq!(approved["state"], "placed");
    assert_eq!(approved["dispatchRunnable"], true);
    assert_eq!(
        f.journal
            .read_utterance(queued["id"].as_str().unwrap().into())
            .await?["state"],
        "held"
    );
    Ok(())
}
#[tokio::test]
async fn cancelled_fresh_survives_body_retirement_and_restart_without_automatically_reopening_a_host()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let job = release_without_a_host(&f).await?;
    let queued = future(&f).await?;
    let plan_id = old_plan(&f, &job.request.track_id).await?;
    let cancel = Uuid::now_v7();
    f.journal
        .fresh(Change::Cancel {
            op_id: cancel,
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    let hold = Uuid::new_v5(&cancel, b"fresh-cancel-hold");
    {
        let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
        crate::fresh_store::retire(&db, &job.request.track_id, &job.request.session_id)?;
        assert_eq!(
            crate::worker_hold::current(&db, &job.request.track_id)?,
            Some(hold)
        );
    }
    let Fixture { root, journal, .. } = f;
    drop(journal);
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    assert!(journal.fresh_job(job.request.op_id).await?.is_none());
    journal.advance_plans().await?;
    let plan = journal
        .plans()
        .await?
        .into_iter()
        .find(|plan| plan.id == plan_id)
        .unwrap();
    let generated = journal
        .read_utterance(plan.items[0].utterance_id.unwrap().to_string())
        .await?;
    assert_eq!(generated["state"], "held");
    assert_eq!(generated["dispatchRunnable"], false);
    journal
        .set_worker_config(
            Uuid::now_v7(),
            crate::runtime::Config {
                source: root.path().join("no-sdk"),
                binary: root.path().join("no-sdk"),
                credentials: root.path().join("no-auth"),
                path: "/usr/bin:/bin".into(),
            },
        )
        .await?;
    fs::create_dir(root.path().join("own-editor"))?;
    let editor = crate::editor::Manager::new(
        &root.path().join("own-editor"),
        &root.path().join("editor.log"),
    )?;
    let runtime = crate::runtime::Runtime::start(
        crate::host_memory::Pool::fixed(crate::host_memory::Sample {
            free_bytes: 8 * 1024 * 1024 * 1024,
            pressured: false,
        }),
        root.path().into(),
        root.path().join("rpc"),
        journal.clone(),
        editor.clone(),
        Arc::new(crate::scheduler::Capacity::new(1)),
        Arc::new(crate::dispatch_control::Gate::new(false)),
    );
    let result = async {
        let project = journal.snapshot().await?["projectSecrets"]["projectId"]
            .as_str()
            .unwrap()
            .to_owned();
        runtime
            .service
            .activate_project_secrets(crate::project_secrets::Load {
                project_id: project,
                revision: 0,
                entries: vec![],
            })
            .await?;
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let snapshot = journal.snapshot().await?;
        assert_eq!(snapshot["workers"][0]["phase"], "stopped");
        assert_eq!(
            journal
                .read_utterance(queued["id"].as_str().unwrap().into())
                .await?["state"],
            "held"
        );
        assert_eq!(
            snapshot["workerHolds"][&job.request.track_id],
            hold.to_string()
        );
        // A deliberately absent SDK produces the existing session's `lost`
        // state. No attempt may occur before this explicit user Start.
        runtime
            .service
            .start_turn(Uuid::now_v7(), &job.request.track_id)
            .await?;
        let attempted = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let snapshot = journal.snapshot().await?;
                if snapshot["workers"][0]["phase"] == "lost" {
                    assert!(snapshot["workers"][0]["identity"].is_null());
                    assert_eq!(snapshot["workers"][0]["sessionId"], job.request.session_id);
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        if let Err(error) = &attempted {
            return Err(format!(
                "Await explicit Start: {error}; workers={}",
                journal.snapshot().await?["workers"]
            )
            .into());
        }
        attempted??;
        assert_eq!(
            journal
                .read_utterance(generated["id"].as_str().unwrap().into())
                .await?["state"],
            "held"
        );
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    runtime.shutdown().await;
    editor.stop().await;
    result
}
#[tokio::test]
async fn replayed_cancel_and_start_do_not_release_a_newer_hold() -> Result<()> {
    let f = Fixture::new(1).await?;
    let job = release_without_a_host(&f).await?;
    let queued = future(&f).await?;
    let cancel = Uuid::now_v7();
    f.journal
        .fresh(Change::Cancel {
            op_id: cancel,
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    let held = f
        .journal
        .read_utterance(queued["id"].as_str().unwrap().into())
        .await?;
    let start = Uuid::now_v7();
    f.journal
        .start_held_input(
            start,
            held["id"].as_str().unwrap().into(),
            held["revision"].as_i64().unwrap(),
        )
        .await?;
    let newer = Uuid::now_v7();
    f.journal
        .hold_inputs(newer, job.request.track_id.clone())
        .await?;
    f.journal
        .fresh(Change::Cancel {
            op_id: cancel,
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    f.journal
        .start_held_input(
            start,
            held["id"].as_str().unwrap().into(),
            held["revision"].as_i64().unwrap(),
        )
        .await?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert_eq!(
        crate::worker_hold::current(&db, &job.request.track_id)?,
        Some(newer)
    );
    assert!(!crate::worker_hold::allowed(
        &db,
        &job.request.track_id,
        held["id"].as_str().unwrap()
    )?);
    assert_eq!(
        f.journal
            .read_utterance(held["id"].as_str().unwrap().into())
            .await?["state"],
        "held"
    );
    Ok(())
}

#[tokio::test]
async fn prepared_or_late_completed_worker_starts_never_release_a_newer_persistent_hold()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let job = release_without_a_host(&f).await?;
    let queued = future(&f).await?;
    f.journal
        .fresh(Change::Cancel {
            op_id: Uuid::now_v7(),
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    let track = &job.request.track_id;
    let input = queued["id"].as_str().unwrap();
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let first = Uuid::now_v7();
    f.journal
        .intent(
            &first.to_string(),
            "worker.start",
            json!({"trackId":track,"scope":"worker"}),
        )
        .await?;
    assert!(!crate::worker_hold::allowed(&db, track, input)?);
    f.journal.finish(&first.to_string(), "superseded").await?;
    assert!(!crate::worker_hold::allowed(&db, track, input)?);
    let pending = Uuid::now_v7();
    f.journal
        .intent(
            &pending.to_string(),
            "worker.start",
            json!({"trackId":track,"scope":"worker"}),
        )
        .await?;
    let newer = Uuid::now_v7();
    f.journal.hold_inputs(newer, track.clone()).await?;
    f.journal.finish(&pending.to_string(), "dispatched").await?;
    f.journal.finish(&pending.to_string(), "complete").await?;
    assert_eq!(crate::worker_hold::current(&db, track)?, Some(newer));
    assert!(!crate::worker_hold::allowed(&db, track, input)?);
    let current = Uuid::now_v7();
    f.journal
        .intent(
            &current.to_string(),
            "worker.start",
            json!({"trackId":track,"scope":"worker"}),
        )
        .await?;
    assert!(!crate::worker_hold::allowed(&db, track, input)?);
    f.journal.finish(&current.to_string(), "dispatched").await?;
    f.journal.finish(&current.to_string(), "complete").await?;
    assert_eq!(crate::worker_hold::current(&db, track)?, None);
    assert!(crate::worker_hold::allowed(&db, track, input)?);
    // Replaying an earlier Hold also cannot undo an accepted resume.
    f.journal.hold_inputs(newer, track.clone()).await?;
    assert_eq!(crate::worker_hold::current(&db, track)?, None);
    Ok(())
}
