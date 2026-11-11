//! Opt-in acceptance on the real provider, using only the fixture's files.
use super::*;
use crate::{comparison, fresh, two_ways};
use std::fs;

const FILE: &str = "own-pair.js";
const CONTENT: &str = "export const value = 2;\n";

async fn saved_worker(fixture: &QueueFixture, track: &str, input: &str) -> Result<WorkerState> {
    tokio::time::timeout(Duration::from_secs(100), async {
        loop {
            let state = fixture.journal.snapshot().await?;
            if let Some(worker) = state["workers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|value| value["trackId"] == track)
            {
                let worker: WorkerState = serde_json::from_value(worker.clone())?;
                if matches!(
                    worker.phase.as_str(),
                    "failed" | "lost" | "needs-recovery" | "held"
                ) {
                    return Err(format!(
                        "Dummy worker {track} stopped in {}: {:?}",
                        worker.phase, worker.detail
                    )
                    .into());
                }
                if worker.phase == "ready"
                    && worker.checkpoint.is_some()
                    && !worker.checkpoint_pending
                    && worker.active_turn.is_none()
                    && fixture.journal.read_utterance(input.into()).await?["state"] == "landed"
                {
                    return Ok(worker);
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?
}

async fn continue_path(
    fixture: &mut QueueFixture,
    worker: WorkerState,
) -> Result<comparison::Point> {
    let original_workspace = fixture.journal.workspace(worker.track_id.clone()).await?;
    let session = worker.session_id.ok_or("Missing dummy session")?;
    let mut job = fixture
        .service
        .prepare_fresh(fresh::Request {
            op_id: Uuid::now_v7(),
            track_id: worker.track_id,
            session_id: session.clone(),
        })
        .await?;
    let job = tokio::time::timeout(Duration::from_secs(150), async {
        loop {
            match job.phase {
                fresh::Phase::Transferred => {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
                }
                fresh::Phase::NeedsAccess => {
                    let inspection = job
                        .inspection
                        .as_ref()
                        .ok_or("Missing dummy access inspection")?;
                    if !inspection.processes.is_empty() {
                        return Err("Unexpected running process in the dummy worktree".into());
                    }
                    job = fixture
                        .service
                        .fresh_command(fresh::Command {
                            op_id: Uuid::now_v7(),
                            id: job.request.op_id,
                            revision: job.revision,
                            action: fresh::Action::AllowAccess {
                                fingerprint: inspection.fingerprint.clone(),
                            },
                        })
                        .await?;
                }
                fresh::Phase::FilesHeld | fresh::Phase::OwnerHeld | fresh::Phase::SummaryHeld => {
                    return Err(format!("Dummy continuation held: {:?}", job.error).into());
                }
                _ => match fixture.service.advance_fresh(job.clone()).await {
                    Ok(next) => job = next,
                    Err(error) if error.is::<fresh::Waiting>() => {}
                    Err(error) => return Err(error),
                },
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;
    assert_ne!(job.successor_session, session);
    assert!(job.summary.is_some());
    assert_eq!(
        fixture
            .journal
            .workspace(job.successor_track.clone())
            .await?
            .path,
        original_workspace.path
    );
    let db = rusqlite::Connection::open(fixture.root.path().join("db"))?;
    let first = crate::continuation::start(&db, &job.successor_track)?
        .ok_or("Missing handoff instruction")?;
    assert!(!first.ready);
    fixture.launch_worker(&job.successor_track, false).await?;
    let worker = saved_worker(fixture, &job.successor_track, &first.first_input).await?;
    assert_eq!(
        worker.session_id.as_deref(),
        Some(job.successor_session.as_str())
    );
    assert!(
        crate::continuation::start(&db, &job.successor_track)?
            .unwrap()
            .ready
    );
    assert_eq!(
        fs::read_to_string(original_workspace.path.join(FILE))?,
        CONTENT
    );
    Ok(comparison::Point {
        track_id: worker.track_id,
        sha: worker.checkpoint.unwrap().sha,
    })
}

#[tokio::test]
#[ignore = "opt-in real Meta provider; only agent-created dummy files, at most six model turns"]
async fn native_paid_pair_compares_both_real_continuations_at_the_original_branch_point()
-> Result<()> {
    let output = PathBuf::from(std::env::var("INTERCAT_M4_PAID_PAIR_OUTPUT")?);
    fs::create_dir(&output)?;
    let mut fixture = QueueFixture::with_initial_instruction(concat!(
        "This is a disposable test. In the current dummy worktree create own-pair.js ",
        "containing exactly one line: export const value = 2; followed by a newline. ",
        "Use only the file editing tools in this worktree. Do not modify any other file, ",
        "run shell commands, access the network, or ask a question. Finish with a short report."
    ))
    .await?;
    fixture.config.credentials =
        PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?).canonicalize()?;
    fixture.service.host_memory = fixture.memory.clone();
    fixture
        .journal
        .set_worker_config(Uuid::now_v7(), fixture.config.clone())
        .await?;
    let (input, source) = fixture.words[0].clone();
    let reports =
        crate::report_analysis::Service::start(fixture.journal.clone(), fixture.editor.clone());
    let driver = crate::two_ways_driver::Driver::start(
        fixture.service.clone(),
        fixture.journal.clone(),
        fixture.stop.subscribe(),
    );
    let result = async {
        fixture.launch_worker(&source, false).await?;
        fixture.wait_workers(&[0], false).await?;
        let word = fixture.journal.read_utterance(input.clone()).await?;
        let options = fixture.service.two_ways_options(two_ways::OptionsQuery {
            input_id: input.clone(), revision: word["revision"].as_i64().unwrap(),
        }).await?;
        assert_eq!(options.selection.variation.provider_id, "meta");
        let pending = fixture.journal.two_ways(Change::Prepare(Request {
            op_id: Uuid::now_v7(), input_id: input.clone(), revision: options.revision, selection: options.selection,
        })).await?;
        start_at_worker_boundary(&fixture.service, &source).await?;
        let mut usage_confirmed = false;
        let pair = tokio::time::timeout(Duration::from_secs(80), async {
            loop {
                let current = fixture.journal.two_ways_job(pending.request.op_id).await?.ok_or("Missing dummy paired request")?;
                match current.phase {
                    Phase::AwaitingUsageConfirmation => {
                        assert!(!usage_confirmed);
                        assert!(fixture.journal.snapshot().await?["branches"].as_array().unwrap().is_empty());
                        fixture.journal.two_ways(Change::Command(two_ways::Command {
                            op_id: Uuid::now_v7(), id: current.request.op_id, revision: current.revision, action: two_ways::Action::ConfirmUsage,
                        })).await?;
                        usage_confirmed = true;
                    }
                    Phase::AwaitingReceipt => {
                        let catalog = fixture.journal.fingerprint_catalog().await?;
                        if catalog.sealed.as_ref() != Some(&catalog.current) {
                            fixture.journal.confirm_fingerprints(catalog.current).await?;
                        }
                    }
                    Phase::Queued => return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(current),
                    Phase::Held | Phase::Declined => return Err(format!("Dummy pair {:?}: {:?}", current.phase, current.error).into()),
                    _ => {}
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }).await??;
        let branch: crate::branch::Branch = serde_json::from_value(fixture.journal.snapshot().await?["branches"][0].clone())?;
        assert_eq!(branch.request.op_id, pair.branch_id);
        fixture.launch_worker(&branch.track_id, false).await?;
        let parent = saved_worker(&fixture, &source, &input).await?;
        let child = saved_worker(&fixture, &branch.track_id, &pair.child_input.to_string()).await?;
        let variation = pair.variation.as_ref().unwrap();
        let state = fixture.journal.snapshot().await?;
        let db = rusqlite::Connection::open(fixture.root.path().join("db"))?;
        let child_input = pair.child_input.to_string();
        assert_eq!(fixture.journal.read_utterance(input.clone()).await?["text"], fixture.journal.read_utterance(child_input.clone()).await?["text"]);
        for (worker, word, effort) in [(&parent, &input, variation.primary), (&child, &child_input, variation.alternate)] {
            let raw: String = db.query_row("SELECT o.payload FROM operations o JOIN utterances u ON u.dispatch_command=o.id WHERE u.id=?1 AND o.kind='turn.start'", [word], |row| row.get(0))?;
            let payload: Value = serde_json::from_str(&raw)?;
            assert_eq!(payload["reasoningEffort"], serde_json::to_value(effort)?);
            assert_eq!(payload["sessionId"].as_str(), worker.session_id.as_deref());
            let workspace = fixture.journal.workspace(worker.track_id.clone()).await?;
            assert_eq!(fs::read_to_string(workspace.path.join(FILE))?, CONTENT);
        }
        let parent = continue_path(&mut fixture, parent).await?;
        let child = continue_path(&mut fixture, child).await?;
        let settings = fixture.journal.check_settings().await?;
        fixture.journal.change_check_settings(crate::check_settings::Change {
            op_id: Uuid::now_v7(), revision: settings.revision,
            folder: state["project"]["folders"][0]["id"].as_str().unwrap().into(),
            command: crate::check_run::CheckCommand::Run("grep -Fqx 'export const value = 2;' own-pair.js".into()), history: false,
        }).await?;
        let comparison = crate::comparison_service::Service::new(fixture.journal.clone(), fixture.root.path().join("comparisons")).await?;
        let id = Uuid::now_v7();
        let checked = async {
            comparison.open(comparison::Open { op_id: id, parent, child }).await?;
            tokio::time::timeout(Duration::from_secs(45), async {
                loop {
                    let job = fixture.journal.comparison_job(id).await?.ok_or("Missing dummy comparison")?;
                    if job.phase == comparison::Phase::Done { return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job); }
                    if job.phase == comparison::Phase::Held { return Err("Dummy comparison held".into()); }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }).await?
        }.await;
        comparison.shutdown().await;
        let checked = checked?;
        assert_eq!(checked.base, pair.point.unwrap());
        for (side, basis) in checked.sides.iter().zip([comparison::DiffBasis::Parent, comparison::DiffBasis::Child]) {
            assert_eq!(side.outcome.as_ref().unwrap().check.as_ref().unwrap().code, Some(0));
            let diff = comparison.diff(comparison::DiffRequest { id, basis, path: FILE.into() }).await?;
            assert!(diff.before.is_empty());
            assert_eq!(diff.after, CONTENT);
        }
        let db = rusqlite::Connection::open(fixture.root.path().join("db"))?;
        let starts: u32 = db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'", [], |row| row.get(0))?;
        let summaries: u32 = db.query_row("SELECT count(*) FROM operations WHERE kind='handoff.summary'", [], |row| row.get(0))?;
        assert_eq!((starts, summaries), (4, 2));
        assert_eq!(native_git(&fixture.root.path().join("source"), &["rev-parse", "HEAD"])?, fixture.source_head);
        assert!(!fixture.root.path().join("source").join(FILE).exists());
        fs::write(output.join("result.json"), serde_json::to_vec_pretty(&json!({
            "verified":true,"provider":"meta","usageConfirmed":usage_confirmed,"workerTurns":starts,
            "summaryTurns":summaries,"originalBranchPoint":checked.base,"comparison":checked.project()?,
        }))?)?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }.await;
    let failure = async {
        if let Err(error) = &result {
            let db = rusqlite::Connection::open(fixture.root.path().join("db"))?;
            let rejections = db.prepare("SELECT json_extract(payload,'$.code') FROM events WHERE kind='worker.startRejected'")?
                .query_map([], |row| row.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let reservations = db.prepare("SELECT state, json_extract(payload,'$.reasoningEffort') FROM operations WHERE kind='turn.start'")?
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            let observed_turns: u32 = db.query_row("SELECT count(*) FROM events WHERE kind='msp.applied' AND json_extract(payload,'$.method')='turn/started'", [], |row| row.get(0))?;
            fs::write(output.join("failure.json"), serde_json::to_vec_pretty(&json!({
                "error": error.to_string(),
                "state": fixture.journal.snapshot().await?,
                "startRejections": rejections,
                "startReservations": reservations,
                "observedTurns": observed_turns,
            }))?)?;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }.await;
    driver.shutdown().await;
    let joined = fixture.stop_hosts().await;
    reports.shutdown().await;
    fixture.editor.stop().await;
    failure?;
    result?;
    joined
}
