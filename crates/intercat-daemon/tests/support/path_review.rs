use super::*;
use intercat_daemon::{
    branch::{Branch, InstructionMode, Phase, Request},
    branch_points::Origin,
};
async fn publish(f: &Fixture, tracks: Option<Vec<String>>) -> Result<Job> {
    let open = f.action(Action::Open { tracks }).await?;
    f.action(Action::Prepare {
        id: open.id,
        revision: open.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    f.action(commit_action(&checked)).await?;
    let done = finished(&f.journal).await?;
    assert_eq!(done.status, Status::Done);
    Ok(done)
}
fn fork(f: &mut Fixture, checkpoint: &Checkpoint) -> Result<()> {
    let mut workspace = f.workspaces[1].clone();
    let reference = format!("{}/A2", workspace.reference.rsplit_once('/').unwrap().0);
    let repo = Repository::open(&f.source)?;
    let old = repo.reference_target(&workspace.reference)?.unwrap();
    repo.compare_and_swap(&reference, None, old)?;
    repo.sync_ref(&reference)?;
    workspace.reference = reference;
    git(
        &workspace.path,
        &["symbolic-ref", "HEAD", &workspace.reference],
    );
    let branch = Branch {
        request: Request {
            op_id: Uuid::now_v7(),
            parent_track_id: f.workspaces[0].track_id.clone(),
            point: intercat_daemon::branch_start::Selection::Turn {
                turn_id: checkpoint.turn_id,
            },
            instructions: InstructionMode::Checkpoint,
        },
        track_id: workspace.track_id.clone(),
        parent_session_id: "own-parent-session".into(),
        session_id: Some("own-child-session".into()),
        workspace: workspace.clone(),
        point: intercat_daemon::branch_start::Point::Turn {
            checkpoint: checkpoint.clone(),
        },
        phase: Phase::Ready,
        instructions: None,
        origin: Origin {
            ended_at: Some(1794211200000),
            timeline_at: Some(1000),
            turn_number: 99,
        },
    };
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    db.execute(
        "UPDATE tracks SET label='A2' WHERE id=?1",
        [&workspace.track_id],
    )?;
    db.execute(
        "UPDATE workspaces SET record=?2 WHERE track_id=?1",
        rusqlite::params![workspace.track_id, serde_json::to_string(&workspace)?],
    )?;
    db.execute(
        "INSERT INTO branches VALUES(?1,?2)",
        rusqlite::params![
            branch.request.op_id.to_string(),
            serde_json::to_string(&branch)?
        ],
    )?;
    f.workspaces[1] = workspace;
    inherit_dummy_checkpoint(f, 1, 0, checkpoint)
}
fn runtime(
    f: &Fixture,
) -> Result<(
    intercat_daemon::runtime::Runtime,
    intercat_daemon::editor::Manager,
)> {
    let editor =
        intercat_daemon::editor::Manager::new(f._root.path(), &f._root.path().join("editor.log"))?;
    let runtime = intercat_daemon::runtime::Runtime::start(
        intercat_daemon::host_memory::Pool::new(),
        f._root.path().join("runtime"),
        f._root.path().join("rpc.sock"),
        f.journal.clone(),
        editor.clone(),
        std::sync::Arc::new(intercat_daemon::scheduler::Capacity::new(4)),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    Ok((runtime, editor))
}
#[tokio::test]
async fn saved_path_choice_builds_its_undo_and_reuses_the_retired_family_on_the_next_commit()
-> Result<()> {
    let mut f = Fixture::new(2).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "Own fork context\n")?;
    let fork_point = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("later.txt"),
        "Own retired later edit\n",
    )?;
    let parent = f.save(0, Some(fork_point.clone())).await?;
    publish(&f, Some(vec![f.workspaces[0].track_id.clone()])).await?;
    fork(&mut f, &fork_point)?;
    fs::write(f.workspaces[1].path.join("file.txt"), "Own fork context\n")?;
    fs::write(
        f.workspaces[1].path.join("alternative.txt"),
        "Own chosen path\n",
    )?;
    let child = f.save(1, None).await?;
    let opened = f.open().await?;
    assert_eq!(opened.status, Status::Choosing);
    assert_eq!(
        opened.path_state.as_ref().unwrap().pending,
        vec![f.workspaces[0].track_id.clone()]
    );
    assert_eq!(opened.path_review.families[0].forks[0].checkpoint.number, 1);
    assert!(opened.undo.is_none());
    let command = IntegrationCommand {
        op_id: Uuid::now_v7(),
        action: Action::ChoosePath {
            id: opened.id,
            revision: opened.revision,
            track: f.workspaces[1].track_id.clone(),
        },
    };
    let chosen = f.service.command(command.clone()).await?;
    assert_eq!(chosen.status, Status::Open, "{:?}", chosen.error);
    assert_eq!(chosen.undo.as_ref().unwrap().records.len(), 1);
    assert_eq!(
        chosen.undo.as_ref().unwrap().records[0].checkpoint.number,
        2
    );
    let replay = f.service.command(command).await?;
    assert_eq!(chosen.revision, replay.revision);
    assert_eq!(
        serde_json::to_value(&chosen.path_review)?,
        serde_json::to_value(&f.journal.integration_job(chosen.id).await?.path_review)?
    );
    assert!(serde_json::from_value::<Action>(json!({"action":"choosePath","id":chosen.id,"revision":chosen.revision,"track":f.workspaces[1].track_id,"checkpoint":999})).is_err());
    f.action(Action::Prepare {
        id: chosen.id,
        revision: chosen.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    assert!(checked.candidate.as_ref().unwrap().undo.is_some());
    let (runtime, editor) = runtime(&f)?;
    f.action(commit_action(&checked)).await?;
    let done = finished(&f.journal).await?;
    assert_eq!(done.status, Status::Done, "{:?}", done.error);
    let snapshot = f.journal.snapshot().await?;
    assert_eq!(snapshot["pathRetirements"][0]["phase"], "aside");
    let saved = &snapshot["pathRetirements"][0]["saved"];
    assert_eq!(saved["sha"], parent.sha);
    assert_eq!(
        Repository::open(&f.source)?.reference_target(saved["reference"].as_str().unwrap())?,
        Some(parent.sha.parse()?)
    );
    let tree = done.candidate.as_ref().unwrap().tree.parse()?;
    let files = Repository::open(&f.source)?.tree_files(tree)?;
    assert!(!files.contains_key("later.txt"));
    assert!(files.contains_key("alternative.txt"));
    assert!(f.workspaces[0].path.join("later.txt").exists());
    let late = Uuid::now_v7();
    f.journal
        .stage_utterance(
            late,
            "Own request while aside".into(),
            Target::Lock(f.workspaces[0].track_id.clone()),
            None,
            vec![],
        )
        .await?;
    let input = f.journal.claim_input().await?.unwrap();
    assert_eq!(input.id, late.to_string());
    f.journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(f.workspaces[0].track_id.clone()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let held = f.journal.read_utterance(late.to_string()).await?;
    assert_eq!(held["state"], "held");
    assert!(
        f.journal
            .start_held_input(
                Uuid::now_v7(),
                late.to_string(),
                held["revision"].as_i64().unwrap()
            )
            .await
            .is_err()
    );
    fs::write(
        f.workspaces[1].path.join("next.txt"),
        "Own next chosen edit\n",
    )?;
    f.save(1, Some(child)).await?;
    let next = f.open().await?;
    assert_eq!(next.status, Status::Open, "{:?}", next.error);
    assert!(next.path_state.as_ref().unwrap().ready());
    assert!(next.undo.is_none());
    assert_eq!(
        next.path_review.choices[&f.workspaces[0].track_id].kept,
        f.workspaces[1].track_id
    );
    f.source_unchanged()?;
    f.service.shutdown().await;
    runtime.shutdown().await;
    editor.stop().await;
    Ok(())
}
#[tokio::test]
async fn a_dependent_commit_requires_a_path_choice_and_mixed_published_changes_stop_replacement()
-> Result<()> {
    let mut f = Fixture::new(3).await?;
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own fork context\n",
    )?;
    let fork_point = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own later parent edit\n",
    )?;
    let later = f.save(0, Some(fork_point.clone())).await?;
    inherit_dummy_checkpoint(&f, 2, 0, &later)?;
    fs::write(
        f.workspaces[2].path.join("parent.txt"),
        "Own later parent edit\n",
    )?;
    fs::write(f.workspaces[2].path.join("child.txt"), "Own mixed child\n")?;
    f.save(2, None).await?;
    publish(&f, Some(vec![f.workspaces[2].track_id.clone()])).await?;
    fork(&mut f, &fork_point)?;
    fs::write(
        f.workspaces[1].path.join("parent.txt"),
        "Own fork context\n",
    )?;
    f.save(1, None).await?;
    let open = f
        .action(Action::Open {
            tracks: Some(vec![f.workspaces[2].track_id.clone()]),
        })
        .await?;
    assert_eq!(open.status, Status::Choosing);
    let choice = f
        .action(Action::ChoosePath {
            id: open.id,
            revision: open.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    assert_eq!(choice.status, Status::Choosing);
    let state = choice.path_state.as_ref().unwrap();
    assert_eq!(state.mixed.len(), 1);
    assert_eq!(state.mixed[0].track, f.workspaces[2].track_id);
    assert!(choice.undo.is_none());
    assert!(choice.candidate.is_none());
    assert!(
        f.action(Action::Prepare {
            id: choice.id,
            revision: choice.revision
        })
        .await
        .is_err()
    );
    assert!(
        f.journal.snapshot().await?["pathRetirements"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let kept = f
        .action(Action::ChoosePath {
            id: choice.id,
            revision: choice.revision,
            track: f.workspaces[0].track_id.clone(),
        })
        .await?;
    assert_eq!(kept.status, Status::Open, "{:?}", kept.error);
    assert!(kept.path_state.as_ref().unwrap().mixed.is_empty());
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_undo_conflict_is_frozen_before_building_and_never_stops_the_original_path() -> Result<()>
{
    let mut f = Fixture::new(2).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "Own fork context\n")?;
    let fork_point = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("file.txt"),
        "Own retired later edit\n",
    )?;
    f.save(0, Some(fork_point.clone())).await?;
    let prior = publish(&f, Some(vec![f.workspaces[0].track_id.clone()])).await?;
    let repo = Repository::open(&f.source)?;
    let parent = prior.candidate.as_ref().unwrap().head.parse()?;
    let file = repo.write_blob(b"Own independent later edit\n")?;
    let other = repo.write_blob(b"other\n")?;
    let tree = repo.write_tree(&[("file.txt", file), ("other.txt", other)])?;
    let external = repo.checkpoint_commit(tree, parent, "Own dummy independent target edit")?;
    repo.sync_objects(&[external])?;
    let target = &prior.plan.as_ref().unwrap().target_ref;
    repo.compare_and_swap(target, Some(parent), external)?;
    repo.sync_ref(target)?;
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    db.execute(
        "UPDATE published_lineage SET head=?2 WHERE reference=?1",
        rusqlite::params![target, external.to_string()],
    )?;
    fork(&mut f, &fork_point)?;
    fs::write(f.workspaces[1].path.join("file.txt"), "Own fork context\n")?;
    f.save(1, None).await?;
    let open = f.open().await?;
    let chosen = f
        .action(Action::ChoosePath {
            id: open.id,
            revision: open.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    assert_eq!(chosen.status, Status::MergeConflict, "{:?}", chosen.error);
    let conflict = chosen
        .plan
        .as_ref()
        .unwrap()
        .undo_conflict
        .as_ref()
        .unwrap();
    assert_eq!(conflict.paths, vec!["file.txt"]);
    assert_eq!(conflict.remaining.len(), 1);
    assert!(conflict.applied.is_empty());
    f.action(Action::Prepare {
        id: chosen.id,
        revision: chosen.revision,
    })
    .await?;
    let blocked = finished(&f.journal).await?;
    assert_eq!(blocked.status, Status::MergeConflict, "{:?}", blocked.error);
    assert_eq!(
        blocked
            .candidate
            .as_ref()
            .unwrap()
            .undo_conflict
            .as_ref()
            .unwrap()
            .paths,
        vec!["file.txt"]
    );
    assert!(
        f.journal.snapshot().await?["pathRetirements"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(repo.reference_target(target)?, Some(external));
    assert_eq!(
        fs::read_to_string(f.workspaces[0].path.join("file.txt"))?,
        "Own retired later edit\n"
    );
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}
