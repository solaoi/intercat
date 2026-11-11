//! Frozen summary evidence, not model quality. The actual provider gets a
//! separate bounded dummy test after the normal owner/session path is wired.
use super::*;
#[path = "fresh_cancel_ownership_acceptance.rs"]
mod cancellation;
#[path = "fresh_capture_acceptance.rs"]
mod captures;
#[path = "fresh_execution_acceptance.rs"]
mod execution;
#[path = "fresh_plan_acceptance.rs"]
mod plans;
#[path = "fresh_summary_acceptance.rs"]
mod summary;
#[path = "fresh_word_acceptance.rs"]
mod words;

#[tokio::test]
#[ignore = "explicit pinned Muse binary for summary admission; no host or paid calls"]
async fn the_fresh_summary_caller_releases_its_waiting_ticket_when_global_stop_changes_its_epoch()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let files = snapshot(&f).await?;
    let changes = crate::fresh_summary::changes(&files).await?;
    let job = f
        .journal
        .fresh(Change::PrepareSummary {
            id: files.request.op_id,
            revision: files.revision,
            changes,
        })
        .await?;
    let binary = PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?).canonicalize()?;
    let credentials = f.root.path().join("own-dummy-auth.json");
    fs::write(&credentials, r#"{"schema_version":1}"#)?;
    let config = crate::runtime::Config {
        source: binary.clone(),
        binary,
        credentials,
        path: "/usr/bin:/bin".into(),
    };
    let capacity = Arc::new(crate::scheduler::Capacity::new(1));
    let mut ticket = capacity.request("own-occupied-slot".into(), crate::scheduler::Priority::User);
    let occupied = ticket.try_acquire().unwrap();
    let memory = crate::host_memory::Pool::fixed(crate::host_memory::Sample {
        free_bytes: 8 * 1024 * 1024 * 1024,
        pressured: false,
    });
    let gate = Arc::new(crate::dispatch_control::Gate::new(false));
    let (_shutdown, stopping) = tokio::sync::watch::channel(false);
    let journal = f.journal.clone();
    let task_job = job.clone();
    let data = f.root.path().to_owned();
    let rpc = data.join("own-rpc");
    let task_capacity = capacity.clone();
    let task_memory = memory.clone();
    let task_gate = gate.clone();
    let mut running = tokio::spawn(async move {
        crate::fresh_summary::run(
            &journal,
            &task_job,
            &config,
            &data,
            &rpc,
            task_memory,
            task_capacity,
            task_gate,
            stopping,
        )
        .await
    });
    let waiting = tokio::time::timeout(Duration::from_secs(10), async {
        while capacity.snapshot().waiting.is_empty() {
            if running.is_finished() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    let before = capacity.snapshot();
    let hosts_before = memory.snapshot().hosts;
    gate.begin(true);
    let paused = f.journal.pause_dispatch(Uuid::now_v7(), true).await;
    let stopped = tokio::time::timeout(Duration::from_secs(10), &mut running).await;
    let result = match stopped {
        Ok(result) => result?,
        Err(error) => {
            running.abort();
            let _ = running.await;
            return Err(error.into());
        }
    };
    waiting?;
    paused?;
    assert_eq!(before.active, 1);
    assert_eq!(before.waiting.len(), 1, "Own summary result: {result:?}");
    assert_eq!(hosts_before, 0);
    let error = result.unwrap_err();
    assert!(error.is::<crate::msp::StartCancelled>());
    assert!(capacity.snapshot().waiting.is_empty());
    assert_eq!(capacity.snapshot().active, 1);
    assert_eq!(memory.snapshot().hosts, 0);
    assert!(!f.root.path().join("summary-hosts").exists());
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM operations WHERE kind IN ('session/start','turn.start')",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    let saved = f.journal.fresh_job(job.request.op_id).await?.unwrap();
    assert_eq!(saved.phase, Phase::Summarizing);
    assert_eq!(saved.summary_id, job.summary_id);
    assert!(saved.summary.is_none());
    let crate::handoff::Next::Task(task) = f.journal.summary_next(job.summary_id.unwrap()).await?
    else {
        panic!("Stopped summary must not have a completed result");
    };
    assert_eq!(task.state, crate::handoff::State::New);
    drop(occupied);
    assert_eq!(capacity.snapshot().active, 0);
    let resume = gate.begin(false);
    assert!(gate.resume(resume));
    f.journal.pause_dispatch(Uuid::now_v7(), false).await?;
    assert!(capacity.snapshot().waiting.is_empty());
    assert!(!f.root.path().join("summary-hosts").exists());
    Ok(())
}

#[tokio::test]
async fn current_file_text_is_screened_without_inheriting_the_old_sessions_fingerprints()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let released = release_without_a_host(&f).await?;
    let secret = "OWN_DUMMY_SESSION_ONLY_VALUE_8842";
    let key = crate::secrets::Key::new([7; 32]);
    let hex = |bytes: [u8; 32]| bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let catalog = f
        .journal
        .register_fingerprint(crate::secrets::Registration {
            op_id: Uuid::now_v7(),
            track_id: released.request.track_id.clone(),
            session_id: released.request.session_id.clone(),
            name: "OWN_TOKEN".into(),
            byte_len: secret.len(),
            tag: hex(key.tag(secret.as_bytes())),
            key_check: hex(key.check()),
        })
        .await?;
    f.journal.confirm_fingerprints(catalog.current).await?;
    let file = released.workspace.path.join("own-session-only.txt");
    fs::write(&file, format!("Own current file contains {secret}\n"))?;
    let job = summarize_snapshot(&f, snapshot_released(&f, released).await?).await?;
    transfer(&f, &job).await?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert_eq!(
        crate::secret_store::read(&db, &job.request.track_id, &job.request.session_id)?.len(),
        1
    );
    assert!(
        crate::secret_store::read(&db, &job.successor_track, &job.successor_session)?.is_empty()
    );
    let body = fs::read_to_string(&file)?;
    assert!(body.contains(secret));
    let previews = f.journal.preview_privacy(vec![body.clone()]).await?;
    assert!(!previews[0].text.contains(secret));
    assert!(previews[0].text.contains("<secret:OWN_TOKEN>"));
    let command = f
        .journal
        .screen_start(json!({
            "commandId": Uuid::now_v7(), "sessionId": job.successor_session,
            "input": [{"type":"text", "text":body}],
        }))
        .await?;
    let outgoing = serde_json::to_string(&command.params)?;
    assert!(!outgoing.contains(secret));
    assert!(outgoing.contains("<secret:OWN_TOKEN>"));
    assert!(fs::read_to_string(file)?.contains(secret));
    assert!(
        crate::secret_store::read(&db, &job.successor_track, &job.successor_session)?.is_empty()
    );
    Ok(())
}

async fn transfer(f: &Fixture, job: &crate::fresh::Job) -> Result<()> {
    let publishing = crate::fresh_files::transfer(&f.journal, job).await?;
    assert_eq!(
        publishing.phase,
        Phase::PublishingOwner,
        "The dummy handoff must reserve its successor before publishing it."
    );
    crate::fresh_files::publish_owner(&f.journal, &publishing).await?;
    Ok(())
}

async fn snapshot(f: &Fixture) -> Result<crate::fresh::Job> {
    let released = release_without_a_host(f).await?;
    snapshot_released(f, released).await
}
async fn snapshot_released(f: &Fixture, released: crate::fresh::Job) -> Result<crate::fresh::Job> {
    fs::write(
        f.workspaces[0].path.join("own-final.txt"),
        "Own final source bytes\n",
    )?;
    let mut job = begin_files(f, released).await?;
    for _ in 0..5 {
        job = crate::fresh_files::capture(&f.journal, &job).await?;
        if job.phase == Phase::PublishingFiles {
            break;
        }
        job = begin_files(f, job).await?;
    }
    crate::fresh_files::publish(&f.journal, &job).await
}
fn document(job: &crate::fresh::Job) -> crate::handoff::Document {
    crate::handoff::Document {
        fields: crate::handoff::Fields {
            purpose: "Continue own saved files".into(),
            constraints: vec!["GREEN_ONLY".into()],
            unresolved: vec![],
            changed_files: vec!["own-final.txt".into()],
            completed: vec!["Own last turn ended".into()],
            unverified: vec!["No new checks have run".into()],
            next_checks: vec!["Inspect current files".into()],
        },
        source_addresses: crate::handoff::addresses(
            job.summary_input.as_ref().unwrap(),
            &job.request.track_id,
        ),
    }
}
#[tokio::test]
async fn summary_waits_for_pause_and_source_report_analysis_then_binds_exact_evidence_and_addresses()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let ready = snapshot(&f).await?;
    let changes = crate::fresh_summary::changes(&ready).await?;
    assert_eq!(changes["sinceSavedTurn"][0]["path"], "own-final.txt");
    assert!(
        changes["currentFilesVsWorkspaceBase"]["authorship"]
            .as_str()
            .unwrap()
            .contains("not proof")
    );
    f.journal.pause_dispatch(Uuid::now_v7(), true).await?;
    assert!(
        f.journal
            .fresh(Change::PrepareSummary {
                id: ready.request.op_id,
                revision: ready.revision,
                changes: changes.clone()
            })
            .await
            .is_err()
    );
    f.journal.pause_dispatch(Uuid::now_v7(), false).await?;
    let report = crate::reports::Report::new(
        ready.request.track_id.clone(),
        ready.request.session_id.clone(),
        ready.boundary.as_ref().unwrap().checkpoint.turn_id,
        "own-report".into(),
        "Own final correction: GREEN_ONLY".into(),
    );
    let observed = f.journal.observe_report(report).await?;
    assert!(
        f.journal
            .fresh(Change::PrepareSummary {
                id: ready.request.op_id,
                revision: ready.revision,
                changes: changes.clone()
            })
            .await
            .unwrap_err()
            .is::<crate::fresh::Waiting>()
    );
    f.journal
        .analyze_report(
            observed.report.id,
            observed.report.revision,
            crate::reports::Analysis::Clear,
        )
        .await?;
    let preparing = f
        .journal
        .fresh(Change::PrepareSummary {
            id: ready.request.op_id,
            revision: ready.revision,
            changes,
        })
        .await?;
    assert_eq!(preparing.phase, Phase::Summarizing);
    assert!(preparing.summary_id.is_some());
    assert!(preparing.summary_input.as_ref().unwrap()["unfinishedTurn"].is_null());
    let mut forged = document(&preparing);
    forged.source_addresses.push("invented-source".into());
    assert!(
        f.journal
            .fresh(Change::Summarized {
                id: preparing.request.op_id,
                revision: preparing.revision,
                document: forged
            })
            .await
            .is_err()
    );
    let saved = f
        .journal
        .fresh(Change::Summarized {
            id: preparing.request.op_id,
            revision: preparing.revision,
            document: document(&preparing),
        })
        .await?;
    assert_eq!(saved.phase, Phase::SummaryReady);
    assert_eq!(
        saved.summary.as_ref().unwrap().fields.constraints,
        ["GREEN_ONLY"]
    );
    assert!(!serde_json::to_string(&saved.status())?.contains("GREEN_ONLY"));
    assert!(
        f.journal
            .fresh(Change::Summarized {
                id: preparing.request.op_id,
                revision: preparing.revision,
                document: document(&preparing)
            })
            .await
            .is_err()
    );
    assert_eq!(
        f.journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    Ok(())
}
#[tokio::test]
async fn retry_keeps_summary_input_and_refuses_late_document_before_any_writer_transfer()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let ready = snapshot(&f).await?;
    let preparing = f
        .journal
        .fresh(Change::PrepareSummary {
            id: ready.request.op_id,
            revision: ready.revision,
            changes: crate::fresh_summary::changes(&ready).await?,
        })
        .await?;
    let held = f
        .journal
        .fresh(Change::Failed {
            id: preparing.request.op_id,
            revision: preparing.revision,
            error: "Own summary interruption".into(),
        })
        .await?;
    assert_eq!(held.phase, Phase::SummaryHeld);
    let retry = f
        .journal
        .fresh(Change::Retry {
            op_id: Uuid::now_v7(),
            id: held.request.op_id,
            revision: held.revision,
        })
        .await?;
    assert_eq!(retry.summary_id, preparing.summary_id);
    assert_eq!(retry.summary_input, preparing.summary_input);
    assert!(
        f.journal
            .fresh(Change::Summarized {
                id: preparing.request.op_id,
                revision: preparing.revision,
                document: document(&preparing)
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
    Ok(())
}

async fn summarized(f: &Fixture) -> Result<crate::fresh::Job> {
    let job = snapshot(f).await?;
    summarize_snapshot(f, job).await
}
async fn summarize_snapshot(f: &Fixture, job: crate::fresh::Job) -> Result<crate::fresh::Job> {
    let changes = crate::fresh_summary::changes(&job).await?;
    let job = f
        .journal
        .fresh(Change::PrepareSummary {
            id: job.request.op_id,
            revision: job.revision,
            changes,
        })
        .await?;
    f.journal
        .fresh(Change::Summarized {
            id: job.request.op_id,
            revision: job.revision,
            document: document(&job),
        })
        .await
}
#[tokio::test]
async fn ownership_transfer_keeps_one_writable_path_freezes_history_and_reserves_summary_first()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let key = crate::secrets::Key::new([7; 32]);
    let secret = "OWN_DUMMY_HISTORICAL_FRESH_VALUE";
    let hex = |bytes: [u8; 32]| bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let catalog = f
        .journal
        .register_fingerprint(crate::secrets::Registration {
            op_id: Uuid::now_v7(),
            track_id: job.request.track_id.clone(),
            session_id: job.request.session_id.clone(),
            name: "OWN_TOKEN".into(),
            byte_len: secret.len(),
            tag: hex(key.tag(secret.as_bytes())),
            key_check: hex(key.check()),
        })
        .await?;
    f.journal.confirm_fingerprints(catalog.current).await?;
    let queued = future(&f).await?;
    let head = git(&job.workspace.path, &["rev-parse", "HEAD"])?;
    let index = PathBuf::from(git(
        &job.workspace.path,
        &["rev-parse", "--absolute-git-dir"],
    )?)
    .join("index");
    let index_bytes = fs::read(&index)?;
    let publishing = crate::fresh_files::transfer(&f.journal, &job).await?;
    assert_eq!(
        publishing.phase,
        Phase::PublishingOwner,
        "Saved access={:?}, reinspection={:?}",
        job.inspection.as_ref().map(|access| (
            &access.fingerprint,
            access.processes.len(),
            access.unreadable
        )),
        publishing.inspection.as_ref().map(|access| (
            &access.fingerprint,
            access.processes.len(),
            access.unreadable
        ))
    );
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert_eq!(
        crate::secret_store::read(&db, &job.request.track_id, &job.request.session_id)?.len(),
        1
    );
    assert!(crate::continuation::closed(&db, &job.request.track_id)?);
    assert_eq!(
        crate::continuation::current(&db, &job.request.track_id)?,
        job.successor_track
    );
    let owner = crate::continuation::start(&db, &job.successor_track)?.unwrap();
    assert!(!owner.ready && !owner.opened);
    let public = f.journal.snapshot().await?["continuations"].clone();
    assert_eq!(public[0]["source"], job.request.track_id);
    assert_eq!(public[0]["successor"], job.successor_track);
    assert_eq!(public[0]["origin"], serde_json::to_value(&owner.origin)?);
    for private in ["firstInput", "startCommand", "checkpoint", "folderId"] {
        assert!(public[0].get(private).is_none(), "{private}");
    }
    assert!(!serde_json::to_string(&public)?.contains("GREEN_ONLY"));
    let workspace = f.journal.workspace(job.successor_track.clone()).await?;
    assert_eq!(workspace.path, job.workspace.path);
    assert_eq!(workspace.base, job.workspace.base);
    assert_eq!(workspace.state, "fresh-publishing");
    assert!(crate::session_start::read(&db, &job.request.track_id).is_err());
    let reservation = crate::session_start::read(&db, &job.successor_track)?.unwrap();
    assert_eq!(reservation.session_id, job.successor_session);
    assert!(!reservation.opened);
    assert_eq!(
        crate::secret_store::read(&db, &job.successor_track, &job.successor_session)?.len(),
        0
    );
    let moved = f
        .journal
        .read_utterance(queued["id"].as_str().unwrap().into())
        .await?;
    assert_eq!(moved["state"], queued["state"]);
    assert_eq!(moved["decision"]["trackId"], job.successor_track);
    assert_eq!(
        moved["target"],
        json!({"mode":"lock","trackId":job.successor_track})
    );
    assert_eq!(moved["freshRunnable"], false);
    let first = f.journal.read_utterance(owner.first_input.clone()).await?;
    assert_eq!(first["freshRunnable"], true);
    assert!(first["text"].as_str().unwrap().contains("GREEN_ONLY"));
    assert!(
        first["text"]
            .as_str()
            .unwrap()
            .contains("No prior conversation history was imported")
    );
    assert!(crate::continuation::allowed(
        &db,
        &job.successor_track,
        &owner.first_input
    )?);
    assert!(!crate::continuation::allowed(
        &db,
        &job.successor_track,
        queued["id"].as_str().unwrap()
    )?);
    assert!(
        f.journal
            .intent(
                &Uuid::now_v7().to_string(),
                "worker.start",
                json!({"trackId":job.request.track_id})
            )
            .await
            .is_err()
    );
    assert!(
        f.journal
            .fresh(Change::Cancel {
                op_id: Uuid::now_v7(),
                id: job.request.op_id,
                revision: publishing.revision
            })
            .await
            .is_err()
    );
    // Recovery resumes publication, rather than allocating another track/input.
    let held = f
        .journal
        .fresh(Change::Failed {
            id: job.request.op_id,
            revision: publishing.revision,
            error: "Own publication retry".into(),
        })
        .await?;
    assert_eq!(held.phase, Phase::OwnerHeld);
    let retry = f
        .journal
        .fresh(Change::Retry {
            op_id: Uuid::now_v7(),
            id: job.request.op_id,
            revision: held.revision,
        })
        .await?;
    assert_eq!(retry.phase, Phase::PublishingOwner);
    let complete = crate::fresh_files::publish_owner(&f.journal, &retry).await?;
    assert_eq!(complete.phase, Phase::Transferred);
    assert_eq!(
        f.journal
            .workspace(job.successor_track.clone())
            .await?
            .state,
        "ready"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM tracks", [], |row| row
            .get::<_, i64>(0))?,
        2
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM branches", [], |row| row
            .get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM timeline_turns WHERE session=?1",
            [&job.successor_session],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    assert_eq!(git(&job.workspace.path, &["rev-parse", "HEAD"])?, head);
    assert_eq!(fs::read(index)?, index_bytes);
    assert_eq!(
        fs::read_to_string(job.workspace.path.join("own-final.txt"))?,
        "Own final source bytes\n"
    );
    // Private source body cleanup does not restore its old writer or erase the continuation.
    crate::fresh_store::retire(&db, &job.request.track_id, &job.request.session_id)?;
    assert!(crate::fresh_store::read(&db, job.request.op_id)?.is_none());
    assert!(crate::continuation::ensure_writer(&db, &job.request.track_id).is_err());
    assert_eq!(
        crate::continuation::start(&db, &job.successor_track)?
            .unwrap()
            .first_input,
        owner.first_input
    );
    Ok(())
}
#[tokio::test]
async fn late_file_edits_invalidate_summary_without_resetting_the_worktree_or_transferring_a_writer()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let old_id = job.summary_id.unwrap();
    fs::write(
        job.workspace.path.join("own-final.txt"),
        "Own edit while the summary was being prepared\n",
    )?;
    let refresh = crate::fresh_files::transfer(&f.journal, &job).await?;
    assert_eq!(refresh.phase, Phase::Inspecting);
    assert!(
        refresh.summary.is_none()
            && refresh.summary_input.is_none()
            && refresh.summary_id.is_none()
    );
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert!(!crate::continuation::closed(&db, &job.request.track_id)?);
    assert_eq!(
        db.query_row("SELECT count(*) FROM tracks", [], |row| row
            .get::<_, i64>(0))?,
        1
    );
    let mut job = begin_files(&f, refresh).await?;
    for _ in 0..5 {
        job = crate::fresh_files::capture(&f.journal, &job).await?;
        if job.phase == Phase::PublishingFiles {
            break;
        }
        job = begin_files(&f, job).await?;
    }
    let job = crate::fresh_files::publish(&f.journal, &job).await?;
    let changes = crate::fresh_summary::changes(&job).await?;
    let job = f
        .journal
        .fresh(Change::PrepareSummary {
            id: job.request.op_id,
            revision: job.revision,
            changes,
        })
        .await?;
    assert_ne!(job.summary_id.unwrap(), old_id);
    assert_eq!(
        fs::read_to_string(job.workspace.path.join("own-final.txt"))?,
        "Own edit while the summary was being prepared\n"
    );
    Ok(())
}

#[tokio::test]
async fn future_plan_items_and_taken_back_restore_follow_the_new_writer_without_rewriting_completed_items()
-> Result<()> {
    use crate::plans::{GenerationState, Item, ItemState, State};
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let source = Uuid::now_v7();
    f.journal
        .stage_utterance(
            source,
            "Own future three-step plan".into(),
            Target::Lock(job.request.track_id.clone()),
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
                kind: Kind::Plan,
                track_id: Some(job.request.track_id.clone()),
                boundary: None,
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let mut db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    crate::clock::register(&db)?;
    let mut plan = crate::plan_store::source(&db, &source.to_string())?.unwrap();
    plan.state = State::Held;
    plan.summary = "Own three-step fixture plan".into();
    plan.generation.state = GenerationState::Ready;
    for (order, title, state) in [
        (0, "Own completed item", ItemState::Succeeded),
        (1, "Own future item", ItemState::Planned),
        (2, "Own taken-back item", ItemState::Planned),
    ] {
        plan.items.push(Item {
            plan_item_id: Uuid::now_v7(),
            track_ref: job.request.track_id.clone(),
            folder_id: job.folder.id.clone(),
            order,
            title: title.into(),
            instruction: title.into(),
            constraints: vec![],
            done_when: "Own result exists".into(),
            after: vec![],
            skip_order: vec![],
            start_requested: false,
            state,
            utterance_id: None,
            detail: None,
        });
    }
    crate::plan_store::save(&db, &plan, "own.plan-fixture")?;
    let taken_back = plan.items[2].plan_item_id;
    let cut = crate::plan_store::command(
        &mut db,
        crate::plan_store::Command {
            op_id: Uuid::now_v7(),
            plan_id: plan.id,
            revision: plan.rev,
            action: crate::plan_store::Action::Cut {
                item_id: taken_back,
            },
        },
    )?;
    assert_eq!(cut.items[2].state, ItemState::Cut);
    let publishing = crate::fresh_files::transfer(&f.journal, &job).await?;
    crate::fresh_files::publish_owner(&f.journal, &publishing).await?;
    let moved = crate::plan_store::read(&db, plan.id)?;
    assert_eq!(moved.items[0], cut.items[0]);
    assert_eq!(moved.items[1].track_ref, job.successor_track);
    assert_eq!(moved.items[2].track_ref, job.successor_track);
    assert_eq!(
        moved.locked_track.as_deref(),
        Some(job.successor_track.as_str())
    );
    crate::plan_store::require_lock_source(&db, &moved)?;
    let restored = crate::plan_store::command(
        &mut db,
        crate::plan_store::Command {
            op_id: Uuid::now_v7(),
            plan_id: plan.id,
            revision: moved.rev,
            action: crate::plan_store::Action::Restore {
                item_id: taken_back,
            },
        },
    )?;
    assert_eq!(restored.items[2].state, ItemState::Planned);
    assert_eq!(restored.items[2].track_ref, job.successor_track);
    assert!(!restored.items[2].start_requested);
    assert_eq!(restored.items[0], cut.items[0]);
    Ok(())
}
#[tokio::test]
async fn replies_before_and_after_transfer_keep_the_old_question_address_and_never_answer_the_old_sdk_session()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let report = crate::reports::Report::new(
        job.request.track_id.clone(),
        job.request.session_id.clone(),
        job.boundary.as_ref().unwrap().checkpoint.turn_id,
        "own-question-report".into(),
        "Own late report".into(),
    );
    let report = f.journal.observe_report(report).await?.report;
    f.journal
        .analyze_report(
            report.id.clone(),
            report.revision,
            crate::reports::Analysis::Questions(vec![
                "Own first question?".into(),
                "Own second question?".into(),
            ]),
        )
        .await?;
    let snapshot = f.journal.snapshot().await?;
    let questions = snapshot["reportQuestions"].as_array().unwrap();
    let first = &questions[0];
    let second = &questions[1];
    let first_id = Uuid::now_v7();
    f.journal
        .respond_report(
            first_id,
            first["id"].as_str().unwrap().into(),
            first["revision"].as_i64().unwrap(),
            crate::reports::Answer::Reply {
                text: "Own answer before transfer".into(),
            },
        )
        .await?;
    let publishing = crate::fresh_files::transfer(&f.journal, &job).await?;
    crate::fresh_files::publish_owner(&f.journal, &publishing).await?;
    let late_id = Uuid::now_v7();
    f.journal
        .respond_report(
            late_id,
            second["id"].as_str().unwrap().into(),
            second["revision"].as_i64().unwrap(),
            crate::reports::Answer::Reply {
                text: "Own answer after transfer".into(),
            },
        )
        .await?;
    for (id, question) in [(first_id, first), (late_id, second)] {
        let reply = f.journal.read_utterance(id.to_string()).await?;
        assert_eq!(reply["decision"]["trackId"], job.successor_track);
        assert_eq!(reply["freshRunnable"], false);
        assert!(reply["replyTo"].is_null());
        assert!(
            reply["text"]
                .as_str()
                .unwrap()
                .contains(question["id"].as_str().unwrap())
        );
        assert!(
            reply["text"]
                .as_str()
                .unwrap()
                .contains(question["text"].as_str().unwrap())
        );
    }
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM report_questions WHERE state='answered-elsewhere'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        2
    );
    assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind IN ('turn.start','input/answer') AND json_extract(payload,'$.sessionId')=?1",[&job.request.session_id],|row|row.get::<_,i64>(0))?,0);
    Ok(())
}

#[tokio::test]
async fn a_late_reply_keeps_the_question_version_the_user_answered_and_stays_orphaned() -> Result<()>
{
    let f = Fixture::new(1).await?;
    let job = summarized(&f).await?;
    let mut report = crate::reports::Report::new(
        job.request.track_id.clone(),
        job.request.session_id.clone(),
        job.boundary.as_ref().unwrap().checkpoint.turn_id,
        "own-changing-report".into(),
        "Own original report".into(),
    );
    let observed = f.journal.observe_report(report.clone()).await?.report;
    f.journal
        .analyze_report(
            observed.id,
            observed.revision,
            crate::reports::Analysis::Questions(vec!["Own OLD question the user answered?".into()]),
        )
        .await?;
    let old = f.journal.snapshot().await?["reportQuestions"][0].clone();
    report.text = "Own changed report".into();
    let observed = f.journal.observe_report(report).await?.report;
    f.journal
        .analyze_report(
            observed.id,
            observed.revision,
            crate::reports::Analysis::Questions(vec![
                "Own NEW question needing a different answer?".into(),
            ]),
        )
        .await?;
    let publishing = crate::fresh_files::transfer(&f.journal, &job).await?;
    crate::fresh_files::publish_owner(&f.journal, &publishing).await?;
    let id = Uuid::now_v7();
    f.journal
        .respond_report(
            id,
            old["id"].as_str().unwrap().into(),
            old["revision"].as_i64().unwrap(),
            crate::reports::Answer::Reply {
                text: "Own answer to the original question".into(),
            },
        )
        .await?;
    let word = f.journal.read_utterance(id.to_string()).await?;
    assert_eq!(word["state"], "orphaned");
    assert_eq!(word["decision"], Value::Null);
    assert_eq!(word["target"]["trackId"], job.successor_track);
    assert!(word["replyTo"].is_null());
    let text = word["text"].as_str().unwrap();
    assert!(text.contains("Own OLD question the user answered?"));
    assert!(!text.contains("Own NEW question"));
    assert_eq!(
        f.journal.snapshot().await?["reportQuestions"][0]["text"],
        "Own NEW question needing a different answer?"
    );
    Ok(())
}
