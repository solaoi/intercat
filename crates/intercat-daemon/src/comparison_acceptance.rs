//! Dummy-only SQLite/Git acceptance through the production comparison service.
use super::*;
use crate::comparison::{self, Action as CompareAction, Control, Phase};

pub(super) fn record_turn(f: &Fixture, index: usize, point: &Checkpoint) -> Result<()> {
    rusqlite::Connection::open(f.root.path().join("events.sqlite"))?.execute(
        "INSERT INTO timeline_turns VALUES(?1,?2,?3)",
        params![
            format!("own-comparison-{index}"),
            point.turn_id.to_string(),
            json!({"track":f.workspaces[index].track_id}).to_string()
        ],
    )?;
    Ok(())
}
async fn pair(f: &Fixture) -> Result<comparison::Open> {
    fs::write(f.workspaces[0].path.join("outcome.txt"), "base\n")?;
    let base = f.save(0, None).await?;
    record_turn(f, 0, &base)?;
    f.branch(&base)?;
    fs::write(f.workspaces[0].path.join("outcome.txt"), "parent\n")?;
    let parent = f.save(0, Some(base)).await?;
    record_turn(f, 0, &parent)?;
    fs::write(f.workspaces[1].path.join("outcome.txt"), "child\n")?;
    let child = f.save(1, None).await?;
    record_turn(f, 1, &child)?;
    Ok(comparison::Open {
        op_id: Uuid::now_v7(),
        parent: comparison::Point {
            track_id: f.workspaces[0].track_id.clone(),
            sha: parent.sha,
        },
        child: comparison::Point {
            track_id: f.workspaces[1].track_id.clone(),
            sha: child.sha,
        },
    })
}
pub(super) async fn command(f: &Fixture, text: &str) -> Result<()> {
    let snapshot = f.journal.snapshot().await?;
    let folder = snapshot["project"]["folders"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let current = f.journal.check_settings().await?;
    f.journal
        .change_check_settings(crate::check_settings::Change {
            op_id: Uuid::now_v7(),
            revision: current.revision,
            folder,
            command: crate::check_run::CheckCommand::Run(text.into()),
            history: false,
        })
        .await?;
    Ok(())
}
pub(super) async fn settled(f: &Fixture, id: Uuid) -> Result<comparison::Job> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let job = f
                .journal
                .comparison_job(id)
                .await?
                .ok_or("Missing own comparison.")?;
            if matches!(job.phase, Phase::Done | Phase::Held) {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}
#[tokio::test]
#[ignore = "Exports an owned SQLite/Git fixture for the Electron real-RPC acceptance test."]
async fn export_electron_comparison_fixture() -> Result<()> {
    let f = Fixture::new(2).await?;
    let request = pair(&f).await?;
    let env = f.root.path().join("own-check.env");
    fs::write(&env, "OWN_COMPARE_TOKEN=OwnDummyCompareOnly20261110\n")?;
    let environment = f.journal.check_environment().await?;
    f.journal
        .change_check_environment(crate::check_environment::Change {
            op_id: Uuid::now_v7(),
            revision: environment.revision,
            file: Some(env),
        })
        .await?;
    command(
        &f,
        "printf '%s\\n' \"$OWN_COMPARE_TOKEN\"; sleep 2; test \"$(cat outcome.txt)\" = parent",
    )
    .await?;
    let catalog = f.journal.fingerprint_catalog().await?;
    let manifest = json!({
        "data":f.root.path(),"source":f.source,"request":request,
        "workspaces":f.workspaces.iter().map(|workspace| &workspace.path).collect::<Vec<_>>(),
        "receipt":{"catalog":catalog.current,"proof":catalog.proof}
    });
    fs::write(
        f.root.path().join("fixture.json"),
        serde_json::to_vec(&manifest)?,
    )?;
    // The Electron owner removes this directory after stopping its own daemon.
    println!("OWN_COMPARISON_FIXTURE={}", f.root.keep().display());
    Ok(())
}
#[tokio::test]
async fn comparison_keeps_saved_points_masks_output_reuses_trees_and_never_selects_a_path()
-> Result<()> {
    let f = Fixture::new(2).await?;
    let request = pair(&f).await?;
    let env = f.root.path().join("own-check.env");
    fs::write(&env, "OWN_COMPARE_TOKEN=OwnDummyCompareOnly20261110\n")?;
    let environment = f.journal.check_environment().await?;
    f.journal
        .change_check_environment(crate::check_environment::Change {
            op_id: Uuid::now_v7(),
            revision: environment.revision,
            file: Some(env),
        })
        .await?;
    command(
        &f,
        "printf '%s' \"$OWN_COMPARE_TOKEN\"; sleep 0.2; test \"$(cat outcome.txt)\" = parent",
    )
    .await?;
    let service = crate::comparison_service::Service::new(
        f.journal.clone(),
        f.root.path().join("comparisons"),
    )
    .await?;
    service.open(request.clone()).await?;
    // Unsaved, ongoing work cannot alter either captured input or its diff.
    fs::write(
        f.workspaces[0].path.join("outcome.txt"),
        "Own continuing parent work\n",
    )?;
    fs::write(
        f.workspaces[1].path.join("outcome.txt"),
        "Own continuing child work\n",
    )?;
    let checked = settled(&f, request.op_id).await?;
    assert_eq!(checked.phase, Phase::Done, "{:?}", checked.sides);
    assert_eq!(
        checked.sides[0]
            .outcome
            .as_ref()
            .unwrap()
            .check
            .as_ref()
            .unwrap()
            .code,
        Some(0)
    );
    assert_eq!(
        checked.sides[1]
            .outcome
            .as_ref()
            .unwrap()
            .check
            .as_ref()
            .unwrap()
            .code,
        Some(1)
    );
    assert!(!serde_json::to_string(&checked)?.contains("OwnDummyCompareOnly20261110"));
    assert_eq!(checked.request, request);
    let diff = service
        .diff(comparison::DiffRequest {
            id: checked.id,
            basis: comparison::DiffBasis::Child,
            path: "outcome.txt".into(),
        })
        .await?;
    assert_eq!(diff.before, "base\n");
    assert_eq!(diff.after, "child\n");
    let evidence = service.evidence(checked.id).await?;
    assert_eq!(evidence.changes.len(), 1);
    assert_eq!(evidence.changes[0].path, "outcome.txt");
    let pair_diff = service
        .diff(comparison::DiffRequest {
            id: checked.id,
            basis: comparison::DiffBasis::Paths,
            path: "outcome.txt".into(),
        })
        .await?;
    assert_eq!(pair_diff.before, "parent\n");
    assert_eq!(pair_diff.after, "child\n");
    let replay = service.open(request.clone()).await?;
    assert_eq!(replay["id"], checked.id.to_string());
    let mut reused = request.clone();
    reused.op_id = Uuid::now_v7();
    service.open(reused.clone()).await?;
    let cached = settled(&f, reused.op_id).await?;
    assert_eq!(cached.phase, Phase::Done);
    assert_eq!(cached.attempt, 0);
    assert!(cached.sides.iter().all(|side| side.cached));
    command(&f, "test \"$(cat outcome.txt)\" = child").await?;
    let mut changed = request.clone();
    changed.op_id = Uuid::now_v7();
    service.open(changed.clone()).await?;
    let fresh = settled(&f, changed.op_id).await?;
    assert_ne!(fresh.settings_hash, checked.settings_hash);
    assert!(fresh.sides.iter().all(|side| !side.cached));
    assert_eq!(
        fresh.sides[1]
            .outcome
            .as_ref()
            .unwrap()
            .check
            .as_ref()
            .unwrap()
            .code,
        Some(0)
    );
    assert_eq!(
        service.read(fresh.id).await?["summary"]["label"],
        "Only A2 passes the checks · Compare the diffs"
    );
    // Equal trees with different checkpoint identities need only one actual Check.
    fs::write(f.workspaces[1].path.join("outcome.txt"), "parent\n")?;
    let child = f
        .save(1, Some(checked.sides[1].source.checkpoint.clone()))
        .await?;
    record_turn(&f, 1, &child)?;
    command(&f, "test -f outcome.txt").await?;
    let same = comparison::Open {
        op_id: Uuid::now_v7(),
        parent: request.parent.clone(),
        child: comparison::Point {
            track_id: request.child.track_id.clone(),
            sha: child.sha,
        },
    };
    service.open(same.clone()).await?;
    let equal = settled(&f, same.op_id).await?;
    assert_eq!(equal.sides[0].tree, equal.sides[1].tree);
    assert!(!equal.sides[0].cached);
    assert!(equal.sides[1].cached);
    assert_eq!(
        service.read(equal.id).await?["summary"]["label"],
        "The checks don’t tell them apart · Compare the diffs"
    );
    let mut wrong = request.clone();
    wrong.child.sha = wrong.parent.sha.clone();
    assert!(service.open(wrong).await.is_err());
    assert!(
        service
            .diff(comparison::DiffRequest {
                id: checked.id,
                basis: comparison::DiffBasis::Child,
                path: "../../outcome.txt".into()
            })
            .await
            .is_err()
    );
    let snapshot = f.journal.snapshot().await?;
    assert!(snapshot["pathRetirements"].as_array().unwrap().is_empty());
    assert!(snapshot["integrations"].as_array().unwrap().is_empty());
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    assert_eq!(
        fs::read_to_string(f.workspaces[0].path.join("outcome.txt"))?,
        "Own continuing parent work\n"
    );
    assert!(
        !service.read(checked.id).await?["folder"]
            .as_object()
            .unwrap()
            .contains_key("root")
    );
    service.shutdown().await;
    f.review.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn hold_keeps_a_completed_side_when_progress_passes_the_displayed_revision() -> Result<()> {
    let f = Fixture::new(2).await?;
    let request = pair(&f).await?;
    command(
        &f,
        "test \"$(cat outcome.txt)\" = parent && exit 0; touch own-child-started; sleep 30",
    )
    .await?;
    let service = crate::comparison_service::Service::new(
        f.journal.clone(),
        f.root.path().join("comparisons"),
    )
    .await?;
    let displayed = service.open(request.clone()).await?;
    let progressed = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let job = f.journal.comparison_job(request.op_id).await?.unwrap();
            let started = walkdir::WalkDir::new(f.root.path().join("comparisons"))
                .max_depth(5)
                .into_iter()
                .filter_map(std::result::Result::ok)
                .any(|entry| entry.file_name() == "own-child-started");
            if job.sides[0].outcome.is_some() && started {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    let hold = Control {
        op_id: Uuid::now_v7(),
        id: progressed.id,
        revision: displayed["revision"].as_u64().unwrap(),
        attempt: displayed["attempt"].as_u64().unwrap(),
        action: CompareAction::Hold,
    };
    assert!(progressed.revision > hold.revision);
    let mut future = hold.clone();
    future.revision = progressed.revision + 1;
    assert!(service.control(future).await.is_err());
    let held = service.control(hold).await?;
    assert_eq!(held["phase"], "held");
    assert_eq!(held["sides"][0]["outcome"]["check"]["code"], 0);
    assert!(held["sides"][1]["outcome"].is_null());
    assert_eq!(held["attempt"], progressed.attempt);
    service.shutdown().await;
    f.review.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn held_comparison_ignores_late_results_and_restart_requires_explicit_retry() -> Result<()> {
    let f = Fixture::new(2).await?;
    let request = pair(&f).await?;
    command(&f, "touch own-check-started; sleep 30; test -f outcome.txt").await?;
    let service = crate::comparison_service::Service::new(
        f.journal.clone(),
        f.root.path().join("comparisons"),
    )
    .await?;
    let opened = service.open(request.clone()).await?;
    let running = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let job = f.journal.comparison_job(request.op_id).await?.unwrap();
            if job.phase == Phase::Running {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if walkdir::WalkDir::new(f.root.path().join("comparisons"))
                .max_depth(5)
                .into_iter()
                .filter_map(std::result::Result::ok)
                .any(|entry| entry.file_name() == "own-check-started")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let control = Control {
        op_id: Uuid::now_v7(),
        id: running.id,
        revision: opened["revision"].as_u64().unwrap(),
        attempt: opened["attempt"].as_u64().unwrap(),
        action: CompareAction::Hold,
    };
    service.control(control.clone()).await?;
    let held = f.journal.comparison_job(running.id).await?.unwrap();
    assert_eq!(held.phase, Phase::Held);
    assert!(held.sides.iter().all(|side| side.outcome.is_none()));
    service.control(control.clone()).await?;
    service.shutdown().await;
    // Reproduce a daemon exiting after durable Begin, before an outcome receipt.
    let ready = f
        .journal
        .control_comparison(Control {
            op_id: Uuid::now_v7(),
            id: held.id,
            revision: held.revision,
            attempt: held.attempt,
            action: CompareAction::Retry,
        })
        .await?;
    let interrupted = f
        .journal
        .change_comparison(crate::comparison_store::Change::Begin {
            id: ready.id,
            revision: ready.revision,
        })
        .await?;
    let restarted = crate::comparison_service::Service::new(
        f.journal.clone(),
        f.root.path().join("comparisons"),
    )
    .await?;
    assert_eq!(
        f.journal
            .comparison_job(interrupted.id)
            .await?
            .unwrap()
            .phase,
        Phase::Held
    );
    let before = f.journal.comparison_job(interrupted.id).await?.unwrap();
    // An old executor cannot publish a result or populate the cache after Hold.
    let late = crate::comparison::Outcome {
        state: crate::check_run::State::Passed,
        check: Some(crate::check_run::Record {
            tree: before.sides[0].tree.clone(),
            settings_hash: before.sides[0].settings_hash.clone(),
            command: Some("own synthetic late receipt".into()),
            code: Some(0),
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
        }),
        reason: None,
        dependency_runs: vec![],
        dependency_error: None,
    };
    let unchanged = f
        .journal
        .change_comparison(crate::comparison_store::Change::Finish {
            id: before.id,
            attempt: interrupted.attempt,
            side: 0,
            outcome: late,
        })
        .await?;
    assert_eq!(unchanged.revision, before.revision);
    assert!(unchanged.sides[0].outcome.is_none());
    assert_eq!(restarted.open(request).await?["phase"], "held");
    // The saved command and settings stay fixed after settings are changed.
    command(&f, "true").await?;
    assert_eq!(
        f.journal
            .comparison_job(before.id)
            .await?
            .unwrap()
            .settings_hash,
        before.settings_hash
    );
    let retry = Control {
        op_id: Uuid::now_v7(),
        id: before.id,
        revision: before.revision,
        attempt: before.attempt,
        action: CompareAction::Retry,
    };
    restarted.control(retry.clone()).await?;
    let retried = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let job = f.journal.comparison_job(before.id).await?.unwrap();
            if job.phase == Phase::Running {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(restarted.control(retry).await?["phase"], "running");
    assert_eq!(
        restarted.control(control.clone()).await?["phase"],
        "running"
    );
    let mut delayed_hold = control;
    delayed_hold.op_id = Uuid::now_v7();
    assert!(restarted.control(delayed_hold).await.is_err());
    assert_eq!(
        f.journal.comparison_job(before.id).await?.unwrap().attempt,
        retried.attempt
    );
    restarted.shutdown().await;
    f.review.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn history_sensitive_checks_keep_equal_trees_with_different_heads_separate() -> Result<()> {
    let f = Fixture::new(2).await?;
    let mut request = pair(&f).await?;
    fs::write(f.workspaces[1].path.join("outcome.txt"), "parent\n")?;
    let previous = f.journal.capture_comparison(request.clone()).await?.sources[1]
        .checkpoint
        .clone();
    let child = f.save(1, Some(previous)).await?;
    record_turn(&f, 1, &child)?;
    request.child.sha = child.sha;
    let folder = f.journal.snapshot().await?["project"]["folders"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let tool = crate::git_tool::GitTool::system().await?;
    let binary = shlex::try_quote(tool.binary().to_str().ok_or("Own Git path is not UTF-8.")?)
        .map_err(|_| "Own Git path cannot be quoted.")?;
    f.journal
        .change_check_settings(crate::check_settings::Change {
            op_id: Uuid::now_v7(),
            revision: 0,
            folder,
            command: crate::check_run::CheckCommand::Run(format!(
                "{binary} rev-parse HEAD && {binary} tag --list"
            )),
            history: true,
        })
        .await?;
    let service = crate::comparison_service::Service::new(
        f.journal.clone(),
        f.root.path().join("comparisons"),
    )
    .await?;
    service.open(request.clone()).await?;
    let checked = settled(&f, request.op_id).await?;
    assert_eq!(checked.phase, Phase::Done, "{:?}", checked.sides);
    assert_eq!(checked.sides[0].tree, checked.sides[1].tree);
    assert_ne!(
        checked.sides[0].settings_hash,
        checked.sides[1].settings_hash
    );
    for side in &checked.sides {
        assert!(!side.cached);
        assert_eq!(
            side.outcome
                .as_ref()
                .unwrap()
                .check
                .as_ref()
                .unwrap()
                .stdout
                .trim(),
            side.source.point.sha
        );
    }
    git(&f.source, &["tag", "own-test-tag", &request.parent.sha])?;
    request.op_id = Uuid::now_v7();
    service.open(request.clone()).await?;
    let cached = settled(&f, request.op_id).await?;
    assert!(!cached.reuse_checks);
    assert!(cached.sides.iter().all(|side| !side.cached));
    for side in &cached.sides {
        assert!(
            side.outcome
                .as_ref()
                .unwrap()
                .check
                .as_ref()
                .unwrap()
                .stdout
                .contains("own-test-tag")
        );
    }
    service.shutdown().await;
    f.review.shutdown().await;
    Ok(())
}
