//! Continue a chosen B fork, then replace its borrowed A suffix. Frozen source
//! checkpoints stay immutable; only the current writer receives file changes.
use super::super::super::shared as fixtures;
use super::*;

async fn continued_b(repeated: bool, consumers: usize) -> Result<()> {
    let (mut f, old_review, _request, shared, later) = fixtures::nested(false, consumers).await?;
    f.action(crate::integration::Action::Discard {
        id: old_review.id,
        revision: old_review.revision,
    })
    .await?;
    let source = f.workspaces[3].track_id.clone();
    let point = latest(&f, 3).await?;
    let first = fresh_at(&mut f, 3, point, "continued-b-final.txt").await?;
    let current = if repeated {
        let point = latest(&f, first).await?;
        fresh_at(&mut f, first, point, "continued-b-again.txt").await?
    } else {
        first
    };
    let previous = latest(&f, current).await?;
    fs::write(
        f.workspaces[current].path.join("continued-b-own.txt"),
        "Own continued B correction\n",
    )?;
    let checkpoint = f.save(current, Some(previous)).await?;
    let snapshot = f.journal.snapshot().await?;
    let mut worker: WorkerState = serde_json::from_value(
        snapshot["workers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|worker| worker["trackId"] == f.workspaces[current].track_id)
            .unwrap()
            .clone(),
    )?;
    worker.checkpoint = Some(checkpoint);
    f.journal.worker_state(worker).await?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let frozen = db
        .prepare("SELECT record FROM checkpoint_lineage WHERE track=?1 ORDER BY number")?
        .query_map([&source], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let job = f
        .action(crate::integration::Action::Open { tracks: None })
        .await?;
    let job = f
        .action(crate::integration::Action::ChoosePath {
            id: job.id,
            revision: job.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    let job = f
        .action(crate::integration::Action::ChoosePath {
            id: job.id,
            revision: job.revision,
            track: f.workspaces[current].track_id.clone(),
        })
        .await?;
    assert!(
        job.path_state
            .as_ref()
            .unwrap()
            .descendants
            .iter()
            .all(|child| child.track != source),
        "Closed B must be shared context, not a live realignment target: {:?}",
        job.path_state
    );
    let mut updated = f
        .reparent(current, if repeated { first } else { 3 }, &job)
        .await?;
    for index in 4..3 + consumers {
        updated = f.reparent(index, 2, &updated).await?;
        for path in ["shared-a.txt", "chosen-a.txt", "shared-b.txt"] {
            assert!(f.workspaces[index].path.join(path).is_file());
        }
        for path in ["retired-a.txt", "later-b.txt", "chosen-b.txt"] {
            assert!(!f.workspaces[index].path.join(path).exists());
        }
    }
    assert!(
        updated.path_state.as_ref().unwrap().ready(),
        "{:?}",
        updated.path_state
    );
    for path in [
        "shared-a.txt",
        "chosen-a.txt",
        "shared-b.txt",
        "chosen-b.txt",
        "continued-b-final.txt",
        "continued-b-own.txt",
    ] {
        assert!(
            f.workspaces[current].path.join(path).is_file(),
            "Missing own continued B file {path}"
        );
    }
    for path in ["retired-a.txt", "later-b.txt"] {
        assert!(
            !f.workspaces[current].path.join(path).exists(),
            "Revived retired source {path}"
        );
    }
    if repeated {
        assert!(
            f.workspaces[current]
                .path
                .join("continued-b-again.txt")
                .is_file()
        );
        assert!(crate::continuation::closed(
            &db,
            &f.workspaces[first].track_id
        )?);
    }
    assert!(f.workspaces[2].path.join("retired-a.txt").exists());
    assert!(f.workspaces[2].path.join("later-b.txt").exists());
    assert_eq!(
        db.prepare("SELECT record FROM checkpoint_lineage WHERE track=?1 ORDER BY number")?
            .query_map([&source], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?,
        frozen
    );
    assert!(crate::continuation::closed(&db, &source)?);
    let opened = f
        .action(crate::integration::Action::Open { tracks: None })
        .await?;
    f.action(crate::integration::Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let checked = f.settled(opened.id).await?;
    assert_eq!(
        checked.status,
        crate::integration::Status::Checked,
        "{:?}",
        checked.error
    );
    let repo = intercat_git::Repository::open(&f.source)?;
    let files = repo.tree_files(checked.candidate.as_ref().unwrap().tree.parse()?)?;
    for path in [
        "shared-a.txt",
        "chosen-a.txt",
        "shared-b.txt",
        "chosen-b.txt",
        "continued-b-final.txt",
        "continued-b-own.txt",
    ] {
        assert!(
            files.contains_key(path),
            "Missing checked B contribution {path}"
        );
    }
    for path in ["retired-a.txt", "later-b.txt"] {
        assert!(!files.contains_key(path), "Leaked retired source {path}");
    }
    if repeated {
        assert!(files.contains_key("continued-b-again.txt"));
    }
    for index in 4..3 + consumers {
        assert!(files.contains_key(&format!("consumer-{index}.txt")));
    }
    let done = world::publish_idle(&f, &checked).await?;
    assert_eq!(
        repo.tree_files(done.candidate.as_ref().unwrap().tree.parse()?)?,
        files
    );
    let retirements: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    assert!(retirements.iter().all(|entry| entry.track != source));
    let now = crate::lineage_store::point(
        &db,
        &f.workspaces[current].track_id,
        &latest(&f, current).await?.sha,
    )?;
    assert!(
        now.views
            .iter()
            .any(|view| view.track == f.workspaces[2].track_id && view.source.sha == shared.sha)
    );
    assert!(
        !now.views
            .iter()
            .any(|view| view.track == f.workspaces[2].track_id && view.source.sha == later.sha)
    );
    if repeated {
        Box::pin(round_trip(&f, current, later, &done)).await?;
        assert_eq!(
            db.prepare("SELECT record FROM checkpoint_lineage WHERE track=?1 ORDER BY number")?
                .query_map([&source], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            frozen
        );
    }
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    f.review.shutdown().await;
    Ok(())
}

async fn round_trip(f: &Fixture, current: usize, later: Checkpoint, done: &Job) -> Result<()> {
    let original_files = world::files(f, done)?;
    Box::pin(world::restore(f, 2)).await?;
    f.worker(2, later).await?;
    let opened = f
        .action(crate::integration::Action::Open {
            tracks: Some(vec![
                f.workspaces[1].track_id.clone(),
                f.workspaces[2].track_id.clone(),
            ]),
        })
        .await?;
    let chosen = f
        .action(crate::integration::Action::ChoosePath {
            id: opened.id,
            revision: opened.revision,
            track: f.workspaces[2].track_id.clone(),
        })
        .await?;
    let changed = f.reparent(2, 0, &chosen).await?;
    assert!(changed.path_state.as_ref().unwrap().ready());
    let ready = f
        .action(crate::integration::Action::Open {
            tracks: Some(vec![
                f.workspaces[1].track_id.clone(),
                f.workspaces[2].track_id.clone(),
            ]),
        })
        .await?;
    assert_eq!(ready.status, crate::integration::Status::Open);
    f.action(crate::integration::Action::Prepare {
        id: ready.id,
        revision: ready.revision,
    })
    .await?;
    let checked = f.settled(ready.id).await?;
    assert_eq!(checked.status, crate::integration::Status::Checked);
    let alternative = world::publish_idle(f, &checked).await?;
    let alternative_files = world::files(f, &alternative)?;
    assert!(alternative_files.contains_key("later-b.txt"));
    assert!(!alternative_files.contains_key("continued-b-own.txt"));
    assert!(!alternative_files.contains_key("retired-a.txt"));
    Box::pin(world::restore(f, current)).await?;
    let checked = world::checked_choice(f, current).await?;
    let returned = world::publish_idle(f, &checked).await?;
    assert_eq!(world::files(f, &returned)?, original_files);
    Ok(())
}

#[tokio::test]
async fn a_continued_b_fork_keeps_its_shared_prefix_and_own_files_while_replacing_a() -> Result<()>
{
    Box::pin(continued_b(false, 1)).await
}

#[tokio::test]
async fn repeated_b_continuations_and_another_consumer_preserve_shared_prefixes_through_restore()
-> Result<()> {
    Box::pin(continued_b(true, 2)).await
}
