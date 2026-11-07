use intercat_daemon::{
    Result,
    checkpoint::Checkpoint,
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    preparation::Workspace,
    proposals::{Action, Command},
    quiet_proposals,
    runtime::WorkerState,
    secrets::Key,
};
use serde_json::json;
use std::{path::Path, process::Command as ProcessCommand, time::Duration};
use uuid::Uuid;

fn git(root: &Path, args: &[&str]) -> String {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn at_offset(offset: i64) -> Stamp {
    let mut at = Stamp::now();
    at.epoch_ms += offset;
    at.mono_ms += offset;
    at.timeline_ms += offset;
    at
}
async fn fixture(
    change: bool,
    failed: bool,
) -> Result<(tempfile::TempDir, Journal, WorkerState, String)> {
    let root = tempfile::tempdir()?;
    let repo = root.path().join("repo");
    std::fs::create_dir(&repo)?;
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.name", "Dummy"]);
    git(&repo, &["config", "user.email", "dummy@example.invalid"]);
    std::fs::write(repo.join("value.txt"), "one\n")?;
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "Dummy baseline"]);
    let baseline = git(&repo, &["rev-parse", "HEAD"]);
    if change {
        std::fs::write(repo.join("value.txt"), "two\n")?;
        git(&repo, &["add", "."]);
    }
    git(
        &repo,
        &["commit", "--allow-empty", "-qm", "Dummy checkpoint"],
    );
    let checkpoint = git(&repo, &["rev-parse", "HEAD"]);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Dummy quiet work".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let track = journal.snapshot().await?["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    journal
        .workspace_result(Workspace {
            track_id: track.clone(),
            path: repo,
            reference: "refs/intercat/dummy".into(),
            base: baseline.clone(),
            state: "ready".into(),
            error: None,
            attachment_roots: vec![],
        })
        .await?;
    let turn = Uuid::now_v7();
    let mut worker = WorkerState {
        track_id: track,
        phase: "working".into(),
        session_id: Some("dummy-session".into()),
        active_turn: Some(turn),
        secret_revision: None,
        identity: None,
        detail: None,
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    journal.worker_state(worker.clone()).await?;
    for frame in [
        json!({"method":"item/completed","params":{"sessionId":"dummy-session","item":{"itemId":"report","turnId":turn,"kind":"agentMessage","messagePhase":"finalAnswer","revision":1,"status":"completed","text":if failed {"Result: failed — dummy incomplete work"} else {"Dummy report without a fixed result."}}}}),
        json!({"method":"turn/completed","params":{"sessionId":"dummy-session","turnId":turn,"terminal":"completed"}}),
    ] {
        journal
            .try_msp(
                worker.track_id.clone(),
                "dummy-session".into(),
                Stamp::now(),
                frame,
                None,
                None,
            )?
            .await??;
    }
    worker.active_turn = None;
    worker.phase = "ready".into();
    worker.checkpoint = Some(Checkpoint {
        turn_id: turn,
        sha: checkpoint,
        reference: "refs/intercat/dummy-checkpoint".into(),
        unmerged_paths: vec![],
        live_processes: false,
    });
    let op = Uuid::now_v7().to_string();
    journal
        .intent(
            &op,
            "checkpoint.save",
            json!({"trackId":worker.track_id,"checkpoint":worker.checkpoint}),
        )
        .await?;
    journal.finish(&op, "dispatched").await?;
    journal.finish(&op, "complete").await?;
    journal.worker_state(worker.clone()).await?;
    assert!(journal.snapshot().await?["proposal"].is_null());
    Ok((root, journal, worker, baseline))
}
async fn ready(journal: &Journal) -> Result<quiet_proposals::Ticket> {
    journal.check_quiet_proposal(at_offset(-301_000)).await?;
    Ok(journal.check_quiet_proposal(Stamp::now()).await?.unwrap())
}
async fn inspect(journal: &Journal, ticket: quiet_proposals::Ticket) -> Result<bool> {
    let changed = quiet_proposals::inspect(&ticket)?;
    journal
        .finish_quiet_proposal(ticket, Some(changed.clone()), Stamp::now())
        .await?;
    Ok(!changed.paths.is_empty())
}

#[tokio::test]
async fn five_minutes_and_real_tree_changes_are_both_required() -> Result<()> {
    let (_root, journal, _, _) = fixture(true, false).await?;
    let now = Stamp::now();
    assert!(journal.check_quiet_proposal(now).await?.is_none());
    let mut wall_jump = Stamp::now();
    wall_jump.epoch_ms += 3_600_000;
    assert!(journal.check_quiet_proposal(wall_jump).await?.is_none());
    assert!(
        journal
            .check_quiet_proposal(at_offset(299_000))
            .await?
            .is_none()
    );
    assert!(
        journal
            .check_quiet_proposal(at_offset(301_000))
            .await?
            .is_some()
    );
    assert!(
        journal.snapshot().await?["proposal"].is_null(),
        "no observation or actual elapsed interval"
    );
    let (_root, journal, _, _) = fixture(false, false).await?;
    assert!(!inspect(&journal, ready(&journal).await?).await?);
    assert!(
        journal.snapshot().await?["proposal"].is_null(),
        "new commit ID with the same tree is not a change"
    );
    Ok(())
}

#[tokio::test]
async fn background_observation_offers_review_without_claiming_completion() -> Result<()> {
    let (_root, journal, worker, _) = fixture(true, false).await?;
    journal.check_quiet_proposal(at_offset(-301_000)).await?;
    let service = quiet_proposals::Service::start(journal.clone());
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let proposal = journal.snapshot().await?["proposal"].clone();
            if !proposal.is_null() {
                break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(proposal);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    service.stop().await;
    let proposal = result??;
    assert_eq!(proposal["kind"], "quiet-changes");
    assert_eq!(
        proposal["source"]["checkpoint"],
        worker.checkpoint.unwrap().sha
    );
    assert!(!proposal["text"].as_str().unwrap().contains("is done"));
    let response = journal
        .answer_proposal(Command {
            op_id: Uuid::now_v7(),
            proposal_id: proposal["id"].as_str().unwrap().parse()?,
            action: Action::Commit,
        })
        .await?;
    let word = journal
        .read_utterance(response["inputId"].as_str().unwrap().into())
        .await?;
    assert_eq!(word["presentation"]["command"], "commit");
    assert!(journal.snapshot().await?["integration"].is_null());
    Ok(())
}

#[tokio::test]
async fn new_activity_and_work_invalidate_old_observations_and_answers() -> Result<()> {
    let (root, journal, mut worker, _) = fixture(true, false).await?;
    let ticket = ready(&journal).await?;
    inspect(&journal, ticket.clone()).await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let repair = intercat_daemon::repair_run::Run {
        id: Uuid::now_v7(),
        revision: 1,
        job_id: Uuid::now_v7(),
        job_revision: 1,
        track: worker.track_id.clone(),
        purpose: intercat_daemon::repair_run::Purpose::Check,
        phase: intercat_daemon::repair_run::Phase::Held,
        created_at: Stamp::now().epoch_ms,
        host: None,
        host_nonce: None,
        secret_revision: None,
        secret_input: None,
        checkpoint: None,
        session: None,
        session_ready: false,
        workspace_ready: false,
        start_command: None,
        turn: None,
        failure: None,
        approvals: vec![],
        prompts: vec![],
        output: None,
        instruction: None,
        selection: None,
        refresh: None,
        restoring: None,
    };
    db.execute(
        "INSERT INTO repair_runs VALUES(?1,?2,1,?3)",
        rusqlite::params![
            repair.id.to_string(),
            repair.job_id.to_string(),
            serde_json::to_string(&repair)?
        ],
    )?;
    assert!(journal.check_quiet_proposal(Stamp::now()).await?.is_none());
    assert!(
        journal
            .finish_quiet_proposal(
                ticket.clone(),
                Some(quiet_proposals::Changes {
                    repository: std::path::PathBuf::new(),
                    paths: vec!["value.txt".into()]
                }),
                Stamp::now()
            )
            .await?
    );
    assert!(journal.snapshot().await?["proposal"].is_null());
    db.execute(
        "DELETE FROM repair_runs WHERE id=?1",
        [repair.id.to_string()],
    )?;
    journal.append("draft.saved", json!({"dummy":true})).await?;
    assert!(journal.snapshot().await?["proposal"].is_null());
    assert!(
        !journal
            .finish_quiet_proposal(
                ticket,
                Some(quiet_proposals::Changes {
                    repository: std::path::PathBuf::new(),
                    paths: vec!["value.txt".into()]
                }),
                Stamp::now()
            )
            .await?
    );
    assert!(
        journal
            .answer_proposal(Command {
                op_id: Uuid::now_v7(),
                proposal_id: proposal["id"].as_str().unwrap().parse()?,
                action: Action::Commit
            })
            .await
            .is_err()
    );
    ready(&journal).await?;
    worker.active_turn = Some(Uuid::now_v7());
    worker.phase = "working".into();
    journal.worker_state(worker).await?;
    journal.check_quiet_proposal(at_offset(-301_000)).await?;
    if let Some(ticket) = journal.check_quiet_proposal(Stamp::now()).await? {
        inspect(&journal, ticket).await?;
    }
    assert!(journal.snapshot().await?["proposal"].is_null());
    Ok(())
}

#[tokio::test]
async fn not_now_stays_answered_for_the_same_saved_changes() -> Result<()> {
    let (_root, journal, _, _) = fixture(true, false).await?;
    inspect(&journal, ready(&journal).await?).await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    let command = Command {
        op_id: Uuid::now_v7(),
        proposal_id: proposal["id"].as_str().unwrap().parse()?,
        action: Action::Later,
    };
    let response = journal.answer_proposal(command.clone()).await?;
    assert_eq!(journal.answer_proposal(command).await?, response);
    inspect(&journal, ready(&journal).await?).await?;
    assert!(journal.snapshot().await?["proposal"].is_null());
    Ok(())
}

#[tokio::test]
async fn compare_against_the_last_integrated_checkpoint_and_never_recommend_failed_work()
-> Result<()> {
    let (_root, journal, worker, _) = fixture(true, false).await?;
    let old = ready(&journal).await?;
    journal.append("integration.published",json!({"status":"done","tracks":[{"id":worker.track_id,"checkpoint":worker.checkpoint}],"candidate":{"commits":[{"track":worker.track_id}]}})).await?;
    assert!(
        !journal
            .finish_quiet_proposal(
                old,
                Some(quiet_proposals::Changes {
                    repository: std::path::PathBuf::new(),
                    paths: vec!["value.txt".into()]
                }),
                Stamp::now()
            )
            .await?
    );
    assert!(!inspect(&journal, ready(&journal).await?).await?);
    assert!(journal.snapshot().await?["proposal"].is_null());
    let (_root, journal, _, _) = fixture(true, true).await?;
    journal.check_quiet_proposal(at_offset(-301_000)).await?;
    assert!(journal.check_quiet_proposal(Stamp::now()).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn git_read_failure_is_not_a_change_and_retries_after_thirty_seconds() -> Result<()> {
    let (_root, journal, _, _) = fixture(true, false).await?;
    let ticket = ready(&journal).await?;
    let mut missing = ticket.clone();
    missing.path = missing.path.join("missing-dummy-repository");
    assert!(quiet_proposals::inspect(&missing).is_err());
    assert!(
        journal
            .finish_quiet_proposal(ticket.clone(), None, Stamp::now())
            .await?
    );
    assert!(journal.snapshot().await?["proposal"].is_null());
    assert!(journal.check_quiet_proposal(Stamp::now()).await?.is_none());
    assert_eq!(
        journal.check_quiet_proposal(at_offset(31_000)).await?,
        Some(ticket)
    );
    Ok(())
}

#[tokio::test]
async fn a_restarted_clock_waits_for_a_new_quiet_interval() -> Result<()> {
    let (_root, journal, _, _) = fixture(true, false).await?;
    let mut before = at_offset(-301_000);
    before.boot_id = "previous-daemon".into();
    journal.check_quiet_proposal(before).await?;
    assert!(journal.check_quiet_proposal(Stamp::now()).await?.is_none());
    assert!(
        journal
            .check_quiet_proposal(at_offset(301_000))
            .await?
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn another_silent_running_track_does_not_hide_saved_work() -> Result<()> {
    let (_root, journal, worker, _) = fixture(true, false).await?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Another dummy task".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let status = journal.snapshot().await?;
    let other = status["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] != worker.track_id)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut running = worker.clone();
    running.track_id = other;
    running.session_id = Some("another-dummy-session".into());
    running.active_turn = Some(Uuid::now_v7());
    running.phase = "working".into();
    running.checkpoint = None;
    journal.worker_state(running).await?;
    let ticket = ready(&journal).await?;
    assert_eq!(ticket.track_id, worker.track_id);
    assert!(inspect(&journal, ticket).await?);
    assert_eq!(
        journal.snapshot().await?["proposal"]["trackId"],
        worker.track_id
    );
    Ok(())
}
async fn second_saved_track(
    root: &Path,
    journal: &Journal,
    first: &WorkerState,
    baseline: &str,
    shared: bool,
    same_repository: bool,
) -> Result<WorkerState> {
    let repo = root.join("other-repo");
    if same_repository {
        git(
            &root.join("repo"),
            &[
                "worktree",
                "add",
                "--detach",
                repo.to_str().unwrap(),
                "HEAD",
            ],
        );
    } else {
        git(
            root,
            &[
                "clone",
                "-q",
                root.join("repo").to_str().unwrap(),
                repo.to_str().unwrap(),
            ],
        );
    }
    git(&repo, &["config", "user.name", "Dummy"]);
    git(&repo, &["config", "user.email", "dummy@example.invalid"]);
    if !shared {
        git(&repo, &["reset", "--hard", baseline]);
        std::fs::write(repo.join("other.txt"), "Independent dummy work\n")?;
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "Different dummy file"]);
    }
    let sha = git(&repo, &["rev-parse", "HEAD"]);
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Second dummy track".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let snapshot = journal.snapshot().await?;
    let track = snapshot["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|track| track["id"] != first.track_id)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    journal
        .workspace_result(Workspace {
            track_id: track.clone(),
            path: repo,
            reference: "refs/intercat/other".into(),
            base: baseline.into(),
            state: "ready".into(),
            error: None,
            attachment_roots: vec![],
        })
        .await?;
    let mut worker = first.clone();
    worker.track_id = track;
    worker.session_id = Some("second-dummy-session".into());
    worker.checkpoint = Some(Checkpoint {
        sha,
        turn_id: Uuid::now_v7(),
        reference: "refs/intercat/other-saved".into(),
        unmerged_paths: vec![],
        live_processes: false,
    });
    let op = Uuid::now_v7().to_string();
    journal
        .intent(
            &op,
            "checkpoint.save",
            json!({"trackId":worker.track_id,"checkpoint":worker.checkpoint}),
        )
        .await?;
    journal.finish(&op, "dispatched").await?;
    journal.finish(&op, "complete").await?;
    journal.worker_state(worker.clone()).await?;
    Ok(worker)
}

async fn inspect_all(journal: &Journal) -> Result<()> {
    journal.check_quiet_proposal(at_offset(-301_000)).await?;
    while let Some(ticket) = journal.check_quiet_proposal(Stamp::now()).await? {
        inspect(journal, ticket).await?;
    }
    Ok(())
}

#[tokio::test]
async fn quiet_overlap_proposal_keeps_both_saved_tracks_and_survives_restart() -> Result<()> {
    let (root, journal, first, baseline) = fixture(true, false).await?;
    let mut second =
        second_saved_track(root.path(), &journal, &first, &baseline, true, true).await?;
    // The running turn stays outside the proposal; its last saved turn can overlap.
    second.active_turn = Some(Uuid::now_v7());
    second.phase = "working".into();
    journal.worker_state(second.clone()).await?;
    let first_ticket = ready(&journal).await?;
    inspect(&journal, first_ticket).await?;
    assert!(journal.snapshot().await?["proposal"].is_null());
    let second_ticket = journal.check_quiet_proposal(Stamp::now()).await?.unwrap();
    assert_eq!(second_ticket.track_id, second.track_id);
    journal
        .finish_quiet_proposal(second_ticket, None, Stamp::now())
        .await?;
    assert!(journal.snapshot().await?["proposal"].is_null());
    let second_ticket = journal
        .check_quiet_proposal(at_offset(31_000))
        .await?
        .unwrap();
    inspect(&journal, second_ticket).await?;
    inspect_all(&journal).await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    assert_eq!(proposal["kind"], "work-overlap");
    assert!(proposal["detail"].as_str().unwrap().contains("value.txt"));
    assert_eq!(
        proposal["source"]["trackIds"],
        json!([first.track_id, second.track_id])
    );
    let action = Command {
        op_id: Uuid::now_v7(),
        proposal_id: proposal["id"].as_str().unwrap().parse()?,
        action: Action::Later,
    };
    journal.answer_proposal(action.clone()).await?;
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(ready(&journal).await?.track_id, first.track_id);
    inspect_all(&journal).await?;
    assert!(journal.snapshot().await?["proposal"].is_null());
    assert_eq!(journal.answer_proposal(action).await?["action"], "later");
    Ok(())
}

#[tokio::test]
async fn quiet_overlap_review_prepares_the_exact_group_and_stales_after_new_activity() -> Result<()>
{
    let (root, journal, first, baseline) = fixture(true, false).await?;
    let second = second_saved_track(root.path(), &journal, &first, &baseline, true, true).await?;
    inspect_all(&journal).await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    journal.append("draft.saved", json!({"dummy":true})).await?;
    assert!(
        journal
            .answer_proposal(Command {
                op_id: Uuid::now_v7(),
                proposal_id: proposal["id"].as_str().unwrap().parse()?,
                action: Action::Commit
            })
            .await
            .is_err()
    );
    inspect_all(&journal).await?;
    let proposal = journal.snapshot().await?["proposal"].clone();
    let response = journal
        .answer_proposal(Command {
            op_id: Uuid::now_v7(),
            proposal_id: proposal["id"].as_str().unwrap().parse()?,
            action: Action::Commit,
        })
        .await?;
    let word = journal
        .read_utterance(response["inputId"].as_str().unwrap().into())
        .await?;
    assert_eq!(
        word["presentation"]["commitTracks"],
        json!([first.track_id, second.track_id])
    );
    assert!(journal.snapshot().await?["integration"].is_null());
    journal.append("draft.saved", json!({"dummy":true})).await?;
    assert!(
        journal
            .answer_proposal(Command {
                op_id: Uuid::now_v7(),
                proposal_id: proposal["id"].as_str().unwrap().parse()?,
                action: Action::Commit
            })
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn quiet_independent_files_do_not_make_an_order_proposal() -> Result<()> {
    let (root, journal, first, baseline) = fixture(true, false).await?;
    second_saved_track(root.path(), &journal, &first, &baseline, false, true).await?;
    inspect_all(&journal).await?;
    assert_eq!(
        journal.snapshot().await?["proposal"]["kind"],
        "quiet-changes"
    );
    Ok(())
}

#[tokio::test]
async fn quiet_matching_names_in_different_repositories_are_not_overlaps() -> Result<()> {
    let (root, journal, first, baseline) = fixture(true, false).await?;
    second_saved_track(root.path(), &journal, &first, &baseline, true, false).await?;
    inspect_all(&journal).await?;
    assert_eq!(
        journal.snapshot().await?["proposal"]["kind"],
        "quiet-changes"
    );
    Ok(())
}
