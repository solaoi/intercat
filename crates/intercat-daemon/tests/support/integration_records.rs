use super::*;
use intercat_daemon::{integration_record::Record, replacement_delta::Boundary};
fn reverse_receipts(
    repo: &Repository,
    tree: ObjectId,
    records: &[Record],
    resolutions: &[intercat_daemon::merge_choices::Solved],
) -> Result<intercat_daemon::replacement_delta::Reversed> {
    let mut merges = intercat_daemon::merge_choices::Merges::new(repo, resolutions)?;
    let result = intercat_daemon::replacement_delta::reverse(&mut merges, tree, records)?;
    merges.finish()?;
    Ok(result)
}
async fn publish(f: &Fixture, tracks: Option<Vec<String>>) -> Result<Job> {
    let opened = f.action(Action::Open { tracks }).await?;
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    f.action(commit_action(&checked)).await?;
    let saved = finished(&f.journal).await?;
    assert_eq!(saved.status, Status::Done);
    Ok(saved)
}
fn records(f: &Fixture) -> Result<Vec<Record>> {
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    db.prepare("SELECT record FROM integration_records ORDER BY rowid")?
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|raw| {
            let record: Record = serde_json::from_str(&raw?)?;
            record.validate()?;
            Ok(record)
        })
        .collect()
}
#[tokio::test]
async fn integration_records_bind_the_actual_published_delta_and_checkpoint_number() -> Result<()> {
    let f = Fixture::new(1).await?;
    let mut previous = None;
    for text in ["Own first dummy edit\n", "Own second dummy edit\n"] {
        fs::write(f.workspaces[0].path.join("file.txt"), text)?;
        previous = Some(f.save(0, previous).await?);
    }
    let saved = publish(&f, None).await?;
    let rows = records(&f)?;
    assert_eq!(rows.len(), 1);
    let record = &rows[0];
    let commit = &saved.candidate.as_ref().unwrap().commits[0];
    assert_eq!(record.job_id, saved.id);
    assert_eq!(record.folder_id, saved.project.id);
    assert_eq!(record.track_id, commit.track);
    assert_eq!(record.sha, commit.sha);
    assert_eq!(record.parent, commit.parent);
    assert_eq!(record.tree, commit.tree);
    assert_eq!(record.ordinal, 1);
    assert_eq!(record.position, 1);
    assert_eq!(record.checkpoint.number, 2);
    let sources = std::collections::BTreeMap::from([(record.track_id.clone(), Boundary::Fork(2))]);
    assert!(
        matches!(intercat_daemon::replacement_delta::select(&rows,&sources,&Default::default())?,intercat_daemon::replacement_delta::Selection::Ready(selected) if selected.len()==1)
    );
    let sources = std::collections::BTreeMap::from([(record.track_id.clone(), Boundary::Fork(3))]);
    assert!(
        matches!(intercat_daemon::replacement_delta::select(&rows,&sources,&Default::default())?,intercat_daemon::replacement_delta::Selection::Ready(selected) if selected.is_empty())
    );
    assert_eq!(
        record.checkpoint.sha,
        saved.tracks[0].checkpoint.as_ref().unwrap().sha
    );
    assert!(!record.coverage_before.contains_key(&record.track_id));
    assert_eq!(record.coverage_after[&record.track_id], record.checkpoint);
    let repository = Repository::open(&f.source)?;
    assert_eq!(
        repository.reference_target(&record.reference)?,
        Some(record.sha.parse()?)
    );
    let details = repository.commit_details(record.sha.parse()?)?;
    assert_eq!(details.parents, vec![record.parent.parse::<ObjectId>()?]);
    assert_eq!(details.tree.to_string(), record.tree);
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn no_change_publication_does_not_invent_another_integration_delta() -> Result<()> {
    let f = Fixture::new(1).await?;
    fs::write(
        f.workspaces[0].path.join("file.txt"),
        "Own saved dummy edit\n",
    )?;
    let checkpoint = f.save(0, None).await?;
    publish(&f, None).await?;
    let before = records(&f)?;
    assert_eq!(before.len(), 1);
    // A new saved checkpoint with an identical tree advances coverage but
    // supplies no commit whose delta could later be undone.
    f.save(0, Some(checkpoint)).await?;
    let saved = publish(&f, None).await?;
    assert!(saved.candidate.as_ref().unwrap().commits.is_empty());
    assert_eq!(records(&f)?, before);
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn integration_records_distinguish_mixed_parent_changes_from_an_already_published_parent()
-> Result<()> {
    for parent_first in [false, true] {
        let f = Fixture::new(2).await?;
        fs::write(
            f.workspaces[0].path.join("parent.txt"),
            "Own parent change\n",
        )?;
        let first = f.save(0, None).await?;
        fs::write(
            f.workspaces[0].path.join("parent.txt"),
            "Own later parent change\n",
        )?;
        let parent = f.save(0, Some(first)).await?;
        inherit_dummy_checkpoint(&f, 1, 0, &parent)?;
        fs::write(
            f.workspaces[1].path.join("parent.txt"),
            "Own later parent change\n",
        )?;
        fs::write(f.workspaces[1].path.join("child.txt"), "Own child change\n")?;
        f.save(1, None).await?;
        let tracks = if parent_first {
            None
        } else {
            Some(vec![f.workspaces[1].track_id.clone()])
        };
        let saved = publish(&f, tracks).await?;
        let rows = records(&f)?;
        assert_eq!(rows.len(), if parent_first { 2 } else { 1 });
        let child = rows
            .iter()
            .find(|record| record.track_id == f.workspaces[1].track_id)
            .unwrap();
        let parent_id = &f.workspaces[0].track_id;
        assert_eq!(child.coverage_before.contains_key(parent_id), parent_first);
        assert_eq!(child.coverage_after[parent_id].sha, parent.sha);
        assert_eq!(child.coverage_after[parent_id].number, 2);
        assert_eq!(
            saved
                .candidate
                .as_ref()
                .unwrap()
                .commits
                .last()
                .unwrap()
                .sha,
            child.sha
        );
        let repo = Repository::open(&f.source)?;
        let files = repo.tree_files(child.tree.parse()?)?;
        assert!(files.contains_key("parent.txt"));
        assert!(files.contains_key("child.txt"));
        let selection = intercat_daemon::replacement_delta::select(
            &rows,
            &std::collections::BTreeMap::from([(parent_id.clone(), Boundary::Fork(1))]),
            &Default::default(),
        )?;
        match selection {
            intercat_daemon::replacement_delta::Selection::Ready(selected) => {
                assert!(parent_first);
                assert_eq!(selected.len(), 1);
                let reversed = reverse_receipts(&repo, child.tree.parse()?, &selected, &[])?;
                let intercat_git::Merge::Clean(tree) = reversed.result else {
                    panic!("Independent child files must not conflict with the parent's inverse");
                };
                let after = repo.tree_files(tree)?;
                assert!(!after.contains_key("parent.txt"));
                assert!(after.contains_key("child.txt"));
                let cancelled = selected.iter().map(|record| record.id).collect();
                let intercat_daemon::replacement_delta::Selection::Ready(again) =
                    intercat_daemon::replacement_delta::select(
                        &rows,
                        &std::collections::BTreeMap::from([(parent_id.clone(), Boundary::Fork(1))]),
                        &cancelled,
                    )?
                else {
                    panic!("Cancelled deltas are not selected again");
                };
                assert!(again.is_empty());
                let retained = reverse_receipts(&repo, child.tree.parse()?, &again, &[])?;
                assert!(
                    matches!(retained.result,intercat_git::Merge::Clean(tree) if tree==child.tree.parse::<ObjectId>()?)
                );
            }
            intercat_daemon::replacement_delta::Selection::Mixed(blocked) => {
                assert!(!parent_first);
                assert_eq!(blocked.len(), 1);
                assert_eq!(blocked[0].track, child.track_id);
                assert_eq!(blocked[0].through, 2);
            }
        }
        let all = std::collections::BTreeMap::from([
            (parent_id.clone(), Boundary::Fork(1)),
            (f.workspaces[1].track_id.clone(), Boundary::Fork(1)),
        ]);
        let intercat_daemon::replacement_delta::Selection::Ready(selected) =
            intercat_daemon::replacement_delta::select(&rows, &all, &Default::default())?
        else {
            panic!("Cancelling the complete mixed commit needs no partial inverse");
        };
        assert_eq!(selected.len(), rows.len());
        assert!(
            selected
                .windows(2)
                .all(|pair| pair[0].position > pair[1].position)
        );
        let all = reverse_receipts(&repo, child.tree.parse()?, &selected, &[])?;
        assert_eq!(all.applied.len(), rows.len());
        assert!(
            matches!(all.result,intercat_git::Merge::Clean(tree) if tree==repo.tree_id(f.head.parse()?)?)
        );
        f.source_unchanged()?;
        f.service.shutdown().await;
    }
    Ok(())
}
#[tokio::test]
async fn replacement_inverse_preserves_an_external_edit_as_a_frozen_conflict() -> Result<()> {
    let f = Fixture::new(2).await?;
    fs::write(
        f.workspaces[0].path.join("file.txt"),
        "Own parent version\n",
    )?;
    let parent = f.save(0, None).await?;
    publish(&f, Some(vec![f.workspaces[0].track_id.clone()])).await?;
    // Simulate the already accepted realignment's basis update, then publish
    // the child's own edit through the ordinary candidate and Commit pipeline.
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    let mut basis: intercat_daemon::lineage::Basis = serde_json::from_str(&db.query_row(
        "SELECT record FROM track_lineage WHERE track=?1",
        [&f.workspaces[1].track_id],
        |row| row.get::<_, String>(0),
    )?)?;
    basis.base = parent.sha;
    basis.base_inherits.insert(
        f.workspaces[0].track_id.clone(),
        intercat_daemon::lineage::Inherited {
            number: 1,
            sha: basis.base.clone(),
        },
    );
    db.execute(
        "UPDATE track_lineage SET record=?2 WHERE track=?1",
        rusqlite::params![f.workspaces[1].track_id, serde_json::to_string(&basis)?],
    )?;
    drop(db);
    fs::write(
        f.workspaces[1].path.join("file.txt"),
        "Own child edit after parent\n",
    )?;
    f.save(1, None).await?;
    let saved = publish(&f, Some(vec![f.workspaces[1].track_id.clone()])).await?;
    let rows = records(&f)?;
    let intercat_daemon::replacement_delta::Selection::Ready(selected) =
        intercat_daemon::replacement_delta::select(
            &rows,
            &std::collections::BTreeMap::from([(
                f.workspaces[0].track_id.clone(),
                Boundary::Fork(1),
            )]),
            &Default::default(),
        )?
    else {
        panic!("The child did not carry an uncommitted parent delta");
    };
    assert_eq!(selected.len(), 1);
    let repo = Repository::open(&f.source)?;
    let reversed = reverse_receipts(
        &repo,
        saved.candidate.as_ref().unwrap().tree.parse()?,
        &selected,
        &[],
    )?;
    assert!(reversed.applied.is_empty());
    let intercat_git::Merge::Conflicts {
        base,
        target,
        incoming,
        paths,
    } = reversed.result
    else {
        panic!("The child's later edit must be retained for conflict resolution");
    };
    assert!(paths.iter().any(|path| path == "file.txt"));
    assert_eq!(base, selected[0].tree.parse::<ObjectId>()?);
    assert_eq!(
        target,
        saved.candidate.as_ref().unwrap().tree.parse::<ObjectId>()?
    );
    assert_eq!(incoming, repo.tree_id(selected[0].parent.parse()?)?);
    let job = f.open().await?;
    let plan = intercat_daemon::replacement_undo::Plan {
        parent: saved.candidate.as_ref().unwrap().head.clone(),
        message: "Undo A’s changes since 14:04 (replaced by A′)".into(),
        records: selected,
        resolutions: vec![],
    };
    let (checkout, settings) = undo_checkout(&f, &job, "undo-conflict").await?;
    let intercat_daemon::replacement_undo::Outcome::Conflict(frozen) =
        build_undo(&f, &job, &checkout, &settings, &plan).await?
    else {
        panic!("The undo builder must retain the exact three-way conflict");
    };
    assert_eq!(frozen.base, base.to_string());
    assert_eq!(frozen.target, target.to_string());
    assert_eq!(frozen.incoming, incoming.to_string());
    assert_eq!(frozen.remaining, plan.ids());
    assert!(frozen.applied.is_empty());
    assert!(frozen.paths.iter().any(|path| path == "file.txt"));
    assert_eq!(
        Repository::open(&checkout.workdir)?
            .head_commit()?
            .to_string(),
        plan.parent
    );
    let choice = intercat_daemon::merge_choices::Solved {
        base: frozen.base,
        target: frozen.target,
        incoming: frozen.incoming,
        tree: target.to_string(),
    };
    let mut resolved = plan.clone();
    resolved.resolutions.push(choice.clone());
    let replay = reverse_receipts(&repo, target, &resolved.records, &resolved.resolutions)?;
    assert_eq!(replay.applied, plan.ids());
    assert!(matches!(replay.result, intercat_git::Merge::Clean(tree) if tree == target));
    let mut stale = choice.clone();
    stale.incoming = stale.target.clone();
    assert!(
        reverse_receipts(&repo, target, &plan.records, &[stale],).is_err(),
        "a solution for other frozen trees must not be silently ignored"
    );
    assert!(
        reverse_receipts(&repo, target, &plan.records, &[choice.clone(), choice],).is_err(),
        "one frozen merge must have one saved choice"
    );
    let (checkout, settings) = undo_checkout(&f, &job, "undo-resolved").await?;
    let intercat_daemon::replacement_undo::Outcome::Built(built) =
        build_undo(&f, &job, &checkout, &settings, &resolved).await?
    else {
        panic!("A saved explicit solution must build the app-owned undo")
    };
    assert_eq!(built.commit.tree, target.to_string());
    assert_eq!(built.commit.records, plan.ids());
    assert_eq!(built.commit.author, "Intercat");
    built
        .commit
        .verify(&Repository::open(&checkout.workdir)?, &resolved, &settings)?;
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn a_mixed_fork_point_is_shared_context_and_is_not_a_later_parent_delta() -> Result<()> {
    let f = Fixture::new(2).await?;
    fs::write(
        f.workspaces[0].path.join("shared.txt"),
        "Own shared fork point\n",
    )?;
    let point = f.save(0, None).await?;
    inherit_dummy_checkpoint(&f, 1, 0, &point)?;
    fs::write(
        f.workspaces[1].path.join("shared.txt"),
        "Own shared fork point\n",
    )?;
    fs::write(f.workspaces[1].path.join("child.txt"), "Own child file\n")?;
    f.save(1, None).await?;
    let saved = publish(&f, Some(vec![f.workspaces[1].track_id.clone()])).await?;
    let rows = records(&f)?;
    assert_eq!(rows.len(), 1);
    let sources =
        std::collections::BTreeMap::from([(f.workspaces[0].track_id.clone(), Boundary::Fork(1))]);
    let intercat_daemon::replacement_delta::Selection::Ready(selected) =
        intercat_daemon::replacement_delta::select(&rows, &sources, &Default::default())?
    else {
        panic!("The fork point itself remains shared context");
    };
    assert!(selected.is_empty());
    let entire = std::collections::BTreeMap::from([(
        f.workspaces[0].track_id.clone(),
        Boundary::EntireTrack,
    )]);
    assert!(
        matches!(intercat_daemon::replacement_delta::select(&rows, &entire, &Default::default())?,
        intercat_daemon::replacement_delta::Selection::Mixed(records)
            if records.len()==1 && records[0].through==1)
    );
    let repo = Repository::open(&f.source)?;
    let current = saved.candidate.unwrap().tree.parse()?;
    assert!(
        matches!(reverse_receipts(&repo,current,&selected,&[])?.result,intercat_git::Merge::Clean(tree) if tree==current)
    );
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_undo_resolution_continues_after_the_exact_already_reversed_prefix() -> Result<()> {
    use intercat_daemon::{
        replacement_delta,
        replacement_undo::{Outcome, Plan},
    };
    let f = Fixture::new(3).await?;
    fs::write(f.workspaces[0].path.join("file.txt"), "Own A edit\n")?;
    let original = f.save(0, None).await?;
    publish(&f, Some(vec![f.workspaces[0].track_id.clone()])).await?;
    fs::write(f.workspaces[1].path.join("independent.txt"), "Own X edit\n")?;
    f.save(1, None).await?;
    publish(&f, Some(vec![f.workspaces[1].track_id.clone()])).await?;
    inherit_dummy_checkpoint(&f, 2, 0, &original)?;
    fs::write(f.workspaces[2].path.join("file.txt"), "Own later B edit\n")?;
    f.save(2, None).await?;
    let saved = publish(&f, Some(vec![f.workspaces[2].track_id.clone()])).await?;
    let records = records(&f)?;
    let sources = std::collections::BTreeMap::from([
        (f.workspaces[0].track_id.clone(), Boundary::EntireTrack),
        (f.workspaces[1].track_id.clone(), Boundary::EntireTrack),
    ]);
    let replacement_delta::Selection::Ready(selected) =
        replacement_delta::select(&records, &sources, &Default::default())?
    else {
        panic!("The later B edit did not newly convey A's published context")
    };
    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0].track_id, f.workspaces[1].track_id);
    let repo = Repository::open(&f.source)?;
    let current: ObjectId = saved.candidate.as_ref().unwrap().tree.parse()?;
    let partial = reverse_receipts(&repo, current, &selected, &[])?;
    assert_eq!(partial.applied, [selected[0].id]);
    let intercat_git::Merge::Conflicts {
        base,
        target,
        incoming,
        ..
    } = partial.result
    else {
        panic!("A's inverse must retain the later B edit as a conflict")
    };
    assert!(!repo.tree_files(target)?.contains_key("independent.txt"));
    assert!(repo.tree_files(current)?.contains_key("independent.txt"));
    let resolution = intercat_daemon::merge_choices::Solved {
        base: base.to_string(),
        target: target.to_string(),
        incoming: incoming.to_string(),
        tree: target.to_string(),
    };
    let mut wrong = resolution.clone();
    wrong.target = current.to_string();
    assert!(reverse_receipts(&repo, current, &selected, &[wrong]).is_err());
    let plan = Plan {
        parent: saved.candidate.as_ref().unwrap().head.clone(),
        message: "Undo own A and X contributions".into(),
        records: selected,
        resolutions: vec![resolution],
    };
    let replay = reverse_receipts(&repo, current, &plan.records, &plan.resolutions)?;
    assert_eq!(replay.applied, plan.ids());
    assert!(matches!(replay.result, intercat_git::Merge::Clean(tree) if tree == target));
    let job = f.open().await?;
    let (checkout, settings) = undo_checkout(&f, &job, "undo-resolved-prefix").await?;
    let Outcome::Built(built) = build_undo(&f, &job, &checkout, &settings, &plan).await? else {
        panic!("The exact resolved prefix must build one undo commit")
    };
    assert_eq!(built.commit.records, plan.ids());
    assert_eq!(built.commit.tree, target.to_string());
    assert_eq!(built.commit.author, "Intercat");
    let files = Repository::open(&checkout.workdir)?.tree_files(target)?;
    assert!(!files.contains_key("independent.txt"));
    assert_eq!(repo.saved_bytes(&files["file.txt"])?, b"Own later B edit\n");
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

async fn undo_checkout(
    f: &Fixture,
    job: &Job,
    name: &str,
) -> Result<(
    intercat_daemon::checkout::Checkout,
    intercat_daemon::candidate::Settings,
)> {
    use intercat_daemon::checkout::Checkout;
    let settings = f
        .journal
        .integration_settings(job.id, job.revision)
        .await?
        .ok_or("Missing dummy Check settings")?;
    let checkout = Checkout::create(
        &f._root.path().join(name),
        job.project.source().await?,
        job.plan.as_ref().unwrap().target_head.parse()?,
        settings.checkout.clone(),
        settings.path.clone().into(),
        settings.toolchain.clone(),
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    Ok((checkout, settings))
}
async fn build_undo(
    f: &Fixture,
    job: &Job,
    checkout: &intercat_daemon::checkout::Checkout,
    settings: &intercat_daemon::candidate::Settings,
    plan: &intercat_daemon::replacement_undo::Plan,
) -> Result<intercat_daemon::replacement_undo::Outcome> {
    let cache = intercat_daemon::dependencies::Cache::new(
        f._root.path().join("undo-downloads"),
        &settings.registry_origins,
    )?;
    intercat_daemon::replacement_undo::build(
        job,
        checkout,
        plan,
        settings,
        &mut intercat_daemon::dependencies::Preparer::new(cache, settings.npm.clone()),
        &settings.scanner()?,
        tokio::sync::watch::channel(false).1,
    )
    .await
}
#[tokio::test]
async fn undo_commit_records_exact_receipts_and_preserves_an_independent_publication() -> Result<()>
{
    use intercat_daemon::replacement_undo::{Outcome, Plan};
    let f = Fixture::new(2).await?;
    fs::write(f.workspaces[0].path.join("parent.txt"), "Own parent file\n")?;
    f.save(0, None).await?;
    publish(&f, Some(vec![f.workspaces[0].track_id.clone()])).await?;
    fs::write(
        f.workspaces[1].path.join("child.txt"),
        "Own independent file\n",
    )?;
    f.save(1, None).await?;
    let saved = publish(&f, Some(vec![f.workspaces[1].track_id.clone()])).await?;
    let job = f.open().await?;
    let selected = records(&f)?
        .into_iter()
        .filter(|record| record.track_id == f.workspaces[0].track_id)
        .collect::<Vec<_>>();
    let plan = Plan {
        parent: saved.candidate.unwrap().head,
        message: "Undo A’s changes since 14:04 (replaced by A′)".into(),
        records: selected,
        resolutions: vec![],
    };
    let (checkout, settings) = undo_checkout(&f, &job, "undo-clean").await?;
    let Outcome::Built(built) = build_undo(&f, &job, &checkout, &settings, &plan).await? else {
        panic!("An independent change must not block undo");
    };
    assert!(built.commit.scan.accepted(&Default::default()));
    assert_eq!(built.commit.records, plan.ids());
    assert!(
        built
            .commit
            .message
            .contains(&format!("Intercat-Undo-Record: {}", plan.records[0].id))
    );
    assert_eq!(built.commit.author, "Intercat");
    let private = Repository::open(&checkout.workdir)?;
    let files = private.tree_files(built.commit.tree.parse()?)?;
    assert!(!files.contains_key("parent.txt"));
    assert!(files.contains_key("child.txt"));
    built.commit.verify(&private, &plan, &settings)?;
    let mut tampered = built.commit.clone();
    tampered.records.clear();
    assert!(tampered.verify(&private, &plan, &settings).is_err());
    tampered = built.commit.clone();
    tampered.scan.binding.message_hash = "changed".into();
    assert!(tampered.verify(&private, &plan, &settings).is_err());
    let source = Repository::open(&f.source)?;
    assert_eq!(
        source.reference_target(&job.plan.as_ref().unwrap().target_ref)?,
        Some(plan.parent.parse()?)
    );
    assert!(source.commit_details(built.commit.sha.parse()?).is_err());
    assert_eq!(records(&f)?.len(), 2);
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn undo_hooks_and_their_final_message_are_screened_before_any_import() -> Result<()> {
    use intercat_daemon::replacement_undo::{Outcome, Plan};
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(1).await?;
    fs::write(f.workspaces[0].path.join("parent.txt"), "Own parent file\n")?;
    f.save(0, None).await?;
    let saved = publish(&f, None).await?;
    let job = f.open().await?;
    let plan = Plan {
        parent: saved.candidate.unwrap().head,
        message: "Undo A’s changes since 14:04 (replaced by A′)".into(),
        records: records(&f)?,
        resolutions: vec![],
    };
    let hook = f.source.join(".git/hooks/commit-msg");
    let pre_commit = f.source.join(".git/hooks/pre-commit");
    for (name, script, secret, invalid) in [
        (
            "hook",
            "#!/bin/sh\nprintf '\\nOwn hook note\\n' >> \"$1\"\n".to_owned(),
            false,
            false,
        ),
        (
            "secret",
            format!(
                "#!/bin/sh\nprintf '\\n{}\\n' >> \"$1\"\n",
                concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H")
            ),
            true,
            false,
        ),
        (
            "invalid",
            "#!/bin/sh\nprintf 'Hook removed the receipt IDs\\n' > \"$1\"\n".to_owned(),
            false,
            true,
        ),
    ] {
        if name == "hook" {
            fs::write(
                &pre_commit,
                "#!/bin/sh\nprintf 'Own hook edit\\n' > hook.txt\ngit add hook.txt\n",
            )?;
            fs::set_permissions(&pre_commit, fs::Permissions::from_mode(0o755))?;
        } else if pre_commit.exists() {
            fs::remove_file(&pre_commit)?;
        }
        fs::write(&hook, script)?;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
        let (checkout, settings) = undo_checkout(&f, &job, &format!("undo-{name}")).await?;
        let result = build_undo(&f, &job, &checkout, &settings, &plan).await;
        if invalid {
            assert!(result.is_err());
            continue;
        }
        let Outcome::Built(built) = result? else {
            panic!("Dummy hook could not commit");
        };
        assert_eq!(!built.commit.scan.accepted(&Default::default()), secret);
        let private = Repository::open(&checkout.workdir)?;
        built.commit.verify(&private, &plan, &settings)?;
        if !secret {
            assert!(
                built
                    .commit
                    .changes
                    .iter()
                    .any(|change| change.path == "hook.txt")
            );
            assert!(built.commit.message.contains("Own hook note"));
        }
        assert!(
            Repository::open(&f.source)?
                .commit_details(built.commit.sha.parse()?)
                .is_err()
        );
    }
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn an_already_removed_delta_still_has_one_undo_commit_with_its_receipt_ids() -> Result<()> {
    use intercat_daemon::replacement_undo::{Outcome, Plan};
    let f = Fixture::new(2).await?;
    fs::write(f.workspaces[0].path.join("parent.txt"), "Own parent file\n")?;
    let parent = f.save(0, None).await?;
    publish(&f, Some(vec![f.workspaces[0].track_id.clone()])).await?;
    // B inherits A, then deletes A's file. Its saved files already contain no
    // parent.txt, so the inverse of A's original addition changes no tree.
    inherit_dummy_checkpoint(&f, 1, 0, &parent)?;
    f.save(1, None).await?;
    let saved = publish(&f, Some(vec![f.workspaces[1].track_id.clone()])).await?;
    let job = f.open().await?;
    let plan = Plan {
        parent: saved.candidate.as_ref().unwrap().head.clone(),
        message: "Undo A’s changes since 14:04 (replaced by A′)".into(),
        records: records(&f)?
            .into_iter()
            .filter(|record| record.track_id == f.workspaces[0].track_id)
            .collect(),
        resolutions: vec![],
    };
    let (checkout, settings) = undo_checkout(&f, &job, "undo-empty").await?;
    let Outcome::Built(built) = build_undo(&f, &job, &checkout, &settings, &plan).await? else {
        panic!("Undo must keep receipt IDs even if a later edit already removed the file");
    };
    assert_eq!(built.commit.tree, saved.candidate.as_ref().unwrap().tree);
    assert!(built.commit.changes.is_empty());
    assert_ne!(built.commit.sha, plan.parent);
    assert_eq!(built.commit.records, plan.ids());
    assert_eq!(built.commit.records.len(), 1);
    assert!(built.commit.scan.accepted(&Default::default()));
    built
        .commit
        .verify(&Repository::open(&checkout.workdir)?, &plan, &settings)?;
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

fn freeze_dummy_undo(f: &Fixture, mut job: Job, selected: Vec<Record>) -> Result<Job> {
    job.undo = Some(intercat_daemon::replacement_undo::Plan {
        parent: job.plan.as_ref().unwrap().target_head.clone(),
        message: "Undo A’s changes since 14:04 (replaced by B)".into(),
        records: selected,
        resolutions: vec![],
    });
    job.plan = Some(integration::plan(&job)?);
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    db.execute(
        "UPDATE integration_jobs SET record=?2 WHERE id=?1",
        rusqlite::params![job.id.to_string(), serde_json::to_string(&job)?],
    )?;
    Ok(job)
}

#[tokio::test]
async fn undoing_a_branch_reintroduces_its_shared_fork_when_the_original_path_is_committed()
-> Result<()> {
    let f = Fixture::new(2).await?;
    let original = &f.workspaces[0].track_id;
    let alternative = &f.workspaces[1].track_id;
    fs::write(
        f.workspaces[0].path.join("shared.txt"),
        "Own fork context\n",
    )?;
    let fork = f.save(0, None).await?;
    inherit_dummy_checkpoint(&f, 1, 0, &fork)?;
    fs::write(
        f.workspaces[1].path.join("shared.txt"),
        "Own fork context\n",
    )?;
    fs::write(
        f.workspaces[1].path.join("alternative.txt"),
        "Own alternative\n",
    )?;
    f.save(1, None).await?;
    publish(&f, Some(vec![alternative.clone()])).await?;
    let rows = records(&f)?;
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].coverage_before.contains_key(original));
    assert_eq!(rows[0].coverage_after[original].sha, fork.sha);
    fs::write(
        f.workspaces[0].path.join("original.txt"),
        "Own original continuation\n",
    )?;
    let continued = f.save(0, Some(fork)).await?;
    let opened = f
        .action(Action::Open {
            tracks: Some(vec![original.clone()]),
        })
        .await?;
    let opened = freeze_dummy_undo(&f, opened, rows.clone())?;
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let candidate = checked.candidate.as_ref().unwrap();
    let source = Repository::open(&f.source)?;
    let files = source.tree_files(candidate.tree.parse()?)?;
    assert!(
        files.contains_key("shared.txt"),
        "undo removed the shared fork from the tree, so the original path must supply it again"
    );
    assert!(files.contains_key("original.txt"));
    assert!(!files.contains_key("alternative.txt"));
    f.action(commit_action(&checked)).await?;
    let saved = finished(&f.journal).await?;
    assert_eq!(saved.status, Status::Done, "{:?}", saved.error);
    let published = records(&f)?;
    let last = published.last().unwrap();
    assert!(!last.coverage_before.contains_key(original));
    assert!(!last.coverage_before.contains_key(alternative));
    assert_eq!(last.coverage_after[original].sha, continued.sha);
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_later_publication_cannot_keep_only_the_tail_of_context_conveyed_by_an_undone_commit()
-> Result<()> {
    use intercat_daemon::replacement_delta::{self, MixedKind, Selection};
    let f = Fixture::new(2).await?;
    let dependency = &f.workspaces[0].track_id;
    let retired = &f.workspaces[1].track_id;
    fs::write(f.workspaces[0].path.join("first.txt"), "Own X1 context\n")?;
    let first = f.save(0, None).await?;
    publish(&f, Some(vec![dependency.clone()])).await?;
    fs::write(f.workspaces[0].path.join("second.txt"), "Own X2 context\n")?;
    let second = f.save(0, Some(first)).await?;
    inherit_dummy_checkpoint(&f, 1, 0, &second)?;
    for (path, contents) in [
        ("first.txt", "Own X1 context\n"),
        ("second.txt", "Own X2 context\n"),
        ("retired.txt", "Own retired contribution\n"),
    ] {
        fs::write(f.workspaces[1].path.join(path), contents)?;
    }
    f.save(1, None).await?;
    publish(&f, Some(vec![retired.clone()])).await?;
    fs::write(
        f.workspaces[0].path.join("third.txt"),
        "Own X3 continuation\n",
    )?;
    f.save(0, Some(second)).await?;
    let saved = publish(&f, Some(vec![dependency.clone()])).await?;
    let rows = records(&f)?;
    let sources = std::collections::BTreeMap::from([(retired.clone(), Boundary::EntireTrack)]);
    let Selection::Mixed(mixed) = replacement_delta::select(&rows, &sources, &Default::default())?
    else {
        panic!("The final publication only conveyed X2..X3")
    };
    assert_eq!(mixed.len(), 1);
    assert_eq!(mixed[0].kind, MixedKind::ExtendsCancelledContext);
    assert_eq!(mixed[0].source, *retired);
    assert_eq!(mixed[0].dependency, *dependency);
    assert_eq!(mixed[0].track, *dependency);
    assert_eq!((mixed[0].after, mixed[0].through), (2, 3));
    let sources = std::collections::BTreeMap::from([
        (retired.clone(), Boundary::EntireTrack),
        (dependency.clone(), Boundary::Fork(2)),
    ]);
    let Selection::Ready(selected) =
        replacement_delta::select(&rows, &sources, &Default::default())?
    else {
        panic!("Undoing both deltas has a complete context boundary")
    };
    let mut coverage = rows.last().unwrap().coverage_after.clone();
    let plan = intercat_daemon::replacement_undo::Plan {
        parent: saved.candidate.as_ref().unwrap().head.clone(),
        message: "Undo own dummy dependent contributions".into(),
        records: selected,
        resolutions: vec![],
    };
    intercat_daemon::replacement_delta::rewind(&plan.records, &mut coverage);
    assert_eq!(coverage[dependency].number, 1);
    assert!(!coverage.contains_key(retired));
    let repo = Repository::open(&f.source)?;
    let intercat_git::Merge::Clean(tree) = reverse_receipts(
        &repo,
        saved.candidate.as_ref().unwrap().tree.parse()?,
        &plan.records,
        &plan.resolutions,
    )?
    .result
    else {
        panic!("The complete inverse should preserve X1")
    };
    let files = repo.tree_files(tree)?;
    assert!(files.contains_key("first.txt"));
    for path in ["second.txt", "third.txt", "retired.txt"] {
        assert!(!files.contains_key(path));
    }
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn undo_prefix_is_checked_and_published_once_with_atomic_cancellation_receipts() -> Result<()>
{
    let f = Fixture::new(3).await?;
    fs::write(f.workspaces[0].path.join("parent.txt"), "Own parent file\n")?;
    f.save(0, None).await?;
    publish(&f, Some(vec![f.workspaces[0].track_id.clone()])).await?;
    fs::write(
        f.workspaces[2].path.join("independent.txt"),
        "Own independent file\n",
    )?;
    f.save(2, None).await?;
    publish(&f, Some(vec![f.workspaces[2].track_id.clone()])).await?;
    fs::write(
        f.workspaces[1].path.join("replacement.txt"),
        "Own replacement file\n",
    )?;
    f.save(1, None).await?;
    let rows = records(&f)?;
    let selected = rows
        .iter()
        .filter(|record| record.track_id == f.workspaces[0].track_id)
        .cloned()
        .collect::<Vec<_>>();
    let opened = f
        .action(Action::Open {
            tracks: Some(vec![f.workspaces[1].track_id.clone()]),
        })
        .await?;
    let opened = freeze_dummy_undo(&f, opened, selected.clone())?;
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    let cancellations = || -> Result<i64> {
        Ok(
            db.query_row("SELECT count(*) FROM integration_cancelled", [], |row| {
                row.get(0)
            })?,
        )
    };
    assert_eq!(cancellations()?, 0);
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let candidate = checked.candidate.as_ref().unwrap();
    let undo = candidate.undo.as_ref().unwrap();
    let undo_sha = undo.sha.clone();
    assert_eq!(
        undo.records,
        selected.iter().map(|record| record.id).collect::<Vec<_>>()
    );
    assert_eq!(candidate.commits.len(), 1);
    assert_eq!(candidate.secret_scans.len(), 1);
    assert_eq!(candidate.commits[0].parent, undo.sha);
    assert!(
        candidate
            .scans()
            .all(|scan| scan.accepted(&checked.kept_secrets))
    );
    assert_eq!(candidate.check.as_ref().unwrap().tree, candidate.tree);
    let source = Repository::open(&f.source)?;
    let files = source.tree_files(candidate.tree.parse()?)?;
    assert!(!files.contains_key("parent.txt"));
    assert!(files.contains_key("independent.txt"));
    assert!(files.contains_key("replacement.txt"));
    assert_eq!(cancellations()?, 0);
    // A repair edits a worker contribution while retaining the exact app
    // prefix. The inverse must not be attributed to that worker or rebuilt.
    let settings = f
        .journal
        .integration_settings(checked.id, checked.revision)
        .await?
        .unwrap();
    let fix = intercat_daemon::repair::Workspace::prepare(
        &checked,
        &candidate.workspace,
        &f._root.path().join("undo-worker-fix"),
        &f.workspaces[1].track_id,
        &settings,
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    fs::write(
        fix.checkout.workdir.join("replacement.txt"),
        "Own repaired replacement\n",
    )?;
    let fix_plan = fix
        .finish(
            &checked,
            &candidate.workspace,
            &settings,
            tokio::sync::watch::channel(false).1,
        )
        .await?;
    let rebuilt = fix
        .rebuild(
            &checked,
            &fix_plan,
            &f._root.path().join("undo-fixed-candidate"),
            &f._root.path().join("undo-fix-downloads"),
            &settings,
            tokio::sync::watch::channel(false).1,
        )
        .await?;
    assert_eq!(
        rebuilt.state,
        intercat_daemon::candidate::State::Checked,
        "{:?}",
        rebuilt.reason
    );
    assert_eq!(rebuilt.undo.as_ref().unwrap().sha, undo_sha);
    assert_eq!(rebuilt.commits[0].parent, undo_sha);
    let repaired_files = Repository::open(&f.source)?.tree_files(rebuilt.tree.parse()?)?;
    assert!(!repaired_files.contains_key("parent.txt"));
    assert!(repaired_files.contains_key("independent.txt"));
    assert_eq!(cancellations()?, 0);
    // Recheck keeps both the app's undo and the worker's actual commit SHA.
    let worker_sha = candidate.commits[0].sha.clone();
    f.action(Action::Recheck {
        id: checked.id,
        revision: checked.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    assert_eq!(
        checked
            .candidate
            .as_ref()
            .unwrap()
            .undo
            .as_ref()
            .unwrap()
            .sha,
        undo_sha
    );
    assert_eq!(
        checked.candidate.as_ref().unwrap().commits[0].sha,
        worker_sha
    );
    let committing = f
        .journal
        .integration_command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: commit_action(&checked),
        })
        .await?;
    assert_eq!(committing.status, Status::Committing);
    db.execute_batch("CREATE TRIGGER fail_undo_cancellation BEFORE INSERT ON integration_cancelled BEGIN SELECT RAISE(ABORT,'dummy storage full'); END;")?;
    assert!(
        intercat_daemon::publication::publish(
            committing.clone(),
            f.journal.clone(),
            tokio::sync::watch::channel(false).1
        )
        .await
        .is_err()
    );
    // Git's branch has moved, while the whole journal transaction rolled back.
    // Recovery finishes that exact publication; it never repeats the inverse.
    assert_eq!(
        source.reference_target(&committing.plan.as_ref().unwrap().target_ref)?,
        Some(committing.candidate.as_ref().unwrap().head.parse()?)
    );
    assert_eq!(cancellations()?, 0);
    assert_eq!(
        db.query_row("SELECT count(*) FROM integration_undos", [], |row| row
            .get::<_, i64>(0))?,
        0
    );
    assert_eq!(records(&f)?.len(), rows.len());
    assert_eq!(
        f.journal.integration_job(committing.id).await?.status,
        Status::Committing
    );
    db.execute_batch("DROP TRIGGER fail_undo_cancellation;")?;
    let recovered = f.journal.integration_job(committing.id).await?;
    let saved = intercat_daemon::publication::publish(
        recovered,
        f.journal.clone(),
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    assert_eq!(saved.status, Status::Done, "{:?}", saved.error);
    assert_eq!(cancellations()?, 1);
    assert_eq!(
        source.reference_target(&format!("refs/intercat/{}/undo/{}", saved.slug, saved.id))?,
        Some(undo_sha.parse()?)
    );
    let raw: String = db.query_row(
        "SELECT record FROM integration_undos WHERE job=?1",
        [saved.id.to_string()],
        |row| row.get(0),
    )?;
    let receipt: serde_json::Value = serde_json::from_str(&raw)?;
    assert_eq!(receipt["commit"]["sha"], undo_sha);
    assert_eq!(receipt["commit"]["records"][0], selected[0].id.to_string());
    let ids = db
        .prepare("SELECT record FROM integration_cancelled")?
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|row| Ok(Uuid::parse_str(&row?)?))
        .collect::<Result<std::collections::BTreeSet<_>>>()?;
    assert!(matches!(intercat_daemon::replacement_delta::select(&rows,
        &std::collections::BTreeMap::from([(f.workspaces[0].track_id.clone(), Boundary::Fork(1))]), &ids)?,
        intercat_daemon::replacement_delta::Selection::Ready(records) if records.is_empty()));
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM timeline_commits WHERE sha=?1",
            [&undo_sha],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    assert!(
        f.journal.snapshot().await?["integrations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|job| job["id"] == saved.id.to_string()
                && job["candidate"]["undo"]["sha"] == undo_sha)
    );
    // A stale caller cannot publish an old receipt a second time, even when
    // the inverse is now an empty commit and would otherwise succeed cleanly.
    let next = f
        .action(Action::Open {
            tracks: Some(vec![f.workspaces[1].track_id.clone()]),
        })
        .await?;
    let next = freeze_dummy_undo(&f, next, selected)?;
    f.action(Action::Prepare {
        id: next.id,
        revision: next.revision,
    })
    .await?;
    let checked_again = finished(&f.journal).await?;
    assert_eq!(
        checked_again.status,
        Status::Checked,
        "{:?}",
        checked_again.error
    );
    assert!(f.action(commit_action(&checked_again)).await.is_err());
    assert_eq!(cancellations()?, 1);
    assert_eq!(
        source.reference_target(&saved.plan.as_ref().unwrap().target_ref)?,
        Some(saved.candidate.as_ref().unwrap().head.parse()?)
    );
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn keeping_a_secret_in_the_undo_resumes_its_saved_checkout_before_building_workers()
-> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(2).await?;
    fs::write(f.workspaces[0].path.join("parent.txt"), "Own parent file\n")?;
    f.save(0, None).await?;
    publish(&f, Some(vec![f.workspaces[0].track_id.clone()])).await?;
    fs::write(
        f.workspaces[1].path.join("replacement.txt"),
        "Own replacement file\n",
    )?;
    f.save(1, None).await?;
    let dummy = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    let hook = f.source.join(".git/hooks/commit-msg");
    let pre_commit = f.source.join(".git/hooks/pre-commit");
    fs::write(
        &pre_commit,
        "#!/bin/sh\nif [ ! -f .git/undo-first ]; then\n  printf 'Own private Undo-only file\\n' > own-undo-note.txt\n  git add own-undo-note.txt\nfi\n",
    )?;
    fs::set_permissions(&pre_commit, fs::Permissions::from_mode(0o755))?;
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nif [ ! -f .git/undo-first ]; then\n  printf '\\n{dummy}\\n' >> \"$1\"\n  : > .git/undo-first\nfi\n"
        ),
    )?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
    let opened = f
        .action(Action::Open {
            tracks: Some(vec![f.workspaces[1].track_id.clone()]),
        })
        .await?;
    let selected = records(&f)?;
    let opened = freeze_dummy_undo(&f, opened, selected)?;
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let failed = finished(&f.journal).await?;
    assert_eq!(failed.status, Status::Failed);
    let candidate = failed.candidate.as_ref().unwrap();
    assert_eq!(
        candidate.state,
        intercat_daemon::candidate::State::SecretFound
    );
    assert!(candidate.pending_undo());
    assert!(candidate.commits.is_empty());
    let diff = f
        .service
        .diff(integration::DiffRequest {
            id: failed.id,
            revision: failed.revision,
            owner: integration::DiffOwner::Undo,
            path: "own-undo-note.txt".into(),
        })
        .await?;
    assert_eq!(diff.after, "Own private Undo-only file\n");
    let undo = candidate.undo.as_ref().unwrap();
    let undo_sha = undo.sha.clone();
    let findings = undo
        .scan
        .findings
        .iter()
        .map(|finding| finding.id.clone())
        .collect::<Vec<_>>();
    assert!(!findings.is_empty());
    assert!(!serde_json::to_string(&failed)?.contains(dummy));
    assert!(
        Repository::open(&f.source)?
            .commit_details(undo_sha.parse()?)
            .is_err()
    );
    let mut current = failed;
    for finding in findings {
        current = f
            .action(Action::KeepSecret {
                id: current.id,
                revision: current.revision,
                finding,
            })
            .await?;
    }
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let candidate = checked.candidate.as_ref().unwrap();
    assert!(!candidate.pending_undo());
    assert_eq!(candidate.undo.as_ref().unwrap().sha, undo_sha);
    assert_eq!(candidate.commits.len(), 1);
    assert_eq!(candidate.commits[0].parent, undo_sha);
    assert!(!serde_json::to_string(&checked)?.contains(dummy));
    assert!(
        candidate
            .scans()
            .all(|scan| scan.accepted(&checked.kept_secrets))
    );
    f.action(commit_action(&checked)).await?;
    let saved = finished(&f.journal).await?;
    assert_eq!(saved.status, Status::Done, "{:?}", saved.error);
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    assert_eq!(
        db.query_row("SELECT count(*) FROM integration_cancelled", [], |row| row
            .get::<_, i64>(
            0
        ))?,
        1
    );
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_published_record_uses_the_actual_head_coverage_after_a_worker_cursor_is_reset()
-> Result<()> {
    let f = Fixture::new(2).await?;
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own already published parent\n",
    )?;
    let parent = f.save(0, None).await?;
    publish(&f, Some(vec![f.workspaces[0].track_id.clone()])).await?;
    // Restore changes a worker's integration cursor independently of the
    // already published head, which can retain shared context from that path.
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    db.execute(
        "DELETE FROM integrated_lineage WHERE track=?1",
        [&f.workspaces[0].track_id],
    )?;
    fs::write(
        f.workspaces[1].path.join("independent.txt"),
        "Own independent work\n",
    )?;
    f.save(1, None).await?;
    let opened = f
        .action(Action::Open {
            tracks: Some(vec![f.workspaces[1].track_id.clone()]),
        })
        .await?;
    assert!(
        !opened
            .lineage
            .integrated
            .contains_key(&f.workspaces[0].track_id)
    );
    assert_eq!(
        opened.plan.as_ref().unwrap().coverage.as_ref().unwrap()[&f.workspaces[0].track_id].sha,
        parent.sha
    );
    f.action(Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    f.action(commit_action(&checked)).await?;
    let saved = finished(&f.journal).await?;
    assert_eq!(saved.status, Status::Done, "{:?}", saved.error);
    let rows = records(&f)?;
    let independent = rows
        .iter()
        .find(|record| record.track_id == f.workspaces[1].track_id)
        .unwrap();
    assert_eq!(
        independent.coverage_before[&f.workspaces[0].track_id].sha,
        parent.sha
    );
    assert_eq!(
        independent.coverage_after[&f.workspaces[0].track_id].sha,
        parent.sha
    );
    let sources = std::collections::BTreeMap::from([(
        f.workspaces[0].track_id.clone(),
        Boundary::EntireTrack,
    )]);
    let intercat_daemon::replacement_delta::Selection::Ready(selected) =
        intercat_daemon::replacement_delta::select(&rows, &sources, &Default::default())?
    else {
        panic!("B must not claim to have conveyed the parent's already published changes");
    };
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].track_id, f.workspaces[0].track_id);
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}
