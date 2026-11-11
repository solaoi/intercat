//! Source closure, immutable replay and cancellation during a real Git filter.
use super::*;
use crate::preparation::Workspace;
use std::time::Duration;

#[tokio::test]
async fn closed_metadata_cannot_capture_current_files_or_reserve_a_late_checkpoint() -> Result<()> {
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let source = job.workspace.clone();
    let old = job.boundary.as_ref().unwrap().checkpoint.clone();
    let captured = checkpoint::capture(
        &source,
        &old.sha,
        "Own completed before transfer",
        &f.journal,
    )
    .await?;
    let job = crate::fresh_files::transfer(&f.journal, &job).await?;
    crate::fresh_files::publish_owner(&f.journal, &job).await?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let count = db.query_row(
        "SELECT count(*) FROM checkpoint_lineage WHERE track=?1",
        [&source.track_id],
        |row| row.get::<_, i64>(0),
    )?;
    let parked = f.root.path().join("own-parked-worktree");
    fs::rename(&source.path, &parked)?;
    let rejected = checkpoint::capture(
        &source,
        &old.sha,
        "Must not open the missing old path",
        &f.journal,
    )
    .await;
    let saved = checkpoint::save(
        source.clone(),
        Uuid::now_v7(),
        Some(old.clone()),
        false,
        f.journal.clone(),
    )
    .await;
    fs::rename(&parked, &source.path)?;
    assert!(
        rejected
            .unwrap_err()
            .to_string()
            .contains("permanently closed")
    );
    assert!(
        saved
            .unwrap_err()
            .to_string()
            .contains("permanently closed")
    );
    assert!(
        f.journal
            .step_source_owner(source.track_id.clone(), job.request.session_id.clone())
            .await?
            .is_none()
    );
    let snapshot = f.journal.snapshot().await?;
    let original = snapshot["workers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|worker| worker["trackId"] == source.track_id)
        .unwrap()
        .clone();
    let mut stale: WorkerState = serde_json::from_value(original.clone())?;
    stale.phase = "working".into();
    stale.active_turn = Some(Uuid::now_v7());
    stale.checkpoint = None;
    assert!(f.journal.worker_state(stale).await.is_err());
    assert_eq!(
        *f.journal.snapshot().await?["workers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|worker| worker["trackId"] == source.track_id)
            .unwrap(),
        original
    );
    let turn = Uuid::now_v7();
    let id = Uuid::new_v5(&turn, b"checkpoint");
    let late = Checkpoint {
        turn_id: turn,
        sha: captured.sha,
        reference: format!("{}-checkpoints/{turn}", source.reference),
        unmerged_paths: vec![],
        live_processes: false,
    };
    assert!(f.journal.intent(&id.to_string(),"checkpoint.save",json!({"trackId":source.track_id,"checkpoint":late,"latestRef":format!("{}-checkpoint",source.reference),"expected":old.sha,"lfsDirectory":captured.lfs_directory})).await.unwrap_err().to_string().contains("permanently closed"));
    assert!(f.journal.operation(id).await?.is_none());
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM checkpoint_lineage WHERE track=?1",
            [&source.track_id],
            |row| row.get::<_, i64>(0)
        )?,
        count
    );
    fs::write(
        source.path.join("new-owner-only.txt"),
        "Own successor bytes\n",
    )?;
    let replay =
        checkpoint::save(source.clone(), old.turn_id, None, false, f.journal.clone()).await?;
    assert_eq!(replay.sha, old.sha);
    let repo = intercat_git::Repository::open(&source.path)?;
    assert!(
        !repo
            .saved_files(replay.sha.parse()?)?
            .contains_key("new-owner-only.txt")
    );
    let successor = f.journal.workspace(job.successor_track).await?;
    let current =
        checkpoint::capture(&successor, &successor.base, "Own current owner", &f.journal).await?;
    assert!(
        repo.saved_files(current.sha.parse()?)?
            .contains_key("new-owner-only.txt")
    );
    Ok(())
}

#[tokio::test]
async fn transfer_waits_for_capture_ownership_without_blocking_journal_stop_controls() -> Result<()>
{
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let scope = f.journal.capture_scope(job.workspace.clone()).await?;
    let journal = f.journal.clone();
    let id = job.request.op_id;
    let revision = job.revision;
    let mut transfer =
        tokio::spawn(async move { journal.fresh(Change::Transfer { id, revision }).await });
    let held = tokio::time::timeout(Duration::from_millis(80), &mut transfer)
        .await
        .is_err();
    f.journal.pause_dispatch(Uuid::now_v7(), true).await?;
    assert_eq!(f.journal.snapshot().await?["dispatchPaused"], true);
    f.journal.pause_dispatch(Uuid::now_v7(), false).await?;
    drop(scope);
    let transferred = tokio::time::timeout(Duration::from_secs(5), transfer).await???;
    assert!(held);
    assert_eq!(transferred.phase, Phase::PublishingOwner);
    assert!(f.journal.capture_scope(job.workspace).await.is_err());
    Ok(())
}

#[tokio::test]
async fn an_isolated_repair_snapshot_with_a_closed_owner_never_borrows_its_live_path() -> Result<()>
{
    let f = Fixture::new(1).await?;
    fs::write(
        f.workspaces[0].path.join("own-code.txt"),
        "Own candidate content\n",
    )?;
    let summary = summarized(&f).await?;
    let opened = f
        .action(crate::integration::Action::Open {
            tracks: Some(vec![f.workspaces[0].track_id.clone()]),
        })
        .await?;
    f.action(crate::integration::Action::Prepare {
        id: opened.id,
        revision: opened.revision,
    })
    .await?;
    let checked = f.settled(opened.id).await?;
    assert_eq!(checked.status, crate::integration::Status::Checked);
    let settings = f
        .journal
        .integration_settings(checked.id, checked.revision)
        .await?
        .unwrap();
    let repair = crate::repair::Workspace::prepare(
        &checked,
        &checked.candidate.as_ref().unwrap().workspace,
        &f.root.path().join("own-private-repair"),
        &f.workspaces[0].track_id,
        &settings,
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    let job = crate::fresh_files::transfer(&f.journal, &summary).await?;
    crate::fresh_files::publish_owner(&f.journal, &job).await?;
    let run = Uuid::now_v7();
    let private = repair.snapshot_workspace(run);
    fs::write(
        private.path.join("own-private.txt"),
        "Own isolated correction\n",
    )?;
    let saved = crate::repair_checkpoint::save(
        private.clone(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        None,
        &f.journal,
    )
    .await?;
    assert!(
        intercat_git::Repository::open(&private.path)?
            .saved_files(saved.sha.parse()?)?
            .contains_key("own-private.txt")
    );
    let deletion=crate::deletion::prepare(private.clone(),Some(run),json!({"itemId":"own-run-delete","approvalId":"own-run-delete","currentRequirementId":{"sourceIndex":0},"subject":{"kind":"shell","command":"rm own-private.txt","stages":[{"argv":["rm","own-private.txt"],"argvComplete":true}]}}),saved.sha.clone(),f.journal.clone()).await?;
    assert!(deletion.complete, "{:?}", deletion.uncovered);
    fs::remove_file(private.path.join("own-private.txt"))?;
    crate::deletion::restore(
        private.clone(),
        Some(run),
        deletion,
        Uuid::now_v7(),
        f.journal.clone(),
    )
    .await?;
    assert_eq!(
        fs::read_to_string(private.path.join("own-private.txt"))?,
        "Own isolated correction\n"
    );
    let mut fake = private.clone();
    fake.path = job.workspace.path.clone();
    assert!(
        checkpoint::capture(
            &fake,
            &fake.base,
            "Must not scan live worker as Run",
            &f.journal
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("private snapshot cannot read")
    );
    fake.path = job.workspace.path.parent().unwrap().to_path_buf();
    assert!(
        checkpoint::capture(
            &fake,
            &fake.base,
            "Must not scan a parent of the live worker",
            &f.journal
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("private snapshot cannot read")
    );
    let alias = f.root.path().join("own-live-alias");
    std::os::unix::fs::symlink(&job.workspace.path, &alias)?;
    fake.path = alias;
    assert!(
        checkpoint::capture(
            &fake,
            &fake.base,
            "Must not scan aliased live worker",
            &f.journal
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("private snapshot cannot read")
    );
    let mut standalone: Workspace = private;
    standalone.track_id = "own-unregistered-reader".into();
    standalone.reference = "refs/intercat/own-standalone".into();
    standalone.path = job.workspace.path;
    assert!(
        checkpoint::capture(
            &standalone,
            &standalone.base,
            "Must not impersonate current writer",
            &f.journal
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("private snapshot cannot read")
    );
    assert!(!standalone.path.join("own-private.txt").exists());
    f.review.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn cancelling_a_capture_caller_keeps_ownership_until_its_real_filter_finishes() -> Result<()>
{
    let f = Fixture::new(1).await?;
    fs::write(
        f.workspaces[0].path.join(".gitattributes"),
        "own-paused.txt filter=intercatpause\n",
    )?;
    fs::write(
        f.workspaces[0].path.join("own-paused.txt"),
        "Own unchanged filter input\n",
    )?;
    let nonce = Uuid::now_v7();
    let started = format!("own-started-{nonce}");
    let release = format!("own-release-{nonce}");
    let script = f.workspaces[0].path.join("own-pause-filter.sh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n/usr/bin/touch \"$TMPDIR/{started}\"\ni=0\nwhile [ ! -f \"$TMPDIR/{release}\" ]; do\n i=$((i+1))\n if [ \"$i\" -gt 500 ]; then exit 73; fi\n /bin/sleep .02\ndone\nexec /bin/cat\n"
        ),
    )?;
    let job = summarized(&f).await?;
    git(
        &job.workspace.path,
        &[
            "config",
            "filter.intercatpause.clean",
            &format!("/bin/sh '{}'", script.display()),
        ],
    )?;
    git(
        &job.workspace.path,
        &["config", "filter.intercatpause.required", "true"],
    )?;
    let workspace = job.workspace.clone();
    let journal = f.journal.clone();
    let capture = tokio::spawn(async move {
        checkpoint::capture(
            &workspace,
            &workspace.base,
            "Own cancellable caller",
            &journal,
        )
        .await
    });
    let directory = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            for entry in fs::read_dir(std::env::temp_dir())? {
                let entry = entry?;
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("intercat-checkpoint-")
                    && entry.path().join(&started).is_file()
                {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(entry.path());
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    capture.abort();
    assert!(capture.await.unwrap_err().is_cancelled());
    let journal = f.journal.clone();
    let id = job.request.op_id;
    let revision = job.revision;
    let mut transfer =
        tokio::spawn(async move { journal.fresh(Change::Transfer { id, revision }).await });
    let held = tokio::time::timeout(Duration::from_millis(100), &mut transfer)
        .await
        .is_err();
    fs::write(directory.join(&release), "Own filter may finish\n")?;
    let transferred = tokio::time::timeout(Duration::from_secs(15), transfer).await???;
    assert!(
        held,
        "Caller cancellation must not release an active snapshot"
    );
    assert_eq!(transferred.phase, Phase::PublishingOwner);
    assert!(
        !directory.exists(),
        "Owned snapshot and its filter must finish before ownership moves"
    );
    Ok(())
}
