//! Real SQLite, private Git refs and worker files for the nested chosen fork.
//! Every file, worker state and credential value belongs to the dummy fixture.
use super::*;
use crate::{
    realign::Phase as QueuePhase,
    realign_queue::Change as Queue,
    shared_context_store::{Change, Phase, Preparation as Shared, Request as SharedRequest},
};
#[path = "shared_undo_acceptance.rs"]
mod borrowed;
#[path = "shared_merge_acceptance.rs"]
mod merge;

fn branch_shared(f: &mut Fixture, shared: &Checkpoint) -> Result<()> {
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let original = f.workspaces[3].reference.clone();
    git(&f.source, &["update-ref", "-d", &original])?;
    f.workspaces[3].reference = format!("{}/B2", original.rsplit_once('/').unwrap().0);
    git(
        &f.source,
        &["update-ref", &f.workspaces[3].reference, &f.head],
    )?;
    db.execute(
        "UPDATE tracks SET label='B2' WHERE id=?1",
        [&f.workspaces[3].track_id],
    )?;
    db.execute(
        "UPDATE workspaces SET record=?2 WHERE track_id=?1",
        params![
            f.workspaces[3].track_id,
            serde_json::to_string(&f.workspaces[3])?
        ],
    )?;
    let branch = crate::branch::Branch {
        request: crate::branch::Request {
            op_id: Uuid::now_v7(),
            parent_track_id: f.workspaces[2].track_id.clone(),
            point: crate::branch_start::Selection::Turn {
                turn_id: shared.turn_id,
            },
            instructions: crate::branch::InstructionMode::Checkpoint,
        },
        track_id: f.workspaces[3].track_id.clone(),
        parent_session_id: "own-shared-B".into(),
        session_id: Some("own-chosen-B".into()),
        workspace: f.workspaces[3].clone(),
        point: crate::branch_start::Point::Turn {
            checkpoint: shared.clone(),
        },
        phase: crate::branch::Phase::Ready,
        instructions: None,
        origin: crate::branch_points::Origin {
            ended_at: Some(2000),
            timeline_at: Some(2000),
            turn_number: 1,
        },
    };
    db.execute(
        "INSERT INTO branches VALUES(?1,?2)",
        params![
            branch.request.op_id.to_string(),
            serde_json::to_string(&branch)?
        ],
    )?;
    f.inherit(3, 2, shared)?;
    Ok(())
}

pub(super) async fn nested(
    conflict: bool,
    consumers: usize,
) -> Result<(Fixture, Job, Request, Checkpoint, Checkpoint)> {
    let mut f = Fixture::new(3 + consumers).await?;
    fs::write(f.workspaces[0].path.join("shared-a.txt"), "Own shared A\n")?;
    if conflict {
        fs::write(f.workspaces[0].path.join("parent.txt"), "Own A fork\n")?;
    }
    let fork = f.save(0, None).await?;
    f.branch(&fork)?;
    fs::write(f.workspaces[0].path.join("retired-a.txt"), "Own A middle\n")?;
    let middle = f.save(0, Some(fork)).await?;
    fs::write(f.workspaces[0].path.join("retired-a.txt"), "Own A later\n")?;
    if conflict {
        fs::write(f.workspaces[0].path.join("parent.txt"), "Own A later\n")?;
    }
    let old = f.save(0, Some(middle)).await?;
    fs::write(f.workspaces[1].path.join("chosen-a.txt"), "Own chosen A\n")?;
    if conflict {
        fs::write(f.workspaces[1].path.join("parent.txt"), "Own chosen A\n")?;
    }
    f.save(1, None).await?;
    f.inherit(2, 0, &old)?;
    fs::write(f.workspaces[2].path.join("shared-b.txt"), "Own shared B\n")?;
    if conflict {
        fs::write(
            f.workspaces[2].path.join("parent.txt"),
            "Own shared B edit\n",
        )?;
    }
    let shared = f.save(2, None).await?;
    branch_shared(&mut f, &shared)?;
    fs::write(f.workspaces[3].path.join("chosen-b.txt"), "Own chosen B\n")?;
    let child = f.save(3, None).await?;
    f.worker(3, child).await?;
    for index in 4..f.workspaces.len() {
        f.inherit(index, 2, &shared)?;
        fs::write(
            f.workspaces[index]
                .path
                .join(format!("consumer-{index}.txt")),
            format!("Own consumer {index}\n"),
        )?;
        let child = f.save(index, None).await?;
        f.worker(index, child).await?;
    }
    fs::write(
        f.workspaces[2].path.join("later-b.txt"),
        "Must never enter the chosen fork\n",
    )?;
    let later = f.save(2, Some(shared.clone())).await?;
    let job = f.action(Action::Open { tracks: None }).await?;
    let job = f
        .action(Action::ChoosePath {
            id: job.id,
            revision: job.revision,
            track: f.workspaces[1].track_id.clone(),
        })
        .await?;
    let job = f
        .action(Action::ChoosePath {
            id: job.id,
            revision: job.revision,
            track: f.workspaces[3].track_id.clone(),
        })
        .await?;
    let request = Request {
        op_id: Uuid::now_v7(),
        track_id: f.workspaces[3].track_id.clone(),
        source: Source::Reparent {
            job_id: job.id,
            revision: job.revision,
            parent: f.workspaces[2].track_id.clone(),
        },
    };
    Ok((f, job, request, shared, later))
}
fn shared_request(job: &Job, request: &Request) -> SharedRequest {
    let Source::Reparent { parent, .. } = &request.source else {
        panic!("Expected own reparent request");
    };
    SharedRequest {
        job: job.id,
        revision: job.revision,
        descendant: request.track_id.clone(),
        parent: parent.clone(),
    }
}

async fn publish_unhosted_replacement(f: &Fixture, checked: &Job) -> Result<Job> {
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let repo = intercat_git::Repository::open(&f.source)?;
    let committing = f
        .journal
        .integration_command(ReviewCommand {
            op_id: Uuid::now_v7(),
            action: commit(checked),
        })
        .await?;
    assert_eq!(committing.status, Status::Committing);
    let entries: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    let entries = entries
        .into_iter()
        .filter(|entry| entry.job == committing.id)
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 2);
    for entry in entries {
        assert!(
            checked
                .path_state
                .as_ref()
                .unwrap()
                .retired
                .contains_key(&entry.track)
        );
        let hosted:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM workers WHERE track_id=?1 AND (json_extract(record,'$.sessionId') IS NOT NULL OR json_extract(record,'$.identity') IS NOT NULL OR json_extract(record,'$.activeTurn') IS NOT NULL))",[&entry.track],|row|row.get(0))?;
        assert!(
            !hosted,
            "This acceptance uses only never-hosted retirement boundaries"
        );
        // Use the existing driver's durable/ref steps for these never-hosted
        // paths. Live SDK Stop is a separate remaining M4 acceptance gate.
        f.journal
            .change_path_retirement(crate::path_retirement::Change::Freeze {
                id: entry.id,
                session: None,
                checkpoint: None,
            })
            .await?;
        let frozen: Vec<crate::path_retirement::Entry> =
            serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
        let saved = frozen
            .iter()
            .find(|saved| saved.id == entry.id)
            .unwrap()
            .saved
            .as_ref()
            .unwrap();
        repo.compare_and_swap(&saved.reference, None, saved.sha.parse()?)?;
        repo.sync_ref(&saved.reference)?;
        f.journal
            .change_path_retirement(crate::path_retirement::Change::Ready(entry.id))
            .await?;
    }
    f.review.recover().await?;
    let done = f.settled(checked.id).await?;
    assert_eq!(done.status, Status::Done, "{:?}", done.error);
    Ok(done)
}

async fn verify_restore_sources(f: &Fixture, published: &Job) -> Result<()> {
    let entries: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    let mut count = 0;
    for entry in entries
        .into_iter()
        .filter(|entry| entry.job == published.id)
    {
        let source = f.journal.path_restore_source(entry.id).await?;
        let repo = source.verify_repository()?;
        assert_eq!(source.entry.track, entry.track);
        assert_eq!(source.entry.workspace.path, entry.workspace.path);
        assert_eq!(
            source.entry.saved.as_ref().unwrap().session_id,
            entry.saved.as_ref().unwrap().session_id
        );
        assert_eq!(source.target, published.candidate.as_ref().unwrap().head);
        assert_eq!(
            source.basis,
            crate::lineage_store::basis(
                &rusqlite::Connection::open(f.root.path().join("events.sqlite"))?,
                &entry.track
            )?
        );
        let expected = published
            .undo
            .as_ref()
            .map(|plan| {
                plan.records
                    .iter()
                    .filter(|record| record.track_id == entry.track)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert_eq!(source.cancelled, expected);
        for record in &source.cancelled {
            assert_eq!(record.track_id, entry.track);
            assert_eq!(
                repo.reference_target(&record.reference)?,
                Some(record.sha.parse()?)
            );
        }
        // Git verification must reject a changed protected aside point.
        let saved = entry.saved.as_ref().unwrap();
        let other: intercat_git::ObjectId = published.project.base.parse()?;
        let saved_sha = saved.sha.parse()?;
        if saved_sha != other {
            repo.compare_and_swap(&saved.reference, Some(saved_sha), other)?;
            assert!(source.verify_repository().is_err());
            repo.compare_and_swap(&saved.reference, Some(other), saved_sha)?;
            source.verify_repository()?;
        }
        let target = source.target.parse()?;
        if target != other {
            repo.compare_and_swap(&source.target_ref, Some(target), other)?;
            assert!(source.verify_repository().is_err());
            repo.compare_and_swap(&source.target_ref, Some(other), target)?;
            source.verify_repository()?;
        }
        let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
        let raw: String = db.query_row(
            "SELECT record FROM path_retirements WHERE id=?1",
            [entry.id.to_string()],
            |row| row.get(0),
        )?;
        let mut not_ready = entry.clone();
        not_ready.phase = crate::path_retirement::Phase::Saving;
        db.execute(
            "UPDATE path_retirements SET record=?2 WHERE id=?1",
            params![entry.id.to_string(), serde_json::to_string(&not_ready)?],
        )?;
        assert!(f.journal.path_restore_source(entry.id).await.is_err());
        db.execute(
            "UPDATE path_retirements SET record=?2 WHERE id=?1",
            params![entry.id.to_string(), raw],
        )?;
        if let Some(record) = source.cancelled.first() {
            let raw: String = db.query_row(
                "SELECT record FROM integration_records WHERE id=?1",
                [record.id.to_string()],
                |row| row.get(0),
            )?;
            let mut changed = record.clone();
            changed.ordinal += 1;
            changed.validate()?;
            db.execute(
                "UPDATE integration_records SET record=?2 WHERE id=?1",
                params![record.id.to_string(), serde_json::to_string(&changed)?],
            )?;
            assert!(f.journal.path_restore_source(entry.id).await.is_err());
            db.execute(
                "UPDATE integration_records SET record=?2 WHERE id=?1",
                params![record.id.to_string(), raw],
            )?;
        }
        f.journal
            .path_restore_source(entry.id)
            .await?
            .verify_repository()?;
        count += 1;
    }
    assert_eq!(count, 2);
    Ok(())
}

async fn restore_saved_paths(f: &Fixture, published: &Job, files: bool) -> Result<()> {
    use crate::path_restore::{Phase as RestorePhase, Request as RestoreRequest};
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let words = || -> Result<Vec<(String, String, i64, String)>> {
        Ok(db
            .prepare(
                "SELECT id,state,revision,coalesce(decision,'null') FROM utterances ORDER BY rowid",
            )?
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    };
    let before_words = words()?;
    let cancelled: i64 = db.query_row("SELECT count(*) FROM integration_cancelled", [], |row| {
        row.get(0)
    })?;
    let before_workers = f.journal.snapshot().await?["workers"].clone();
    let entries: Vec<crate::path_retirement::Entry> =
        serde_json::from_value(f.journal.snapshot().await?["pathRetirements"].clone())?;
    for entry in entries
        .into_iter()
        .filter(|entry| entry.job == published.id)
    {
        let held:i64=db.query_row("SELECT count(*) FROM utterances WHERE state='held' AND json_extract(decision,'$.trackId')=?1",[&entry.track],|row|row.get(0))?;
        assert!(held > 0, "The own dummy path must have held words");
        let pending = entry.workspace.path.join("restore-pending.txt");
        fs::write(&pending, "Own held Restore edit\n")?;
        let source = f.journal.path_restore_source(entry.id).await?;
        assert_eq!(!source.cancelled.is_empty(), files);
        let request = RestoreRequest {
            op_id: Uuid::now_v7(),
            retirement_id: entry.id,
        };
        let saved_review = f
            .action(Action::Open {
                tracks: Some(vec![f.workspaces[3].track_id.clone()]),
            })
            .await?;
        let queued = f.journal.restore_path(request.clone()).await?;
        assert_eq!(queued.phase, RestorePhase::Queued);
        assert!(
            f.journal
                .restore_path(RestoreRequest {
                    op_id: Uuid::now_v7(),
                    retirement_id: entry.id
                })
                .await
                .is_err()
        );
        let gate = Arc::new(crate::dispatch_control::Gate::new(false));
        let guard = || crate::inheritance_apply::Guard::Recovery {
            generation: Arc::new(AtomicU64::new(0)),
            expected: 0,
            epoch: gate.epoch(),
            gate: gate.clone(),
        };
        let identity = f._host.identity()?;
        let tracker = Arc::new(Mutex::new(crate::process::Tracker::new(
            format!("own-restore-{}", entry.id),
            identity,
        )?));
        let operation = crate::path_restore::file_operation(&request);
        let checkpoint_turn = Uuid::new_v5(&operation, b"restored-checkpoint");
        let trigger = if files {
            format!(
                "CREATE TRIGGER own_restore_fault BEFORE INSERT ON operations WHEN NEW.kind='checkpoint.save' AND json_extract(NEW.payload,'$.checkpoint.turnId')='{checkpoint_turn}' BEGIN SELECT RAISE(ABORT,'Own Restore interrupted'); END;"
            )
        } else {
            format!(
                "CREATE TRIGGER own_restore_fault BEFORE UPDATE ON path_retirements WHEN NEW.id='{}' AND NEW.active=0 BEGIN SELECT RAISE(ABORT,'Own Restore interrupted'); END;",
                entry.id
            )
        };
        db.execute_batch(&trigger)?;
        let mut interrupted = false;
        for _ in 0..4 {
            let boundary = if files {
                crate::path_restore_apply::Boundary::Live(crate::inheritance_apply::Boundary {
                    guard: guard(),
                    tracker: tracker.clone(),
                    identity,
                })
            } else {
                crate::path_restore_apply::Boundary::Offline {
                    guard: guard(),
                    directory: f.root.path().join("never-hosted"),
                    workspace: entry.workspace.path.clone(),
                    expected: None,
                }
            };
            match crate::path_restore_apply::apply(&f.journal, request.clone(), boundary).await {
                Ok(_) => {
                    return Err(
                        "The own Restore fault did not interrupt its durable boundary".into(),
                    );
                }
                Err(error) => {
                    if error.to_string().contains("Own Restore interrupted") {
                        interrupted = true;
                        break;
                    }
                    let Some(required) =
                        error.downcast_ref::<crate::workspace_access::ConfirmationRequired>()
                    else {
                        return Err(error);
                    };
                    // Consent is confined to this fixture's own temporary files.
                    f.journal
                        .approve_path_restore_access(crate::path_restore::AccessConsent {
                            op_id: Uuid::now_v7(),
                            request: request.clone(),
                            fingerprint: required.inspection.fingerprint.clone(),
                        })
                        .await?;
                }
            }
        }
        assert!(
            interrupted,
            "The own Restore access boundary did not settle"
        );
        let invalidated = f.journal.integration_job(saved_review.id).await?;
        assert_eq!(invalidated.status, Status::Failed);
        assert_eq!(invalidated.revision, saved_review.revision + 1);
        assert!(invalidated.candidate.is_none());
        assert!(
            invalidated
                .error
                .as_deref()
                .unwrap()
                .contains("was restored · Refresh")
        );
        let refresh = f
            .action(Action::Refresh {
                id: invalidated.id,
                revision: invalidated.revision,
            })
            .await;
        assert!(
            refresh.is_err_and(|error| error.to_string().contains("Finish Restore")),
            "A new Commit capture must not overlap the saved file operation"
        );
        assert!(
            crate::path_retirement::holds(&db, &entry.track)?,
            "An interrupted Restore must keep its hold"
        );
        assert_eq!(
            crate::lineage_store::basis(&db, &entry.track)?,
            source.basis,
            "A failed checkpoint reservation must roll back its lineage change"
        );
        let frozen = f.journal.operation(operation).await?.unwrap();
        assert_eq!(
            frozen["state"],
            if files { "dispatched" } else { "prepared" }
        );
        let frozen_payload = frozen["payload"].clone();
        db.execute_batch("DROP TRIGGER own_restore_fault;")?;
        // A fresh SQLite writer and the actual no-window runtime driver must
        // finish the same intent without a worker configuration or model call.
        let reopened = Journal::open(
            f.root.path().join("events.sqlite"),
            crate::secrets::Key::new([7; 32]),
        )
        .await?;
        let editor = crate::editor::Manager::new(
            f.root.path(),
            &f.root.path().join("own-unused-editor.log"),
        )?;
        let runtime = crate::runtime::Runtime::start(
            crate::host_memory::Pool::new(),
            f.root.path().to_owned(),
            f.root.path().join("own-unused-rpc"),
            reopened.clone(),
            editor,
            Arc::new(crate::scheduler::Capacity::new(1)),
            gate.clone(),
        );
        let finished = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let job = reopened.restore_path(request.clone()).await?;
                if job.phase == RestorePhase::Done {
                    break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
                }
                if job.phase == RestorePhase::Failed {
                    return Err(format!("Own background Restore failed: {:?}", job.error).into());
                }
                if job.phase == RestorePhase::WaitingForAccess {
                    reopened
                        .approve_path_restore_access(crate::path_restore::AccessConsent {
                            op_id: Uuid::now_v7(),
                            request: request.clone(),
                            fingerprint: job.inspection.as_ref().unwrap().fingerprint.clone(),
                        })
                        .await?;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        runtime.shutdown().await;
        finished??;
        assert_eq!(
            reopened.operation(operation).await?.unwrap()["payload"],
            frozen_payload,
            "Recovery must keep the original before/after objects"
        );
        let done = f.journal.restore_path(request.clone()).await?;
        assert_eq!(done.phase, RestorePhase::Done);
        let restored = done.restored.as_ref().unwrap();
        assert_eq!(restored.track_id, entry.track);
        assert_eq!(
            restored.session_id,
            entry.saved.as_ref().unwrap().session_id
        );
        assert_eq!(
            restored.suggest_realign,
            !files && source.basis.base != source.target
        );
        assert_eq!(restored.checkpoint.is_some(), files);
        let followups = crate::path_restore_control::snapshot(&db)?;
        let followup = followups
            .iter()
            .find(|item| item.request_id == request.op_id);
        assert_eq!(followup.is_some(), restored.suggest_realign);
        if let Some(followup) = followup {
            assert_eq!(followup.target, source.target);
            assert_eq!(followup.track, entry.track);
            assert_eq!(
                followup.source,
                crate::realign::Source::Commit {
                    job_id: published.id,
                    revision: published.revision
                }
            );
            let dismiss = crate::path_restore::Control {
                op_id: Uuid::now_v7(),
                id: request.op_id,
                revision: done.revision,
                action: crate::path_restore::ControlAction::DismissRealign {
                    head: followup.target.clone(),
                },
            };
            let old = crate::path_restore::Control {
                op_id: Uuid::now_v7(),
                action: crate::path_restore::ControlAction::DismissRealign {
                    head: "0".repeat(40),
                },
                ..dismiss.clone()
            };
            assert!(
                f.journal
                    .change_path_restore(crate::path_restore_store::Change::Control(old))
                    .await
                    .is_err()
            );
            let dismissed = f
                .journal
                .change_path_restore(crate::path_restore_store::Change::Control(dismiss.clone()))
                .await?;
            assert_eq!(dismissed.revision, done.revision);
            f.journal
                .change_path_restore(crate::path_restore_store::Change::Control(dismiss))
                .await?;
            assert!(
                !crate::path_restore_control::snapshot(&db)?
                    .iter()
                    .any(|item| item.request_id == request.op_id)
            );
        }
        assert!(!crate::path_retirement::holds(&db, &entry.track)?);
        assert_eq!(fs::read_to_string(&pending)?, "Own held Restore edit\n");
        if files {
            assert_eq!(
                crate::lineage_store::basis(&db, &entry.track)?.base,
                source.target
            );
            assert!(entry.workspace.path.join("independent.txt").exists());
            for path in [
                "shared-a.txt",
                "shared-b.txt",
                "chosen-a.txt",
                "chosen-b.txt",
            ] {
                assert!(entry.workspace.path.join(path).exists(), "{path}");
            }
        } else {
            assert_eq!(
                crate::lineage_store::basis(&db, &entry.track)?,
                source.basis
            );
        }
        let checkpoints: i64 = db.query_row(
            "SELECT count(*) FROM checkpoint_lineage WHERE track=?1",
            [&entry.track],
            |row| row.get(0),
        )?;
        let replay = crate::path_restore_apply::apply(
            &f.journal,
            request.clone(),
            crate::path_restore_apply::Boundary::Offline {
                guard: guard(),
                directory: f.root.path().join("never-hosted"),
                workspace: entry.workspace.path.clone(),
                expected: None,
            },
        )
        .await?;
        assert!(replay.is_none());
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM checkpoint_lineage WHERE track=?1",
                [&entry.track],
                |row| row.get::<_, i64>(0)
            )?,
            checkpoints
        );
    }
    assert_eq!(
        words()?,
        before_words,
        "Restore must leave every held word and its revision unchanged"
    );
    assert_eq!(
        f.journal.snapshot().await?["workers"],
        before_workers,
        "Never-hosted paths must not invent another worker or session"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM integration_cancelled", [], |row| row
            .get::<_, i64>(
            0
        ))?,
        cancelled
    );
    Ok(())
}

#[tokio::test]
async fn a_shared_context_reopens_partial_pins_and_applies_the_exact_fork_through_check_and_commit()
-> Result<()> {
    let (mut f, job, request, shared, later) = nested(false, 1).await?;
    let queued = f
        .journal
        .change_realign_queue(Queue::Request(request.clone()))
        .await?
        .unwrap();
    let own = shared_request(&job, &request);
    let Shared::Step(pending) = f.journal.prepare_shared_context(own.clone()).await? else {
        panic!("A shared input must be computed before applying files");
    };
    assert_eq!(pending.phase, Phase::Pending);
    assert_eq!(pending.input.projection.source.sha, shared.sha);
    assert!(
        !pending
            .input
            .graph
            .points
            .iter()
            .any(|point| point.track == f.workspaces[2].track_id && point.sha == later.sha)
    );
    let computed = crate::shared_context_service::compute(&pending)?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    db.execute_batch("CREATE TRIGGER own_shared_begin_failure BEFORE UPDATE ON shared_contexts WHEN json_extract(NEW.record,'$.phase')='pinning' BEGIN SELECT RAISE(ABORT,'Own shared result write failure'); END;")?;
    assert!(
        f.journal
            .change_shared_context(Change::Begin {
                id: pending.id,
                revision: pending.revision,
                computed: Box::new(computed.clone())
            })
            .await
            .is_err()
    );
    assert_eq!(
        f.journal.shared_context(pending.id).await?.phase,
        Phase::Pending
    );
    db.execute_batch("DROP TRIGGER own_shared_begin_failure;")?;
    let pinning = f
        .journal
        .change_shared_context(Change::Begin {
            id: pending.id,
            revision: pending.revision,
            computed: Box::new(computed.clone()),
        })
        .await?;
    let replay = f
        .journal
        .change_shared_context(Change::Begin {
            id: pending.id,
            revision: pending.revision,
            computed: Box::new(computed.clone()),
        })
        .await?;
    assert_eq!(pinning.revision, replay.revision);
    let repo = intercat_git::Repository::open(&f.source)?;
    let source_ref = crate::shared_context_service::source_ref(&job.slug, &shared.sha);
    assert!(repo.reference_target(&source_ref)?.is_none());
    // Model a crash after protecting only one root. Pinning must remain the
    // durable phase until every root and the exact source ref are synced.
    let one = format!(
        "refs/intercat/{}/shared-contexts/{}/objects/{}",
        job.slug, pending.id, computed.anchor
    );
    repo.compare_and_swap(&one, None, computed.anchor.parse()?)?;
    repo.sync_ref(&one)?;
    f.review.shutdown().await;
    f.journal = Journal::open(
        f.root.path().join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    f.review = Service::new(f.journal.clone(), f.root.path().join("integration"));
    assert_eq!(
        f.journal.shared_context(pending.id).await?.phase,
        Phase::Pinning
    );
    db.execute_batch("CREATE TRIGGER own_shared_admit_failure BEFORE UPDATE ON shared_contexts WHEN json_extract(NEW.record,'$.phase')='ready' BEGIN SELECT RAISE(ABORT,'Own shared admission write failure'); END;")?;
    assert!(
        crate::shared_context_service::admit(&f.journal, pending.id)
            .await
            .is_err()
    );
    assert_eq!(
        f.journal.shared_context(pending.id).await?.phase,
        Phase::Pinning
    );
    assert_eq!(
        repo.reference_target(&source_ref)?.unwrap().to_string(),
        shared.sha
    );
    db.execute_batch("DROP TRIGGER own_shared_admit_failure;")?;
    f.review.shutdown().await;
    f.journal = Journal::open(
        f.root.path().join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    f.review = Service::new(f.journal.clone(), f.root.path().join("integration"));
    let claimed = f
        .journal
        .change_realign_queue(Queue::Claim {
            id: request.op_id,
            revision: queued.revision,
        })
        .await?
        .unwrap();
    let applied = f
        .apply_files(3, &request)
        .await?
        .ok_or("Missing shared file application")?;
    let ready = f.journal.shared_context(pending.id).await?;
    assert_eq!(ready.phase, Phase::Ready);
    assert_eq!(ready.computed.as_ref(), Some(&computed));
    assert!(f.workspaces[3].path.join("shared-b.txt").exists());
    assert!(f.workspaces[3].path.join("chosen-a.txt").exists());
    assert!(f.workspaces[3].path.join("chosen-b.txt").exists());
    assert!(!f.workspaces[3].path.join("retired-a.txt").exists());
    assert!(!f.workspaces[3].path.join("later-b.txt").exists());
    assert!(f.workspaces[2].path.join("retired-a.txt").exists());
    assert!(f.workspaces[2].path.join("later-b.txt").exists());
    assert!(matches!(
        f.journal.prepare_shared_context(own).await?,
        Shared::Ready(_)
    ));
    let mut worker: WorkerState = serde_json::from_value(
        f.journal.snapshot().await?["workers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|worker| worker["trackId"] == request.track_id)
            .unwrap()
            .clone(),
    )?;
    worker.checkpoint = Some(applied.checkpoint);
    f.journal.worker_state(worker).await?;
    f.journal
        .change_realign(crate::realign_store::Change::Complete(applied.operation))
        .await?;
    f.journal
        .change_realign_queue(Queue::Finish {
            id: request.op_id,
            revision: claimed.revision,
            phase: QueuePhase::Done,
            error: None,
            inspection: None,
            consume_stop: false,
        })
        .await?;
    let updated = f.journal.integration_job(job.id).await?;
    assert_eq!(updated.revision, job.revision + 1);
    assert!(updated.path_state.as_ref().unwrap().ready());
    let updated = f.action(Action::Open { tracks: None }).await?;
    assert_eq!(updated.status, Status::Open, "{:?}", updated.error);
    f.action(Action::Prepare {
        id: updated.id,
        revision: updated.revision,
    })
    .await?;
    let checked = f.settled(updated.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let candidate = checked.candidate.as_ref().unwrap();
    let files = repo.tree_files(candidate.tree.parse()?)?;
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
    let done = publish_unhosted_replacement(&f, &checked).await?;
    verify_restore_sources(&f, &done).await?;
    let published = repo
        .reference_target(&done.plan.as_ref().unwrap().target_ref)?
        .unwrap();
    assert_eq!(published.to_string(), done.candidate.as_ref().unwrap().head);
    assert_eq!(repo.tree_files(repo.tree_id(published)?)?, files);
    let raw: String = db.query_row(
        "SELECT record FROM integration_records WHERE job=?1 AND track=?2",
        params![done.id.to_string(), request.track_id],
        |row| row.get(0),
    )?;
    let receipt: crate::integration_record::Record = serde_json::from_str(&raw)?;
    assert_eq!(
        receipt.coverage_after[&f.workspaces[2].track_id].sha,
        shared.sha
    );
    assert!(
        !receipt
            .coverage_before
            .contains_key(&f.workspaces[2].track_id)
    );
    let point = crate::lineage_store::point(&db, &request.track_id, &receipt.checkpoint.sha)?;
    assert!(point.views.iter().any(|view| view.source.sha == shared.sha));
    restore_saved_paths(&f, &done, false).await?;
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    f.review.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn a_shared_conflict_is_saved_with_its_own_three_trees_and_never_changes_worker_files()
-> Result<()> {
    let (f, job, request, shared, later) = nested(true, 1).await?;
    let before = fs::read(f.workspaces[3].path.join("parent.txt"))?;
    let Shared::Step(record) =
        crate::shared_context_service::advance(&f.journal, shared_request(&job, &request)).await?
    else {
        panic!("Shared B edits must conflict with the chosen A edit");
    };
    assert_eq!(record.phase, Phase::NeedsMerge);
    assert_eq!(record.input.projection.source.sha, shared.sha);
    assert!(
        !record
            .input
            .graph
            .points
            .iter()
            .any(|point| point.sha == later.sha)
    );
    let crate::shared_context::Outcome::Conflict(conflict) =
        &record.computed.as_ref().unwrap().outcome
    else {
        panic!("Expected the shared-source conflict");
    };
    assert_eq!(conflict.stage, crate::replacement_reparent::Stage::Worktree);
    assert_eq!(conflict.paths, ["parent.txt"]);
    let repo = intercat_git::Repository::open(&f.source)?;
    assert_eq!(
        repo.tree_id(record.computed.as_ref().unwrap().anchor.parse()?)?
            .to_string(),
        conflict.target
    );
    assert_eq!(fs::read(f.workspaces[3].path.join("parent.txt"))?, before);
    assert!(f.workspaces[3].path.join("retired-a.txt").exists());
    let Shared::Step(replayed) =
        crate::shared_context_service::advance(&f.journal, shared_request(&job, &request)).await?
    else {
        panic!("The same unresolved shared conflict must remain frozen");
    };
    assert_eq!(record.id, replayed.id);
    assert_eq!(record.revision, replayed.revision);
    assert_eq!(record.computed, replayed.computed);
    assert_eq!(
        f.journal.integration_job(job.id).await?.revision,
        job.revision
    );
    f.review.shutdown().await;
    Ok(())
}
