//! Plan generations and live request edits across the one-writer transfer.
//! All files, envelopes and summary text belong to the local dummy fixture.
use super::*;
use crate::{
    plan_store::{Action as PlanAction, Command as PlanCommand, Generated},
    plans::{Draft, DraftItem, GenerationState, Plan, State},
};

pub(super) async fn plan(f: &Fixture, track: Option<&str>) -> Result<Plan> {
    let id = Uuid::now_v7();
    f.journal
        .stage_utterance(
            id,
            "Own two-step plan".into(),
            track.map_or(Target::None, |track| Target::Lock(track.into())),
            None,
            vec![],
        )
        .await?;
    let input = f.journal.claim_input().await?.unwrap();
    assert_eq!(input.id, id.to_string());
    f.journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Plan,
                track_id: track.map(str::to_owned),
                boundary: None,
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    current(f, Uuid::new_v5(&id, b"plan")).await
}
pub(super) async fn current(f: &Fixture, id: Uuid) -> Result<Plan> {
    f.journal
        .plans()
        .await?
        .into_iter()
        .find(|plan| plan.id == id)
        .ok_or_else(|| "Missing own plan".into())
}
pub(super) fn draft(folder: &str, track: &str, summary: &str) -> Draft {
    Draft {
        assumptions: vec![],
        conflicts: vec![],
        summary: summary.into(),
        items: (0..2)
            .map(|i| DraftItem {
                id: format!("own-{i}"),
                track_ref: track.into(),
                folder_id: folder.into(),
                title: format!("Own step {i}"),
                instruction: format!("Inspect own-{i}.txt"),
                constraints: vec![],
                done_when: "Own file is inspected".into(),
                after: vec![],
            })
            .collect(),
    }
}

#[tokio::test]
async fn queued_and_running_planners_restart_for_the_current_writer_and_reject_late_results()
-> Result<()> {
    for running in [false, true] {
        let f = Fixture::new(1).await?;
        let job = summarized(&f).await?;
        let before = plan(&f, Some(&job.request.track_id)).await?;
        if running {
            assert!(
                f.journal
                    .begin_plan(before.id, before.generation.id, before.rev)
                    .await?
            );
        }
        transfer(&f, &job).await?;
        let moved = current(&f, before.id).await?;
        assert_eq!(
            moved.locked_track.as_deref(),
            Some(job.successor_track.as_str())
        );
        assert_ne!(moved.generation.id, before.generation.id);
        assert_eq!(moved.generation.state, GenerationState::Queued);
        assert!(moved.rev > before.rev);
        assert_eq!(moved.source_revision, before.source_revision + 1);
        assert!(
            !f.journal
                .generated_plan(
                    before.id,
                    before.generation.id,
                    before.rev,
                    Generated::Draft(draft(
                        &job.folder.id,
                        &job.request.track_id,
                        "Own stale plan"
                    )),
                    vec![]
                )
                .await?
        );
        assert!(
            f.journal
                .begin_plan(moved.id, moved.generation.id, moved.rev)
                .await?
        );
        assert!(
            f.journal
                .generated_plan(
                    moved.id,
                    moved.generation.id,
                    moved.rev,
                    Generated::Draft(draft(
                        &job.folder.id,
                        &job.successor_track,
                        "Own current plan"
                    )),
                    vec![]
                )
                .await?
        );
        let ready = current(&f, before.id).await?;
        assert_eq!(ready.summary, "Own current plan");
        assert_eq!(ready.generation.state, GenerationState::Ready);
        assert!(
            ready
                .items
                .iter()
                .all(|item| item.track_ref == job.successor_track)
        );
        assert!(
            !f.journal
                .generated_plan(
                    before.id,
                    before.generation.id,
                    before.rev,
                    Generated::Failed("Own delayed failure".into()),
                    vec![]
                )
                .await?
        );
        assert_eq!(
            serde_json::to_value(current(&f, before.id).await?)?,
            serde_json::to_value(ready)?
        );
        let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
        crate::plan_store::require_lock_source(&db, &current(&f, before.id).await?)?;
        assert!(crate::continuation::closed(&db, &job.request.track_id)?);
    }
    Ok(())
}

#[tokio::test]
async fn a_failed_plan_runs_as_one_on_the_successor_without_overtaking_its_summary() -> Result<()> {
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let before = plan(&f, Some(&job.request.track_id)).await?;
    assert!(
        f.journal
            .begin_plan(before.id, before.generation.id, before.rev)
            .await?
    );
    assert!(
        f.journal
            .generated_plan(
                before.id,
                before.generation.id,
                before.rev,
                Generated::TimedOut,
                vec![]
            )
            .await?
    );
    transfer(&f, &job).await?;
    let moved = current(&f, before.id).await?;
    let command = PlanCommand {
        op_id: Uuid::now_v7(),
        plan_id: moved.id,
        revision: moved.rev,
        action: PlanAction::RunAsOne,
    };
    let single = f.journal.plan_command(command.clone()).await?;
    assert_eq!(single.state, State::SingleJob);
    assert_eq!(f.journal.plan_command(command).await?.rev, single.rev);
    let input = f.journal.claim_input().await?.unwrap();
    assert_eq!(input.id, before.source_id);
    f.journal
        .apply_input(
            input.id.clone(),
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(job.request.track_id.clone()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let source = f.journal.read_utterance(input.id).await?;
    assert_eq!(source["target"]["trackId"], job.successor_track);
    assert_eq!(source["decision"]["trackId"], job.successor_track);
    assert_eq!(source["freshRunnable"], false);
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let owner = crate::continuation::start(&db, &job.successor_track)?.unwrap();
    assert!(!owner.ready);
    assert_ne!(owner.first_input, before.source_id);
    assert!(crate::continuation::allowed(
        &db,
        &job.successor_track,
        &owner.first_input
    )?);
    let command = Uuid::now_v7();
    let error=f.journal.reserve_turn(command,before.source_id.clone(),source["revision"].as_i64().unwrap(),job.successor_track.clone(),json!({"commandId":command,"sessionId":owner.session_id,"input":[{"type":"text","text":source["text"]}]}), None).await.unwrap_err();
    assert!(error.is::<crate::journal::StartSuperseded>());
    assert!(f.journal.operation(command).await?.is_none());
    assert!(
        !f.journal
            .generated_plan(
                before.id,
                before.generation.id,
                before.rev,
                Generated::Draft(draft(
                    &job.folder.id,
                    &job.request.track_id,
                    "Own late result"
                )),
                vec![]
            )
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn a_pending_single_job_plan_keeps_its_source_revision_and_destination_through_transfer()
-> Result<()> {
    for locked in [false, true] {
        let f = Fixture::new(1).await?;
        let job = summarized(&f).await?;
        let before = plan(&f, locked.then_some(job.request.track_id.as_str())).await?;
        assert!(
            f.journal
                .begin_plan(before.id, before.generation.id, before.rev)
                .await?
        );
        assert!(
            f.journal
                .generated_plan(
                    before.id,
                    before.generation.id,
                    before.rev,
                    Generated::TimedOut,
                    vec![]
                )
                .await?
        );
        let failed = current(&f, before.id).await?;
        let single = f
            .journal
            .plan_command(PlanCommand {
                op_id: Uuid::now_v7(),
                plan_id: failed.id,
                revision: failed.rev,
                action: PlanAction::RunAsOne,
            })
            .await?;
        let input = f.journal.claim_input().await?.unwrap();
        f.journal
            .apply_input(
                input.id.clone(),
                input.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Extend,
                    track_id: Some(job.request.track_id.clone()),
                    boundary: Some("now".into()),
                    command: None,
                    confidence: 1.,
                    inference_ms: 0.,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await?;
        transfer(&f, &job).await?;
        let moved = current(&f, before.id).await?;
        let source = f.journal.read_utterance(before.source_id.clone()).await?;
        assert_eq!(moved.state, State::SingleJob);
        assert!(moved.rev > single.rev);
        assert_eq!(source["revision"], moved.source_revision);
        assert_eq!(source["decision"]["trackId"], job.successor_track);
        assert_eq!(source["freshRunnable"], false);
        if locked {
            assert_eq!(source["target"]["trackId"], job.successor_track);
        } else {
            assert_eq!(source["target"]["mode"], "none");
        }
    }
    Ok(())
}

#[tokio::test]
async fn only_an_edit_of_the_transferred_plans_items_waits_for_the_editor() -> Result<()> {
    use crate::plan_rewrite::{Action, Request};
    for affected in [false, true] {
        let f = Fixture::new(2).await?;
        let job = summarized(&f).await?;
        let track = if affected {
            job.request.track_id.clone()
        } else {
            f.workspaces[1].track_id.clone()
        };
        let initial = plan(&f, Some(&track)).await?;
        assert!(
            f.journal
                .begin_plan(initial.id, initial.generation.id, initial.rev)
                .await?
        );
        assert!(
            f.journal
                .generated_plan(
                    initial.id,
                    initial.generation.id,
                    initial.rev,
                    Generated::Draft(draft(&job.folder.id, &track, "Own editable plan")),
                    vec![]
                )
                .await?
        );
        let ready = current(&f, initial.id).await?;
        let owner = Uuid::now_v7();
        let token = Uuid::now_v7();
        let editing = f
            .journal
            .edit_plan(Request {
                owner,
                action: Action::Begin {
                    plan_id: ready.id,
                    item_id: ready.items[0].plan_item_id,
                    token,
                    revision: ready.rev,
                },
            })
            .await?
            .unwrap();
        if affected {
            let error = crate::fresh_files::transfer(&f.journal, &job)
                .await
                .unwrap_err();
            assert!(error.is::<crate::fresh::Waiting>());
            assert_eq!(
                serde_json::to_value(current(&f, initial.id).await?)?,
                serde_json::to_value(&editing)?
            );
        } else {
            transfer(&f, &job).await?;
            assert_eq!(
                serde_json::to_value(current(&f, initial.id).await?)?,
                serde_json::to_value(&editing)?
            );
        }
        f.journal
            .edit_plan(Request {
                owner,
                action: Action::Cancel {
                    plan_id: editing.id,
                    token,
                },
            })
            .await?;
        if affected {
            transfer(&f, &job).await?;
        }
        let moved = current(&f, initial.id).await?;
        assert!(moved.rewrite.is_none());
        let expected = if affected {
            &job.successor_track
        } else {
            &track
        };
        assert!(moved.items.iter().all(|item| &item.track_ref == expected));
    }
    Ok(())
}

#[tokio::test]
async fn a_single_job_waiting_for_an_editor_answer_keeps_its_original_scope() -> Result<()> {
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let initial = plan(&f, Some(&job.request.track_id)).await?;
    assert!(
        f.journal
            .begin_plan(initial.id, initial.generation.id, initial.rev)
            .await?
    );
    assert!(
        f.journal
            .generated_plan(
                initial.id,
                initial.generation.id,
                initial.rev,
                Generated::TimedOut,
                vec![]
            )
            .await?
    );
    let failed = current(&f, initial.id).await?;
    f.journal
        .plan_command(PlanCommand {
            op_id: Uuid::now_v7(),
            plan_id: failed.id,
            revision: failed.rev,
            action: PlanAction::RunAsOne,
        })
        .await?;
    let input = f.journal.claim_input().await?.unwrap();
    f.journal
        .apply_input(
            input.id.clone(),
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Ask,
                track_id: Some(job.request.track_id.clone()),
                boundary: None,
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let before = f.journal.read_utterance(input.id.clone()).await?;
    let plan_before = current(&f, initial.id).await?;
    assert_eq!(before["state"], "asked");
    transfer(&f, &job).await?;
    let after = f.journal.read_utterance(input.id).await?;
    for key in ["text", "target", "revision", "decision", "state"] {
        assert_eq!(after[key], before[key], "{key}");
    }
    assert_eq!(
        serde_json::to_value(current(&f, initial.id).await?)?,
        serde_json::to_value(plan_before)?
    );
    Ok(())
}

#[tokio::test]
async fn a_submitted_item_revision_can_finish_before_transfer_and_its_late_result_is_ignored()
-> Result<()> {
    use crate::plan_rewrite::{Action, Request};
    let f = Fixture::new(1)
        .await
        .map_err(|error| format!("Revised item fixture: {error}"))?;
    let job = summarized(&f)
        .await
        .map_err(|error| format!("Revised item summary: {error}"))?;
    let initial = plan(&f, Some(&job.request.track_id))
        .await
        .map_err(|error| format!("Revised item initial plan: {error}"))?;
    assert!(
        f.journal
            .begin_plan(initial.id, initial.generation.id, initial.rev)
            .await?
    );
    assert!(
        f.journal
            .generated_plan(
                initial.id,
                initial.generation.id,
                initial.rev,
                Generated::Draft(draft(
                    &job.folder.id,
                    &job.request.track_id,
                    "Own initial plan"
                )),
                vec![]
            )
            .await?
    );
    let ready = current(&f, initial.id).await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let editing = f
        .journal
        .edit_plan(Request {
            owner,
            action: Action::Begin {
                plan_id: ready.id,
                item_id: ready.items[0].plan_item_id,
                token,
                revision: ready.rev,
            },
        })
        .await
        .map_err(|error| format!("Revised item begin edit: {error}"))?
        .unwrap();
    let submitted = f
        .journal
        .edit_plan(Request {
            owner,
            action: Action::Submit {
                plan_id: editing.id,
                token,
                revision: editing.rev,
                op_id: Uuid::now_v7(),
                text: "Own revised instruction".into(),
            },
        })
        .await
        .map_err(|error| format!("Revised item submit edit: {error}"))?
        .unwrap();
    assert!(
        f.journal
            .begin_plan(submitted.id, submitted.generation.id, submitted.rev)
            .await?
    );
    let error = crate::fresh_files::transfer(&f.journal, &job)
        .await
        .unwrap_err();
    assert!(error.is::<crate::fresh::Waiting>());
    let mut proposal = draft(&job.folder.id, &job.request.track_id, "Own item revision");
    proposal.items.truncate(1);
    proposal.items[0].id = ready.items[0].plan_item_id.to_string();
    proposal.items[0].instruction = "Own revised instruction".into();
    assert!(
        f.journal
            .generated_plan(
                submitted.id,
                submitted.generation.id,
                submitted.rev,
                Generated::Draft(proposal.clone()),
                vec![]
            )
            .await?
    );
    let proposed = current(&f, initial.id).await?;
    assert!(proposed.rewrite.as_ref().unwrap().proposal.is_some());
    f.journal
        .edit_plan(Request {
            owner,
            action: Action::Apply {
                plan_id: proposed.id,
                token,
                revision: proposed.rev,
                op_id: Uuid::now_v7(),
            },
        })
        .await
        .map_err(|error| format!("Revised item apply proposal: {error}"))?;
    transfer(&f, &job)
        .await
        .map_err(|error| format!("Revised item transfer: {error}"))?;
    let moved = current(&f, initial.id).await?;
    assert!(moved.rewrite.is_none());
    assert_eq!(moved.items[0].instruction, "Own revised instruction");
    assert!(
        moved
            .items
            .iter()
            .all(|item| item.track_ref == job.successor_track)
    );
    assert!(
        !f.journal
            .generated_plan(
                submitted.id,
                submitted.generation.id,
                submitted.rev,
                Generated::Draft(proposal),
                vec![]
            )
            .await?
    );
    assert_eq!(
        serde_json::to_value(current(&f, initial.id).await?)?,
        serde_json::to_value(moved)?
    );
    let history = f.journal.snapshot().await?;
    assert_eq!(history["editHistory"]["undo"]["label"], "Revise plan item");
    assert!(history["editHistory"]["undo"]["blocked"].is_null());
    let undone = f
        .journal
        .edit_history(crate::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: crate::timeline_history::Action::Undo {
                entry: history["editHistory"]["undo"]["entryId"]
                    .as_str()
                    .unwrap()
                    .into(),
            },
        })
        .await
        .map_err(|error| format!("Revised item Undo: {error}"))?;
    let reverted = current(&f, initial.id).await?;
    assert_eq!(reverted.items[0].instruction, "Inspect own-0.txt");
    assert!(
        reverted
            .items
            .iter()
            .all(|item| item.track_ref == job.successor_track)
    );
    f.journal
        .edit_history(crate::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: crate::timeline_history::Action::Redo {
                entry: undone["editHistory"]["redo"]["entryId"]
                    .as_str()
                    .unwrap()
                    .into(),
            },
        })
        .await
        .map_err(|error| format!("Revised item Redo: {error}"))?;
    let redone = current(&f, initial.id).await?;
    assert_eq!(redone.items[0].instruction, "Own revised instruction");
    assert!(
        redone
            .items
            .iter()
            .all(|item| item.track_ref == job.successor_track)
    );
    Ok(())
}
