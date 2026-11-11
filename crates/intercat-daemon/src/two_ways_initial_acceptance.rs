//! Initial-file points use real Git; budget and SDK responses are explicitly modeled.
use super::*;
use crate::branch_start::Point;

#[tokio::test]
async fn an_interrupted_initial_intent_finishes_the_same_tree_after_explicit_retry() -> Result<()> {
    let f = Fixture::new(1).await?;
    let (_, job) = empty(&f).await?;
    let capturing = capture(&f, &job).await?;
    let workspace = &f.workspaces[0];
    fs::write(
        workspace.path.join("initial.txt"),
        "Own frozen before interruption\n",
    )?;
    let (files, permit) = crate::checkpoint::capture_owned(
        workspace,
        &workspace.base,
        "Own interrupted initial capture",
        &f.journal,
    )
    .await?;
    let intent = crate::two_ways_initial::Intent {
        job: job.request.op_id,
        track_id: job.source_track.clone(),
        session_id: job.source_session.clone(),
        snapshot: crate::branch_start::Initial {
            operation: crate::branch_start::initial_operation(job.request.op_id),
            sha: files.sha,
            reference: format!("{}-initial/{}", workspace.reference, job.request.op_id),
        },
        lfs_directory: files.lfs_directory,
    };
    f.journal
        .intent(
            &intent.snapshot.operation.to_string(),
            "two-ways.initial",
            serde_json::to_value(&intent)?,
        )
        .await?;
    drop(permit);
    fs::write(
        workspace.path.join("initial.txt"),
        "Own changed after interruption\n",
    )?;
    let held = f
        .journal
        .two_ways(PairChange::Hold {
            id: job.request.op_id,
            revision: capturing.revision,
            error: "Own modeled interruption after durable intent".into(),
        })
        .await?;
    let retried = f
        .journal
        .two_ways(PairChange::Command(two_ways::Command {
            op_id: Uuid::now_v7(),
            id: job.request.op_id,
            revision: held.revision,
            action: two_ways::Action::Retry,
        }))
        .await?;
    assert_eq!(retried.phase, PairPhase::CapturingInitial);
    let saved = crate::two_ways_initial::capture(f.journal.clone(), retried).await?;
    assert_eq!(saved.point.as_ref().unwrap().sha(), intent.snapshot.sha);
    assert_eq!(
        git(
            &f.source,
            &["show", &format!("{}:initial.txt", intent.snapshot.sha)]
        )?,
        "Own frozen before interruption"
    );
    assert_eq!(
        fs::read_to_string(workspace.path.join("initial.txt"))?,
        "Own changed after interruption\n"
    );
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM checkpoint_lineage WHERE track=?1 AND complete=1",
            [&job.source_track],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}

#[test]
fn empty_metadata_requires_an_explicit_zero_count_idle_session_and_no_pending_requests()
-> Result<()> {
    let valid = json!({"session":{"sessionId":"own-empty-session","activeTurnId":null,"turnCount":0},"pendingRequests":[]});
    crate::branch_start::verify_empty(&valid, "own-empty-session")?;
    for key in ["sessionId", "activeTurnId", "turnCount"] {
        let mut missing = valid.clone();
        missing["session"].as_object_mut().unwrap().remove(key);
        assert!(crate::branch_start::verify_empty(&missing, "own-empty-session").is_err());
    }
    for invalid in [
        json!({"session":{"sessionId":"other","activeTurnId":null,"turnCount":0},"pendingRequests":[]}),
        json!({"session":{"sessionId":"own-empty-session","activeTurnId":null,"turnCount":1},"pendingRequests":[]}),
        json!({"session":{"sessionId":"own-empty-session","activeTurnId":"own-turn","turnCount":0},"pendingRequests":[]}),
        json!({"session":{"sessionId":"own-empty-session","activeTurnId":null,"turnCount":0},"pendingRequests":[{}]}),
    ] {
        assert!(crate::branch_start::verify_empty(&invalid, "own-empty-session").is_err());
    }
    Ok(())
}

async fn empty(f: &Fixture) -> Result<(WorkerState, two_ways::Job)> {
    let source = WorkerState {
        track_id: f.workspaces[0].track_id.clone(),
        phase: "ready".into(),
        session_id: Some(Uuid::now_v7().to_string()),
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
    f.journal.worker_state(source.clone()).await?;
    // The first Fork instruction was placed by the fixture; no turn has run.
    let word = f.journal.snapshot().await?["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|word| word["decision"]["trackId"] == source.track_id)
        .unwrap()
        .clone();
    assert_eq!(word["decision"]["kind"], "fork");
    let query = two_ways::OptionsQuery {
        input_id: word["id"].as_str().unwrap().into(),
        revision: word["revision"].as_i64().unwrap(),
    };
    assert_eq!(
        f.journal.two_ways_source(query.clone()).await?.session,
        source.session_id.as_deref().unwrap()
    );
    let job = f
        .journal
        .two_ways(PairChange::Prepare(two_ways::Request {
            op_id: Uuid::now_v7(),
            input_id: query.input_id,
            revision: query.revision,
            selection: two_ways::Selection {
                track_id: source.track_id.clone(),
                session_id: source.session_id.clone().unwrap(),
                variation: two_ways::Variation::from_catalog(&models())?,
            },
        }))
        .await?;
    Ok((source, job))
}
async fn initial(f: &Fixture, job: &two_ways::Job) -> Result<two_ways::Job> {
    let capturing = capture(f, job).await?;
    assert_eq!(capturing.phase, PairPhase::CapturingInitial);
    assert!(capturing.point.is_none());
    crate::two_ways_initial::capture(f.journal.clone(), capturing).await
}

#[tokio::test]
async fn initial_files_survive_the_first_real_checkpoints_comparison_and_choosing_the_child()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let (mut source, pending) = empty(&f).await?;
    let workspace = &f.workspaces[0];
    fs::write(
        workspace.path.join("shared-before-first.txt"),
        "Own shared unsaved initial files\n",
    )?;
    let head = git(&workspace.path, &["rev-parse", "HEAD"])?;
    let index = fs::read(git(
        &workspace.path,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?)?;
    let captured = initial(&f, &pending).await?;
    assert_eq!(captured.phase, PairPhase::Inspecting);
    let Some(Point::Initial { snapshot }) = &captured.point else {
        panic!("Expected initial files")
    };
    let initial_sha = snapshot.sha.clone();
    let repo = intercat_git::Repository::open(&workspace.path)?;
    assert_eq!(
        repo.reference_target(&snapshot.reference)?,
        Some(initial_sha.parse()?)
    );
    assert_eq!(
        repo.reference_target(&format!("{}-checkpoint", workspace.reference))?,
        None
    );
    assert_eq!(git(&workspace.path, &["rev-parse", "HEAD"])?, head);
    assert_eq!(
        fs::read(git(
            &workspace.path,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"]
        )?)?,
        index
    );
    assert!(f.journal.snapshot().await?["workers"][0]["checkpoint"].is_null());
    let observed = inspect(&f, &captured, usage(10, 10)).await?;
    let branch = branch(&f, &observed).await?;
    assert_eq!(branch.origin.turn_number, 0);
    f.journal
        .confirm_fingerprints(f.journal.fingerprint_catalog().await?.current)
        .await?;
    let awaiting = f
        .journal
        .two_ways(PairChange::BranchReady {
            id: pending.request.op_id,
            revision: observed.revision,
        })
        .await?;
    let branch = crate::branch_run::activate(&f.journal, branch.request.op_id).await?;
    assert!(branch.point.checkpoint().is_none());
    assert!(
        f.journal.snapshot().await?["workers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|worker| worker["trackId"] == branch.track_id)
            .unwrap()["checkpoint"]
            .is_null()
    );
    crate::two_ways_files::before_publish(&f.journal, &awaiting, &branch.track_id).await?;
    let queued = f
        .journal
        .two_ways(PairChange::Publish {
            id: pending.request.op_id,
            revision: awaiting.revision,
        })
        .await?;
    assert_eq!(queued.phase, PairPhase::Queued);
    for (input, track) in [
        (&queued.request.input_id, &source.track_id),
        (&queued.child_input.to_string(), &branch.track_id),
    ] {
        crate::two_ways_files::before_turn(&f.journal, input, track).await?;
    }
    fs::write(
        workspace.path.join("parent-only.txt"),
        "Own original result\n",
    )?;
    let parent = f.save(0, None).await?;
    source.checkpoint = Some(parent.clone());
    f.journal.worker_state(source.clone()).await?;
    receipt(&f, &source, &parent, "Own first parent result")?;
    fs::write(
        branch.workspace.path.join("child-only.txt"),
        "Own alternate result\n",
    )?;
    let child = crate::checkpoint::save(
        branch.workspace.clone(),
        Uuid::now_v7(),
        None,
        false,
        f.journal.clone(),
    )
    .await?;
    let mut child_worker: WorkerState = serde_json::from_value(
        f.journal.snapshot().await?["workers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|worker| worker["trackId"] == branch.track_id)
            .unwrap()
            .clone(),
    )?;
    child_worker.checkpoint = Some(child.clone());
    child_worker.phase = "ready".into();
    f.journal.worker_state(child_worker.clone()).await?;
    receipt(&f, &child_worker, &child, "Own first alternate result")?;
    let comparison = f
        .journal
        .capture_comparison(crate::comparison::Open {
            op_id: Uuid::now_v7(),
            parent: crate::comparison::Point {
                track_id: source.track_id.clone(),
                sha: parent.sha.clone(),
            },
            child: crate::comparison::Point {
                track_id: branch.track_id.clone(),
                sha: child.sha.clone(),
            },
        })
        .await?;
    assert_eq!(comparison.base.sha(), initial_sha);
    let open = f
        .action(Action::Open {
            tracks: Some(vec![source.track_id.clone(), branch.track_id.clone()]),
        })
        .await?;
    let chosen = f
        .action(Action::ChoosePath {
            id: open.id,
            revision: open.revision,
            track: branch.track_id.clone(),
        })
        .await?;
    f.action(Action::Prepare {
        id: chosen.id,
        revision: chosen.revision,
    })
    .await?;
    let checked = f.settled(chosen.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let tree = checked.plan.as_ref().unwrap().tree.as_ref().unwrap();
    assert_eq!(
        git(
            &f.source,
            &["show", &format!("{tree}:shared-before-first.txt")]
        )?,
        "Own shared unsaved initial files"
    );
    assert_eq!(
        git(&f.source, &["show", &format!("{tree}:child-only.txt")])?,
        "Own alternate result"
    );
    assert!(!git(&f.source, &["ls-tree", "-r", "--name-only", tree])?.contains("parent-only.txt"));
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM operations WHERE kind='checkpoint.save'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        2
    );
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    Ok(())
}

#[tokio::test]
async fn cancelled_initial_capture_cannot_publish_or_release_a_writer_while_it_is_reading()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let (_, job) = empty(&f).await?;
    let capturing = capture(&f, &job).await?;
    let cancelling = f
        .journal
        .two_ways(PairChange::Command(two_ways::Command {
            op_id: Uuid::now_v7(),
            id: job.request.op_id,
            revision: capturing.revision,
            action: two_ways::Action::Cancel,
        }))
        .await?;
    assert_eq!(cancelling.phase, PairPhase::Cancelling);
    assert!(
        crate::two_ways_initial::capture(f.journal.clone(), capturing)
            .await
            .is_err()
    );
    assert!(
        !f.journal
            .read_utterance(job.request.input_id.clone())
            .await?["twoWaysRunnable"]
            .as_bool()
            .unwrap()
    );
    let scope = f.journal.capture_scope(f.workspaces[0].clone()).await?;
    let journal = f.journal.clone();
    let command = tokio::spawn(async move {
        journal
            .two_ways(PairChange::Cancelled {
                id: job.request.op_id,
                revision: cancelling.revision,
            })
            .await
    });
    tokio::task::yield_now().await;
    assert!(!command.is_finished());
    drop(scope);
    assert_eq!(command.await??.phase, PairPhase::Cancelled);
    assert!(
        f.journal.read_utterance(job.request.input_id).await?["twoWaysRunnable"]
            .as_bool()
            .unwrap()
    );
    assert!(
        f.journal.snapshot().await?["branches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn initial_publication_replays_its_frozen_files_and_rejects_foreign_branch_owners()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let (_, job) = empty(&f).await?;
    let capturing = capture(&f, &job).await?;
    let captured = crate::two_ways_initial::capture(f.journal.clone(), capturing.clone()).await?;
    let snapshot = captured.point.as_ref().unwrap();
    fs::write(
        f.workspaces[0].path.join("later.txt"),
        "Own later unsaved bytes\n",
    )?;
    // A late completion cannot update the reducer again or recapture changed bytes.
    assert!(
        crate::two_ways_initial::capture(f.journal.clone(), capturing)
            .await
            .is_err()
    );
    assert_eq!(
        f.journal
            .two_ways_job(job.request.op_id)
            .await?
            .unwrap()
            .point
            .unwrap()
            .sha(),
        snapshot.sha()
    );
    let observed = inspect(&f, &captured, usage(10, 10)).await?;
    let mut forged = observed.branch_request()?;
    forged.op_id = Uuid::now_v7();
    assert!(
        f.journal
            .branch(crate::branch::Change::Prepare(forged))
            .await
            .is_err()
    );
    let branch = branch(&f, &observed).await?;
    assert!(!branch.workspace.path.join("later.txt").exists());
    assert!(
        crate::two_ways_files::before_publish(&f.journal, &observed, &branch.track_id)
            .await
            .is_err()
    );
    Ok(())
}
