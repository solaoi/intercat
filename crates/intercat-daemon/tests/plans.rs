use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    plan_store::{Action, Command, Generated},
    plans::{Draft, DraftItem, GenerationState, Plan, State},
    request_sources::Submission,
    secrets::Key,
};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let project = root.path().join("project");
    std::fs::create_dir(&project)?;
    let status = std::process::Command::new("/usr/bin/git")
        .args(["init", "-q", "-b", "main"])
        .arg(&project)
        .status()?;
    assert!(status.success());
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    journal
        .configure_project(
            Uuid::now_v7(),
            intercat_daemon::project::inspect(
                uuid::Uuid::now_v7(),
                "Dummy project".into(),
                vec![project],
            )
            .await?,
        )
        .await?;
    Ok((root, journal))
}

async fn queue(journal: &Journal) -> Result<Plan> {
    journal
        .stage_document(Submission {
            op_id: Uuid::now_v7(),
            text: "Split this dummy task into two steps.".into(),
            target: Target::New,
            selected_track_id: None,
            attachment_ids: vec![],
            sources: None,
            execute_allowed: false,
            plan_item: None,
        })
        .await?;
    Ok(journal.plans().await?.pop().unwrap())
}
async fn draft(journal: &Journal, plan: &Plan) -> Result<Draft> {
    let folder = journal
        .plan_context(plan.id)
        .await?
        .folders
        .into_iter()
        .next()
        .unwrap();
    Ok(Draft {
        assumptions: vec![],
        conflicts: vec![],
        summary: "Prepare a dummy page.".into(),
        items: vec![DraftItem {
            id: "page".into(),
            track_ref: "new:1".into(),
            folder_id: folder,
            title: "Dummy page".into(),
            instruction: "Create a page with the word example.".into(),
            constraints: vec!["Use dummy data only.".into()],
            done_when: "The page contains example.".into(),
            after: vec![],
        }],
    })
}
fn command(plan: &Plan, action: Action) -> Command {
    Command {
        op_id: Uuid::now_v7(),
        plan_id: plan.id,
        revision: plan.rev,
        action,
    }
}

#[test]
fn command_wire_rejects_extra_fields() {
    let wire = serde_json::to_value(command(
        &Plan::new(Uuid::now_v7(), 0, false, None),
        Action::Hold,
    ))
    .unwrap();
    let decoded: Command = serde_json::from_value(wire.clone()).unwrap();
    assert!(matches!(decoded.action, Action::Hold));
    let mut extra = wire;
    extra["extra"] = true.into();
    assert!(serde_json::from_value::<Command>(extra).is_err());
    let item_id = Uuid::now_v7();
    let wire = serde_json::to_value(command(
        &Plan::new(Uuid::now_v7(), 0, false, None),
        Action::Cut { item_id },
    ))
    .unwrap();
    assert_eq!(wire["action"]["itemId"], item_id.to_string());
    assert!(
        matches!(serde_json::from_value::<Command>(wire).unwrap().action,Action::Cut{item_id: id} if id==item_id)
    );
}

#[tokio::test]
async fn plan_only_saves_attachment_identity_without_creating_a_worker() -> Result<()> {
    let (root, journal) = fixture().await?;
    let attachment = Uuid::now_v7();
    let file = root.path().join("dummy.txt");
    std::fs::write(&file, "Dummy requirements only.")?;
    let metadata = journal.import_attachment(attachment, file).await?;
    let request = Submission {
        op_id: Uuid::now_v7(),
        text: "Plan this dummy task.".into(),
        target: Target::New,
        selected_track_id: None,
        attachment_ids: vec![attachment],
        sources: None,
        execute_allowed: false,
        plan_item: None,
    };
    journal.stage_document(request.clone()).await?;
    journal.stage_document(request.clone()).await?;
    let mut changed = request;
    changed.execute_allowed = true;
    assert!(journal.stage_document(changed).await.is_err());
    assert!(journal.claim_input().await?.is_none());
    let plans = journal.plans().await?;
    assert_eq!(plans.len(), 1);
    assert!(!plans[0].execute_allowed);
    assert_eq!(plans[0].attachments[0], metadata);
    let snapshot = journal.snapshot().await?;
    assert!(snapshot["tracks"].as_array().unwrap().is_empty());
    assert!(snapshot["workers"].as_array().unwrap().is_empty());
    assert_eq!(snapshot["utterances"][0]["state"], "landed");
    assert_eq!(snapshot["utterances"][0]["executeAllowed"], false);
    Ok(())
}

#[tokio::test]
async fn generated_plan_is_revision_checked_durable_and_hold_invalidates_old_results() -> Result<()>
{
    let (root, journal) = fixture().await?;
    let plan = queue(&journal).await?;
    let content = draft(&journal, &plan).await?;
    assert!(
        journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?
    );
    assert!(
        !journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?
    );
    assert!(
        journal
            .generated_plan(
                plan.id,
                plan.generation.id,
                plan.rev,
                Generated::Draft(content.clone()),
                vec![]
            )
            .await?
    );
    let ready = journal.plans().await?.pop().unwrap();
    assert_eq!(ready.rev, plan.rev + 1);
    assert_eq!(ready.generation.state, GenerationState::Ready);
    assert!(!ready.execute_allowed);
    assert!(
        journal
            .plan_command(command(&plan, Action::Review))
            .await
            .is_err()
    );
    let reviewed = journal
        .plan_command(command(&ready, Action::Review))
        .await?;
    assert_eq!(reviewed.seen_revision, Some(ready.rev));
    let hold = command(&ready, Action::Hold);
    let held = journal.plan_command(hold.clone()).await?;
    assert_eq!(held.state, State::Held);
    assert_eq!(held.seen_revision, None);
    assert_eq!(journal.plan_command(hold).await?.rev, held.rev);
    let retry = journal.plan_command(command(&held, Action::Retry)).await?;
    assert_eq!(retry.source_revision, held.source_revision + 1);
    assert_eq!(
        journal.read_utterance(held.source_id.clone()).await?["revision"],
        retry.source_revision
    );
    assert!(
        journal
            .begin_plan(retry.id, retry.generation.id, retry.rev)
            .await?
    );
    assert!(
        !journal
            .generated_plan(
                plan.id,
                plan.generation.id,
                plan.rev,
                Generated::Draft(content.clone()),
                vec![]
            )
            .await?
    );
    let held = journal.plan_command(command(&retry, Action::Hold)).await?;
    assert!(
        !journal
            .generated_plan(
                retry.id,
                retry.generation.id,
                retry.rev,
                Generated::Draft(content),
                vec![]
            )
            .await?
    );
    drop(journal);
    let reopened = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(
        serde_json::to_value(reopened.plans().await?.pop().unwrap())?,
        serde_json::to_value(held)?
    );
    Ok(())
}

#[tokio::test]
async fn timed_out_plan_returns_the_original_request_to_single_job_routing_once() -> Result<()> {
    let (root, journal) = fixture().await?;
    let id = Uuid::now_v7();
    let attachment = Uuid::now_v7();
    let path = root.path().join("dummy-reference.txt");
    std::fs::write(&path, "A dummy blue badge reference.")?;
    journal.import_attachment(attachment, path).await?;
    journal
        .stage_document(Submission {
            op_id: id,
            text: "Build one dummy blue badge with its checks.".into(),
            target: Target::None,
            selected_track_id: None,
            attachment_ids: vec![attachment],
            sources: None,
            execute_allowed: true,
            plan_item: None,
        })
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Plan,
                track_id: None,
                boundary: None,
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: serde_json::json!({}),
            }),
        )
        .await?;
    let initial = journal.plans().await?.pop().unwrap();
    assert!(
        journal
            .plan_command(command(&initial, Action::RunAsOne))
            .await
            .is_err()
    );
    journal
        .begin_plan(initial.id, initial.generation.id, initial.rev)
        .await?;
    journal
        .generated_plan(
            initial.id,
            initial.generation.id,
            initial.rev,
            Generated::TimedOut,
            vec![],
        )
        .await?;
    let failed = journal.plans().await?.pop().unwrap();
    assert_eq!(failed.generation.state, GenerationState::TimedOut);
    journal.pause_dispatch(Uuid::now_v7(), true).await?;
    let paused = journal.plans().await?.pop().unwrap();
    assert!(
        journal
            .plan_command(command(&paused, Action::RunAsOne))
            .await
            .is_err()
    );
    assert_eq!(
        journal.read_utterance(initial.source_id.clone()).await?["revision"],
        initial.source_revision
    );
    journal.pause_dispatch(Uuid::now_v7(), false).await?;
    let failed = journal.plans().await?.pop().unwrap();
    let action = command(&failed, Action::RunAsOne);
    let delegated = journal.plan_command(action.clone()).await?;
    assert_eq!(delegated.state, State::SingleJob);
    assert_eq!(delegated.source_revision, initial.source_revision + 1);
    assert_eq!(journal.plan_command(action).await?.rev, delegated.rev);
    assert!(
        !journal
            .generated_plan(
                initial.id,
                initial.generation.id,
                initial.rev,
                Generated::Draft(draft(&journal, &initial).await?),
                vec![]
            )
            .await?
    );
    let source = journal.read_utterance(initial.source_id.clone()).await?;
    assert_eq!(source["state"], "staged");
    assert_eq!(
        source["text"],
        "Build one dummy blue badge with its checks."
    );
    assert_eq!(source["attachments"][0]["id"], attachment.to_string());
    assert!(source.get("planId").is_none());
    let input = journal.claim_input().await?.unwrap();
    assert_eq!(input.id, initial.source_id);
    assert!(input.single_job);
    let mut request = intercat_daemon::decision::Request::new(
        &input.text,
        input.tracks.clone(),
        input.target.clone(),
        input.selected.as_deref(),
    )?;
    request.run_as_one()?;
    for stage in request.wire["stages"].as_array().unwrap() {
        if stage["name"] == "decision" {
            assert!(
                !stage["candidates"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!(" G"))
            );
        }
        if stage["name"] == "command" {
            assert!(
                !stage["candidates"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!("9"))
            );
        }
    }
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: serde_json::json!({}),
            }),
        )
        .await?;
    assert!(journal.claim_input().await?.is_none());
    let source = journal.read_utterance(initial.source_id.clone()).await?;
    assert_eq!(source["state"], "placed");
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        journal
            .plan_command(command(&delegated, Action::Retry))
            .await
            .is_err()
    );
    drop(journal);
    let reopened = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(reopened.plans().await?[0].state, State::SingleJob);
    assert_eq!(
        reopened.read_utterance(initial.source_id).await?["text"],
        source["text"]
    );
    Ok(())
}

#[tokio::test]
async fn plan_only_cannot_be_run_as_a_single_job_and_retry_is_idempotent() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let plan = queue(&journal).await?;
    journal
        .begin_plan(plan.id, plan.generation.id, plan.rev)
        .await?;
    journal
        .generated_plan(
            plan.id,
            plan.generation.id,
            plan.rev,
            Generated::TimedOut,
            vec![],
        )
        .await?;
    let failed = journal.plans().await?.pop().unwrap();
    assert!(
        journal
            .plan_command(command(&failed, Action::RunAsOne))
            .await
            .is_err()
    );
    let retry = command(&failed, Action::Retry);
    let retried = journal.plan_command(retry.clone()).await?;
    assert_eq!(retried.source_revision, failed.source_revision + 1);
    assert_eq!(
        journal.plan_command(retry).await?.source_revision,
        retried.source_revision
    );
    assert!(journal.claim_input().await?.is_none());
    assert!(!retried.execute_allowed);
    Ok(())
}

#[tokio::test]
async fn invalid_plan_retries_once_and_restart_never_replays_a_running_generation() -> Result<()> {
    let (root, journal) = fixture().await?;
    let original = queue(&journal).await?;
    let mut invalid = draft(&journal, &original).await?;
    invalid.items[0]
        .after
        .push(intercat_daemon::plans::Dependency {
            id: "missing".into(),
            inherit: true,
        });
    let mut plan = original;
    for attempt in 0..2 {
        assert!(
            journal
                .begin_plan(plan.id, plan.generation.id, plan.rev)
                .await?
        );
        assert!(
            journal
                .generated_plan(
                    plan.id,
                    plan.generation.id,
                    plan.rev,
                    Generated::Draft(invalid.clone()),
                    vec![]
                )
                .await?
        );
        let next = journal.plans().await?.pop().unwrap();
        assert_eq!(
            next.generation.state,
            if attempt == 0 {
                GenerationState::Queued
            } else {
                GenerationState::Invalid
            }
        );
        assert!(next.items.is_empty());
        plan = next;
    }
    let retry = journal.plan_command(command(&plan, Action::Retry)).await?;
    assert!(
        journal
            .begin_plan(retry.id, retry.generation.id, retry.rev)
            .await?
    );
    drop(journal);
    let reopened = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let interrupted = reopened.plans().await?.pop().unwrap();
    assert_eq!(interrupted.generation.state, GenerationState::Failed);
    assert_eq!(interrupted.rev, retry.rev + 1);
    assert!(
        !reopened
            .begin_plan(retry.id, retry.generation.id, retry.rev)
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn classification_retains_plan_only_intent_and_pause_blocks_generations() -> Result<()> {
    let (_root, journal) = fixture().await?;
    for (kind, name, expected) in [
        (Kind::Plan, None, true),
        (Kind::Command, Some("plan-only"), false),
    ] {
        journal
            .stage_utterance(
                Uuid::now_v7(),
                "Plan this dummy task.".into(),
                Target::None,
                None,
                vec![],
            )
            .await?;
        let input = journal.claim_input().await?.unwrap();
        journal
            .apply_input(
                input.id,
                input.revision,
                Outcome::Classified(Decision {
                    kind,
                    command: name.map(str::to_owned),
                    track_id: None,
                    boundary: None,
                    confidence: 1.,
                    inference_ms: 1.,
                    timed_out: false,
                    scores: serde_json::json!({}),
                }),
            )
            .await?;
        assert_eq!(
            journal.plans().await?.last().unwrap().execute_allowed,
            expected
        );
    }
    let plan = journal.plans().await?.pop().unwrap();
    journal.pause_dispatch(Uuid::now_v7(), true).await?;
    assert!(
        !journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?
    );
    journal.pause_dispatch(Uuid::now_v7(), false).await?;
    assert!(
        journal
            .plans()
            .await?
            .iter()
            .all(|plan| plan.state == State::Held
                && plan.generation.state != GenerationState::Queued)
    );
    Ok(())
}

#[tokio::test]
async fn a_repeated_locked_track_violation_requires_an_atomic_explicit_choice() -> Result<()> {
    for (execute_allowed, unlock) in [(true, false), (true, true), (false, true)] {
        let (root, journal) = fixture().await?;
        journal
            .stage_utterance(
                Uuid::now_v7(),
                "Create a dummy worker.".into(),
                Target::New,
                None,
                vec![],
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
                    boundary: Some("now".into()),
                    command: None,
                    confidence: 1.,
                    inference_ms: 0.,
                    timed_out: false,
                    scores: serde_json::json!({}),
                }),
            )
            .await?;
        let track = journal.snapshot().await?["tracks"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        journal
            .stage_document(Submission {
                op_id: Uuid::now_v7(),
                text: "Plan the dummy tasks on this worker.".into(),
                target: Target::Lock(track.clone()),
                selected_track_id: Some(track.clone()),
                attachment_ids: vec![],
                sources: None,
                execute_allowed,
                plan_item: None,
            })
            .await?;
        if execute_allowed {
            let input = journal.claim_input().await?.unwrap();
            journal
                .apply_input(
                    input.id,
                    input.revision,
                    Outcome::Classified(Decision {
                        kind: Kind::Plan,
                        track_id: Some(track.clone()),
                        boundary: None,
                        command: None,
                        confidence: 1.,
                        inference_ms: 0.,
                        timed_out: false,
                        scores: serde_json::json!({}),
                    }),
                )
                .await?;
        }
        let mut plan = journal.plans().await?.pop().unwrap();
        let content = draft(&journal, &plan).await?;
        if !execute_allowed {
            let mut malformed = content.clone();
            malformed.items[0].instruction.clear();
            for _ in 0..2 {
                assert!(
                    journal
                        .begin_plan(plan.id, plan.generation.id, plan.rev)
                        .await?
                );
                assert!(
                    journal
                        .generated_plan(
                            plan.id,
                            plan.generation.id,
                            plan.rev,
                            Generated::Draft(malformed.clone()),
                            vec![]
                        )
                        .await?
                );
                plan = journal.plans().await?.pop().unwrap();
            }
            assert_eq!(plan.generation.state, GenerationState::Invalid);
            assert!(
                journal
                    .plan_command(command(&plan, Action::ResolveLock { unlock: true }))
                    .await
                    .is_err()
            );
            plan = journal.plan_command(command(&plan, Action::Retry)).await?;
        }
        for attempt in 0..2 {
            assert!(
                journal
                    .begin_plan(plan.id, plan.generation.id, plan.rev)
                    .await?
            );
            assert!(
                journal
                    .generated_plan(
                        plan.id,
                        plan.generation.id,
                        plan.rev,
                        Generated::Draft(content.clone()),
                        vec![]
                    )
                    .await?
            );
            plan = journal.plans().await?.pop().unwrap();
            assert_eq!(
                plan.generation.state,
                if attempt == 0 {
                    GenerationState::Queued
                } else {
                    GenerationState::UnlockRequired
                }
            );
            assert!(plan.items.is_empty());
            assert_eq!(plan.locked_track.as_deref(), Some(track.as_str()));
        }
        drop(journal);
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        plan = journal.plans().await?.pop().unwrap();
        assert_eq!(plan.generation.state, GenerationState::UnlockRequired);
        assert!(
            journal
                .plan_command(command(&plan, Action::Retry))
                .await
                .is_err()
        );
        assert!(
            journal
                .plan_command(command(&plan, Action::RunAsOne))
                .await
                .is_err()
        );
        if !execute_allowed {
            assert!(
                journal
                    .plan_command(command(&plan, Action::ResolveLock { unlock: false }))
                    .await
                    .is_err()
            );
        }
        let choice = command(&plan, Action::ResolveLock { unlock });
        let fault = rusqlite::Connection::open(root.path().join("db"))?;
        fault.execute_batch("CREATE TRIGGER fail_lock_choice BEFORE INSERT ON operations WHEN NEW.kind='plan.command' BEGIN SELECT RAISE(ABORT,'dummy save failure'); END;")?;
        assert!(journal.plan_command(choice.clone()).await.is_err());
        assert_eq!(
            journal.plans().await?.pop().unwrap().generation.state,
            GenerationState::UnlockRequired
        );
        assert_eq!(
            journal.read_utterance(plan.source_id.clone()).await?["target"]["trackId"],
            track
        );
        fault.execute_batch("DROP TRIGGER fail_lock_choice;")?;
        let queued = journal.plan_command(choice.clone()).await?;
        assert_eq!(queued.rev, plan.rev + 1);
        assert_eq!(queued.source_revision, plan.source_revision + 1);
        assert_eq!(queued.generation.state, GenerationState::Ready);
        assert_eq!(
            queued.locked_track,
            if unlock { None } else { Some(track.clone()) }
        );
        assert_eq!(journal.plan_command(choice).await?.rev, queued.rev);
        assert!(
            journal
                .plan_command(command(&plan, Action::ResolveLock { unlock }))
                .await
                .is_err()
        );
        assert!(
            !journal
                .generated_plan(
                    plan.id,
                    plan.generation.id,
                    plan.rev,
                    Generated::Draft(content.clone()),
                    vec![]
                )
                .await?
        );
        let source = journal.read_utterance(plan.source_id.clone()).await?;
        assert_eq!(source["revision"], queued.source_revision);
        assert_eq!(
            source["target"]["mode"],
            if unlock { "none" } else { "lock" }
        );
        assert_eq!(queued.execute_allowed, execute_allowed);
        drop(journal);
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        let queued = journal.plans().await?.pop().unwrap();
        assert_eq!(
            queued.locked_track,
            if unlock { None } else { Some(track.clone()) }
        );
        if unlock {
            assert_eq!(queued.state, State::Draft);
            assert_eq!(queued.items[0].track_ref, "new:1");
            assert_eq!(queued.items[0].instruction, content.items[0].instruction);
            assert!(queued.seen_revision.is_none());
            assert!(
                journal
                    .plan_command(command(&queued, Action::Start))
                    .await
                    .is_err()
            );
            assert!(journal.claim_input().await?.is_none());
        } else {
            assert_eq!(queued.state, State::SingleJob);
            let input = journal.claim_input().await?.unwrap();
            assert_eq!(input.id, queued.source_id);
            assert!(input.single_job);
            assert!(matches!(input.target, Target::Lock(ref id) if id == &track));
        }
    }
    Ok(())
}

#[tokio::test]
async fn queue_placement_follows_accepted_generation_and_survives_edits_and_restart() -> Result<()>
{
    let (root, journal) = fixture().await?;
    let first = queue(&journal).await?;
    let second = queue(&journal).await?;
    journal
        .begin_plan(first.id, first.generation.id, first.rev)
        .await?;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER fail_queue_placement BEFORE UPDATE OF queue_position ON plans WHEN OLD.queue_position IS NULL AND NEW.queue_position IS NOT NULL BEGIN SELECT RAISE(ABORT,'dummy placement failure'); END;")?;
    let content = draft(&journal, &first).await?;
    assert!(
        journal
            .generated_plan(
                first.id,
                first.generation.id,
                first.rev,
                Generated::Draft(content),
                vec![]
            )
            .await
            .is_err()
    );
    assert_eq!(
        db.query_row(
            "SELECT queue_position FROM plans WHERE id=?1",
            [first.id.to_string()],
            |row| row.get::<_, Option<i64>>(0)
        )?,
        None
    );
    let failed = journal
        .plans()
        .await?
        .into_iter()
        .find(|plan| plan.id == first.id)
        .unwrap();
    assert!(failed.items.is_empty());
    assert_eq!(failed.generation.state, GenerationState::Running);
    db.execute_batch("DROP TRIGGER fail_queue_placement;")?;
    drop(db);
    for plan in [&second, &first] {
        let content = draft(&journal, plan).await?;
        journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?;
        journal
            .generated_plan(
                plan.id,
                plan.generation.id,
                plan.rev,
                Generated::Draft(content),
                vec![],
            )
            .await?;
    }
    let plans = journal.plans().await?;
    assert_eq!(
        plans.iter().map(|plan| plan.id).collect::<Vec<_>>(),
        vec![second.id, first.id]
    );
    let first = plans.iter().find(|plan| plan.id == first.id).unwrap();
    let held = journal.plan_command(command(first, Action::Hold)).await?;
    let reviewed = journal.plan_command(command(&held, Action::Review)).await?;
    assert_eq!(journal.plans().await?[0].id, second.id);
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let positions = db
        .prepare("SELECT queue_position FROM plans ORDER BY queue_position")?
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    assert_eq!(positions, vec![1, 2]);
    drop(db);
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let restored = journal.plans().await?;
    assert_eq!(
        restored.iter().map(|plan| plan.id).collect::<Vec<_>>(),
        vec![second.id, first.id]
    );
    assert_eq!(restored[1].rev, reviewed.rev);
    Ok(())
}

async fn conflict_draft(journal: &Journal, queued: &Plan, inherit: bool) -> Result<Draft> {
    let mut content = draft(journal, queued).await?;
    let mut second = content.items[0].clone();
    second.id = "guide".into();
    second.track_ref = "new:2".into();
    second.title = "Dummy guide".into();
    second.after = vec![intercat_daemon::plans::Dependency {
        id: "page".into(),
        inherit,
    }];
    content.items.push(second);
    content
        .conflicts
        .push(intercat_daemon::plans::DraftConflict {
            track_a: "new:1".into(),
            track_b: "new:2".into(),
            what: "Both jobs revise the dummy index.".into(),
        });
    Ok(content)
}
async fn conflicted_plan(journal: &Journal, inherit: bool) -> Result<Plan> {
    let queued = queue(journal).await?;
    let content = conflict_draft(journal, &queued, inherit).await?;
    journal
        .begin_plan(queued.id, queued.generation.id, queued.rev)
        .await?;
    journal
        .generated_plan(
            queued.id,
            queued.generation.id,
            queued.rev,
            Generated::Draft(content),
            vec![],
        )
        .await?;
    Ok(journal.plans().await?.pop().unwrap())
}
fn conflict_answer(
    proposal: &serde_json::Value,
    action: intercat_daemon::proposals::Action,
) -> intercat_daemon::proposals::Command {
    intercat_daemon::proposals::Command {
        op_id: Uuid::now_v7(),
        proposal_id: proposal["id"].as_str().unwrap().parse().unwrap(),
        action,
    }
}

#[tokio::test]
async fn planner_conflict_is_durable_and_fine_acknowledges_without_starting_or_changing_order()
-> Result<()> {
    let (root, journal) = fixture().await?;
    let plan = conflicted_plan(&journal, false).await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    assert_eq!(proposal["kind"], "plan-conflict");
    assert_eq!(proposal["planId"], plan.id.to_string());
    assert!(proposal["detail"].as_str().unwrap().contains("dummy index"));
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(journal.snapshot().await?["proposal"], proposal);
    let answer = conflict_answer(&proposal, intercat_daemon::proposals::Action::KeepPlan);
    let result = journal.answer_proposal(answer.clone()).await?;
    assert_eq!(journal.answer_proposal(answer).await?, result);
    let current = journal.plans().await?.pop().unwrap();
    assert_eq!(current.rev, plan.rev);
    assert_eq!(current.items, plan.items);
    assert!(journal.snapshot().await?["proposal"].is_null());
    assert!(
        journal.snapshot().await?["workers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let current = journal
        .plan_command(command(&current, Action::Hold))
        .await?;
    assert!(current.rev > plan.rev);
    assert!(journal.snapshot().await?["proposal"].is_null());
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert!(journal.snapshot().await?["proposal"].is_null());
    Ok(())
}

#[tokio::test]
async fn run_together_removes_only_pair_order_and_can_be_undone_after_lost_acknowledgement()
-> Result<()> {
    let (root, journal) = fixture().await?;
    let plan = conflicted_plan(&journal, false).await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    let answer = conflict_answer(&proposal, intercat_daemon::proposals::Action::ParallelPlan);
    let result = journal.answer_proposal(answer.clone()).await?;
    let updated = journal.plans().await?.pop().unwrap();
    assert_eq!(updated.rev, plan.rev + 1);
    assert!(updated.items[1].after.is_empty());
    assert!(updated.items.iter().all(|item| !item.start_requested));
    let snapshot = journal.snapshot().await?;
    assert!(snapshot["proposal"].is_null());
    assert!(snapshot["workers"].as_array().unwrap().is_empty());
    let undo = snapshot["editHistory"]["undo"]["entryId"]
        .as_str()
        .unwrap()
        .to_owned();
    journal
        .edit_history(intercat_daemon::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: intercat_daemon::timeline_history::Action::Undo { entry: undo },
        })
        .await?;
    let undone = journal.plans().await?.pop().unwrap();
    assert_eq!(undone.items[1].after, plan.items[1].after);
    // Retrying the saved choice cannot apply it again after Undo or restart.
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(journal.answer_proposal(answer).await?, result);
    assert_eq!(
        journal.plans().await?.pop().unwrap().items[1].after,
        plan.items[1].after
    );
    Ok(())
}

#[tokio::test]
async fn planner_conflict_rejects_stale_answers_and_preserves_inherited_dependencies() -> Result<()>
{
    let (_root, journal) = fixture().await?;
    let plan = conflicted_plan(&journal, true).await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    assert!(
        proposal["options"][1]["disabledReason"]
            .as_str()
            .unwrap()
            .contains("inherited files")
    );
    assert!(
        journal
            .answer_proposal(conflict_answer(
                &proposal,
                intercat_daemon::proposals::Action::ParallelPlan
            ))
            .await
            .is_err()
    );
    assert_eq!(journal.plans().await?.pop().unwrap().items, plan.items);
    let current = journal.plan_command(command(&plan, Action::Hold)).await?;
    assert!(
        journal
            .answer_proposal(conflict_answer(
                &proposal,
                intercat_daemon::proposals::Action::KeepPlan
            ))
            .await
            .is_err()
    );
    let updated = journal.snapshot().await?["proposal"].clone();
    assert_ne!(updated["id"], proposal["id"]);
    assert_eq!(updated["source"]["planRevision"], current.rev);
    journal
        .answer_proposal(conflict_answer(
            &updated,
            intercat_daemon::proposals::Action::KeepPlan,
        ))
        .await?;
    assert!(journal.plans().await?.pop().unwrap().items[1].after[0].inherit);
    Ok(())
}

#[tokio::test]
async fn conflict_references_follow_new_track_assignment_and_start_closes_order_changes()
-> Result<()> {
    let (_root, journal) = fixture().await?;
    let plan = conflicted_plan(&journal, false).await?;
    let original = journal.snapshot().await?["proposal"].clone();
    journal.plan_command(command(&plan, Action::Review)).await?;
    let running = journal.plan_command(command(&plan, Action::Start)).await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    assert!(proposal["text"].as_str().unwrap().contains("A and B"));
    assert_eq!(proposal["source"]["trackA"], running.items[0].track_ref);
    assert_eq!(proposal["source"]["trackB"], running.items[1].track_ref);
    assert!(
        proposal["options"][1]["disabledReason"]
            .as_str()
            .unwrap()
            .contains("Hold")
    );
    assert!(
        journal
            .answer_proposal(conflict_answer(
                &original,
                intercat_daemon::proposals::Action::KeepPlan
            ))
            .await
            .is_err()
    );
    assert!(
        journal
            .answer_proposal(conflict_answer(
                &proposal,
                intercat_daemon::proposals::Action::ParallelPlan
            ))
            .await
            .is_err()
    );
    assert_eq!(
        journal.plans().await?.pop().unwrap().items[1].after,
        plan.items[1].after
    );
    Ok(())
}

#[tokio::test]
async fn conflict_choices_keep_other_links_and_show_one_finding_at_a_time() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let queued = queue(&journal).await?;
    let mut content = draft(&journal, &queued).await?;
    let first = content.items[0].clone();
    let mut second = first.clone();
    second.id = "guide".into();
    second.track_ref = "new:2".into();
    second.after = vec![intercat_daemon::plans::Dependency {
        id: "page".into(),
        inherit: false,
    }];
    let mut third = first;
    third.id = "index".into();
    third.track_ref = "new:3".into();
    third.after = vec![intercat_daemon::plans::Dependency {
        id: "page".into(),
        inherit: true,
    }];
    content.items.extend([second, third]);
    content.conflicts = vec![
        intercat_daemon::plans::DraftConflict {
            track_a: "new:1".into(),
            track_b: "new:2".into(),
            what: "Two dummy jobs overlap.".into(),
        },
        intercat_daemon::plans::DraftConflict {
            track_a: "new:1".into(),
            track_b: "new:3".into(),
            what: "A third dummy job overlaps.".into(),
        },
    ];
    journal
        .begin_plan(queued.id, queued.generation.id, queued.rev)
        .await?;
    journal
        .generated_plan(
            queued.id,
            queued.generation.id,
            queued.rev,
            Generated::Draft(content),
            vec![],
        )
        .await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    assert_eq!(proposal["source"]["trackB"], "new:2");
    journal
        .answer_proposal(conflict_answer(
            &proposal,
            intercat_daemon::proposals::Action::ParallelPlan,
        ))
        .await?;
    let current = journal.plans().await?.pop().unwrap();
    assert!(current.items[1].after.is_empty());
    assert_eq!(current.items[2].after.len(), 1);
    assert!(current.items[2].after[0].inherit);
    let next = journal.snapshot().await?["proposal"].clone();
    assert_ne!(next["id"], proposal["id"]);
    assert_eq!(next["source"]["trackB"], "new:3");
    assert_eq!(next["source"]["planRevision"], current.rev);
    Ok(())
}

#[tokio::test]
async fn storing_findings_and_placing_the_plan_are_one_transaction() -> Result<()> {
    let (root, journal) = fixture().await?;
    let queued = queue(&journal).await?;
    let mut content = draft(&journal, &queued).await?;
    let mut second = content.items[0].clone();
    second.id = "guide".into();
    second.track_ref = "new:2".into();
    content.items.push(second);
    content
        .conflicts
        .push(intercat_daemon::plans::DraftConflict {
            track_a: "new:1".into(),
            track_b: "new:2".into(),
            what: "Dummy overlap.".into(),
        });
    journal
        .begin_plan(queued.id, queued.generation.id, queued.rev)
        .await?;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER dummy_conflict_failure BEFORE INSERT ON plan_conflicts BEGIN SELECT RAISE(ABORT,'dummy storage full'); END;")?;
    assert!(
        journal
            .generated_plan(
                queued.id,
                queued.generation.id,
                queued.rev,
                Generated::Draft(content.clone()),
                vec![]
            )
            .await
            .is_err()
    );
    let current = journal.plans().await?.pop().unwrap();
    assert_eq!(current.rev, queued.rev);
    assert_eq!(current.generation.state, GenerationState::Running);
    assert!(current.items.is_empty());
    assert!(journal.snapshot().await?["proposal"].is_null());
    db.execute_batch("DROP TRIGGER dummy_conflict_failure;")?;
    assert!(
        journal
            .generated_plan(
                queued.id,
                queued.generation.id,
                queued.rev,
                Generated::Draft(content),
                vec![]
            )
            .await?
    );
    assert_eq!(
        journal.snapshot().await?["proposal"]["kind"],
        "plan-conflict"
    );
    Ok(())
}

#[tokio::test]
async fn regenerating_replaces_findings_without_reusing_old_answers() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let plan = conflicted_plan(&journal, false).await?;
    let original = journal.snapshot().await?["proposal"].clone();
    journal
        .answer_proposal(conflict_answer(
            &original,
            intercat_daemon::proposals::Action::KeepPlan,
        ))
        .await?;
    let next = journal.plan_command(command(&plan, Action::Retry)).await?;
    assert!(journal.snapshot().await?["proposal"].is_null());
    let content = conflict_draft(&journal, &next, false).await?;
    journal
        .begin_plan(next.id, next.generation.id, next.rev)
        .await?;
    journal
        .generated_plan(
            next.id,
            next.generation.id,
            next.rev,
            Generated::Draft(content),
            vec![],
        )
        .await?;
    let renewed = journal.snapshot().await?["proposal"].clone();
    assert_eq!(renewed["kind"], "plan-conflict");
    assert_ne!(
        renewed["source"]["conflictId"],
        original["source"]["conflictId"]
    );
    assert!(
        journal
            .answer_proposal(conflict_answer(
                &original,
                intercat_daemon::proposals::Action::ParallelPlan
            ))
            .await
            .is_err()
    );
    Ok(())
}

fn plan_reply(proposal: &serde_json::Value, text: &str) -> intercat_daemon::plan_feedback::Reply {
    intercat_daemon::plan_feedback::Reply {
        op_id: Uuid::now_v7(),
        plan_id: proposal["planId"].as_str().unwrap().parse().unwrap(),
        revision: proposal["planRevision"].as_u64().unwrap(),
        question_id: proposal["id"].as_str().unwrap().parse().unwrap(),
        question: proposal["text"].as_str().unwrap().into(),
        text: text.into(),
    }
}

#[tokio::test]
async fn a_plan_reply_revises_existing_identities_and_keeps_removed_items_cut() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let queued = queue(&journal).await?;
    let mut content = conflict_draft(&journal, &queued, false).await?;
    let mut extra = content.items[0].clone();
    extra.id = "removed".into();
    extra.title = "Removed dummy job".into();
    content.items.push(extra);
    journal
        .begin_plan(queued.id, queued.generation.id, queued.rev)
        .await?;
    journal
        .generated_plan(
            queued.id,
            queued.generation.id,
            queued.rev,
            Generated::Draft(content),
            vec![],
        )
        .await?;
    let plan = journal.plans().await?.pop().unwrap();
    let plan = journal
        .plan_command(command(
            &plan,
            Action::Cut {
                item_id: plan.items[2].plan_item_id,
            },
        ))
        .await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    let request = plan_reply(
        &proposal,
        "Keep the first step. The second step writes a dummy checklist instead.",
    );
    let saved = journal.reply_to_plan(request.clone()).await?;
    assert_eq!(saved["state"], "landed");
    assert_eq!(journal.reply_to_plan(request.clone()).await?, saved);
    let replanning = journal.plans().await?.pop().unwrap();
    assert_eq!(replanning.rev, plan.rev + 1);
    assert_eq!(replanning.source_revision, plan.source_revision + 1);
    assert_eq!(replanning.generation.state, GenerationState::Queued);
    assert!(
        journal.snapshot().await?["workers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(journal.snapshot().await?["proposal"].is_null());
    let text = journal.read_utterance(plan.source_id.clone()).await?;
    assert_eq!(text["text"], "Split this dummy task into two steps.");
    let mut items: Vec<_> = plan.items[..2]
        .iter()
        .map(|item| DraftItem {
            id: item.plan_item_id.to_string(),
            track_ref: item.track_ref.clone(),
            folder_id: item.folder_id.clone(),
            title: item.title.clone(),
            instruction: item.instruction.clone(),
            constraints: item.constraints.clone(),
            done_when: item.done_when.clone(),
            after: item.after.clone(),
        })
        .collect();
    items[1].instruction = "Write the revised dummy checklist.".into();
    let content = Draft {
        summary: "Revised dummy plan".into(),
        assumptions: vec![],
        conflicts: vec![],
        items,
    };
    journal
        .begin_plan(replanning.id, replanning.generation.id, replanning.rev)
        .await?;
    journal
        .generated_plan(
            replanning.id,
            replanning.generation.id,
            replanning.rev,
            Generated::Draft(content),
            vec![],
        )
        .await?;
    let current = journal.plans().await?.pop().unwrap();
    assert_eq!(current.items.len(), 3);
    assert_eq!(current.items[0].plan_item_id, plan.items[0].plan_item_id);
    assert_eq!(current.items[1].plan_item_id, plan.items[1].plan_item_id);
    assert_eq!(current.items[2].plan_item_id, plan.items[2].plan_item_id);
    assert_eq!(
        current.items[2].state,
        intercat_daemon::plans::ItemState::Cut
    );
    assert!(
        !serde_json::to_value(&current)?["feedback"]["pending"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(journal.reply_to_plan(request).await?, saved);
    assert_eq!(journal.plans().await?.pop().unwrap().rev, current.rev);
    Ok(())
}

#[tokio::test]
async fn changed_and_answered_plan_replies_are_durable_orphans() -> Result<()> {
    for answered in [false, true] {
        let (root, journal) = fixture().await?;
        let plan = conflicted_plan(&journal, false).await?;
        let proposal = journal.snapshot().await?["proposal"].clone();
        let request = plan_reply(
            &proposal,
            "Keep this dummy reply without sending it to any worker.",
        );
        if answered {
            journal
                .answer_proposal(conflict_answer(
                    &proposal,
                    intercat_daemon::proposals::Action::KeepPlan,
                ))
                .await?;
        } else {
            journal.plan_command(command(&plan, Action::Hold)).await?;
        }
        let before = journal.plans().await?.pop().unwrap();
        let saved = journal.reply_to_plan(request.clone()).await?;
        assert_eq!(saved["state"], "orphaned");
        let word = journal
            .read_utterance(saved["inputId"].as_str().unwrap().into())
            .await?;
        assert_eq!(word["state"], "orphaned");
        assert_eq!(word["text"], request.text);
        assert_eq!(word["planReply"]["question"], request.question);
        assert_eq!(
            word["reason"],
            if answered {
                "the question was answered meanwhile"
            } else {
                "the question changed meanwhile"
            }
        );
        assert_eq!(journal.plans().await?.pop().unwrap().rev, before.rev);
        assert!(journal.claim_input().await?.is_none());
        drop(journal);
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        assert_eq!(journal.reply_to_plan(request).await?, saved);
        let input = saved["inputId"].as_str().unwrap().to_owned();
        journal.cut_input(Uuid::now_v7(), input.clone()).await?;
        let cut = journal.read_utterance(input.clone()).await?;
        journal
            .edit_history(intercat_daemon::timeline_history::Request {
                op_id: Uuid::now_v7(),
                action: intercat_daemon::timeline_history::Action::Restore {
                    input: input.clone(),
                    revision: cut["revision"].as_i64().unwrap(),
                },
            })
            .await?;
        assert_eq!(journal.read_utterance(input).await?["state"], "orphaned");
        assert!(journal.claim_input().await?.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn plan_reply_failure_rolls_back_the_reply_and_the_new_generation() -> Result<()> {
    let (root, journal) = fixture().await?;
    let plan = conflicted_plan(&journal, false).await?;
    let request = plan_reply(
        &journal.snapshot().await?["proposal"],
        "Use a revised dummy heading.",
    );
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER fail_plan_reply BEFORE INSERT ON plan_feedback_replies BEGIN SELECT RAISE(ABORT,'dummy full storage'); END;")?;
    assert!(journal.reply_to_plan(request.clone()).await.is_err());
    assert_eq!(journal.plans().await?.pop().unwrap().rev, plan.rev);
    assert!(
        journal
            .read_utterance(request.op_id.to_string())
            .await
            .is_err()
    );
    db.execute_batch("DROP TRIGGER fail_plan_reply;")?;
    let saved = journal.reply_to_plan(request.clone()).await?;
    assert_eq!(saved["state"], "landed");
    let mut changed = request;
    changed.text = "A different reply must not reuse this operation.".into();
    assert!(journal.reply_to_plan(changed).await.is_err());
    Ok(())
}

#[tokio::test]
async fn a_revised_model_cannot_duplicate_an_identity_or_restore_a_removed_item() -> Result<()> {
    for restore in [false, true] {
        let (_root, journal) = fixture().await?;
        let plan = conflicted_plan(&journal, false).await?;
        let proposal = journal.snapshot().await?["proposal"].clone();
        journal
            .reply_to_plan(plan_reply(&proposal, "Keep the existing dummy identities."))
            .await?;
        let queued = journal.plans().await?.pop().unwrap();
        let mut content = conflict_draft(&journal, &queued, false).await?;
        if restore {
            // Keep the reply source current while supplying the state of an explicit cut.
            let db = rusqlite::Connection::open(_root.path().join("db"))?;
            let mut cut = queued.clone();
            cut.items[1].state = intercat_daemon::plans::ItemState::Cut;
            db.execute(
                "UPDATE plans SET record=?1 WHERE id=?2",
                rusqlite::params![serde_json::to_string(&cut)?, cut.id.to_string()],
            )?;
            content.items[1].id = plan.items[1].plan_item_id.to_string();
        } else {
            content.items[0].id = plan.items[0].plan_item_id.to_string();
            content.items[1].id = "page".into();
            content.items[1].after.clear();
        }
        content.conflicts.clear();
        journal
            .begin_plan(queued.id, queued.generation.id, queued.rev)
            .await?;
        journal
            .generated_plan(
                queued.id,
                queued.generation.id,
                queued.rev,
                Generated::Draft(content),
                vec![],
            )
            .await?;
        let current = journal.plans().await?.pop().unwrap();
        assert_eq!(current.generation.state, GenerationState::Queued);
        assert_eq!(current.generation.attempt, 1);
        assert_eq!(current.items[0].plan_item_id, plan.items[0].plan_item_id);
        if restore {
            assert_eq!(
                current.items[1].state,
                intercat_daemon::plans::ItemState::Cut
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn overlong_titles_retry_once_then_wait_and_accept_a_corrected_generation() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let mut plan = queue(&journal).await?;
    let mut content = draft(&journal, &plan).await?;
    content.items[0].title = "計".repeat(31);
    for expected in [GenerationState::Queued, GenerationState::Invalid] {
        journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?;
        journal
            .generated_plan(
                plan.id,
                plan.generation.id,
                plan.rev,
                Generated::Draft(content.clone()),
                vec![],
            )
            .await?;
        plan = journal.plans().await?.pop().unwrap();
        assert_eq!(plan.generation.state, expected);
        assert!(plan.items.is_empty());
        assert!(
            plan.generation
                .detail
                .as_deref()
                .unwrap()
                .contains("30 displayed characters")
        );
    }
    assert!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    plan = journal.plan_command(command(&plan, Action::Retry)).await?;
    content.items[0].title = "計".repeat(30);
    journal
        .begin_plan(plan.id, plan.generation.id, plan.rev)
        .await?;
    journal
        .generated_plan(
            plan.id,
            plan.generation.id,
            plan.rev,
            Generated::Draft(content.clone()),
            vec![],
        )
        .await?;
    let ready = journal.plans().await?.pop().unwrap();
    assert_eq!(ready.generation.state, GenerationState::Ready);
    assert_eq!(ready.items[0].title, content.items[0].title);
    Ok(())
}
