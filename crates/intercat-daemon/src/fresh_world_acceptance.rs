//! Continuation ownership combined with real private Git publication/Undo and
//! Restore. SDK lifecycle is accepted separately by the offline native tests.
use super::super::world;
use super::*;
use std::collections::BTreeMap;
#[path = "fresh_shared_acceptance.rs"]
mod dependents;

async fn fresh_at(f: &mut Fixture, index: usize, point: Checkpoint, marker: &str) -> Result<usize> {
    let workspace = f.workspaces[index].clone();
    let mut source = WorkerState {
        track_id: workspace.track_id.clone(),
        phase: "ready".into(),
        session_id: Some(Uuid::now_v7().to_string()),
        secret_revision: Some(0),
        identity: None,
        detail: None,
        active_turn: None,
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: Some(point.clone()),
        prompts: vec![],
    };
    // A repeated continuation retains its actually reserved SDK identity.
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    if let Some(owner) = crate::continuation::start(&db, &workspace.track_id)? {
        source.session_id = Some(owner.session_id);
    }
    f.journal.worker_state(source.clone()).await?;
    receipt(
        f,
        &source,
        &point,
        "Own completed source before a world handoff",
    )?;
    let mut job = f.journal.fresh(Change::Request(request(&source))).await?;
    job = f
        .journal
        .fresh(Change::Capture {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    job = f
        .journal
        .fresh(Change::Release {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    source.phase = "stopped".into();
    f.journal.worker_state(source).await?;
    job = f
        .journal
        .fresh(Change::Released {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    fs::write(
        workspace.path.join(marker),
        format!("Own final bytes: {marker}\n"),
    )?;
    for _ in 0..5 {
        job = begin_files(f, job).await?;
        job = crate::fresh_files::capture(&f.journal, &job).await?;
        if job.phase == Phase::PublishingFiles {
            break;
        }
    }
    job = crate::fresh_files::publish(&f.journal, &job).await?;
    let changes = crate::fresh_summary::changes(&job).await?;
    job = f
        .journal
        .fresh(Change::PrepareSummary {
            id: job.request.op_id,
            revision: job.revision,
            changes,
        })
        .await?;
    let document = crate::handoff::Document {
        fields: crate::handoff::Fields {
            purpose: "Continue own world files".into(),
            constraints: vec!["Keep existing user files".into()],
            unresolved: vec![],
            changed_files: vec![marker.into()],
            completed: vec!["Own source turn ended".into()],
            unverified: vec!["No new checks run".into()],
            next_checks: vec!["Inspect current files".into()],
        },
        source_addresses: crate::handoff::addresses(
            job.summary_input.as_ref().unwrap(),
            &workspace.track_id,
        ),
    };
    job = f
        .journal
        .fresh(Change::Summarized {
            id: job.request.op_id,
            revision: job.revision,
            document,
        })
        .await?;
    job = crate::fresh_files::transfer(&f.journal, &job).await?;
    job = crate::fresh_files::publish_owner(&f.journal, &job).await?;
    assert_eq!(job.phase, Phase::Transferred);
    assert!(crate::continuation::closed(&db, &workspace.track_id)?);
    let child = f.journal.workspace(job.successor_track.clone()).await?;
    assert_eq!(child.path, workspace.path);
    f.workspaces.push(child);
    let child_index = f.workspaces.len() - 1;
    let owner = crate::continuation::start(&db, &job.successor_track)?.unwrap();
    let mut child = WorkerState {
        track_id: job.successor_track.clone(),
        phase: "ready".into(),
        session_id: Some(owner.session_id.clone()),
        secret_revision: Some(0),
        identity: None,
        detail: None,
        active_turn: None,
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    f.journal.worker_state(child.clone()).await?;
    let first = f.journal.read_utterance(owner.first_input.clone()).await?;
    let command = Uuid::now_v7();
    let turn = Uuid::now_v7();
    f.journal
        .session_start_opened(&job.successor_track, &owner.session_id)
        .await?;
    f.journal.reserve_turn(command,owner.first_input.clone(),first["revision"].as_i64().unwrap(),job.successor_track.clone(),json!({"commandId":command,"sessionId":owner.session_id,"input":[{"type":"text","text":first["text"]}]}), None).await?;
    for frame in [
        json!({"method":"turn/started","params":{"sessionId":owner.session_id,"turnId":turn,"commandId":command}}),
        json!({"method":"item/completed","params":{"sessionId":owner.session_id,"turnId":turn,"item":{"itemId":Uuid::now_v7(),"turnId":turn,"commandId":command,"kind":"userMessage","text":first["text"],"revision":1}}}),
        json!({"method":"turn/completed","params":{"sessionId":owner.session_id,"turnId":turn,"terminal":"completed"}}),
    ] {
        f.journal
            .try_msp(
                job.successor_track.clone(),
                owner.session_id.clone(),
                crate::clock::Stamp::now(),
                frame,
                None,
                None,
            )?
            .await??;
    }
    f.journal
        .input_execution(
            owner.first_input.clone(),
            first["revision"].as_i64().unwrap(),
            "landed",
        )
        .await?;
    f.journal.finish(&command.to_string(), "dispatched").await?;
    f.journal.finish(&command.to_string(), "complete").await?;
    let point = checkpoint::save(
        f.workspaces[child_index].clone(),
        turn,
        None,
        false,
        f.journal.clone(),
    )
    .await?;
    child.checkpoint = Some(point);
    f.journal.worker_state(child).await?;
    assert!(
        crate::continuation::start(&db, &job.successor_track)?
            .unwrap()
            .ready
    );
    Ok(child_index)
}

fn branch_at(f: &Fixture, parent: usize, point: &Checkpoint) -> Result<()> {
    let branch = crate::branch::Branch {
        request: crate::branch::Request {
            op_id: Uuid::now_v7(),
            parent_track_id: f.workspaces[parent].track_id.clone(),
            point: crate::branch_start::Selection::Turn {
                turn_id: point.turn_id,
            },
            instructions: crate::branch::InstructionMode::Checkpoint,
        },
        track_id: f.workspaces[1].track_id.clone(),
        parent_session_id: "own-world-source-session".into(),
        session_id: Some("own-world-alternate-session".into()),
        workspace: f.workspaces[1].clone(),
        point: crate::branch_start::Point::Turn {
            checkpoint: point.clone(),
        },
        phase: crate::branch::Phase::Ready,
        instructions: None,
        origin: crate::branch_points::Origin {
            ended_at: Some(1000),
            timeline_at: Some(1000),
            turn_number: 1,
        },
    };
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    db.execute(
        "INSERT INTO branches VALUES(?1,?2)",
        params![
            branch.request.op_id.to_string(),
            serde_json::to_string(&branch)?
        ],
    )?;
    f.inherit(1, parent, point)
}
async fn latest(f: &Fixture, index: usize) -> Result<Checkpoint> {
    Ok(serde_json::from_value::<WorkerState>(
        f.journal.snapshot().await?["workers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|worker| worker["trackId"] == f.workspaces[index].track_id)
            .unwrap()
            .clone(),
    )?
    .checkpoint
    .unwrap())
}

#[tokio::test]
async fn saved_comparison_keeps_the_branch_base_after_either_path_continues() -> Result<()> {
    use super::super::comparisons::{command, record_turn, settled};
    for (parent_fresh, child_fresh) in [(true, false), (false, true), (true, true)] {
        let mut f = Fixture::new(3).await?;
        fs::write(f.workspaces[0].path.join("outcome.txt"), "before fork\n")?;
        let before = f.save(0, None).await?;
        record_turn(&f, 0, &before)?;
        fs::write(f.workspaces[0].path.join("outcome.txt"), "base\n")?;
        let base = f.save(0, Some(before.clone())).await?;
        record_turn(&f, 0, &base)?;
        branch_at(&f, 0, &base)?;
        fs::write(f.workspaces[0].path.join("outcome.txt"), "parent\n")?;
        let parent = f.save(0, Some(base.clone())).await?;
        fs::write(f.workspaces[1].path.join("outcome.txt"), "child\n")?;
        let child = f.save(1, None).await?;
        let left = if parent_fresh {
            fresh_at(&mut f, 0, parent, "own-parent-late.txt").await?
        } else {
            record_turn(&f, 0, &parent)?;
            0
        };
        let mut right = if child_fresh {
            fresh_at(&mut f, 1, child, "own-child-late.txt").await?
        } else {
            record_turn(&f, 1, &child)?;
            1
        };
        if parent_fresh && child_fresh {
            let current = latest(&f, right).await?;
            right = fresh_at(&mut f, right, current, "own-child-second-fresh.txt").await?;
        }
        fs::write(
            f.workspaces[2].path.join("unrelated.txt"),
            "Own other path\n",
        )?;
        let unrelated = f.save(2, None).await?;
        record_turn(&f, 2, &unrelated)?;
        let point = |index: usize| -> Result<Checkpoint> {
            let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
            let raw: String = db.query_row(
                "SELECT payload FROM operations WHERE kind='checkpoint.save' AND state='complete' AND json_extract(payload,'$.trackId')=?1 ORDER BY rowid DESC LIMIT 1",
                [&f.workspaces[index].track_id], |row| row.get(0),
            )?;
            Ok(serde_json::from_value(
                serde_json::from_str::<Value>(&raw)?["checkpoint"].clone(),
            )?)
        };
        let request = crate::comparison::Open {
            op_id: Uuid::now_v7(),
            parent: crate::comparison::Point {
                track_id: f.workspaces[left].track_id.clone(),
                sha: point(left)?.sha,
            },
            child: crate::comparison::Point {
                track_id: f.workspaces[right].track_id.clone(),
                sha: point(right)?.sha,
            },
        };
        let capture = f.journal.capture_comparison(request.clone()).await?;
        assert_eq!(capture.base.sha(), base.sha);
        assert_eq!(capture.request, request);
        let mut too_early = request.clone();
        too_early.op_id = Uuid::now_v7();
        too_early.parent = crate::comparison::Point {
            track_id: f.workspaces[0].track_id.clone(),
            sha: before.sha,
        };
        assert!(f.journal.capture_comparison(too_early).await.is_err());
        let mut other = request.clone();
        other.op_id = Uuid::now_v7();
        other.parent = crate::comparison::Point {
            track_id: f.workspaces[2].track_id.clone(),
            sha: unrelated.sha,
        };
        assert!(f.journal.capture_comparison(other).await.is_err());
        if parent_fresh {
            let same_path = crate::comparison::Open {
                op_id: Uuid::now_v7(),
                parent: crate::comparison::Point {
                    track_id: f.workspaces[0].track_id.clone(),
                    sha: base.sha.clone(),
                },
                child: request.parent.clone(),
            };
            assert!(f.journal.capture_comparison(same_path).await.is_err());
        }
        command(&f, "test \"$(cat outcome.txt)\" = parent").await?;
        let service = crate::comparison_service::Service::new(
            f.journal.clone(),
            f.root.path().join("comparisons"),
        )
        .await?;
        service.open(request.clone()).await?;
        let result = settled(&f, request.op_id).await?;
        assert_eq!(result.phase, crate::comparison::Phase::Done);
        assert_eq!(
            result.sides.each_ref().map(|side| side
                .outcome
                .as_ref()
                .and_then(|outcome| outcome.check.as_ref())
                .and_then(|check| check.code)),
            [Some(0), Some(1)]
        );
        let diff = service
            .diff(crate::comparison::DiffRequest {
                id: result.id,
                basis: crate::comparison::DiffBasis::Paths,
                path: "outcome.txt".into(),
            })
            .await?;
        assert_eq!(
            (diff.before.as_str(), diff.after.as_str()),
            ("parent\n", "child\n")
        );
        service.shutdown().await;
        f.review.shutdown().await;
    }
    Ok(())
}
async fn continued_world(
    published_source: bool,
    branch_after_fresh: bool,
    repeated: bool,
    delete_prefix: bool,
) -> Result<()> {
    let mut f = Fixture::new(3).await?;
    fs::write(f.workspaces[0].path.join("shared.txt"), "Own shared fork\n")?;
    let initial = f.save(0, None).await?;
    fs::write(
        f.workspaces[0].path.join("source-tail.txt"),
        "Own source tail\n",
    )?;
    let last = f.save(0, Some(initial.clone())).await?;
    if published_source {
        f.publish(vec![f.workspaces[0].track_id.clone()]).await?;
    }
    let source_receipts = world::records(&f, &f.workspaces[0].track_id)?;
    if !branch_after_fresh {
        branch_at(&f, 0, &initial)?;
    }
    let first = fresh_at(&mut f, 0, last, "late-first.txt").await?;
    let first_point = latest(&f, first).await?;
    if branch_after_fresh {
        branch_at(&f, first, &first_point)?;
    }
    fs::write(
        f.workspaces[first].path.join("continued-tail.txt"),
        "Own continued tail\n",
    )?;
    let point = f.save(first, Some(first_point)).await?;
    let current = if repeated {
        fresh_at(&mut f, first, point, "late-second.txt").await?
    } else {
        let mut worker: WorkerState = serde_json::from_value(
            f.journal.snapshot().await?["workers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|worker| worker["trackId"] == f.workspaces[first].track_id)
                .unwrap()
                .clone(),
        )?;
        worker.checkpoint = Some(point);
        f.journal.worker_state(worker).await?;
        first
    };
    fs::write(
        f.workspaces[1].path.join("alternate.txt"),
        "Own real alternate\n",
    )?;
    f.save(1, None).await?;
    fs::write(
        f.workspaces[2].path.join("independent.txt"),
        "Own independent change\n",
    )?;
    let independent = f.save(2, None).await?;
    f.worker(2, independent).await?;
    f.publish(vec![f.workspaces[2].track_id.clone()]).await?;
    let selected = world::checked_choice(&f, current).await?;
    assert!(selected.undo.is_none());
    let original = world::publish_idle(&f, &selected).await?;
    assert!(world::cancelled(&f)?.is_empty());
    assert_eq!(
        world::records(&f, &f.workspaces[0].track_id)?,
        source_receipts
    );
    let mut original_files = world::files(&f, &original)?;
    for path in [
        "shared.txt",
        "source-tail.txt",
        "late-first.txt",
        "continued-tail.txt",
        "independent.txt",
    ] {
        assert!(
            original_files.contains_key(path),
            "Own continued world missing {path}"
        );
    }
    if repeated {
        assert!(original_files.contains_key("late-second.txt"));
    }
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let closed = crate::continuation::list(&db)?
        .into_iter()
        .map(|owner| owner.source)
        .collect::<Vec<_>>();
    let retirements: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    assert!(
        retirements
            .iter()
            .all(|entry| !closed.contains(&entry.track))
    );
    world::restore(&f, 1).await?;
    let selected = world::checked_choice(&f, 1).await?;
    let replaced = world::publish_idle(&f, &selected).await?;
    let mut expected = BTreeMap::from([
        ("parent.txt".into(), "Own initial base".into()),
        ("shared.txt".into(), "Own shared fork".into()),
        ("alternate.txt".into(), "Own real alternate".into()),
        ("independent.txt".into(), "Own independent change".into()),
    ]);
    if branch_after_fresh {
        expected.insert("source-tail.txt".into(), "Own source tail".into());
        expected.insert(
            "late-first.txt".into(),
            "Own final bytes: late-first.txt".into(),
        );
    }
    assert_eq!(world::files(&f, &replaced)?, expected);
    if delete_prefix {
        // A later independent Commit removes a file shared by both paths.
        // Returning to the continued path must not restore that older file.
        let independent = world::realign(&f, 2, latest(&f, 2).await?, &replaced).await?;
        assert!(f.workspaces[2].path.join("shared.txt").is_file());
        fs::remove_file(f.workspaces[2].path.join("shared.txt"))?;
        let deletion = f.save(2, Some(independent)).await?;
        f.worker(2, deletion).await?;
        let deleted = f.publish(vec![f.workspaces[2].track_id.clone()]).await?;
        expected.remove("shared.txt");
        assert_eq!(world::files(&f, &deleted)?, expected);
        original_files.remove("shared.txt");
    }
    // Only the current writer is restored. Historic metadata shares its path
    // but never receives retirement, Stop, file rewrites or renewed ownership.
    world::restore(&f, current).await?;
    assert_eq!(
        f.workspaces[current].path.join("shared.txt").exists(),
        !delete_prefix,
        "Restore must retain a later independent deletion of shared context"
    );
    assert_eq!(
        fs::read_to_string(f.workspaces[current].path.join("source-tail.txt"))?,
        "Own source tail\n"
    );
    for source in &closed {
        assert!(crate::continuation::ensure_writer(&db, source).is_err());
    }
    let selected = world::checked_choice(&f, current).await?;
    if let Some(output) = std::env::var_os("INTERCAT_M4_FRESH_WORLD_OUTPUT") {
        fs::create_dir_all(&output)?;
        fs::write(
            PathBuf::from(output).join("returning.json"),
            serde_json::to_vec_pretty(&selected)?,
        )?;
    }
    let returned = world::publish_idle(&f, &selected).await?;
    assert_eq!(world::files(&f, &returned)?, original_files);
    assert_eq!(
        world::records(&f, &f.workspaces[0].track_id)?,
        source_receipts
    );
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    f.review.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn an_unpublished_continuation_and_real_alternate_survive_commit_undo_and_restore()
-> Result<()> {
    continued_world(false, false, false, false).await
}
#[tokio::test]
async fn published_source_receipts_and_two_continuations_survive_commit_undo_and_restore()
-> Result<()> {
    continued_world(true, false, true, false).await
}
#[tokio::test]
async fn a_real_branch_between_two_continuations_preserves_its_exact_shared_prefix() -> Result<()> {
    continued_world(false, true, true, false).await
}
#[tokio::test]
async fn a_branch_after_a_published_source_continues_to_preserve_its_receipts() -> Result<()> {
    continued_world(true, true, true, false).await
}

#[tokio::test]
async fn restoring_two_continuations_preserves_a_later_independent_deletion() -> Result<()> {
    continued_world(true, false, true, true).await
}

#[tokio::test]
async fn restoring_a_continuation_with_a_later_fork_preserves_an_independent_deletion() -> Result<()>
{
    continued_world(false, true, true, true).await
}
