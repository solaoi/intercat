//! A shared source can already exist in the descendant's borrowed world base.
//! Cancelling its published receipt must not lose the shared source's own edits.
use super::*;

#[tokio::test]
async fn a_previously_borrowed_shared_base_is_reintroduced_after_its_receipt_is_cancelled()
-> Result<()> {
    let mut f = Fixture::new(5).await?;
    fs::write(f.workspaces[0].path.join("shared-a.txt"), "Own shared A\n")?;
    let fork = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("retired-a.txt"),
        "Own retired A later\n",
    )?;
    let old = f.save(0, Some(fork.clone())).await?;
    f.inherit(2, 0, &old)?;
    fs::write(
        f.workspaces[2].path.join("shared-b.txt"),
        "Own published shared B\n",
    )?;
    let shared = f.save(2, None).await?;
    let published = f
        .publish(vec![
            f.workspaces[0].track_id.clone(),
            f.workspaces[2].track_id.clone(),
        ])
        .await?;
    f.branch(&fork)?;
    fs::write(f.workspaces[1].path.join("chosen-a.txt"), "Own chosen A\n")?;
    f.save(1, None).await?;
    branch_shared(&mut f, &shared)?;
    fs::write(f.workspaces[3].path.join("chosen-b.txt"), "Own chosen B\n")?;
    let child = f.save(3, None).await?;
    f.worker(3, child).await?;
    let borrow = Request {
        op_id: Uuid::now_v7(),
        track_id: f.workspaces[3].track_id.clone(),
        source: Source::Commit {
            job_id: published.id,
            revision: published.revision,
        },
    };
    let applied = f
        .apply_files(3, &borrow)
        .await?
        .ok_or("Missing actual borrowed-base realignment")?;
    f.worker(3, applied.checkpoint).await?;
    f.journal
        .change_realign(crate::realign_store::Change::Complete(applied.operation))
        .await?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let before = crate::lineage_store::basis(&db, &f.workspaces[3].track_id)?;
    assert_eq!(
        before.base_inherits[&f.workspaces[2].track_id].sha,
        shared.sha
    );
    fs::write(
        f.workspaces[2].path.join("later-b.txt"),
        "Own retired B after the fork\n",
    )?;
    f.save(2, Some(shared.clone())).await?;
    let opened = f.action(Action::Open { tracks: None }).await?;
    let chosen = f
        .action(Action::ChoosePath {
            id: opened.id,
            revision: opened.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    let chosen = f
        .action(Action::ChoosePath {
            id: chosen.id,
            revision: chosen.revision,
            track: f.workspaces[3].track_id.clone(),
        })
        .await?;
    let undo = chosen
        .undo
        .as_ref()
        .ok_or("The published source receipts must be cancelled")?;
    assert!(
        undo.records
            .iter()
            .any(|receipt| receipt.track_id == f.workspaces[2].track_id
                && receipt.checkpoint.sha == shared.sha)
    );
    let changed = f.reparent(3, 2, &chosen).await?;
    assert!(changed.path_state.as_ref().unwrap().ready());
    let ready = f.action(Action::Open { tracks: None }).await?;
    assert_eq!(ready.status, Status::Open, "{:?}", ready.error);
    f.action(Action::Prepare {
        id: ready.id,
        revision: ready.revision,
    })
    .await?;
    let checked = f.settled(ready.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let repo = intercat_git::Repository::open(&f.source)?;
    let tree = checked.candidate.as_ref().unwrap().tree.parse()?;
    let files = repo.tree_files(tree)?;
    for path in [
        "shared-a.txt",
        "chosen-a.txt",
        "shared-b.txt",
        "chosen-b.txt",
    ] {
        assert!(files.contains_key(path), "{path}");
    }
    for path in ["retired-a.txt", "later-b.txt"] {
        assert!(!files.contains_key(path), "{path}");
    }
    assert!(checked.candidate.as_ref().unwrap().undo.is_some());
    let ids = checked.undo.as_ref().unwrap().ids();
    let done = publish_unhosted_replacement(&f, &checked).await?;
    verify_restore_sources(&f, &done).await?;
    let target = repo
        .reference_target(&done.plan.as_ref().unwrap().target_ref)?
        .ok_or("Missing published replacement")?;
    assert_eq!(repo.tree_files(repo.tree_id(target)?)?, files);
    for id in ids {
        let cancelled: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM integration_cancelled WHERE record=?1)",
            [id.to_string()],
            |row| row.get(0),
        )?;
        assert!(
            cancelled,
            "Each reversed receipt must be cancelled exactly once"
        );
    }
    let raw: String = db.query_row(
        "SELECT record FROM integration_records WHERE job=?1 AND track=?2",
        params![done.id.to_string(), f.workspaces[3].track_id],
        |row| row.get(0),
    )?;
    let receipt: crate::integration_record::Record = serde_json::from_str(&raw)?;
    assert!(
        !receipt
            .coverage_before
            .contains_key(&f.workspaces[2].track_id)
    );
    assert_eq!(
        receipt.coverage_after[&f.workspaces[2].track_id].sha,
        shared.sha
    );
    assert!(f.workspaces[2].path.join("later-b.txt").exists());
    // Restore takes the current session head, rather than the head at which
    // the retirement was published. Its undo inputs still belong to that
    // exact retirement even after an independent worker commits.
    fs::write(
        f.workspaces[4].path.join("independent.txt"),
        "Own later independent work\n",
    )?;
    f.save(4, None).await?;
    let later = f.publish(vec![f.workspaces[4].track_id.clone()]).await?;
    let entries: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    for entry in entries.into_iter().filter(|entry| entry.job == done.id) {
        let source = f.journal.path_restore_source(entry.id).await?;
        let repo = source.verify_repository()?;
        assert_eq!(source.target, later.candidate.as_ref().unwrap().head);
        assert_eq!(source.entry.job, done.id);
        assert!(!source.cancelled.is_empty());
        let inputs = crate::path_restore::Inputs {
            track: source.entry.track.clone(),
            before: source.entry.saved.as_ref().unwrap().sha.clone(),
            basis: source.basis.clone(),
            target: source.target.clone(),
            coverage: source.coverage.clone(),
            prefix: source.prefix.clone(),
            cancelled: source.cancelled.clone(),
            resolutions: vec![],
        };
        let crate::path_restore::Outcome::Prepared(restored) =
            crate::path_restore::prepare(&repo, &source.graph, &inputs)?
        else {
            panic!("Independent later Commit must stay compatible with Restore");
        };
        let files = repo.tree_files(restored.tree.parse()?)?;
        assert!(files.contains_key("independent.txt"));
        assert!(files.contains_key("shared-a.txt"));
        assert!(files.contains_key("shared-b.txt"));
        assert!(files.contains_key("chosen-a.txt"));
        assert!(files.contains_key("chosen-b.txt"));
        let own = if entry.track == f.workspaces[0].track_id {
            "retired-a.txt"
        } else {
            "later-b.txt"
        };
        assert!(files.contains_key(own));
    }
    restore_saved_paths(&f, &done, true).await?;
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    f.review.shutdown().await;
    Ok(())
}
