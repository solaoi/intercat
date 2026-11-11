//! Actual private Git checkpoints and journal admission; never signal fixture PIDs.
use super::*;
#[path = "fresh_cancel_acceptance.rs"]
mod cancellation;
#[path = "fresh_handoff_acceptance.rs"]
mod handoff;
#[path = "fresh_rpc_acceptance.rs"]
mod rpc;
#[path = "two_ways_acceptance.rs"]
mod two_ways;
#[path = "fresh_world_acceptance.rs"]
mod worlds;
use crate::fresh::{Change, Phase, Request};
use serde_json::Value;

async fn worker(f: &Fixture, checkpoint: Checkpoint) -> Result<WorkerState> {
    let worker = WorkerState {
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
        checkpoint: Some(checkpoint),
        prompts: vec![],
    };
    f.journal.worker_state(worker.clone()).await?;
    Ok(worker)
}
fn request(worker: &WorkerState) -> Request {
    Request {
        op_id: Uuid::now_v7(),
        track_id: worker.track_id.clone(),
        session_id: worker.session_id.clone().unwrap(),
    }
}
fn receipt(f: &Fixture, worker: &WorkerState, checkpoint: &Checkpoint, text: &str) -> Result<()> {
    let mut db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let tx = db.transaction()?;
    let session = worker.session_id.as_ref().unwrap();
    for frame in [
        json!({"method":"turn/started","params":{"sessionId":session,"turnId":checkpoint.turn_id}}),
        json!({"method":"item/completed","params":{"sessionId":session,"turnId":checkpoint.turn_id,"item":{"itemId":Uuid::now_v7(),"revision":1,"kind":"userMessage","text":text}}}),
        json!({"method":"item/completed","params":{"sessionId":session,"turnId":checkpoint.turn_id,"item":{"itemId":Uuid::now_v7(),"revision":1,"kind":"toolCall","status":"completed","tool":"write_file","args":"{}","visibleOutput":"Own dummy receipt"}}}),
        json!({"method":"turn/completed","params":{"sessionId":session,"turnId":checkpoint.turn_id,"status":"completed"}}),
    ] {
        crate::timeline::fold(
            &tx,
            &worker.track_id,
            session,
            &crate::clock::Stamp::now(),
            &frame,
        )?;
    }
    tx.commit()?;
    Ok(())
}
async fn future(f: &Fixture) -> Result<Value> {
    let id = Uuid::now_v7();
    f.journal
        .stage_utterance(
            id,
            "Own future work".into(),
            Target::Lock(f.workspaces[0].track_id.clone()),
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
                track_id: Some(f.workspaces[0].track_id.clone()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    f.journal.read_utterance(id.to_string()).await
}

#[tokio::test]
async fn fresh_captures_late_corrections_at_the_finished_turn_without_changing_queued_words_or_files()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let first = f.save(0, None).await?;
    let mut current = worker(&f, first.clone()).await?;
    receipt(&f, &current, &first, "Previous constraint: BLUE_OLD")?;
    let secret = "DUMMY_FRESH_SECRET";
    let key = crate::secrets::Key::new([7; 32]);
    let hex = |bytes: [u8; 32]| bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let catalog = f
        .journal
        .register_fingerprint(crate::secrets::Registration {
            op_id: Uuid::now_v7(),
            track_id: current.track_id.clone(),
            session_id: current.session_id.clone().unwrap(),
            name: "TOKEN".into(),
            byte_len: secret.len(),
            tag: hex(key.tag(secret.as_bytes())),
            key_check: hex(key.check()),
        })
        .await?;
    f.journal.confirm_fingerprints(catalog.current).await?;
    let turn = Uuid::now_v7();
    current.active_turn = Some(turn);
    current.phase = "working".into();
    f.journal.worker_state(current.clone()).await?;
    let queued = future(&f).await?;
    let request = request(&current);
    let pending = f.journal.fresh(Change::Request(request.clone())).await?;
    assert_eq!(pending.phase, Phase::WaitingForTurn);
    assert!(pending.boundary.is_none());
    assert!(
        f.journal
            .fresh(Change::Capture {
                id: request.op_id,
                revision: 1
            })
            .await
            .is_err()
    );
    fs::write(
        f.workspaces[0].path.join("latest.txt"),
        "Own late completed turn\n",
    )?;
    let last = checkpoint::save(
        f.workspaces[0].clone(),
        turn,
        Some(first),
        false,
        f.journal.clone(),
    )
    .await?;
    receipt(
        &f,
        &current,
        &last,
        "Latest correction: GREEN_ONLY. Never publish.",
    )?;
    current.active_turn = None;
    current.phase = "held".into();
    current.checkpoint = Some(last.clone());
    current.output = Some(format!(
        "No tests were run. Follow GREEN_ONLY. Own marker: {secret}"
    ));
    f.journal.worker_state(current.clone()).await?;
    let worker_index = PathBuf::from(git(
        &f.workspaces[0].path,
        &["rev-parse", "--absolute-git-dir"],
    )?)
    .join("index");
    let before_index = fs::read(&worker_index)?;
    let captured = f
        .journal
        .fresh(Change::Capture {
            id: request.op_id,
            revision: 1,
        })
        .await?;
    assert_eq!(captured.phase, Phase::Captured);
    assert_eq!(captured.revision, 2);
    let boundary = captured.boundary.as_ref().unwrap();
    assert_eq!(boundary.checkpoint.sha, last.sha);
    assert!(!serde_json::to_string(&captured)?.contains(secret));
    assert!(boundary.context["unfinishedTurn"].is_null());
    assert!(
        boundary.context["unverified"]
            .as_str()
            .unwrap()
            .contains("turn ended")
    );
    assert_eq!(
        boundary.context["completedTools"].as_array().unwrap().len(),
        1
    );
    assert!(
        serde_json::to_string(&boundary.context["instructions"])?
            .contains("Latest correction: GREEN_ONLY")
    );
    let after = f
        .journal
        .read_utterance(queued["id"].as_str().unwrap().into())
        .await?;
    for key in ["state", "revision", "decision", "text"] {
        assert_eq!(after[key], queued[key]);
    }
    assert_eq!(
        fs::read(f.workspaces[0].path.join("latest.txt"))?,
        b"Own late completed turn\n"
    );
    assert_eq!(fs::read(&worker_index)?, before_index);
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    let snapshot = f.journal.snapshot().await?;
    assert_eq!(snapshot["freshTracks"][0]["phase"], "captured");
    assert!(snapshot["freshTracks"][0].get("boundary").is_none());
    assert!(!serde_json::to_string(&snapshot["freshTracks"])?.contains("GREEN_ONLY"));
    // No successor row, host or second workspace exists before the later transfer.
    assert_eq!(snapshot["workspaces"].as_array().unwrap().len(), 1);
    assert!(
        !snapshot["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|track| track["id"] == captured.successor_track)
    );
    assert!(
        f.journal
            .fresh(Change::Capture {
                id: request.op_id,
                revision: 1
            })
            .await
            .is_err()
    );
    let replay = f.journal.fresh(Change::Request(request)).await?;
    assert_eq!(
        serde_json::to_value(&replay)?,
        serde_json::to_value(&captured)?
    );
    Ok(())
}

#[tokio::test]
async fn fresh_fences_dispatch_and_manual_start_until_an_exact_cancellation_and_preserves_restart_ids()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let point = f.save(0, None).await?;
    let current = worker(&f, point.clone()).await?;
    receipt(&f, &current, &point, "Own completed work")?;
    let queued = future(&f).await?;
    let request = request(&current);
    let first = f.journal.fresh(Change::Request(request.clone())).await?;
    let input = queued["id"].as_str().unwrap().to_owned();
    let revision = queued["revision"].as_i64().unwrap();
    let command = Uuid::now_v7();
    let params = json!({"commandId":command,"sessionId":request.session_id,"input":[{"type":"text","text":"Own future work"}]});
    let blocked = f
        .journal
        .reserve_turn(
            command,
            input.clone(),
            revision,
            request.track_id.clone(),
            params.clone(),
            None,
        )
        .await
        .unwrap_err();
    assert!(blocked.is::<crate::journal::StartSuperseded>());
    assert!(f.journal.operation(command).await?.is_none());
    assert!(
        f.journal
            .intent(
                &Uuid::now_v7().to_string(),
                "worker.start",
                json!({"trackId":request.track_id})
            )
            .await
            .is_err()
    );
    let Fixture { root, journal, .. } = f;
    drop(journal);
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    let project = journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    journal
        .activate_project_secrets(crate::project_secrets::Load {
            project_id: project,
            revision: 0,
            entries: vec![],
        })
        .await?;
    let replay = journal.fresh(Change::Request(request.clone())).await?;
    assert_eq!(first.successor_track, replay.successor_track);
    assert_eq!(first.successor_session, replay.successor_session);
    assert!(
        journal
            .reserve_turn(
                command,
                input.clone(),
                revision,
                request.track_id.clone(),
                params.clone(),
                None,
            )
            .await
            .is_err()
    );
    let cancel = Uuid::now_v7();
    assert!(
        journal
            .fresh(Change::Cancel {
                op_id: cancel,
                id: request.op_id,
                revision: 2
            })
            .await
            .is_err()
    );
    assert!(journal.operation(cancel).await?.is_none());
    let cancelled = journal
        .fresh(Change::Cancel {
            op_id: cancel,
            id: request.op_id,
            revision: 1,
        })
        .await?;
    assert_eq!(cancelled.phase, Phase::Cancelled);
    assert_eq!(
        journal
            .fresh(Change::Cancel {
                op_id: cancel,
                id: request.op_id,
                revision: 1
            })
            .await?
            .revision,
        2
    );
    assert!(
        journal
            .fresh(Change::Capture {
                id: request.op_id,
                revision: 1
            })
            .await
            .is_err()
    );
    assert_eq!(
        journal.fresh(Change::Request(request.clone())).await?.phase,
        Phase::Cancelled
    );
    assert!(
        journal
            .reserve_turn(
                command,
                input.clone(),
                revision,
                request.track_id.clone(),
                params.clone(),
                None,
            )
            .await
            .is_err()
    );
    let held = journal.read_utterance(input.clone()).await?;
    assert_eq!(held["state"], "held");
    let resumed = journal
        .start_held_input(
            Uuid::now_v7(),
            input.clone(),
            held["revision"].as_i64().unwrap(),
        )
        .await?;
    assert!(matches!(
        journal
            .reserve_turn(
                command,
                input,
                resumed["revision"].as_i64().unwrap(),
                request.track_id,
                params,
                None,
            )
            .await?,
        crate::journal::Admission::New
    ));
    Ok(())
}

#[tokio::test]
async fn an_admitted_turn_and_unsaved_checkpoint_cannot_be_skipped_even_if_worker_state_looks_idle()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let point = f.save(0, None).await?;
    let mut current = worker(&f, point.clone()).await?;
    receipt(&f, &current, &point, "Own completed work")?;
    let command = Uuid::now_v7();
    f.journal
        .intent(
            &command.to_string(),
            "turn.start",
            json!({"trackId":current.track_id,"sessionId":current.session_id}),
        )
        .await?;
    let request = request(&current);
    f.journal.fresh(Change::Request(request.clone())).await?;
    assert!(
        f.journal
            .fresh(Change::Capture {
                id: request.op_id,
                revision: 1
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("admitted turn")
    );
    f.journal.finish(&command.to_string(), "superseded").await?;
    current.checkpoint_pending = true;
    f.journal.worker_state(current.clone()).await?;
    assert!(
        f.journal
            .fresh(Change::Capture {
                id: request.op_id,
                revision: 1
            })
            .await
            .is_err()
    );
    current.checkpoint_pending = false;
    // A checkpoint-shaped WorkerState alone is not a journal-owned receipt.
    current.checkpoint.as_mut().unwrap().sha = "f".repeat(40);
    f.journal.worker_state(current.clone()).await?;
    assert!(
        f.journal
            .fresh(Change::Capture {
                id: request.op_id,
                revision: 1
            })
            .await
            .is_err()
    );
    current.checkpoint = Some(point);
    f.journal.worker_state(current).await?;
    assert_eq!(
        f.journal
            .fresh(Change::Capture {
                id: request.op_id,
                revision: 1
            })
            .await?
            .phase,
        Phase::Captured
    );
    Ok(())
}

#[tokio::test]
async fn changed_sessions_workspaces_and_oversized_history_never_release_the_fresh_dispatch_hold()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let point = f.save(0, None).await?;
    let mut current = worker(&f, point.clone()).await?;
    receipt(&f, &current, &point, "Own completed work")?;
    let request = request(&current);
    let mut stale = request.clone();
    stale.session_id = Uuid::now_v7().to_string();
    assert!(f.journal.fresh(Change::Request(stale)).await.is_err());
    f.journal.fresh(Change::Request(request.clone())).await?;
    let mut duplicate = request.clone();
    duplicate.op_id = Uuid::now_v7();
    assert!(f.journal.fresh(Change::Request(duplicate)).await.is_err());
    current.output = Some("x".repeat(crate::handoff::CONTEXT_BYTES + 1));
    f.journal.worker_state(current).await?;
    assert!(
        f.journal
            .fresh(Change::Capture {
                id: request.op_id,
                revision: 1
            })
            .await
            .is_err()
    );
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert!(crate::fresh_store::holds(&db, &request.track_id)?);
    assert_eq!(
        f.journal
            .fresh(Change::Request(request.clone()))
            .await?
            .phase,
        Phase::WaitingForTurn
    );
    db.execute(
        "UPDATE workspaces SET record=json_set(record,'$.path',?2) WHERE track_id=?1",
        params![
            request.track_id,
            f.root.path().join("different-own-dir").to_string_lossy()
        ],
    )?;
    assert!(
        f.journal
            .fresh(Change::Capture {
                id: request.op_id,
                revision: 1
            })
            .await
            .unwrap_err()
            .to_string()
            .contains("workspace changed")
    );
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    Ok(())
}

#[tokio::test]
async fn retired_history_removes_captured_fresh_context_without_reopening_its_source() -> Result<()>
{
    let f = Fixture::new(1).await?;
    let point = f.save(0, None).await?;
    let current = worker(&f, point.clone()).await?;
    receipt(&f, &current, &point, "Own context to retire")?;
    let request = request(&current);
    f.journal.fresh(Change::Request(request.clone())).await?;
    f.journal
        .fresh(Change::Capture {
            id: request.op_id,
            revision: 1,
        })
        .await?;
    let mut db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    crate::clock::register(&db)?;
    let tx = db.transaction()?;
    crate::session_retirement::retire(&tx, &request.session_id, &request.track_id, Uuid::now_v7())?;
    tx.commit()?;
    assert!(crate::fresh_store::snapshot(&db)?.is_empty());
    assert!(
        f.journal
            .fresh(Change::Request(request.clone()))
            .await
            .is_err()
    );
    assert!(
        f.journal
            .fresh(Change::Capture {
                id: request.op_id,
                revision: 1
            })
            .await
            .is_err()
    );
    assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='worker.fresh' OR (kind='handoff.input' AND json_extract(payload,'$.source.sessionId')=?1)",[&request.session_id],|row|row.get::<_,i64>(0))?,0);
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    Ok(())
}

#[tokio::test]
async fn a_saved_file_update_keeps_the_last_real_tool_receipts_without_inventing_an_sdk_turn()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let older = f.save(0, None).await?;
    let mut current = worker(&f, older.clone()).await?;
    receipt(&f, &current, &older, "Own older SDK request")?;
    let sdk_point = f.save(0, Some(older.clone())).await?;
    receipt(&f, &current, &sdk_point, "Own latest SDK request")?;
    // History pages may replay older terminal records after newer records.
    let replay_db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let old_record: String = replay_db.query_row(
        "SELECT record FROM timeline_turns WHERE session=?1 AND turn=?2",
        params![current.session_id, older.turn_id.to_string()],
        |row| row.get(0),
    )?;
    replay_db.execute(
        "DELETE FROM timeline_turns WHERE session=?1 AND turn=?2",
        params![current.session_id, older.turn_id.to_string()],
    )?;
    replay_db.execute(
        "INSERT INTO timeline_turns VALUES(?1,?2,?3)",
        params![current.session_id, older.turn_id.to_string(), old_record],
    )?;
    fs::write(
        f.workspaces[0].path.join("after-turn.txt"),
        "Own saved file update\n",
    )?;
    let file_point = f.save(0, Some(sdk_point.clone())).await?;
    // Like a completed realignment, this saved file point has no SDK turn.
    current.checkpoint = Some(file_point.clone());
    f.journal.worker_state(current.clone()).await?;
    let request = request(&current);
    f.journal.fresh(Change::Request(request.clone())).await?;
    let captured = f
        .journal
        .fresh(Change::Capture {
            id: request.op_id,
            revision: 1,
        })
        .await?;
    let boundary = captured.boundary.unwrap();
    assert_eq!(boundary.checkpoint.sha, file_point.sha);
    assert_eq!(
        boundary.context["completedTools"].as_array().unwrap().len(),
        1
    );
    assert!(boundary.context["unfinishedTurn"].is_null());
    let latest_clip = boundary.context["lastClips"]
        .as_array()
        .unwrap()
        .iter()
        .find(|clip| clip["clip"]["turnId"] == sdk_point.turn_id.to_string())
        .unwrap();
    assert_eq!(
        boundary.context["completedTools"][0]["address"],
        latest_clip["address"]
    );
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM timeline_turns WHERE session=?1",
            [&request.session_id],
            |row| row.get::<_, i64>(0)
        )?,
        2
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM timeline_turns WHERE turn=?1",
            [file_point.turn_id.to_string()],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(
        fs::read_to_string(f.workspaces[0].path.join("after-turn.txt"))?,
        "Own saved file update\n"
    );
    Ok(())
}

async fn release_without_a_host(f: &Fixture) -> Result<crate::fresh::Job> {
    let point = f.save(0, None).await?;
    let source = worker(f, point.clone()).await?;
    receipt(f, &source, &point, "Own completed handoff source")?;
    release_source(f, source).await
}
async fn release_source(f: &Fixture, mut source: WorkerState) -> Result<crate::fresh::Job> {
    let request = request(&source);
    f.journal.fresh(Change::Request(request.clone())).await?;
    let captured = f
        .journal
        .fresh(Change::Capture {
            id: request.op_id,
            revision: 1,
        })
        .await?;
    let releasing = f
        .journal
        .fresh(Change::Release {
            id: request.op_id,
            revision: captured.revision,
        })
        .await?;
    if source.phase != "stopped" {
        assert!(
            f.journal
                .fresh(Change::Released {
                    id: request.op_id,
                    revision: releasing.revision
                })
                .await
                .is_err()
        );
    }
    // No synthetic fixture PID is signalled. This source never had a host.
    source.phase = "stopped".into();
    f.journal.worker_state(source).await?;
    f.journal
        .fresh(Change::Released {
            id: request.op_id,
            revision: releasing.revision,
        })
        .await
}
async fn begin_files(f: &Fixture, mut job: crate::fresh::Job) -> Result<crate::fresh::Job> {
    for _ in 0..5 {
        if job.phase == Phase::NeedsAccess {
            let fingerprint = job.inspection.as_ref().unwrap().fingerprint.clone();
            job = f
                .journal
                .fresh(Change::ApproveAccess {
                    op_id: Uuid::now_v7(),
                    id: job.request.op_id,
                    revision: job.revision,
                    fingerprint,
                })
                .await?;
        }
        let inspection = crate::fresh_files::inspect(&job).await?;
        job = f
            .journal
            .fresh(Change::Inspect {
                id: job.request.op_id,
                revision: job.revision,
                inspection,
            })
            .await?;
        if job.phase == Phase::CapturingFiles {
            return Ok(job);
        }
    }
    Err("Own dummy file-access evidence did not settle.".into())
}
#[tokio::test]
async fn final_fresh_files_preserve_post_turn_edits_without_editing_head_index_or_sdk_history()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let released = release_without_a_host(&f).await?;
    let path = &f.workspaces[0].path;
    fs::write(
        path.join("late-files.txt"),
        "Own final change after the saved turn\n",
    )?;
    let index = PathBuf::from(git(path, &["rev-parse", "--absolute-git-dir"])?).join("index");
    let before_index = fs::read(&index)?;
    let before_head = git(path, &["rev-parse", "HEAD"])?;
    let mut job = begin_files(&f, released).await?;
    for _ in 0..5 {
        job = crate::fresh_files::capture(&f.journal, &job).await?;
        if job.phase == Phase::PublishingFiles {
            break;
        }
        job = begin_files(&f, job).await?;
    }
    assert_eq!(job.phase, Phase::PublishingFiles);
    let files = job.files.clone().unwrap();
    let repo = intercat_git::Repository::open(path)?;
    assert!(
        repo.reference_target(&files.checkpoint.reference)?
            .is_none()
    );
    let frozen_files = repo.saved_files(files.checkpoint.sha.parse()?)?;
    assert_eq!(
        repo.saved_bytes(&frozen_files["late-files.txt"])?,
        b"Own final change after the saved turn\n"
    );
    let ready = crate::fresh_files::publish(&f.journal, &job).await?;
    assert_eq!(ready.phase, Phase::SnapshotReady);
    assert_eq!(
        repo.reference_target(&files.checkpoint.reference)?,
        Some(files.checkpoint.sha.parse()?)
    );
    assert_eq!(git(path, &["rev-parse", "HEAD"])?, before_head);
    assert_eq!(fs::read(&index)?, before_index);
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='checkpoint.save' AND json_extract(payload,'$.checkpoint.turnId')=?1",[files.checkpoint.turn_id.to_string()],|row|row.get::<_,i64>(0))?,0);
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM timeline_turns WHERE turn=?1",
            [files.checkpoint.turn_id.to_string()],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    assert!(crate::fresh_store::holds(&db, &ready.request.track_id)?);
    assert!(
        f.journal
            .fresh(Change::Published {
                id: job.request.op_id,
                revision: job.revision
            })
            .await
            .is_err()
    );
    // Later edits remain in the live files; a publish retry keeps its frozen tree.
    fs::write(
        path.join("late-files.txt"),
        "Own even later edit, not in this snapshot\n",
    )?;
    assert_eq!(
        repo.saved_bytes(&frozen_files["late-files.txt"])?,
        b"Own final change after the saved turn\n"
    );
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    Ok(())
}
#[tokio::test]
async fn changed_access_requires_current_consent_and_late_file_callbacks_cannot_advance_a_retry()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let released = release_without_a_host(&f).await?;
    let inspection = crate::workspace_access::Inspection {
        processes: vec![],
        unreadable: 1,
        fingerprint: "own-old-access".into(),
    };
    let waiting = f
        .journal
        .fresh(Change::Inspect {
            id: released.request.op_id,
            revision: released.revision,
            inspection,
        })
        .await?;
    assert_eq!(waiting.phase, Phase::NeedsAccess);
    let op = Uuid::now_v7();
    assert!(
        f.journal
            .fresh(Change::ApproveAccess {
                op_id: op,
                id: waiting.request.op_id,
                revision: waiting.revision,
                fingerprint: "own-changed-access".into()
            })
            .await
            .is_err()
    );
    assert!(f.journal.operation(op).await?.is_none());
    let approved = f
        .journal
        .fresh(Change::ApproveAccess {
            op_id: op,
            id: waiting.request.op_id,
            revision: waiting.revision,
            fingerprint: "own-old-access".into(),
        })
        .await?;
    let changed = crate::workspace_access::Inspection {
        processes: vec![],
        unreadable: 2,
        fingerprint: "own-changed-access".into(),
    };
    let waiting = f
        .journal
        .fresh(Change::Inspect {
            id: approved.request.op_id,
            revision: approved.revision,
            inspection: changed,
        })
        .await?;
    assert_eq!(waiting.phase, Phase::NeedsAccess);
    assert!(waiting.access_consent.is_none());
    let clear = crate::workspace_access::Inspection {
        processes: vec![],
        unreadable: 0,
        fingerprint: "own-clear-access".into(),
    };
    let preparing = f
        .journal
        .fresh(Change::Inspect {
            id: waiting.request.op_id,
            revision: waiting.revision,
            inspection: clear,
        })
        .await?;
    let failed = f
        .journal
        .fresh(Change::Failed {
            id: preparing.request.op_id,
            revision: preparing.revision,
            error: "Own injected file capture failure".into(),
        })
        .await?;
    assert_eq!(failed.phase, Phase::FilesHeld);
    let retry = f
        .journal
        .fresh(Change::Retry {
            op_id: Uuid::now_v7(),
            id: failed.request.op_id,
            revision: failed.revision,
        })
        .await?;
    assert_eq!(retry.phase, Phase::Inspecting);
    let fake = crate::fresh::Files {
        checkpoint: Checkpoint {
            turn_id: crate::fresh_files::turn(preparing.request.op_id, preparing.revision),
            sha: "f".repeat(40),
            reference: crate::fresh_files::reference(&preparing, &"f".repeat(40)),
            unmerged_paths: vec![],
            live_processes: false,
        },
        summary_id: Uuid::now_v7(),
        lfs_directory: f.root.path().join("own-dummy-lfs"),
    };
    assert!(
        f.journal
            .fresh(Change::Files {
                id: preparing.request.op_id,
                revision: preparing.revision,
                files: fake
            })
            .await
            .is_err()
    );
    assert_eq!(
        f.journal
            .fresh_job(retry.request.op_id)
            .await?
            .unwrap()
            .revision,
        retry.revision
    );

    let secret = "DUMMY_FRESH_FAILURE";
    let key = crate::secrets::Key::new([7; 32]);
    let hex = |bytes: [u8; 32]| bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let catalog = f
        .journal
        .register_fingerprint(crate::secrets::Registration {
            op_id: Uuid::now_v7(),
            track_id: retry.request.track_id.clone(),
            session_id: retry.request.session_id.clone(),
            name: "TOKEN".into(),
            byte_len: secret.len(),
            tag: hex(key.tag(secret.as_bytes())),
            key_check: hex(key.check()),
        })
        .await?;
    f.journal.confirm_fingerprints(catalog.current).await?;
    let message = format!("Own capture error containing {secret}");
    let failed = f
        .journal
        .fresh(Change::Failed {
            id: retry.request.op_id,
            revision: retry.revision,
            error: message.clone(),
        })
        .await?;
    assert!(!serde_json::to_string(&failed)?.contains(secret));
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let count = || -> Result<i64> {
        Ok(db.query_row("SELECT count(*) FROM events WHERE kind='worker.fresh' AND json_extract(payload,'$.id')=?1",[retry.request.op_id.to_string()],|row|row.get(0))?)
    };
    let before = count()?;
    f.journal
        .fresh(Change::Failed {
            id: retry.request.op_id,
            revision: retry.revision,
            error: message,
        })
        .await?;
    assert_eq!(count()?, before);
    Ok(())
}

#[tokio::test]
async fn a_published_fresh_snapshot_resumes_after_database_restart_without_recreating_files_or_a_turn()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let released = release_without_a_host(&f).await?;
    fs::write(
        f.workspaces[0].path.join("restart-final.txt"),
        "Own final snapshot before restart\n",
    )?;
    let mut job = begin_files(&f, released).await?;
    for _ in 0..5 {
        job = crate::fresh_files::capture(&f.journal, &job).await?;
        if job.phase == Phase::PublishingFiles {
            break;
        }
        job = begin_files(&f, job).await?;
    }
    assert_eq!(job.phase, Phase::PublishingFiles);
    let files = job.files.clone().unwrap();
    let repo = intercat_git::Repository::open(&job.workspace.path)?;
    // Crash after the durable private ref, before its SQLite completion.
    repo.compare_and_swap(
        &files.checkpoint.reference,
        None,
        files.checkpoint.sha.parse()?,
    )?;
    repo.sync_ref(&files.checkpoint.reference)?;
    drop(repo);
    let Fixture { root, journal, .. } = f;
    drop(journal);
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    let restored = journal.fresh_job(job.request.op_id).await?.unwrap();
    assert_eq!(restored.phase, Phase::PublishingFiles);
    assert_eq!(
        serde_json::to_value(&restored.files)?,
        serde_json::to_value(&job.files)?
    );
    let ready = crate::fresh_files::publish(&journal, &restored).await?;
    assert_eq!(ready.phase, Phase::SnapshotReady);
    assert_eq!(ready.files.as_ref().unwrap().summary_id, files.summary_id);
    let repo = intercat_git::Repository::open(&ready.workspace.path)?;
    assert_eq!(
        repo.reference_target(&files.checkpoint.reference)?,
        Some(files.checkpoint.sha.parse()?)
    );
    assert_eq!(
        fs::read_to_string(ready.workspace.path.join("restart-final.txt"))?,
        "Own final snapshot before restart\n"
    );
    let db = rusqlite::Connection::open(root.path().join("events.sqlite"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM timeline_turns WHERE turn=?1",
            [files.checkpoint.turn_id.to_string()],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    assert!(crate::fresh_store::holds(&db, &job.request.track_id)?);
    Ok(())
}
