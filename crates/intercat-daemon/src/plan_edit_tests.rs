use crate::{
    Result,
    decision::Target,
    journal::Journal,
    plan_store::{Action, Command, Generated},
    plans::{Draft, DraftItem, Plan, State},
    request_sources::Submission,
    secrets::Key,
};
use uuid::Uuid;

async fn dependencies(
    journal: &Journal,
    plan: &Plan,
    item_id: Uuid,
    after: Vec<crate::plans::Dependency>,
) -> Result<Plan> {
    use crate::plan_rewrite::{Action as EditAction, Request};
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let held = journal
        .edit_plan(Request {
            owner,
            action: EditAction::Begin {
                plan_id: plan.id,
                item_id,
                token,
                revision: plan.rev,
            },
        })
        .await?
        .unwrap();
    let result = journal
        .edit_plan(Request {
            owner,
            action: EditAction::Dependencies {
                plan_id: held.id,
                token,
                revision: held.rev,
                op_id: Uuid::now_v7(),
                after,
            },
        })
        .await;
    match result {
        Ok(Some(plan)) => Ok(plan),
        Ok(None) => unreachable!(),
        Err(error) => {
            journal
                .edit_plan(Request {
                    owner,
                    action: EditAction::Cancel {
                        plan_id: plan.id,
                        token,
                    },
                })
                .await?;
            Err(error)
        }
    }
}

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    fixture_folders(1).await
}
async fn fixture_folders(count: usize) -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let project = root.path().join("project");
    std::fs::create_dir(&project)?;
    let status = std::process::Command::new("/usr/bin/git")
        .args(["init", "-q", "-b", "main"])
        .arg(&project)
        .status()?;
    assert!(status.success());
    let mut paths = vec![project];
    for index in 1..count {
        let path = root.path().join(format!("project-{index}"));
        std::fs::create_dir(&path)?;
        gix::init(&path)?;
        paths.push(path);
    }
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    journal
        .configure_project(
            Uuid::now_v7(),
            crate::project::inspect(uuid::Uuid::now_v7(), "Dummy project".into(), paths).await?,
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

async fn editable_plan(journal: &Journal) -> Result<Plan> {
    let plan = queue(journal).await?;
    let mut value = draft(journal, &plan).await?;
    let first = value.items[0].clone();
    value.items = (0..3)
        .map(|index| {
            let mut item = first.clone();
            item.id = format!("dummy-{index}");
            item.title = format!("Dummy job {index}");
            item.instruction = format!("Create dummy-{index}.txt only.");
            item.track_ref = if index == 2 { "new:2" } else { "new:1" }.into();
            item
        })
        .collect();
    journal
        .begin_plan(plan.id, plan.generation.id, plan.rev)
        .await?;
    journal
        .generated_plan(
            plan.id,
            plan.generation.id,
            plan.rev,
            Generated::Draft(value),
            vec![],
        )
        .await?;
    Ok(journal.plans().await?.pop().unwrap())
}

async fn history_step(journal: &Journal, direction: &str) -> Result<serde_json::Value> {
    let snapshot = journal.snapshot().await?;
    let id = snapshot["editHistory"][direction]["entryId"]
        .as_str()
        .ok_or("Missing history head")?
        .to_string();
    let action = if direction == "undo" {
        crate::timeline_history::Action::Undo { entry: id }
    } else {
        crate::timeline_history::Action::Redo { entry: id }
    };
    journal
        .edit_history(crate::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action,
        })
        .await
}

#[tokio::test]
async fn plan_moves_undo_and_redo_invalidate_queued_work_without_reusing_old_input() -> Result<()> {
    use crate::plan_store::Direction;
    let (_root, journal) = fixture().await?;
    let plan = editable_plan(&journal).await?;
    journal.plan_command(command(&plan, Action::Review)).await?;
    journal.plan_command(command(&plan, Action::Start)).await?;
    journal.advance_plans().await?;
    let original = journal.plans().await?.pop().unwrap();
    let first = original.items[0].utterance_id.unwrap();
    let movement = command(
        &original,
        Action::Move {
            item_id: original.items[1].plan_item_id,
            direction: Direction::Earlier,
        },
    );
    let moved = journal.plan_command(movement.clone()).await?;
    assert_eq!(journal.plan_command(movement).await?.rev, moved.rev);
    assert_eq!(moved.items[1].order, 0);
    assert_eq!(moved.items[0].plan_item_id, original.items[0].plan_item_id);
    assert!(moved.items[0].utterance_id.is_none());
    assert_eq!(
        journal.read_utterance(first.to_string()).await?["planRunnable"],
        false
    );
    assert!(
        journal.snapshot().await?["editHistory"]["takenBack"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    journal.advance_plans().await?;
    let queued = journal.plans().await?.pop().unwrap();
    assert!(queued.items[1].utterance_id.is_some());
    let old_second = queued.items[1].utterance_id.unwrap();
    history_step(&journal, "undo").await?;
    let undone = journal.plans().await?.pop().unwrap();
    assert_eq!(undone.items[0].order, 0);
    assert!(undone.rev > moved.rev);
    journal.advance_plans().await?;
    let queued = journal.plans().await?.pop().unwrap();
    assert_ne!(queued.items[0].utterance_id.unwrap(), first);
    assert_eq!(
        journal.read_utterance(old_second.to_string()).await?["planRunnable"],
        false
    );
    history_step(&journal, "redo").await?;
    assert_eq!(journal.plans().await?[0].items[1].order, 0);
    Ok(())
}

#[tokio::test]
async fn retry_discards_unstarted_input_reservations_once_before_regenerating() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let plan = editable_plan(&journal).await?;
    journal.plan_command(command(&plan, Action::Review)).await?;
    journal.plan_command(command(&plan, Action::Start)).await?;
    journal.advance_plans().await?;
    let queued = journal.plans().await?.pop().unwrap();
    let inputs: Vec<_> = queued
        .items
        .iter()
        .filter_map(|item| item.utterance_id)
        .collect();
    assert_eq!(inputs.len(), 2);
    let held = journal.plan_command(command(&queued, Action::Hold)).await?;
    let retry = command(&held, Action::Retry);
    let changed = journal.plan_command(retry.clone()).await?;
    assert_eq!(changed.source_revision, held.source_revision + 1);
    assert!(changed.items.iter().all(|item| item.utterance_id.is_none()));
    assert!(history_step(&journal, "undo").await.is_err());
    assert_eq!(journal.plans().await?[0].state, State::Draft);
    for id in &inputs {
        let old = journal.read_utterance(id.to_string()).await?;
        assert_eq!(old["state"], "cut");
        assert_eq!(old["planRunnable"], false);
        let version = old["revision"].clone();
        assert_eq!(journal.plan_command(retry.clone()).await?.rev, changed.rev);
        assert_eq!(
            journal.read_utterance(id.to_string()).await?["revision"],
            version
        );
    }
    assert!(
        journal
            .begin_plan(changed.id, changed.generation.id, changed.rev)
            .await?
    );
    journal
        .generated_plan(
            changed.id,
            changed.generation.id,
            changed.rev,
            Generated::Draft(draft(&journal, &changed).await?),
            vec![],
        )
        .await?;
    let new = journal.plans().await?.pop().unwrap();
    assert!(new.items.iter().all(|item| item.utterance_id.is_none()));
    assert!(
        journal.snapshot().await?["workers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn a_failed_started_item_cannot_be_replanned_or_change_other_queued_inputs() -> Result<()> {
    let (root, journal) = fixture().await?;
    let plan = editable_plan(&journal).await?;
    journal.plan_command(command(&plan, Action::Review)).await?;
    journal.plan_command(command(&plan, Action::Start)).await?;
    journal.advance_plans().await?;
    let mut saved = journal.plans().await?.pop().unwrap();
    saved.items[2].state = crate::plans::ItemState::Failed;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    crate::clock::register(&db)?;
    crate::plan_store::save(&db, &saved, "dummy.failed-work")?;
    let input = saved.items[0].utterance_id.unwrap();
    let old = journal.read_utterance(input.to_string()).await?;
    assert!(
        journal
            .plan_command(command(&saved, Action::Retry))
            .await
            .is_err()
    );
    assert_eq!(journal.plans().await?[0].rev, saved.rev);
    assert_eq!(journal.read_utterance(input.to_string()).await?, old);
    assert_eq!(
        journal.read_utterance(saved.source_id).await?["revision"],
        saved.source_revision
    );
    Ok(())
}

#[tokio::test]
async fn plan_edit_validation_rejects_cycles_and_preserves_the_revision_and_history() -> Result<()>
{
    use crate::plan_store::Direction;
    use crate::plans::Dependency;
    let (_root, journal) = fixture().await?;
    let plan = editable_plan(&journal).await?;
    let dependent = dependencies(
        &journal,
        &plan,
        plan.items[1].plan_item_id,
        vec![Dependency {
            id: plan.items[0].plan_item_id.to_string(),
            inherit: false,
        }],
    )
    .await?;
    let before = journal.snapshot().await?;
    assert!(
        journal
            .plan_command(command(
                &dependent,
                Action::Move {
                    item_id: dependent.items[1].plan_item_id,
                    direction: Direction::Earlier
                }
            ))
            .await
            .is_err()
    );
    assert_eq!(journal.plans().await?[0].rev, dependent.rev);
    assert_eq!(
        journal.snapshot().await?["editHistory"],
        before["editHistory"]
    );
    assert!(
        journal
            .plan_command(command(
                &plan,
                Action::Cut {
                    item_id: plan.items[0].plan_item_id
                }
            ))
            .await
            .is_err()
    );
    history_step(&journal, "undo").await?;
    assert!(journal.plans().await?[0].items[1].after.is_empty());
    Ok(())
}

#[tokio::test]
async fn deleted_plans_restore_independently_of_a_started_history_head() -> Result<()> {
    use crate::plan_store::Direction;
    let (root, journal) = fixture().await?;
    let plan = editable_plan(&journal).await?;
    let cut = journal
        .plan_command(command(
            &plan,
            Action::Cut {
                item_id: plan.items[2].plan_item_id,
            },
        ))
        .await?;
    let moved = journal
        .plan_command(command(
            &cut,
            Action::Move {
                item_id: cut.items[1].plan_item_id,
                direction: Direction::Earlier,
            },
        ))
        .await?;
    let mut started = moved.clone();
    started.items[1].state = crate::plans::ItemState::Running;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute(
        "UPDATE plans SET record=?2 WHERE id=?1",
        rusqlite::params![started.id.to_string(), serde_json::to_string(&started)?],
    )?;
    assert!(
        journal.snapshot().await?["editHistory"]["undo"]["blocked"]
            .as_str()
            .unwrap()
            .contains("has started")
    );
    assert!(history_step(&journal, "undo").await.is_err());
    let restored = journal
        .plan_command(command(
            &started,
            Action::Restore {
                item_id: started.items[2].plan_item_id,
            },
        ))
        .await?;
    assert_eq!(restored.items[1].state, crate::plans::ItemState::Running);
    assert_eq!(restored.items[2].state, crate::plans::ItemState::Planned);
    assert_eq!(restored.state, State::Held);
    let history = journal.snapshot().await?["editHistory"].clone();
    assert_eq!(history["undo"]["label"], "Move plan item");
    assert!(
        history["undo"]["blocked"]
            .as_str()
            .unwrap()
            .contains("has started")
    );
    assert_eq!(history["redo"]["label"], "Cut plan item");
    history_step(&journal, "redo").await?;
    let redone = journal.plans().await?.pop().unwrap();
    assert_eq!(redone.items[2].state, crate::plans::ItemState::Cut);
    assert_eq!(redone.items[1].state, crate::plans::ItemState::Running);
    assert_eq!(redone.state, State::Held);
    Ok(())
}

#[tokio::test]
async fn restoring_an_older_cut_keeps_the_other_edits_and_uses_that_cut_for_redo() -> Result<()> {
    let (root, mut journal) = fixture().await?;
    let plan = editable_plan(&journal).await?;
    let first = journal
        .plan_command(command(
            &plan,
            Action::Cut {
                item_id: plan.items[2].plan_item_id,
            },
        ))
        .await?;
    let cut_entry = journal.snapshot().await?["editHistory"]["undo"]["entryId"].clone();
    let moved = journal
        .plan_command(command(
            &first,
            Action::Move {
                item_id: first.items[1].plan_item_id,
                direction: crate::plan_store::Direction::Earlier,
            },
        ))
        .await?;
    let move_entry = journal.snapshot().await?["editHistory"]["undo"]["entryId"].clone();
    let request = command(
        &moved,
        Action::Restore {
            item_id: moved.items[2].plan_item_id,
        },
    );
    let restored = journal.plan_command(request.clone()).await?;
    assert_eq!(restored.state, State::Held);
    assert_eq!(restored.items[1].order, moved.items[1].order);
    assert_eq!(restored.items[2].state, crate::plans::ItemState::Planned);
    let history = journal.snapshot().await?["editHistory"].clone();
    assert_eq!(history["undo"]["entryId"], move_entry);
    assert_eq!(history["redo"]["entryId"], cut_entry);
    assert!(history["undo"]["blocked"].is_null());
    assert!(history["redo"]["blocked"].is_null());
    assert_eq!(
        journal.plan_command(request.clone()).await?.rev,
        restored.rev
    );
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM operations WHERE kind='plan.edit'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        2
    );
    journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(
        journal.snapshot().await?["editHistory"]["redo"]["entryId"],
        cut_entry
    );
    history_step(&journal, "undo").await?;
    let history = journal.snapshot().await?["editHistory"].clone();
    assert!(history["undo"].is_null());
    assert_eq!(history["redo"]["entryId"], cut_entry);
    history_step(&journal, "redo").await?;
    let redone = journal.plans().await?.pop().unwrap();
    assert_eq!(redone.items[2].state, crate::plans::ItemState::Cut);
    assert_eq!(redone.items[1].order, plan.items[1].order);
    assert_eq!(redone.state, State::Held);
    history_step(&journal, "redo").await?;
    let redone = journal.plans().await?.pop().unwrap();
    assert_eq!(redone.items[1].order, moved.items[1].order);
    assert_eq!(journal.plan_command(request).await?.rev, redone.rev);
    assert_eq!(
        journal.plans().await?[0].items[2].state,
        crate::plans::ItemState::Cut
    );
    Ok(())
}

#[tokio::test]
async fn restoring_a_cut_does_not_revive_its_previous_start_request() -> Result<()> {
    let (root, journal) = fixture().await?;
    let mut plan = editable_plan(&journal).await?;
    plan.items[2].start_requested = true;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    crate::clock::register(&db)?;
    crate::plan_store::save(&db, &plan, "dummy.start-request")?;
    let cut = journal
        .plan_command(command(
            &plan,
            Action::Cut {
                item_id: plan.items[2].plan_item_id,
            },
        ))
        .await?;
    let restored = journal
        .plan_command(command(
            &cut,
            Action::Restore {
                item_id: cut.items[2].plan_item_id,
            },
        ))
        .await?;
    assert_eq!(restored.state, State::Held);
    assert!(!restored.items[2].start_requested);
    journal.advance_plans().await?;
    assert!(
        journal.plans().await?[0]
            .items
            .iter()
            .all(|item| item.utterance_id.is_none())
    );
    history_step(&journal, "redo").await?;
    history_step(&journal, "undo").await?;
    let undone = journal.plans().await?.pop().unwrap();
    assert_eq!(undone.state, State::Held);
    assert_eq!(undone.items[2].state, crate::plans::ItemState::Planned);
    assert!(!undone.items[2].start_requested);
    Ok(())
}

#[tokio::test]
async fn restoring_a_cut_is_atomic_with_its_existing_history_entry() -> Result<()> {
    let (root, journal) = fixture().await?;
    let plan = editable_plan(&journal).await?;
    let cut = journal
        .plan_command(command(
            &plan,
            Action::Cut {
                item_id: plan.items[2].plan_item_id,
            },
        ))
        .await?;
    let before = journal.snapshot().await?["editHistory"].clone();
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER fail_dummy_restore BEFORE UPDATE ON operations WHEN OLD.kind='plan.edit' BEGIN SELECT RAISE(ABORT,'dummy restore write failure'); END;")?;
    let request = command(
        &cut,
        Action::Restore {
            item_id: cut.items[2].plan_item_id,
        },
    );
    assert!(journal.plan_command(request.clone()).await.is_err());
    assert_eq!(journal.plans().await?[0].rev, cut.rev);
    assert_eq!(
        journal.plans().await?[0].items[2].state,
        crate::plans::ItemState::Cut
    );
    assert_eq!(journal.snapshot().await?["editHistory"], before);
    assert!(journal.operation(request.op_id).await?.is_none());
    db.execute_batch("DROP TRIGGER fail_dummy_restore;")?;
    journal.plan_command(request).await?;
    assert!(journal.snapshot().await?["editHistory"]["undo"].is_null());
    Ok(())
}

#[tokio::test]
async fn assigning_real_tracks_keeps_pre_start_undo_and_reopen_history() -> Result<()> {
    use crate::plan_store::Direction;
    let (root, journal) = fixture().await?;
    let plan = editable_plan(&journal).await?;
    let moved = journal
        .plan_command(command(
            &plan,
            Action::Move {
                item_id: plan.items[1].plan_item_id,
                direction: Direction::Earlier,
            },
        ))
        .await?;
    journal
        .plan_command(command(&moved, Action::Review))
        .await?;
    let started = journal.plan_command(command(&moved, Action::Start)).await?;
    assert!(!started.items[0].track_ref.starts_with("new:"));
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    history_step(&journal, "undo").await?;
    let undone = journal.plans().await?.pop().unwrap();
    assert_eq!(undone.items[0].order, 0);
    assert_eq!(undone.items[0].track_ref, started.items[0].track_ref);
    assert_eq!(undone.state, State::Running);
    Ok(())
}

#[tokio::test]
async fn plan_changes_and_taken_back_words_share_one_stack_and_branch() -> Result<()> {
    use crate::plan_store::Direction;
    let (_root, journal) = fixture().await?;
    let plan = editable_plan(&journal).await?;
    journal
        .plan_command(command(
            &plan,
            Action::Move {
                item_id: plan.items[1].plan_item_id,
                direction: Direction::Earlier,
            },
        ))
        .await?;
    let word = Uuid::now_v7();
    journal
        .stage_utterance(
            word,
            "Dummy words to take back".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    journal.cut_input(Uuid::now_v7(), word.to_string()).await?;
    assert_eq!(
        journal.snapshot().await?["editHistory"]["undo"]["label"],
        "Take back"
    );
    history_step(&journal, "undo").await?;
    assert_ne!(
        journal.read_utterance(word.to_string()).await?["state"],
        "cut"
    );
    assert_eq!(
        journal.snapshot().await?["editHistory"]["undo"]["label"],
        "Move plan item"
    );
    history_step(&journal, "undo").await?;
    assert_eq!(journal.plans().await?[0].items[0].order, 0);
    history_step(&journal, "redo").await?;
    assert_eq!(journal.plans().await?[0].items[1].order, 0);
    let current = journal.plans().await?.pop().unwrap();
    journal
        .plan_command(command(
            &current,
            Action::Cut {
                item_id: current.items[2].plan_item_id,
            },
        ))
        .await?;
    assert!(journal.snapshot().await?["editHistory"]["redo"].is_null());
    assert!(
        !journal.snapshot().await?["editHistory"]["takenBack"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["id"]
                == current.items[2]
                    .utterance_id
                    .map(|id| id.to_string())
                    .unwrap_or_default())
    );
    Ok(())
}

#[tokio::test]
async fn failing_to_save_history_rolls_back_the_plan_and_pending_input() -> Result<()> {
    use crate::plan_store::Direction;
    let (root, journal) = fixture().await?;
    let plan = editable_plan(&journal).await?;
    journal.plan_command(command(&plan, Action::Review)).await?;
    journal.plan_command(command(&plan, Action::Start)).await?;
    journal.advance_plans().await?;
    let before = journal.plans().await?.pop().unwrap();
    let input = before.items[0].utterance_id.unwrap();
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER reject_plan_history BEFORE INSERT ON operations WHEN NEW.kind='plan.edit' BEGIN SELECT RAISE(ABORT,'dummy save failure'); END;")?;
    assert!(
        journal
            .plan_command(command(
                &before,
                Action::Move {
                    item_id: before.items[1].plan_item_id,
                    direction: Direction::Earlier
                }
            ))
            .await
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(journal.plans().await?.pop().unwrap())?,
        serde_json::to_value(before)?
    );
    assert_eq!(
        journal.read_utterance(input.to_string()).await?["state"],
        "placed"
    );
    db.execute_batch("DROP TRIGGER reject_plan_history;")?;
    Ok(())
}

#[tokio::test]
async fn moves_reject_cross_folder_dependencies_and_accept_an_independent_item() -> Result<()> {
    use crate::plans::Dependency;
    let (root, journal) = fixture_folders(2).await?;
    let project: crate::project::Project =
        serde_json::from_value(journal.snapshot().await?["project"].clone())?;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    crate::clock::register(&db)?;
    let plan = editable_plan(&journal).await?;
    let other = project
        .folders
        .iter()
        .find(|folder| folder.id() != plan.items[0].folder_id)
        .unwrap();
    let target = Uuid::now_v7().to_string();
    db.execute(
        "INSERT INTO tracks VALUES(?1,'Z','Dummy target','waiting')",
        [&target],
    )?;
    crate::folders::bind(&db, &target, &other.id())?;
    let dependent = dependencies(
        &journal,
        &plan,
        plan.items[0].plan_item_id,
        vec![Dependency {
            id: plan.items[2].plan_item_id.to_string(),
            inherit: true,
        }],
    )
    .await?;
    let rejected = journal
        .plan_command(command(
            &dependent,
            Action::MoveTo {
                item_id: dependent.items[0].plan_item_id,
                track_id: target.clone(),
            },
        ))
        .await;
    assert!(rejected.unwrap_err().to_string().contains("dependencies"));
    assert_eq!(journal.plans().await?[0].rev, dependent.rev);
    let independent = dependencies(
        &journal,
        &dependent,
        dependent.items[0].plan_item_id,
        vec![],
    )
    .await?;
    let moved = journal
        .plan_command(command(
            &independent,
            Action::MoveTo {
                item_id: independent.items[0].plan_item_id,
                track_id: target.clone(),
            },
        ))
        .await?;
    assert_eq!(moved.items[0].track_ref, target);
    assert_eq!(moved.items[0].folder_id, other.id());
    history_step(&journal, "undo").await?;
    assert_eq!(
        journal.plans().await?[0].items[0].folder_id,
        plan.items[0].folder_id
    );
    Ok(())
}

#[tokio::test]
async fn repeating_a_held_plan_does_not_add_an_empty_history_entry() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let plan = editable_plan(&journal).await?;
    let held = journal.plan_command(command(&plan, Action::Hold)).await?;
    let before = journal.snapshot().await?["editHistory"].clone();
    let repeated = journal.plan_command(command(&held, Action::Hold)).await?;
    assert_eq!(repeated.rev, held.rev);
    assert_eq!(journal.snapshot().await?["editHistory"], before);
    history_step(&journal, "undo").await?;
    assert_eq!(journal.plans().await?[0].state, State::Draft);
    Ok(())
}

#[tokio::test]
async fn file_preparation_blocks_rearrangement_but_does_not_block_cut() -> Result<()> {
    use crate::plan_store::Direction;
    let (root, journal) = fixture().await?;
    let plan = editable_plan(&journal).await?;
    journal.plan_command(command(&plan, Action::Review)).await?;
    journal.plan_command(command(&plan, Action::Start)).await?;
    journal.advance_plans().await?;
    let queued = journal.plans().await?.pop().unwrap();
    let input = queued.items[0].utterance_id.unwrap();
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute(
        "INSERT INTO operations VALUES(?1,'workspace.inherit',?2,'prepared')",
        rusqlite::params![
            Uuid::now_v7().to_string(),
            serde_json::json!({"dependencies":{"input":input.to_string()}}).to_string()
        ],
    )?;
    assert!(
        journal
            .plan_command(command(&queued, Action::Retry))
            .await
            .unwrap_err()
            .to_string()
            .contains("preparation")
    );
    assert!(
        journal
            .plan_command(command(
                &queued,
                Action::Move {
                    item_id: queued.items[1].plan_item_id,
                    direction: Direction::Earlier
                }
            ))
            .await
            .unwrap_err()
            .to_string()
            .contains("preparation")
    );
    let cut = journal
        .plan_command(command(
            &queued,
            Action::Cut {
                item_id: queued.items[0].plan_item_id,
            },
        ))
        .await?;
    assert_eq!(cut.items[0].state, crate::plans::ItemState::Cut);
    assert_eq!(
        journal.read_utterance(input.to_string()).await?["planRunnable"],
        false
    );
    Ok(())
}

#[tokio::test]
async fn another_plans_inheritance_is_included_when_editing_a_track_cycle() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let first = editable_plan(&journal).await?;
    journal
        .plan_command(command(&first, Action::Review))
        .await?;
    let first = journal.plan_command(command(&first, Action::Start)).await?;
    let second = queue(&journal).await?;
    let mut value = draft(&journal, &second).await?;
    value.items[0].track_ref = first.items[2].track_ref.clone();
    value.items[0].after = vec![crate::plans::Dependency {
        id: first.items[0].plan_item_id.to_string(),
        inherit: true,
    }];
    journal
        .begin_plan(second.id, second.generation.id, second.rev)
        .await?;
    journal
        .generated_plan(
            second.id,
            second.generation.id,
            second.rev,
            Generated::Draft(value),
            vec![],
        )
        .await?;
    let second = journal
        .plans()
        .await?
        .into_iter()
        .find(|plan| plan.id == second.id)
        .unwrap();
    let result = dependencies(
        &journal,
        &first,
        first.items[1].plan_item_id,
        vec![crate::plans::Dependency {
            id: second.items[0].plan_item_id.to_string(),
            inherit: true,
        }],
    )
    .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("cycle between tracks")
    );
    journal
        .plan_command(command(
            &second,
            Action::Cut {
                item_id: second.items[0].plan_item_id,
            },
        ))
        .await?;
    let first = journal
        .plans()
        .await?
        .into_iter()
        .find(|plan| plan.id == first.id)
        .unwrap();
    let changed = dependencies(
        &journal,
        &first,
        first.items[1].plan_item_id,
        vec![crate::plans::Dependency {
            id: second.items[0].plan_item_id.to_string(),
            inherit: true,
        }],
    )
    .await?;
    assert_eq!(
        changed.items[1].after[0].id,
        second.items[0].plan_item_id.to_string()
    );
    Ok(())
}

async fn locked_move_fixture(
    chain: bool,
) -> Result<(tempfile::TempDir, Journal, Plan, String, String)> {
    let (root, journal) = fixture().await?;
    let folder = journal
        .plan_context(Uuid::now_v7())
        .await?
        .folders
        .into_iter()
        .next()
        .unwrap();
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let a = Uuid::now_v7().to_string();
    let b = Uuid::now_v7().to_string();
    for (id, label) in [(&a, "A"), (&b, "B")] {
        db.execute(
            "INSERT INTO tracks VALUES(?1,?2,'Dummy move target','waiting')",
            rusqlite::params![id, label],
        )?;
        crate::folders::bind(&db, id, &folder)?;
    }
    journal
        .stage_document(Submission {
            op_id: Uuid::now_v7(),
            text: "Plan three dummy jobs on A.".into(),
            target: Target::Lock(a.clone()),
            selected_track_id: Some(a.clone()),
            attachment_ids: vec![],
            sources: None,
            execute_allowed: false,
            plan_item: None,
        })
        .await?;
    let plan = journal.plans().await?.pop().unwrap();
    let mut content = draft(&journal, &plan).await?;
    let first = content.items[0].clone();
    content.items = (0..3)
        .map(|index| {
            let mut item = first.clone();
            item.id = format!("dummy-{index}");
            item.title = format!("Dummy move {index}");
            item.track_ref = a.clone();
            if chain && index > 0 {
                item.after.push(crate::plans::Dependency {
                    id: format!("dummy-{}", index - 1),
                    inherit: true,
                });
            }
            item
        })
        .collect();
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
                Generated::Draft(content),
                vec![]
            )
            .await?
    );
    let ready = journal.plans().await?.pop().unwrap();
    assert_eq!(ready.generation.state, crate::plans::GenerationState::Ready);
    Ok((root, journal, ready, a, b))
}

#[tokio::test]
async fn unlock_move_is_atomic_and_history_restores_the_source_lock_after_restart() -> Result<()> {
    let (root, journal, plan, a, b) = locked_move_fixture(false).await?;
    let source = journal.read_utterance(plan.source_id.clone()).await?;
    assert!(
        journal
            .plan_command(command(
                &plan,
                Action::MoveTo {
                    item_id: plan.items[1].plan_item_id,
                    track_id: b.clone()
                }
            ))
            .await
            .is_err()
    );
    assert!(
        journal
            .plan_command(command(
                &plan,
                Action::UnlockAndMove {
                    item_id: plan.items[1].plan_item_id,
                    track_id: Uuid::now_v7().to_string()
                }
            ))
            .await
            .is_err()
    );
    let request = command(
        &plan,
        Action::UnlockAndMove {
            item_id: plan.items[1].plan_item_id,
            track_id: b.clone(),
        },
    );
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER reject_unlock_history BEFORE INSERT ON operations WHEN NEW.kind='plan.edit' BEGIN SELECT RAISE(ABORT,'dummy history failure'); END;")?;
    assert!(journal.plan_command(request.clone()).await.is_err());
    assert_eq!(
        serde_json::to_value(journal.plans().await?.pop().unwrap())?,
        serde_json::to_value(&plan)?
    );
    assert_eq!(
        journal.read_utterance(plan.source_id.clone()).await?,
        source
    );
    db.execute_batch("DROP TRIGGER reject_unlock_history;")?;
    let moved = journal.plan_command(request.clone()).await?;
    assert_eq!(moved.rev, plan.rev + 1);
    assert_eq!(moved.source_revision, plan.source_revision + 1);
    assert_eq!(moved.items[1].track_ref, b);
    assert!(moved.locked_track.is_none());
    assert_eq!(moved.items[0], plan.items[0]);
    assert_eq!(moved.items[2], plan.items[2]);
    assert_eq!(
        journal.read_utterance(plan.source_id.clone()).await?["target"]["mode"],
        "none"
    );
    assert_eq!(journal.plan_command(request).await?.rev, moved.rev);
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert!(
        journal
            .plan_command(command(
                &plan,
                Action::UnlockAndMove {
                    item_id: plan.items[1].plan_item_id,
                    track_id: b.clone()
                }
            ))
            .await
            .is_err()
    );
    history_step(&journal, "undo").await?;
    let undone = journal.plans().await?.pop().unwrap();
    assert_eq!(undone.items[1].track_ref, a);
    assert_eq!(undone.locked_track, Some(a.clone()));
    assert_eq!(undone.source_revision, plan.source_revision + 2);
    assert_eq!(
        journal.read_utterance(plan.source_id.clone()).await?["target"]["trackId"],
        a
    );
    history_step(&journal, "redo").await?;
    let redone = journal.plans().await?.pop().unwrap();
    assert_eq!(redone.items[1].track_ref, b);
    assert!(redone.locked_track.is_none());
    assert_eq!(redone.source_revision, plan.source_revision + 3);
    assert_eq!(
        journal.read_utterance(plan.source_id.clone()).await?["target"]["mode"],
        "none"
    );
    db.execute(
        "UPDATE utterances SET revision=revision+1 WHERE id=?1",
        [&plan.source_id],
    )?;
    assert!(
        journal.snapshot().await?["editHistory"]["undo"]["blocked"]
            .as_str()
            .unwrap()
            .contains("original request")
    );
    assert!(history_step(&journal, "undo").await.is_err());
    assert_eq!(journal.plans().await?.pop().unwrap().items[1].track_ref, b);
    Ok(())
}

#[tokio::test]
async fn unlock_move_rejects_an_inheritance_cycle_without_changing_the_lock() -> Result<()> {
    let (_root, journal, plan, a, b) = locked_move_fixture(true).await?;
    let source = journal.read_utterance(plan.source_id.clone()).await?;
    let error = journal
        .plan_command(command(
            &plan,
            Action::UnlockAndMove {
                item_id: plan.items[1].plan_item_id,
                track_id: b,
            },
        ))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("inheritance"));
    let after = journal.plans().await?.pop().unwrap();
    assert_eq!(serde_json::to_value(after)?, serde_json::to_value(&plan)?);
    assert_eq!(journal.read_utterance(plan.source_id).await?, source);
    assert_eq!(plan.locked_track, Some(a));
    Ok(())
}

#[tokio::test]
async fn unlock_move_and_undo_leave_other_running_items_unchanged() -> Result<()> {
    let (root, journal, mut plan, a, b) = locked_move_fixture(false).await?;
    plan.state = State::Running;
    plan.items[0].state = crate::plans::ItemState::Running;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    crate::clock::register(&db)?;
    crate::plan_store::save(&db, &plan, "dummy.running-item")?;
    let original = plan.items[0].clone();
    let moved = journal
        .plan_command(command(
            &plan,
            Action::UnlockAndMove {
                item_id: plan.items[1].plan_item_id,
                track_id: b,
            },
        ))
        .await?;
    assert_eq!(moved.items[0], original);
    assert_eq!(moved.state, State::Running);
    history_step(&journal, "undo").await?;
    let restored = journal.plans().await?.pop().unwrap();
    assert_eq!(restored.items[0], original);
    assert_eq!(restored.state, State::Running);
    assert_eq!(restored.locked_track, Some(a));
    assert!(
        journal
            .plan_command(command(
                &restored,
                Action::UnlockAndMove {
                    item_id: plan.items[0].plan_item_id,
                    track_id: Uuid::now_v7().to_string()
                }
            ))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn lost_destinations_are_rejected_at_generation_start_and_admission() -> Result<()> {
    let (root, journal, plan, a, _b) = locked_move_fixture(false).await?;
    let mut worker = crate::runtime::WorkerState {
        track_id: a.clone(),
        phase: "lost".into(),
        session_id: None,
        secret_revision: None,
        identity: None,
        detail: None,
        active_turn: None,
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    journal.worker_state(worker.clone()).await?;
    let context = journal.plan_context(plan.id).await?;
    assert!(!context.tracks.contains_key(&a));
    let mut content = draft(&journal, &plan).await?;
    content.items[0].track_ref = a.clone();
    assert!(crate::plans::materialize(Uuid::now_v7(), None, &content, &context).is_err());
    journal.plan_command(command(&plan, Action::Review)).await?;
    let failure = journal
        .plan_command(command(&plan, Action::Start))
        .await
        .unwrap_err();
    assert!(
        failure
            .to_string()
            .contains("tracks or dependencies changed")
    );
    assert_eq!(journal.plans().await?.pop().unwrap().rev, plan.rev);
    worker.phase = "ready".into();
    journal.worker_state(worker.clone()).await?;
    let running = journal.plan_command(command(&plan, Action::Start)).await?;
    journal.advance_plans().await?;
    let queued = journal.plans().await?.pop().unwrap();
    let input = queued.items[0].utterance_id.unwrap();
    worker.phase = "lost".into();
    journal.worker_state(worker).await?;
    let mut db = rusqlite::Connection::open(root.path().join("db"))?;
    let tx = db.transaction()?;
    assert!(crate::plan_execution::admit(&tx, &input.to_string()).is_err());
    tx.rollback()?;
    journal.advance_plans().await?;
    let held = journal.plans().await?.pop().unwrap();
    assert_eq!(held.state, running.state);
    assert!(
        held.items
            .iter()
            .all(|item| item.state == crate::plans::ItemState::Planned)
    );
    assert!(
        held.items[0]
            .detail
            .as_deref()
            .unwrap()
            .contains("destination is unavailable")
    );
    Ok(())
}
