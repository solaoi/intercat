//! Own real Git files and saved checkpoints; SDK responses are explicitly modeled.
use super::*;
#[path = "two_ways_controls_acceptance.rs"]
mod controls;
#[path = "two_ways_initial_acceptance.rs"]
mod initial;
#[path = "two_ways_model_acceptance.rs"]
mod presentation;
use crate::two_ways::{self, Change as PairChange, Phase as PairPhase};
use muse_code_sdk::msp::{ModelListResult, UsageReadResult};

fn usage(window: i64, weekly: i64) -> UsageReadResult {
    let now = crate::journal::timestamp();
    serde_json::from_value(json!({"usage":{"window":{"usedPercent":window,"windowDurationMins":300,"resetsAtMs":now+3600000},"weekly":{"usedPercent":weekly,"resetsAtMs":now+86400000},"tier":"own-dummy-tier","observedAtMs":now-1}})).unwrap()
}
fn models() -> ModelListResult {
    serde_json::from_value(json!({"providerId":"own-provider","profileId":null,"source":"fakeCatalog","models":[{"modelId":"own-model","displayLabel":"Generated model","providerId":"own-provider","profileId":null,"releaseDate":null,"description":null,"contextLimit":null,"outputLimit":null,"cost":null,"variants":["minimal","high","xhigh"],"isActive":true,"isDefault":true}]})).unwrap()
}
async fn pending(f: &Fixture) -> Result<(WorkerState, Value, crate::two_ways::Job)> {
    let point = f.save(0, None).await?;
    let worker = worker(f, point.clone()).await?;
    receipt(
        f,
        &worker,
        &point,
        "Own bounded source before the paired instruction",
    )?;
    let path = f.root.path().join("own-reference.txt");
    fs::write(&path, "Own generated reference bytes\n")?;
    let reference = f.journal.import_attachment(Uuid::now_v7(), path).await?;
    let id = Uuid::now_v7();
    f.journal
        .stage_utterance(
            id,
            "Own future work".into(),
            Target::Lock(worker.track_id.clone()),
            None,
            vec![reference.id],
        )
        .await?;
    let input = f.journal.claim_input().await?.unwrap();
    f.journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(worker.track_id.clone()),
                boundary: Some("now".into()),
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
            input_id: word["id"].as_str().unwrap().into(),
            revision: word["revision"].as_i64().unwrap(),
            selection: two_ways::Selection {
                track_id: worker.track_id.clone(),
                session_id: worker.session_id.clone().unwrap(),
                variation: two_ways::Variation::from_catalog(&models())?,
            },
        }))
        .await?;
    Ok((worker, word, job))
}
async fn capture(f: &Fixture, job: &two_ways::Job) -> Result<two_ways::Job> {
    f.journal
        .two_ways(PairChange::Capture {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await
}
async fn inspect(f: &Fixture, job: &two_ways::Job, read: UsageReadResult) -> Result<two_ways::Job> {
    f.journal
        .two_ways(PairChange::Inspect {
            id: job.request.op_id,
            revision: job.revision,
            usage: read,
            models: models(),
        })
        .await
}
async fn branch(f: &Fixture, job: &two_ways::Job) -> Result<crate::branch::Branch> {
    let mut branch = f
        .journal
        .branch(crate::branch::Change::Prepare(job.branch_request()?))
        .await?;
    let snapshot = f.journal.snapshot().await?;
    let folder = crate::folders::from_snapshot(&snapshot, &branch.track_id)?;
    let mut source = folder.source().await?;
    source.head = branch.point.sha().parse()?;
    crate::worktree::prepare(
        &source,
        &branch.workspace.path,
        &branch.workspace.reference,
        Uuid::new_v5(&job.request.op_id, b"own-paired-worktree"),
        &f.journal,
    )
    .await?;
    let instructions = crate::branch_instructions::freeze(&branch, &folder.root).await?;
    branch = f
        .journal
        .branch(crate::branch::Change::Instructions {
            operation: job.branch_id,
            snapshot: instructions,
        })
        .await?;
    let roots = crate::attachments::reserve(&branch.workspace.path, &branch.workspace.base)?;
    branch = f
        .journal
        .branch(crate::branch::Change::WorkspaceReady {
            operation: job.branch_id,
            roots,
        })
        .await?;
    crate::branch_instructions::apply(&branch).await?;
    let branch = f
        .journal
        .branch(crate::branch::Change::Received {
            operation: job.branch_id,
            session: Uuid::now_v7().to_string(),
        })
        .await?;
    Ok(branch)
}
async fn ready(f: &Fixture, job: &two_ways::Job) -> Result<(two_ways::Job, crate::branch::Branch)> {
    let job = inspect(f, &capture(f, job).await?, usage(20, 30)).await?;
    let branch = branch(f, &job).await?;
    f.journal
        .confirm_fingerprints(f.journal.fingerprint_catalog().await?.current)
        .await?;
    let job = f
        .journal
        .two_ways(PairChange::BranchReady {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    let branch = crate::branch_run::activate(&f.journal, branch.request.op_id).await?;
    Ok((job, branch))
}
fn admit_project_secrets(f: &Fixture, session: &str) -> Result<()> {
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    // Model the local session opening with the fixture's empty project-secret revision.
    db.execute("UPDATE workers SET record=json_set(record,'$.secretRevision',0) WHERE json_extract(record,'$.sessionId')=?1",[session])?;
    Ok(())
}
fn turn_params(session: &str, effort: &str) -> Value {
    json!({"commandId":Uuid::now_v7(),"sessionId":session,"reasoningEffort":effort,"input":[{"type":"skill","source":"user","selector":"intercat-report","arguments":"Own future work"}]})
}

#[test]
fn provider_percentages_and_declared_efforts_keep_unknown_usage_distinct() -> Result<()> {
    let now = crate::journal::timestamp() + 1000;
    assert!(matches!(
        two_ways::budget(&usage(80, 70), now)?,
        two_ways::Budget::Available { remaining: 20, .. }
    ));
    assert!(matches!(
        two_ways::budget(&usage(20, 81), now)?,
        two_ways::Budget::Insufficient { remaining: 19, .. }
    ));
    assert!(matches!(
        two_ways::budget(&usage(120, 0), now)?,
        two_ways::Budget::Insufficient { remaining: 0, .. }
    ));
    assert_eq!(
        two_ways::budget(&serde_json::from_value::<UsageReadResult>(json!({}))?, now)?,
        two_ways::Budget::Unknown
    );
    assert!(two_ways::budget(&usage(-1, 20), now).is_err());
    let mut expired = usage(10, 10);
    expired.usage.as_mut().unwrap().weekly.resets_at_ms = now;
    assert!(two_ways::budget(&expired, now).is_err());
    let choice = two_ways::Variation::from_catalog(&models())?;
    assert_eq!(serde_json::to_value(choice.primary)?, "minimal");
    assert_eq!(serde_json::to_value(choice.alternate)?, "xhigh");
    let mut unknown = models();
    unknown.models[0].is_active = false;
    assert!(two_ways::Variation::from_catalog(&unknown).is_err());
    let mut unclear = models();
    unclear.models[0].variants = serde_json::from_value(json!("unknown"))?;
    assert!(two_ways::Variation::from_catalog(&unclear).is_err());
    Ok(())
}
#[tokio::test]
async fn the_original_waits_at_its_exact_checkpoint_and_only_effort_differs_at_two_real_admissions()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let (source, word, job) = pending(&f).await?;
    let before = fs::read(f.workspaces[0].path.join("parent.txt"))?;
    assert_eq!(
        f.journal
            .two_ways(PairChange::Prepare(job.request.clone()))
            .await?
            .revision,
        job.revision
    );
    let original = turn_params(source.session_id.as_ref().unwrap(), "minimal");
    assert!(
        f.journal
            .reserve_turn(
                Uuid::now_v7(),
                job.request.input_id.clone(),
                job.request.revision,
                job.source_track.clone(),
                original,
                None,
            )
            .await
            .is_err()
    );
    let later = future(&f).await?;
    assert_eq!(later["twoWaysRunnable"], true);
    let (job, branch) = ready(&f, &job).await?;
    assert_eq!(
        f.journal
            .read_utterance(later["id"].as_str().unwrap().into())
            .await?["twoWaysRunnable"],
        false
    );
    let attempt = f
        .journal
        .two_ways(PairChange::Publish {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    assert_eq!(attempt.phase, PairPhase::Queued);
    let first = f
        .journal
        .read_utterance(job.request.input_id.clone())
        .await?;
    let second = f
        .journal
        .read_utterance(job.child_input.to_string())
        .await?;
    assert_eq!(first["text"], word["text"]);
    assert_eq!(second["text"], word["text"]);
    assert_eq!(first["attachments"], second["attachments"]);
    assert_eq!(second["attachments"].as_array().unwrap().len(), 1);
    assert_eq!(first["reasoningEffort"], "minimal");
    assert_eq!(second["reasoningEffort"], "xhigh");
    assert_eq!(second["decision"]["trackId"], branch.track_id);
    assert_eq!(fs::read(branch.workspace.path.join("parent.txt"))?, before);
    assert_eq!(fs::read(f.workspaces[0].path.join("parent.txt"))?, before);
    assert!(
        f.journal
            .two_ways(PairChange::Publish {
                id: job.request.op_id,
                revision: job.revision
            })
            .await
            .is_err()
    );
    admit_project_secrets(&f, &job.source_session)?;
    admit_project_secrets(&f, branch.session_id.as_ref().unwrap())?;
    for (input, revision, track, session, effort) in [
        (
            &job.request.input_id,
            job.request.revision,
            &job.source_track,
            &job.source_session,
            "minimal",
        ),
        (
            &job.child_input.to_string(),
            1,
            &branch.track_id,
            branch.session_id.as_ref().unwrap(),
            "xhigh",
        ),
    ] {
        let params = turn_params(session, effort);
        let operation = Uuid::parse_str(params["commandId"].as_str().unwrap())?;
        let admitted = f
            .journal
            .reserve_turn(
                operation,
                input.clone(),
                revision,
                track.clone(),
                params.clone(),
                Some(two_ways::Variation::from_catalog(&models())?),
            )
            .await;
        if let Err(error) = admitted {
            return Err(format!(
                "admit {input}: {error}; word {}",
                f.journal.read_utterance(input.clone()).await?
            )
            .into());
        }
        assert_eq!(
            f.journal.operation(operation).await?.unwrap()["payload"],
            params
        );
    }
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM utterances WHERE id=?1",
            [job.child_input.to_string()],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}
#[tokio::test]
async fn low_weekly_budget_leaves_the_original_alone_and_unknown_usage_is_not_permission()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let (_, _, job) = pending(&f).await?;
    let captured = capture(&f, &job).await?;
    let declined = inspect(&f, &captured, usage(20, 81)).await?;
    assert_eq!(declined.phase, PairPhase::Declined);
    assert!(
        f.journal.snapshot().await?["branches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        f.journal
            .read_utterance(job.request.input_id.clone())
            .await?["twoWaysRunnable"],
        true
    );
    let again = f
        .journal
        .two_ways(PairChange::Prepare(two_ways::Request {
            op_id: Uuid::now_v7(),
            ..job.request.clone()
        }))
        .await?;
    let awaiting = inspect(
        &f,
        &capture(&f, &again).await?,
        serde_json::from_value(json!({}))?,
    )
    .await?;
    assert_eq!(awaiting.phase, PairPhase::AwaitingUsageConfirmation);
    assert!(awaiting.variation.is_none());
    assert!(
        f.journal.snapshot().await?["branches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let cancelled = f
        .journal
        .two_ways(PairChange::Command(crate::two_ways::Command {
            op_id: Uuid::now_v7(),
            id: awaiting.request.op_id,
            revision: awaiting.revision,
            action: crate::two_ways::Action::Cancel,
        }))
        .await?;
    assert_eq!(cancelled.phase, PairPhase::Cancelled);
    assert!(
        f.journal
            .two_ways(PairChange::Inspect {
                id: awaiting.request.op_id,
                revision: awaiting.revision,
                usage: usage(10, 10),
                models: models()
            })
            .await
            .is_err()
    );
    assert_eq!(
        f.journal
            .read_utterance(job.request.input_id.clone())
            .await?["twoWaysRunnable"],
        true
    );
    for (recheck, expected) in [
        (usage(20, 81), PairPhase::Declined),
        (serde_json::from_value(json!({}))?, PairPhase::Forking),
    ] {
        let request = f
            .journal
            .two_ways(PairChange::Prepare(two_ways::Request {
                op_id: Uuid::now_v7(),
                ..job.request.clone()
            }))
            .await?;
        let inspecting = capture(&f, &request).await?;
        let awaiting = inspect(&f, &inspecting, serde_json::from_value(json!({}))?).await?;
        assert!(inspect(&f, &awaiting, recheck.clone()).await.is_err());
        let command = two_ways::Command {
            op_id: Uuid::now_v7(),
            id: awaiting.request.op_id,
            revision: awaiting.revision,
            action: two_ways::Action::ConfirmUsage,
        };
        let confirmed = f
            .journal
            .two_ways(PairChange::Command(command.clone()))
            .await?;
        assert_eq!(confirmed.phase, PairPhase::InspectingConfirmedUsage);
        assert_eq!(
            f.journal
                .two_ways(PairChange::Command(command))
                .await?
                .revision,
            confirmed.revision
        );
        let result = inspect(&f, &confirmed, recheck).await?;
        assert_eq!(result.phase, expected);
        if expected == PairPhase::Forking {
            assert!(result.usage_observed_at.is_none());
            assert_eq!(result.variation, Some(request.request.selection.variation));
        }
    }
    Ok(())
}
#[tokio::test]
async fn a_changed_model_or_effort_cannot_create_a_mismatched_pair() -> Result<()> {
    let f = Fixture::new(1).await?;
    let (_, _, job) = pending(&f).await?;
    let (job, branch) = ready(&f, &job).await?;
    let published = f
        .journal
        .two_ways(PairChange::Publish {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    admit_project_secrets(&f, &job.source_session)?;
    let changed = two_ways::Variation {
        model_id: "changed-model".into(),
        ..two_ways::Variation::from_catalog(&models())?
    };
    let params = turn_params(&job.source_session, "minimal");
    let operation = Uuid::parse_str(params["commandId"].as_str().unwrap())?;
    assert!(
        f.journal
            .reserve_turn(
                operation,
                job.request.input_id.clone(),
                job.request.revision,
                job.source_track.clone(),
                params,
                Some(changed),
            )
            .await
            .is_err()
    );
    assert!(f.journal.operation(operation).await?.is_none());
    admit_project_secrets(&f, branch.session_id.as_ref().unwrap())?;
    let wrong = turn_params(branch.session_id.as_ref().unwrap(), "minimal");
    let operation = Uuid::parse_str(wrong["commandId"].as_str().unwrap())?;
    assert!(
        f.journal
            .reserve_turn(
                operation,
                job.child_input.to_string(),
                1,
                branch.track_id,
                wrong,
                Some(two_ways::Variation::from_catalog(&models())?),
            )
            .await
            .is_err()
    );
    assert!(f.journal.operation(operation).await?.is_none());
    assert_eq!(
        published.variation.unwrap().alternate,
        muse_code_sdk::msp::ReasoningEffort::Xhigh
    );
    Ok(())
}
#[tokio::test]
async fn a_stop_before_materialization_keeps_the_alternate_held_after_database_reopen() -> Result<()>
{
    let f = Fixture::new(1).await?;
    let (_, _, job) = pending(&f).await?;
    let (job, branch) = ready(&f, &job).await?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    crate::worker_hold::record(&db, &Uuid::now_v7().to_string(), &branch.track_id)?;
    let published = f
        .journal
        .two_ways(PairChange::Publish {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    let child = f
        .journal
        .read_utterance(job.child_input.to_string())
        .await?;
    assert_eq!(child["state"], "held");
    assert_eq!(child["dispatchRunnable"], false);
    let root = f.root.path().to_owned();
    let reread = Journal::open(
        root.join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    assert_eq!(
        reread
            .two_ways_job(job.request.op_id)
            .await?
            .unwrap()
            .revision,
        published.revision
    );
    assert_eq!(
        reread.read_utterance(job.child_input.to_string()).await?["state"],
        "held"
    );
    assert_eq!(
        reread.read_utterance(job.request.input_id).await?["state"],
        "placed"
    );
    Ok(())
}

#[tokio::test]
async fn a_later_source_checkpoint_cannot_be_substituted_for_the_paired_boundary() -> Result<()> {
    let f = Fixture::new(1).await?;
    let (mut source, _, job) = pending(&f).await?;
    let captured = capture(&f, &job).await?;
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own later bytes must remain current\n",
    )?;
    let point = f.save(0, source.checkpoint.clone()).await?;
    source.checkpoint = Some(point);
    f.journal.worker_state(source).await?;
    assert!(inspect(&f, &captured, usage(10, 10)).await.is_err());
    assert!(
        f.journal.snapshot().await?["branches"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fs::read_to_string(f.workspaces[0].path.join("parent.txt"))?,
        "Own later bytes must remain current\n"
    );
    assert_eq!(
        f.journal
            .two_ways_job(job.request.op_id)
            .await?
            .unwrap()
            .point
            .unwrap()
            .sha(),
        captured.point.unwrap().sha()
    );
    Ok(())
}

#[tokio::test]
async fn late_unsaved_files_are_rejected_without_resetting_either_worktree() -> Result<()> {
    let f = Fixture::new(1).await?;
    let (_, _, job) = pending(&f).await?;
    let (job, branch) = ready(&f, &job).await?;
    crate::two_ways_files::before_publish(&f.journal, &job, &branch.track_id).await?;
    f.journal
        .two_ways(PairChange::Publish {
            id: job.request.op_id,
            revision: job.revision,
        })
        .await?;
    fs::write(
        f.workspaces[0].path.join("parent.txt"),
        "Own unsaved IDE bytes\n",
    )?;
    let head = git(&f.workspaces[0].path, &["rev-parse", "HEAD"])?;
    assert!(
        crate::two_ways_files::before_publish(&f.journal, &job, &branch.track_id)
            .await
            .is_err()
    );
    assert_eq!(
        fs::read_to_string(f.workspaces[0].path.join("parent.txt"))?,
        "Own unsaved IDE bytes\n"
    );
    assert_eq!(git(&f.workspaces[0].path, &["rev-parse", "HEAD"])?, head);
    assert_eq!(
        fs::read_to_string(branch.workspace.path.join("parent.txt"))?,
        "Own initial base\n"
    );

    assert!(
        crate::two_ways_files::before_turn(&f.journal, &job.request.input_id, &job.source_track)
            .await
            .is_err()
    );
    assert!(
        crate::two_ways_files::before_turn(
            &f.journal,
            &job.child_input.to_string(),
            &branch.track_id
        )
        .await
        .is_ok()
    );
    Ok(())
}
