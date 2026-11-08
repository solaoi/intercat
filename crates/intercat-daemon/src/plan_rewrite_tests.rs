use super::*;
use crate::{
    journal::Journal,
    plan_store::{Command, Generated},
    plans::{Draft, DraftItem},
    secrets::Key,
};

async fn fixture() -> Result<(tempfile::TempDir, Journal, Plan)> {
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    std::fs::create_dir(&source)?;
    gix::init(&source)?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let project = crate::project::inspect(Uuid::now_v7(), "Dummy".into(), vec![source]).await?;
    let folder = project.folders[0].id();
    journal.configure_project(Uuid::now_v7(), project).await?;
    journal
        .stage_document(crate::request_sources::Submission {
            op_id: Uuid::now_v7(),
            text: "Two dummy jobs".into(),
            target: crate::decision::Target::New,
            selected_track_id: None,
            attachment_ids: vec![],
            sources: None,
            execute_allowed: false,
            plan_item: None,
        })
        .await?;
    let plan = journal.plans().await?.pop().unwrap();
    journal
        .begin_plan(plan.id, plan.generation.id, plan.rev)
        .await?;
    journal
        .generated_plan(
            plan.id,
            plan.generation.id,
            plan.rev,
            Generated::Draft(Draft {
                assumptions: vec![],
                conflicts: vec![],
                summary: "Two dummy jobs".into(),
                items: (0..2)
                    .map(|i| DraftItem {
                        id: format!("job-{i}"),
                        track_ref: format!("new:{}", i + 1),
                        folder_id: folder.clone(),
                        title: format!("Dummy {i}"),
                        instruction: format!("Write dummy-{i}.txt"),
                        constraints: vec![],
                        done_when: "Dummy file exists".into(),
                        after: vec![],
                    })
                    .collect(),
            }),
            vec![],
        )
        .await?;
    let plan = journal.plans().await?.pop().unwrap();
    journal
        .plan_command(Command {
            op_id: Uuid::now_v7(),
            plan_id: plan.id,
            revision: plan.rev,
            action: crate::plan_store::Action::Review,
        })
        .await?;
    let plan = journal
        .plan_command(Command {
            op_id: Uuid::now_v7(),
            plan_id: plan.id,
            revision: plan.rev,
            action: crate::plan_store::Action::Start,
        })
        .await?;
    journal.advance_plans().await?;
    Ok((root, journal, plan))
}
async fn begin(journal: &Journal, plan: &Plan, owner: Uuid, token: Uuid) -> Result<Plan> {
    Ok(journal
        .edit_plan(Request {
            owner,
            action: Action::Begin {
                plan_id: plan.id,
                item_id: plan.items[0].plan_item_id,
                token,
                revision: plan.rev,
            },
        })
        .await?
        .unwrap())
}

#[tokio::test]
async fn lease_holds_only_its_item_and_expiration_does_not_start_it() -> Result<()> {
    let (root, journal, plan) = fixture().await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let prior = journal.plans().await?.pop().unwrap();
    let input = prior.items[0].utterance_id.unwrap();
    let held = begin(&journal, &plan, owner, token).await?;
    assert_eq!(held.items[0].state, ItemState::Editing);
    assert_eq!(held.items[1].state, ItemState::Planned);
    assert_eq!(
        journal.read_utterance(input.to_string()).await?["planRunnable"],
        false
    );
    assert!(
        begin(&journal, &held, Uuid::now_v7(), Uuid::now_v7())
            .await
            .is_err()
    );
    let renewed = journal
        .edit_plan(Request {
            owner,
            action: Action::Renew {
                plan_id: held.id,
                token,
            },
        })
        .await?
        .unwrap();
    assert_eq!(renewed.rev, held.rev);
    let paused = journal
        .plan_command(Command {
            op_id: Uuid::now_v7(),
            plan_id: renewed.id,
            revision: renewed.rev,
            action: crate::plan_store::Action::Hold,
        })
        .await?;
    journal
        .plan_command(Command {
            op_id: Uuid::now_v7(),
            plan_id: paused.id,
            revision: paused.rev,
            action: crate::plan_store::Action::Review,
        })
        .await?;
    assert!(
        journal
            .plan_command(Command {
                op_id: Uuid::now_v7(),
                plan_id: paused.id,
                revision: paused.rev,
                action: crate::plan_store::Action::Start,
            })
            .await
            .is_err()
    );
    let mut later = Stamp::now();
    later.mono_ms += 31_000;
    let mut db = Connection::open(root.path().join("db"))?;
    crate::clock::register(&db)?;
    expire(&mut db, &later)?;
    journal.advance_plans().await?;
    let expired = journal.plans().await?.pop().unwrap();
    assert!(expired.rewrite.is_none());
    assert_eq!(expired.state, State::Held);
    assert_eq!(expired.items[0].state, ItemState::Planned);
    assert!(expired.items[0].utterance_id.is_none());
    Ok(())
}

#[tokio::test]
async fn generated_revision_waits_for_acceptance_and_undo_keeps_other_running_work() -> Result<()> {
    let (root, journal, plan) = fixture().await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let held = begin(&journal, &plan, owner, token).await?;
    let submitted = journal
        .edit_plan(Request {
            owner,
            action: Action::Submit {
                plan_id: held.id,
                token,
                revision: held.rev,
                op_id: Uuid::now_v7(),
                text: "Use a blue dummy badge".into(),
            },
        })
        .await?
        .unwrap();
    assert!(
        journal
            .begin_plan(submitted.id, submitted.generation.id, submitted.rev)
            .await?
    );
    let item = &submitted.items[0];
    let proposal = DraftItem {
        id: item.plan_item_id.to_string(),
        track_ref: item.track_ref.clone(),
        folder_id: item.folder_id.clone(),
        title: "Blue dummy".into(),
        instruction: "Write a blue dummy badge.".into(),
        constraints: vec![],
        done_when: "A blue dummy badge exists".into(),
        after: vec![],
    };
    assert!(
        journal
            .generated_plan(
                submitted.id,
                submitted.generation.id,
                submitted.rev,
                Generated::Draft(Draft {
                    assumptions: vec![],
                    conflicts: vec![],
                    summary: submitted.summary.clone(),
                    items: vec![proposal]
                }),
                vec![]
            )
            .await?
    );
    let ready = journal.plans().await?.pop().unwrap();
    assert_eq!(ready.items[0].state, ItemState::Editing);
    assert_eq!(ready.items[0].instruction, plan.items[0].instruction);
    assert!(ready.rewrite.as_ref().unwrap().proposal.is_some());
    let db = Connection::open(root.path().join("db"))?;
    crate::clock::register(&db)?;
    let mut current = ready.clone();
    current.items[1].state = ItemState::Running;
    crate::plan_store::save(&db, &current, "dummy.other-running")?;
    let action = Action::Apply {
        plan_id: ready.id,
        token,
        revision: ready.rev,
        op_id: Uuid::now_v7(),
    };
    let applied = journal
        .edit_plan(Request {
            owner,
            action: action.clone(),
        })
        .await?
        .unwrap();
    assert_eq!(applied.items[0].instruction, "Write a blue dummy badge.");
    assert_eq!(applied.items[1].state, ItemState::Running);
    assert_eq!(applied.items[0].state, ItemState::Planned);
    assert!(applied.rewrite.is_none());
    assert_eq!(
        journal
            .edit_plan(Request {
                owner: Uuid::now_v7(),
                action
            })
            .await?
            .unwrap()
            .rev,
        applied.rev
    );
    let snapshot = journal.snapshot().await?;
    journal
        .edit_history(crate::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: crate::timeline_history::Action::Undo {
                entry: snapshot["editHistory"]["undo"]["entryId"]
                    .as_str()
                    .unwrap()
                    .into(),
            },
        })
        .await?;
    let restored = journal.plans().await?.pop().unwrap();
    assert_eq!(restored.items[0].instruction, plan.items[0].instruction);
    assert_eq!(restored.items[1].state, ItemState::Running);
    Ok(())
}

#[tokio::test]
async fn dependency_edit_keeps_its_lease_on_rejection_and_replays_only_the_saved_change()
-> Result<()> {
    let (_root, journal, plan) = fixture().await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let held = begin(&journal, &plan, owner, token).await?;
    let cycle = Action::Dependencies {
        plan_id: held.id,
        token,
        revision: held.rev,
        op_id: Uuid::now_v7(),
        after: vec![crate::plans::Dependency {
            id: held.items[0].plan_item_id.to_string(),
            inherit: true,
        }],
    };
    assert!(
        journal
            .edit_plan(Request {
                owner,
                action: cycle
            })
            .await
            .is_err()
    );
    let rejected = journal.plans().await?.pop().unwrap();
    assert_eq!(rejected.rev, held.rev);
    assert_eq!(rejected.items[0].state, ItemState::Editing);
    let action = Action::Dependencies {
        plan_id: held.id,
        token,
        revision: held.rev,
        op_id: Uuid::now_v7(),
        after: vec![crate::plans::Dependency {
            id: held.items[1].plan_item_id.to_string(),
            inherit: false,
        }],
    };
    let changed = journal
        .edit_plan(Request {
            owner,
            action: action.clone(),
        })
        .await?
        .unwrap();
    assert!(changed.rewrite.is_none());
    assert_eq!(changed.items[0].state, ItemState::Planned);
    assert_eq!(
        changed.items[0].after[0].id,
        held.items[1].plan_item_id.to_string()
    );
    let replay = journal
        .edit_plan(Request {
            owner: Uuid::now_v7(),
            action,
        })
        .await?
        .unwrap();
    assert_eq!(changed.rev, replay.rev);
    let history = journal.snapshot().await?["editHistory"].clone();
    journal
        .edit_history(crate::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: crate::timeline_history::Action::Undo {
                entry: history["undo"]["entryId"].as_str().unwrap().into(),
            },
        })
        .await?;
    let undone = journal.plans().await?.pop().unwrap();
    assert!(undone.items[0].after.is_empty());
    assert_eq!(undone.items[0].state, ItemState::Planned);
    Ok(())
}

#[tokio::test]
async fn item_revision_rejects_routing_changes_and_late_results_after_a_plan_change() -> Result<()>
{
    let (_root, journal, plan) = fixture().await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let held = begin(&journal, &plan, owner, token).await?;
    let submitted = journal
        .edit_plan(Request {
            owner,
            action: Action::Submit {
                plan_id: held.id,
                token,
                revision: held.rev,
                op_id: Uuid::now_v7(),
                text: "Use a blue dummy badge.".into(),
            },
        })
        .await?
        .unwrap();
    journal
        .begin_plan(submitted.id, submitted.generation.id, submitted.rev)
        .await?;
    let item = &submitted.items[0];
    let mut proposal = DraftItem {
        id: item.plan_item_id.to_string(),
        track_ref: item.track_ref.clone(),
        folder_id: item.folder_id.clone(),
        title: "Blue badge".into(),
        instruction: "Write a blue dummy badge.".into(),
        constraints: vec![],
        done_when: "A blue dummy badge exists.".into(),
        after: vec![crate::plans::Dependency {
            id: submitted.items[1].plan_item_id.to_string(),
            inherit: false,
        }],
    };
    journal
        .generated_plan(
            submitted.id,
            submitted.generation.id,
            submitted.rev,
            Generated::Draft(Draft {
                assumptions: vec![],
                conflicts: vec![],
                summary: submitted.summary.clone(),
                items: vec![proposal.clone()],
            }),
            vec![],
        )
        .await?;
    let retrying = journal.plans().await?.pop().unwrap();
    assert_eq!(retrying.generation.state, GenerationState::Queued);
    assert!(retrying.rewrite.as_ref().unwrap().proposal.is_none());
    assert_eq!(retrying.items[0].instruction, item.instruction);
    journal
        .begin_plan(retrying.id, retrying.generation.id, retrying.rev)
        .await?;
    let held_plan = journal
        .plan_command(Command {
            op_id: Uuid::now_v7(),
            plan_id: retrying.id,
            revision: retrying.rev,
            action: crate::plan_store::Action::Hold,
        })
        .await?;
    proposal.after.clear();
    assert!(
        !journal
            .generated_plan(
                retrying.id,
                retrying.generation.id,
                retrying.rev,
                Generated::Draft(Draft {
                    assumptions: vec![],
                    conflicts: vec![],
                    summary: retrying.summary,
                    items: vec![proposal]
                }),
                vec![]
            )
            .await?
    );
    assert!(
        journal
            .edit_plan(Request {
                owner,
                action: Action::Apply {
                    plan_id: held_plan.id,
                    token,
                    revision: retrying.rev,
                    op_id: Uuid::now_v7()
                }
            })
            .await
            .is_err()
    );
    let current = journal.plans().await?.pop().unwrap();
    assert_eq!(current.state, State::Held);
    assert!(current.rewrite.as_ref().unwrap().proposal.is_none());
    assert_eq!(current.items[0].instruction, item.instruction);
    Ok(())
}

#[tokio::test]
async fn cut_and_a_new_plan_revision_reject_late_rewrites() -> Result<()> {
    let (_root, journal, plan) = fixture().await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let held = begin(&journal, &plan, owner, token).await?;
    let submitted = journal
        .edit_plan(Request {
            owner,
            action: Action::Submit {
                plan_id: held.id,
                token,
                revision: held.rev,
                op_id: Uuid::now_v7(),
                text: "Change this dummy item".into(),
            },
        })
        .await?
        .unwrap();
    assert!(
        journal
            .begin_plan(submitted.id, submitted.generation.id, submitted.rev)
            .await?
    );
    let cut = journal
        .plan_command(Command {
            op_id: Uuid::now_v7(),
            plan_id: submitted.id,
            revision: submitted.rev,
            action: crate::plan_store::Action::Cut {
                item_id: submitted.items[0].plan_item_id,
            },
        })
        .await?;
    assert_eq!(cut.items[0].state, ItemState::Cut);
    assert!(cut.rewrite.is_none());
    assert!(
        !journal
            .generated_plan(
                submitted.id,
                submitted.generation.id,
                submitted.rev,
                Generated::Failed("Late dummy response".into()),
                vec![]
            )
            .await?
    );
    Ok(())
}

#[tokio::test]
#[ignore = "one paid dummy Wonder turn revising a held plan item"]
async fn real_planner_revises_only_the_held_dummy_item_before_acceptance() -> Result<()> {
    let (root, journal, plan) = fixture().await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let held = begin(&journal, &plan, owner, token).await?;
    let submitted=journal.edit_plan(Request{owner,action:Action::Submit{plan_id:held.id,token,revision:held.rev,op_id:Uuid::now_v7(),text:"Change only this item to write a blue robot badge. Keep the same track, folder and dependencies. Keep this a plan, do not create any files.".into()}}).await?.unwrap();
    let config = crate::runtime::Config {
        source: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
        binary: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
        credentials: std::env::var("INTERCAT_MUSE_AUTH")?.into(),
        path: "/usr/bin:/bin:/usr/sbin:/sbin".into(),
    };
    let rpc = root.path().join("rpc");
    std::fs::create_dir(&rpc)?;
    journal.set_worker_config(Uuid::now_v7(), config).await?;
    let capacity = std::sync::Arc::new(crate::scheduler::Capacity::new(1));
    let service = crate::planning::Service::start(
        crate::host_memory::Pool::new(),
        journal.clone(),
        root.path().into(),
        rpc,
        capacity.clone(),
        std::sync::Arc::new(crate::dispatch_control::Gate::new(false)),
    );
    let result = tokio::time::timeout(std::time::Duration::from_secs(125), async {
        loop {
            let current = journal.plans().await?.pop().unwrap();
            if current
                .rewrite
                .as_ref()
                .is_some_and(|edit| edit.proposal.is_some())
            {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(current);
            }
            if matches!(
                current.generation.state,
                GenerationState::Invalid | GenerationState::TimedOut | GenerationState::Failed
            ) {
                return Err(current
                    .generation
                    .detail
                    .unwrap_or_else(|| "Item revision failed".into())
                    .into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await;
    service.shutdown().await;
    let proposed = result??;
    assert_eq!(capacity.snapshot().active, 0);
    assert_eq!(
        proposed.items[0].plan_item_id,
        submitted.items[0].plan_item_id
    );
    assert!(
        proposed
            .rewrite
            .as_ref()
            .unwrap()
            .proposal
            .as_ref()
            .unwrap()
            .instruction
            .to_lowercase()
            .contains("blue")
    );
    assert_eq!(proposed.generation.state, GenerationState::Ready);
    assert!(proposed.rewrite.as_ref().unwrap().proposal.is_some());
    assert_eq!(proposed.items[0].instruction, plan.items[0].instruction);
    assert_eq!(proposed.items[0].state, ItemState::Editing);
    assert_eq!(proposed.items[1].instruction, plan.items[1].instruction);
    assert_eq!(
        std::fs::read_dir(root.path().join("planner-hosts"))?.count(),
        0
    );
    let applied = journal
        .edit_plan(Request {
            owner,
            action: Action::Apply {
                plan_id: proposed.id,
                token,
                revision: proposed.rev,
                op_id: Uuid::now_v7(),
            },
        })
        .await?
        .unwrap();
    assert!(applied.items[0].instruction.to_lowercase().contains("blue"));
    assert_eq!(applied.items[1].instruction, plan.items[1].instruction);
    assert!(
        journal.snapshot().await?["workers"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let output = std::path::PathBuf::from(std::env::var("INTERCAT_M3_REWRITE_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    std::fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"proposal":proposed,"applied":applied,"heldUntilAccepted":true,"otherItemUnchanged":true,"workersStarted":0,"hostCleaned":true,"paidTurns":proposed.generation.attempt + 1,"schedulerReleased":true}),
        )?,
    )?;
    Ok(())
}

#[tokio::test]
async fn disconnect_holds_unsent_edits_but_keeps_a_submitted_proposal_pending() -> Result<()> {
    let (_root, journal, plan) = fixture().await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let held = begin(&journal, &plan, owner, token).await?;
    assert_eq!(held.items[0].state, ItemState::Editing);
    journal
        .edit_plan(Request {
            owner,
            action: Action::Disconnect,
        })
        .await?;
    let cancelled = journal.plans().await?.pop().unwrap();
    assert!(cancelled.rewrite.is_none());
    assert_eq!(cancelled.state, State::Held);
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let held = begin(&journal, &cancelled, owner, token).await?;
    let submit = Action::Submit {
        plan_id: held.id,
        token,
        revision: held.rev,
        op_id: Uuid::now_v7(),
        text: "Revise the dummy badge.".into(),
    };
    let saved = journal
        .edit_plan(Request {
            owner,
            action: submit.clone(),
        })
        .await?
        .unwrap();
    journal
        .edit_plan(Request {
            owner,
            action: Action::Disconnect,
        })
        .await?;
    assert_eq!(journal.plans().await?[0].generation.id, saved.generation.id);
    assert_eq!(
        journal
            .edit_plan(Request {
                owner: Uuid::now_v7(),
                action: submit
            })
            .await?
            .unwrap()
            .rev,
        saved.rev
    );
    assert!(held.rev > plan.rev);
    Ok(())
}

#[tokio::test]
async fn overlong_rewrite_titles_leave_the_current_item_unchanged() -> Result<()> {
    let (_root, journal, plan) = fixture().await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let held = begin(&journal, &plan, owner, token).await?;
    let submitted = journal
        .edit_plan(Request {
            owner,
            action: Action::Submit {
                plan_id: held.id,
                token,
                revision: held.rev,
                op_id: Uuid::now_v7(),
                text: "Use a short dummy title".into(),
            },
        })
        .await?
        .unwrap();
    let before = submitted.items[0].clone();
    let mut current = submitted;
    for expected in [GenerationState::Queued, GenerationState::Invalid] {
        journal
            .begin_plan(current.id, current.generation.id, current.rev)
            .await?;
        let item = &current.items[0];
        journal
            .generated_plan(
                current.id,
                current.generation.id,
                current.rev,
                Generated::Draft(Draft {
                    assumptions: vec![],
                    conflicts: vec![],
                    summary: current.summary.clone(),
                    items: vec![DraftItem {
                        id: item.plan_item_id.to_string(),
                        track_ref: item.track_ref.clone(),
                        folder_id: item.folder_id.clone(),
                        title: "x".repeat(31),
                        instruction: "Revised dummy work".into(),
                        constraints: vec![],
                        done_when: item.done_when.clone(),
                        after: item.after.clone(),
                    }],
                }),
                vec![],
            )
            .await?;
        current = journal.plans().await?.pop().unwrap();
        assert_eq!(current.generation.state, expected);
        assert_eq!(current.items[0], before);
        assert!(current.rewrite.as_ref().unwrap().proposal.is_none());
        assert!(
            current
                .generation
                .detail
                .as_deref()
                .unwrap()
                .contains("30 displayed characters")
        );
    }
    Ok(())
}
