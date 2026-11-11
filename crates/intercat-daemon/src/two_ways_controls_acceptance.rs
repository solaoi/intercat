//! Actual journal transactions and owned cleanup, never synthetic PID signals.
use super::*;
use crate::two_ways::{Action, Command};
#[path = "two_ways_rpc_acceptance.rs"]
mod rpc;
fn command(job: &two_ways::Job, action: Action) -> Command {
    Command {
        op_id: Uuid::now_v7(),
        id: job.request.op_id,
        revision: job.revision,
        action,
    }
}

#[tokio::test]
async fn ending_the_owned_edit_and_preparing_two_paths_is_atomic_and_replayed_once() -> Result<()> {
    let f = Fixture::new(1).await?;
    let (_, word, initial) = pending(&f).await?;
    f.journal
        .two_ways(PairChange::Command(command(&initial, Action::Cancel)))
        .await?;
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let held = f
        .journal
        .edit_input(crate::edit_hold::Request {
            owner,
            action: crate::edit_hold::Action::Begin {
                token,
                input: word["id"].as_str().unwrap().into(),
                revision: word["revision"].as_i64().unwrap(),
            },
        })
        .await?;
    let op_id = Uuid::now_v7();
    let action = crate::edit_hold::Action::End {
        token,
        revision: held["editHold"]["revision"].as_i64().unwrap(),
        edit: crate::edit_hold::Edit::TryTwoWays {
            op_id,
            selection: initial.request.selection.clone(),
        },
    };
    assert!(
        f.journal
            .edit_input(crate::edit_hold::Request {
                owner: Uuid::now_v7(),
                action: action.clone()
            })
            .await
            .is_err()
    );
    assert!(f.journal.two_ways_job(op_id).await?.is_none());
    let input = word["id"].as_str().unwrap().to_owned();
    let track = initial.source_track.clone();
    let params = turn_params(&initial.source_session, "minimal");
    let (ended, reserved) = tokio::join!(
        f.journal.edit_input(crate::edit_hold::Request {
            owner,
            action: action.clone()
        }),
        f.journal.reserve_turn(
            Uuid::now_v7(),
            input.clone(),
            held["revision"].as_i64().unwrap(),
            track,
            params,
            None,
        )
    );
    let saved = ended?;
    assert!(reserved.is_err());
    assert!(saved["editHold"].is_null());
    assert_eq!(saved["twoWays"]["request"]["opId"], op_id.to_string());
    assert_eq!(
        f.journal
            .edit_input(crate::edit_hold::Request { owner, action })
            .await?["twoWays"]["request"]["opId"],
        op_id.to_string()
    );
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM operations WHERE kind='utterance.two-ways' AND id=?1",
            [op_id.to_string()],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    assert_eq!(f.journal.read_utterance(input).await?["state"], "placed");
    Ok(())
}
#[tokio::test]
async fn old_retry_and_cancel_commands_do_not_change_a_new_attempt_or_current_stage() -> Result<()>
{
    let f = Fixture::new(1).await?;
    let (_, _, job) = pending(&f).await?;
    let job = capture(&f, &job).await?;
    let held = f
        .journal
        .two_ways(PairChange::Hold {
            id: job.request.op_id,
            revision: job.revision,
            error: "Dummy model catalogue query failed.".into(),
        })
        .await?;
    let retry = command(&held, Action::Retry);
    let retried = f
        .journal
        .two_ways(PairChange::Command(retry.clone()))
        .await?;
    assert_eq!(retried.phase, PairPhase::Inspecting);
    let progressed = inspect(&f, &retried, usage(10, 10)).await?;
    assert_eq!(
        f.journal
            .two_ways(PairChange::Command(retry.clone()))
            .await?
            .revision,
        progressed.revision
    );
    assert!(
        f.journal
            .two_ways(PairChange::Command(Command {
                action: Action::Cancel,
                ..retry
            }))
            .await
            .is_err()
    );
    let cancel = command(&progressed, Action::Cancel);
    let cancelling = f
        .journal
        .two_ways(PairChange::Command(cancel.clone()))
        .await?;
    assert_eq!(cancelling.phase, PairPhase::Cancelling);
    assert_eq!(
        f.journal.operation(cancel.op_id).await?.unwrap()["state"],
        "prepared"
    );
    assert!(
        f.journal
            .two_ways(PairChange::BranchReady {
                id: job.request.op_id,
                revision: progressed.revision
            })
            .await
            .is_err()
    );
    let done = f
        .journal
        .two_ways(PairChange::Cancelled {
            id: job.request.op_id,
            revision: cancelling.revision,
        })
        .await?;
    assert_eq!(done.phase, PairPhase::Cancelled);
    assert_eq!(
        f.journal
            .two_ways(PairChange::Command(cancel.clone()))
            .await?
            .revision,
        done.revision
    );
    assert_eq!(
        f.journal.operation(cancel.op_id).await?.unwrap()["state"],
        "complete"
    );
    Ok(())
}
#[tokio::test]
async fn cancelling_a_prepared_branch_survives_restart_orphans_other_words_and_never_reopens_it()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let (_, _, job) = pending(&f).await?;
    let (job, branch) = ready(&f, &job).await?;
    let id = Uuid::now_v7();
    f.journal
        .stage_utterance(
            id,
            "Own separate unsent words on the new branch".into(),
            Target::Lock(branch.track_id.clone()),
            None,
            vec![],
        )
        .await?;
    let input = f.journal.claim_input().await?.unwrap();
    f.journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(branch.track_id.clone()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let before = f.journal.read_utterance(id.to_string()).await?;
    assert_eq!(before["twoWaysRunnable"], false);
    let reading_id = Uuid::now_v7();
    f.journal
        .stage_utterance(
            reading_id,
            "Own reading words".into(),
            Target::Lock(branch.track_id.clone()),
            None,
            vec![],
        )
        .await?;
    let reading = f.journal.claim_input().await?.unwrap();
    let staged_id = Uuid::now_v7();
    f.journal
        .stage_utterance(
            staged_id,
            "Own staged words".into(),
            Target::Lock(branch.track_id.clone()),
            None,
            vec![],
        )
        .await?;
    let cancelled = f
        .journal
        .two_ways(PairChange::Command(command(&job, Action::Cancel)))
        .await?;
    assert_eq!(cancelled.phase, PairPhase::Cancelling);
    let Fixture { root, journal, .. } = f;
    drop(journal);
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    let editor = crate::editor::Manager::new(root.path(), &root.path().join("editor.log"))?;
    let runtime = crate::runtime::Runtime::start(
        crate::host_memory::Pool::fixed(crate::host_memory::Sample {
            free_bytes: 8 * 1024 * 1024 * 1024,
            pressured: false,
        }),
        root.path().into(),
        root.path().join("rpc"),
        journal.clone(),
        editor.clone(),
        Arc::new(crate::scheduler::Capacity::new(1)),
        Arc::new(crate::dispatch_control::Gate::new(false)),
    );
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = journal.two_ways_job(job.request.op_id).await?.unwrap();
            if current.phase == PairPhase::Cancelled {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(current);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    runtime.shutdown().await;
    editor.stop().await;
    completed??;
    let after = journal.read_utterance(id.to_string()).await?;
    assert_eq!(after["state"], "orphaned");
    assert_eq!(after["text"], before["text"]);
    journal
        .apply_input(
            reading.id,
            reading.revision,
            Outcome::Asked("Late classifier answer".into()),
        )
        .await?;
    for input in [reading_id, staged_id] {
        let kept = journal.read_utterance(input.to_string()).await?;
        assert_eq!(kept["state"], "orphaned");
        assert!(kept["text"].as_str().unwrap().starts_with("Own "));
    }
    let new_id = Uuid::now_v7();
    let new = journal
        .stage_utterance(
            new_id,
            "Own stale locked draft".into(),
            Target::Lock(branch.track_id.clone()),
            None,
            vec![],
        )
        .await?;
    assert_eq!(new["state"], "orphaned");
    assert_eq!(
        journal
            .stage_utterance(
                new_id,
                "Own stale locked draft".into(),
                Target::Lock(branch.track_id.clone()),
                None,
                vec![]
            )
            .await?["revision"],
        new["revision"]
    );
    assert!(journal.claim_input().await?.is_none());
    assert!(
        journal
            .resolve_input(
                Uuid::now_v7(),
                new_id.to_string(),
                new["revision"].as_i64().unwrap(),
                crate::decision::Resolution::Retry {
                    target: Target::Lock(branch.track_id.clone())
                }
            )
            .await
            .is_err()
    );
    let rerouted = journal
        .resolve_input(
            Uuid::now_v7(),
            new_id.to_string(),
            new["revision"].as_i64().unwrap(),
            crate::decision::Resolution::Retry {
                target: Target::Lock(job.source_track.clone()),
            },
        )
        .await?;
    assert_eq!(rerouted["state"], "placed");
    assert_eq!(rerouted["text"], new["text"]);
    assert_eq!(rerouted["decision"]["trackId"], job.source_track);
    let snapshot = journal.snapshot().await?;
    assert!(snapshot["branches"].as_array().unwrap().is_empty());
    assert!(
        !snapshot["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|track| track["id"] == branch.track_id)
    );
    assert!(
        journal
            .branch(crate::branch::Change::Activate(branch.request.op_id))
            .await
            .is_err()
    );
    assert!(
        journal
            .branch(crate::branch::Change::Received {
                operation: branch.request.op_id,
                session: branch.session_id.unwrap()
            })
            .await
            .is_err()
    );
    assert_eq!(
        journal.read_utterance(job.request.input_id).await?["state"],
        "placed"
    );
    Ok(())
}
#[tokio::test]
async fn cancellation_waits_for_owned_file_capture_and_keeps_stop_responsive() -> Result<()> {
    let f = Fixture::new(1).await?;
    let (_, _, job) = pending(&f).await?;
    let (job, _) = ready(&f, &job).await?;
    let scope = f.journal.capture_scope(f.workspaces[0].clone()).await?;
    let cancelling = f
        .journal
        .two_ways(PairChange::Command(command(&job, Action::Cancel)))
        .await?;
    let copy = f.journal.clone();
    let id = job.request.op_id;
    let revision = cancelling.revision;
    let mut completion =
        tokio::spawn(async move { copy.two_ways(PairChange::Cancelled { id, revision }).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(80), &mut completion)
            .await
            .is_err()
    );
    f.journal.pause_dispatch(Uuid::now_v7(), true).await?;
    assert_eq!(f.journal.snapshot().await?["dispatchPaused"], true);
    drop(scope);
    assert_eq!(completion.await??.phase, PairPhase::Cancelled);
    Ok(())
}

#[tokio::test]
async fn metadata_cleanup_stops_only_its_recorded_host_even_if_another_attempt_is_unreadable()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let (_, _, job) = pending(&f).await?;
    let (_, branch) = ready(&f, &job).await?;
    let directory = f
        .root
        .path()
        .join("branch-hosts")
        .join(branch.request.op_id.to_string());
    let good = directory.join(Uuid::now_v7().to_string());
    let bad = directory.join(Uuid::now_v7().to_string());
    fs::create_dir_all(&good)?;
    fs::create_dir_all(&bad)?;
    use std::os::unix::fs::PermissionsExt;
    for path in [&good, &bad] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let nonce = Uuid::now_v7().to_string();
    let mut tracker =
        crate::process::Tracker::prepare(&good, &branch.workspace.path, nonce.clone())?;
    let mut child = tokio::process::Command::new("/bin/sleep")
        .arg("60")
        .env("INTERCAT_HOST", nonce)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    tracker.attach_process_group(crate::process::identity(child.id().unwrap())?)?;
    drop(tracker);
    let mut unrelated = tokio::process::Command::new("/bin/sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()?;
    assert!(
        crate::branch_run::release_hosts(f.root.path(), &branch)
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(3), child.wait()).await??;
    assert!(unrelated.try_wait()?.is_none());
    // Repair only the dummy prelaunch directory: no process was spawned for it.
    let mut empty =
        crate::process::Tracker::prepare(&bad, &branch.workspace.path, Uuid::now_v7().to_string())?;
    empty.cancel_launch()?;
    drop(empty);
    crate::branch_run::release_hosts(f.root.path(), &branch).await?;
    assert!(unrelated.try_wait()?.is_none());
    unrelated.kill().await?;
    unrelated.wait().await?;
    assert!(branch.workspace.path.join("parent.txt").exists());
    Ok(())
}

#[tokio::test]
async fn cancelling_an_unconfirmed_child_waits_for_the_external_receipt_and_does_not_block_later_branches()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let (_, _, job) = pending(&f).await?;
    let job = inspect(&f, &capture(&f, &job).await?, usage(10, 10)).await?;
    let child = branch(&f, &job).await?;
    let job = f
        .journal
        .two_ways(PairChange::BranchReady {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    let catalog = f.journal.fingerprint_catalog().await?;
    assert_ne!(catalog.sealed.as_ref(), Some(&catalog.current));
    let cancelling = f
        .journal
        .two_ways(PairChange::Command(command(&job, Action::Cancel)))
        .await?;
    assert_eq!(cancelling.phase, PairPhase::Cancelling);
    assert!(
        f.journal
            .two_ways(PairChange::Cancelled {
                id: job.request.op_id,
                revision: cancelling.revision
            })
            .await
            .is_err()
    );
    f.journal.pause_dispatch(Uuid::now_v7(), true).await?;
    let editor = crate::editor::Manager::new(f.root.path(), &f.root.path().join("editor.log"))?;
    let runtime = crate::runtime::Runtime::start(
        crate::host_memory::Pool::fixed(crate::host_memory::Sample {
            free_bytes: 8 * 1024 * 1024 * 1024,
            pressured: false,
        }),
        f.root.path().into(),
        f.root.path().join("rpc"),
        f.journal.clone(),
        editor.clone(),
        Arc::new(crate::scheduler::Capacity::new(1)),
        Arc::new(crate::dispatch_control::Gate::new(true)),
    );
    let result = async {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(
            f.journal
                .two_ways_job(job.request.op_id)
                .await?
                .unwrap()
                .phase,
            PairPhase::Cancelling
        );
        // Model the outside receipt only after confirming that cancellation waits.
        f.journal.confirm_fingerprints(catalog.current).await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if f.journal
                    .two_ways_job(job.request.op_id)
                    .await?
                    .unwrap()
                    .phase
                    == PairPhase::Cancelled
                {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        assert_eq!(f.journal.snapshot().await?["dispatchPaused"], true);
        let later = f
            .journal
            .branch(crate::branch::Change::Prepare(crate::branch::Request {
                op_id: Uuid::now_v7(),
                parent_track_id: job.source_track.clone(),
                point: crate::branch_start::Selection::Turn {
                    turn_id: child.point.checkpoint().unwrap().turn_id,
                },
                instructions: crate::branch::InstructionMode::Checkpoint,
            }))
            .await?;
        assert_eq!(later.phase, crate::branch::Phase::Preparing);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    runtime.shutdown().await;
    editor.stop().await;
    result
}

#[tokio::test]
async fn the_reviewed_model_and_effort_cannot_be_replaced_by_a_later_catalog() -> Result<()> {
    let f = Fixture::new(1).await?;
    let (mut original, _, job) = pending(&f).await?;
    let query = two_ways::OptionsQuery {
        input_id: job.request.input_id.clone(),
        revision: job.request.revision,
    };
    let source = f.journal.two_ways_source(query.clone()).await?;
    assert_eq!(source.session, job.source_session);
    original.phase = "lost".into();
    f.journal.worker_state(original.clone()).await?;
    assert!(f.journal.two_ways_source(query.clone()).await.is_err());
    original.phase = "ready".into();
    f.journal.worker_state(original).await?;
    assert!(
        f.journal
            .two_ways_source(two_ways::OptionsQuery {
                revision: query.revision + 1,
                ..query.clone()
            })
            .await
            .is_err()
    );
    let mut request = job.request.clone();
    request.op_id = Uuid::now_v7();
    request.selection.session_id = Uuid::now_v7().to_string();
    f.journal
        .two_ways(PairChange::Command(command(&job, Action::Cancel)))
        .await?;
    assert!(
        f.journal
            .two_ways(PairChange::Prepare(request))
            .await
            .is_err()
    );
    let mut request = job.request.clone();
    request.op_id = Uuid::now_v7();
    let selected = f.journal.two_ways(PairChange::Prepare(request)).await?;
    let captured = capture(&f, &selected).await?;
    let mut catalog = models();
    catalog.models[0].variants = serde_json::from_value(json!(["minimal", "high"]))?;
    assert!(
        f.journal
            .two_ways(PairChange::Inspect {
                id: captured.request.op_id,
                revision: captured.revision,
                usage: usage(10, 10),
                models: catalog
            })
            .await
            .is_err()
    );
    let kept = f
        .journal
        .two_ways_job(captured.request.op_id)
        .await?
        .unwrap();
    assert_eq!(
        kept.request.selection.variation.alternate,
        muse_code_sdk::msp::ReasoningEffort::Xhigh
    );
    assert_eq!(kept.phase, PairPhase::Inspecting);
    assert!(
        f.journal.snapshot().await?["branches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn an_active_first_turn_is_saved_before_the_paired_instruction_can_capture_a_boundary()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let mut source = WorkerState {
        track_id: f.workspaces[0].track_id.clone(),
        phase: "working".into(),
        session_id: Some(Uuid::now_v7().to_string()),
        secret_revision: Some(0),
        identity: None,
        detail: None,
        active_turn: Some(Uuid::now_v7()),
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    f.journal.worker_state(source.clone()).await?;
    let id = Uuid::now_v7();
    f.journal
        .stage_utterance(
            id,
            "Own next instruction after the first turn".into(),
            Target::Lock(source.track_id.clone()),
            None,
            vec![],
        )
        .await?;
    let input = f.journal.claim_input().await?.unwrap();
    f.journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(source.track_id.clone()),
                boundary: Some("after-turn".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let word = f.journal.read_utterance(id.to_string()).await?;
    let job = f
        .journal
        .two_ways(PairChange::Prepare(two_ways::Request {
            op_id: Uuid::now_v7(),
            input_id: id.to_string(),
            revision: word["revision"].as_i64().unwrap(),
            selection: two_ways::Selection {
                track_id: source.track_id.clone(),
                session_id: source.session_id.clone().unwrap(),
                variation: two_ways::Variation::from_catalog(&models())?,
            },
        }))
        .await?;
    assert!(capture(&f, &job).await.is_err());
    assert!(job.point.is_none());
    let first = crate::checkpoint::save(
        f.workspaces[0].clone(),
        source.active_turn.unwrap(),
        None,
        false,
        f.journal.clone(),
    )
    .await?;
    source.active_turn = None;
    source.phase = "ready".into();
    source.checkpoint = Some(first.clone());
    f.journal.worker_state(source.clone()).await?;
    receipt(&f, &source, &first, "Own first actual completed turn")?;
    let captured = capture(&f, &job).await?;
    assert_eq!(
        captured.point.unwrap().checkpoint().unwrap().turn_id,
        first.turn_id
    );
    assert_eq!(captured.phase, PairPhase::Inspecting);
    assert_eq!(
        f.journal.read_utterance(id.to_string()).await?["state"],
        "placed"
    );
    Ok(())
}
