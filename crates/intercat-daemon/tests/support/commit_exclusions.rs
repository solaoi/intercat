use super::*;

async fn branch_from(f: &mut Fixture, parent: usize, point: &Checkpoint) -> Result<usize> {
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    let session = Uuid::now_v7().to_string();
    db.execute(
        "INSERT INTO timeline_turns VALUES(?1,?2,?3)",
        rusqlite::params![
            session,
            point.turn_id.to_string(),
            json!({"track":f.workspaces[parent].track_id}).to_string()
        ],
    )?;
    let branch = f
        .journal
        .branch(intercat_daemon::branch::Change::Prepare(
            intercat_daemon::branch::Request {
                instructions: intercat_daemon::branch::InstructionMode::Checkpoint,
                op_id: Uuid::now_v7(),
                parent_track_id: f.workspaces[parent].track_id.clone(),
                point: intercat_daemon::branch_start::Selection::Turn {
                    turn_id: point.turn_id,
                },
            },
        ))
        .await?;
    let mut workspace = branch.workspace;
    git(
        &f.source,
        &[
            "worktree",
            "add",
            "--detach",
            workspace.path.to_str().unwrap(),
            &point.sha,
        ],
    );
    workspace.state = "ready".into();
    f.journal.workspace_result(workspace.clone()).await?;
    f.workspaces.push(workspace);
    Ok(f.workspaces.len() - 1)
}

async fn publish(f: &Fixture, job: &Job) -> Result<Job> {
    f.action(Action::Prepare {
        id: job.id,
        revision: job.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    f.action(commit_action(&checked)).await?;
    let done = finished(&f.journal).await?;
    assert_eq!(done.status, Status::Done, "{:?}", done.error);
    Ok(done)
}

#[tokio::test]
async fn a_conflict_omits_transitive_multiple_parent_work_and_keeps_its_alternate_excluded()
-> Result<()> {
    let mut f = Fixture::new(6).await?;
    fs::write(f.workspaces[1].path.join("b.txt"), "B first half\n")?;
    let b1 = f.save(1, None).await?;
    fs::write(f.workspaces[5].path.join("f.txt"), "independent F\n")?;
    let f1 = f.save(5, None).await?;
    inherit_dummy_checkpoint(&f, 2, 1, &b1)?;
    inherit_dummy_checkpoint(&f, 2, 5, &f1)?;
    for (name, text) in [
        ("b.txt", "B first half\n"),
        ("f.txt", "independent F\n"),
        ("c.txt", "C work\n"),
    ] {
        fs::write(f.workspaces[2].path.join(name), text)?;
    }
    let c1 = f.save(2, None).await?;
    inherit_dummy_checkpoint(&f, 3, 2, &c1)?;
    for (name, text) in [
        ("b.txt", "B first half\n"),
        ("f.txt", "independent F\n"),
        ("c.txt", "C work\n"),
        ("d.txt", "D work\n"),
    ] {
        fs::write(f.workspaces[3].path.join(name), text)?;
    }
    f.save(3, None).await?;
    fs::write(f.workspaces[4].path.join("e.txt"), "order-only E\n")?;
    f.save(4, None).await?;
    let branch = branch_from(&mut f, 1, &b1).await?;
    fs::write(
        f.workspaces[branch].path.join("branch.txt"),
        "branched B work\n",
    )?;
    f.save(branch, None).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "A wins\n")?;
    f.save(0, None).await?;
    fs::write(
        f.workspaces[1].path.join("file.txt"),
        "B conflicting second half\n",
    )?;
    f.save(1, Some(b1)).await?;
    let choosing = f.open().await?;
    assert_eq!(choosing.status, Status::Choosing);
    let opened = f
        .action(Action::ChoosePath {
            id: choosing.id,
            revision: choosing.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    assert_eq!(opened.status, Status::MergeConflict);
    let projection = f.journal.snapshot().await?["integrations"][0].clone();
    let b = f.workspaces[1].track_id.clone();
    let active_dependents: std::collections::BTreeSet<_> = [2, 3]
        .into_iter()
        .map(|index| f.workspaces[index].track_id.clone())
        .collect();
    let mut expected = active_dependents.clone();
    expected.insert(f.workspaces[branch].track_id.clone());
    assert!(
        !opened
            .tracks
            .iter()
            .find(|track| track.id == f.workspaces[branch].track_id)
            .unwrap()
            .included
    );
    let impact = projection["exclusions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["track"] == b)
        .unwrap();
    assert_eq!(
        impact["dependents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>(),
        active_dependents
    );
    let command = IntegrationCommand {
        op_id: Uuid::now_v7(),
        action: Action::Select {
            id: opened.id,
            revision: opened.revision,
            track: b.clone(),
            included: false,
        },
    };
    let mut left = f.service.command(command.clone()).await?;
    assert_eq!(left.status, Status::Open);
    assert_eq!(
        left.omitted_conflicts,
        std::collections::BTreeSet::from([b.clone()])
    );
    for track in &left.tracks {
        assert_eq!(
            track.included,
            track.id != b && !expected.contains(&track.id)
        );
    }
    let projection = f.journal.snapshot().await?["integrations"][0].clone();
    let omitted = projection["exclusions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|value| value["track"] == b)
        .unwrap();
    assert_eq!(
        omitted["dependents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap().to_owned())
            .collect::<std::collections::BTreeSet<_>>(),
        expected
    );
    let replay = f.service.command(command).await?;
    assert_eq!(replay.revision, left.revision);
    let mut invalid = left.clone();
    invalid
        .tracks
        .iter_mut()
        .find(|track| track.id == f.workspaces[2].track_id)
        .unwrap()
        .included = true;
    assert!(
        integration::plan(&invalid)
            .unwrap_err()
            .to_string()
            .contains("omitted conflict")
    );
    let tree = left.plan.as_ref().unwrap().tree.as_ref().unwrap();
    let files = Repository::open(&f.source)?.tree_files(tree.parse()?)?;
    for name in ["b.txt", "c.txt", "d.txt", "branch.txt"] {
        assert!(!files.contains_key(name));
    }
    for name in ["f.txt", "e.txt"] {
        assert!(files.contains_key(name));
    }
    assert!(
        f.action(Action::Select {
            id: left.id,
            revision: opened.revision,
            track: b.clone(),
            included: true
        })
        .await
        .unwrap_err()
        .to_string()
        .contains("changed")
    );
    assert!(
        f.action(Action::Select {
            id: left.id,
            revision: left.revision,
            track: f.workspaces[2].track_id.clone(),
            included: true
        })
        .await
        .unwrap_err()
        .to_string()
        .contains("Include B")
    );
    let with_parent = f
        .action(Action::Select {
            id: left.id,
            revision: left.revision,
            track: b.clone(),
            included: true,
        })
        .await?;
    assert!(with_parent.omitted_conflicts.is_empty());
    let with_child = f
        .action(Action::Select {
            id: with_parent.id,
            revision: with_parent.revision,
            track: f.workspaces[2].track_id.clone(),
            included: true,
        })
        .await?;
    assert!(
        with_child
            .tracks
            .iter()
            .find(|track| track.id == f.workspaces[2].track_id)
            .unwrap()
            .included
    );
    left = f
        .action(Action::Select {
            id: with_child.id,
            revision: with_child.revision,
            track: b,
            included: false,
        })
        .await?;
    assert!(
        left.tracks
            .iter()
            .filter(|track| expected.contains(&track.id))
            .all(|track| !track.included)
    );
    let mut unavailable = f.workspaces[1].clone();
    unavailable.state = "failed".into();
    f.journal.workspace_result(unavailable).await?;
    assert!(
        f.action(Action::Refresh {
            id: left.id,
            revision: left.revision
        })
        .await
        .unwrap_err()
        .to_string()
        .contains("omitted conflict track is unavailable")
    );
    f.journal.workspace_result(f.workspaces[1].clone()).await?;
    let refreshed = f
        .action(Action::Refresh {
            id: left.id,
            revision: left.revision,
        })
        .await?;
    assert_eq!(refreshed.omitted_conflicts, left.omitted_conflicts);
    assert!(
        refreshed
            .tracks
            .iter()
            .filter(|track| expected.contains(&track.id))
            .all(|track| !track.included)
    );
    f.service.shutdown().await;
    let reopened = Journal::open(
        f._root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let saved: Job = serde_json::from_value(reopened.snapshot().await?["integrations"][0].clone())?;
    assert_eq!(saved.omitted_conflicts, left.omitted_conflicts);
    assert!(
        reopened
            .integration_command(IntegrationCommand {
                op_id: Uuid::now_v7(),
                action: Action::Select {
                    id: saved.id,
                    revision: saved.revision,
                    track: f.workspaces[branch].track_id.clone(),
                    included: true
                }
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("Include B")
    );
    let resumed = Service::new(reopened.clone(), f._root.path().join("resumed"));
    resumed.recover().await?;
    resumed
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Prepare {
                id: saved.id,
                revision: saved.revision,
            },
        })
        .await?;
    let checked = finished(&reopened).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    resumed
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: commit_action(&checked),
        })
        .await?;
    let done = finished(&reopened).await?;
    assert_eq!(done.status, Status::Done);
    let repo = Repository::open(&f.source)?;
    let head = done.candidate.as_ref().unwrap().head.parse()?;
    let published = repo.saved_files(head)?;
    for name in ["b.txt", "c.txt", "d.txt", "branch.txt"] {
        assert!(!published.contains_key(name));
    }
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    for index in [1, 2, 3, branch] {
        assert!(!db.query_row(
            "SELECT EXISTS(SELECT 1 FROM integrated_lineage WHERE track=?1)",
            [&f.workspaces[index].track_id],
            |row| row.get::<_, bool>(0)
        )?);
    }
    assert_eq!(
        fs::read_to_string(f.workspaces[1].path.join("b.txt"))?,
        "B first half\n"
    );
    assert_eq!(
        fs::read_to_string(f.workspaces[1].path.join("file.txt"))?,
        "B conflicting second half\n"
    );
    f.source_unchanged()?;
    resumed.shutdown().await;
    Ok(())
}

async fn integrated_inherited_version(advance: bool) -> Result<()> {
    let f = Fixture::new(4).await?;
    fs::write(f.workspaces[1].path.join("b.txt"), "already integrated\n")?;
    let b1 = f.save(1, None).await?;
    let first = f
        .action(Action::Open {
            tracks: Some(vec![f.workspaces[1].track_id.clone()]),
        })
        .await?;
    publish(&f, &first).await?;
    inherit_dummy_checkpoint(&f, 2, 1, &b1)?;
    fs::write(f.workspaces[2].path.join("b.txt"), "already integrated\n")?;
    fs::write(f.workspaces[2].path.join("c.txt"), "C own work\n")?;
    f.save(2, None).await?;
    let previous = if advance {
        fs::write(
            f.workspaces[1].path.join("later.txt"),
            "later integrated B\n",
        )?;
        let later = f.save(1, Some(b1)).await?;
        let only_b = f
            .action(Action::Open {
                tracks: Some(vec![f.workspaces[1].track_id.clone()]),
            })
            .await?;
        publish(&f, &only_b).await?;
        later
    } else {
        b1
    };
    fs::write(f.workspaces[1].path.join("file.txt"), "conflicting B\n")?;
    let b2 = f.save(1, Some(previous)).await?;
    inherit_dummy_checkpoint(&f, 3, 1, &b2)?;
    fs::write(f.workspaces[3].path.join("b.txt"), "already integrated\n")?;
    fs::write(f.workspaces[3].path.join("file.txt"), "conflicting B\n")?;
    fs::write(f.workspaces[3].path.join("d.txt"), "D own work\n")?;
    f.save(3, None).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "A wins\n")?;
    f.save(0, None).await?;
    let job = f.open().await?;
    assert_eq!(job.status, Status::MergeConflict);
    let left = f
        .action(Action::Select {
            id: job.id,
            revision: job.revision,
            track: f.workspaces[1].track_id.clone(),
            included: false,
        })
        .await?;
    assert!(
        left.tracks
            .iter()
            .find(|track| track.id == f.workspaces[2].track_id)
            .unwrap()
            .included
    );
    assert!(
        !left
            .tracks
            .iter()
            .find(|track| track.id == f.workspaces[3].track_id)
            .unwrap()
            .included
    );
    let done = publish(&f, &left).await?;
    let head = &done.candidate.as_ref().unwrap().head;
    assert_eq!(
        git(&f.source, &["show", &format!("{head}:b.txt")]),
        "already integrated"
    );
    assert_eq!(
        git(&f.source, &["show", &format!("{head}:c.txt")]),
        "C own work"
    );
    assert_eq!(
        git(&f.source, &["show", &format!("{head}:file.txt")]),
        "A wins"
    );
    if advance {
        assert_eq!(
            git(&f.source, &["show", &format!("{head}:later.txt")]),
            "later integrated B"
        );
    }
    assert!(
        !Repository::open(&f.source)?
            .saved_files(head.parse()?)?
            .contains_key("d.txt")
    );
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_integrated_inherited_version_stays_selected_when_a_newer_parent_conflicts() -> Result<()>
{
    integrated_inherited_version(false).await
}

#[tokio::test]
async fn an_older_inherited_version_stays_selected_after_a_later_parent_integration() -> Result<()>
{
    integrated_inherited_version(true).await
}

#[tokio::test]
async fn an_ordinary_parent_omission_still_allows_a_child_only_commit() -> Result<()> {
    let f = Fixture::new(2).await?;
    fs::write(f.workspaces[0].path.join("parent.txt"), "parent work\n")?;
    let parent = f.save(0, None).await?;
    inherit_dummy_checkpoint(&f, 1, 0, &parent)?;
    fs::write(f.workspaces[1].path.join("parent.txt"), "parent work\n")?;
    fs::write(f.workspaces[1].path.join("child.txt"), "child work\n")?;
    f.save(1, None).await?;
    let job = f.open().await?;
    let child_only = f
        .action(Action::Select {
            id: job.id,
            revision: job.revision,
            track: f.workspaces[0].track_id.clone(),
            included: false,
        })
        .await?;
    assert!(child_only.omitted_conflicts.is_empty());
    assert!(
        child_only
            .tracks
            .iter()
            .find(|track| track.id == f.workspaces[1].track_id)
            .unwrap()
            .included
    );
    let done = publish(&f, &child_only).await?;
    let head = &done.candidate.as_ref().unwrap().head;
    assert_eq!(
        git(&f.source, &["show", &format!("{head}:parent.txt")]),
        "parent work"
    );
    assert_eq!(
        git(&f.source, &["show", &format!("{head}:child.txt")]),
        "child work"
    );
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn inherited_sources_precede_their_children_and_cannot_be_reordered_behind_them() -> Result<()>
{
    let f = Fixture::new(2).await?;
    fs::write(f.workspaces[1].path.join("file.txt"), "200\n")?;
    let parent = f.save(1, None).await?;
    inherit_dummy_checkpoint(&f, 0, 1, &parent)?;
    fs::write(f.workspaces[0].path.join("file.txt"), "300\n")?;
    f.save(0, None).await?;
    let pending = f
        .service
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Open { tracks: None },
        })
        .await?;
    assert_eq!(pending.status, Status::Ordering);
    let parent_first = vec![
        f.workspaces[1].track_id.clone(),
        f.workspaces[0].track_id.clone(),
    ];
    assert_eq!(pending.order_review.as_ref().unwrap().order, parent_first);
    let reverse = f
        .action(Action::Order {
            id: pending.id,
            revision: pending.revision,
            tracks: parent_first.iter().rev().cloned().collect(),
        })
        .await
        .unwrap_err();
    assert!(reverse.to_string().contains("sources before"));
    let ordered = f
        .action(Action::Order {
            id: pending.id,
            revision: pending.revision,
            tracks: parent_first,
        })
        .await?;
    assert_eq!(ordered.status, Status::Open);
    let tree = ordered.plan.as_ref().unwrap().tree.as_ref().unwrap();
    assert_eq!(
        git(&f.source, &["show", &format!("{tree}:file.txt")]),
        "300"
    );
    f.service.shutdown().await;
    Ok(())
}
