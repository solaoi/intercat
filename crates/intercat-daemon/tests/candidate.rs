use intercat_daemon::{
    Result,
    candidate::{self, Settings, State},
    checkout::Configuration,
    integration::{self, Job, Status, Track},
    project,
};
use intercat_git::{ObjectId, Repository};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};
use tokio::sync::watch;
use uuid::Uuid;

fn git(path: &Path, args: &[&str], index: Option<&Path>) -> String {
    let mut command = Command::new("/usr/bin/git");
    command
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn executable(path: &Path, text: &str) -> Result<()> {
    fs::write(path, text)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

async fn repair_fixture(
    f: &mut Fixture,
    settings: &Settings,
    owner: usize,
) -> Result<(intercat_daemon::repair::Workspace, PathBuf)> {
    let candidate_root = f.root.path().join("failed-candidate");
    let candidate = candidate::build(
        &f.job,
        &candidate_root,
        &f.root.path().join("downloads"),
        settings,
        &BTreeMap::new(),
        watch::channel(false).1,
    )
    .await?;
    assert!(matches!(
        candidate.state,
        State::CheckFailed | State::SecretFound
    ));
    f.job.candidate = Some(candidate);
    f.job.status = Status::Failed;
    let repair = intercat_daemon::repair::Workspace::prepare(
        &f.job,
        &candidate_root,
        &f.root.path().join("fix"),
        &f.job.tracks[owner].id,
        settings,
        watch::channel(false).1,
    )
    .await?;
    Ok((repair, candidate_root))
}

async fn repair_run_fixture(secret: bool) -> Result<(Fixture, intercat_daemon::journal::Journal)> {
    let mut fixture = Fixture::new(&[]).await?;
    let content = if secret {
        concat!("token = ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H\n")
    } else {
        "A\n"
    };
    fixture.track("A", &[("file.txt", Some(content), false)])?;
    fixture.track("B", &[("other.txt", Some("B\n"), false)])?;
    let settings = fixture.settings(Some("test \"$(cat file.txt)\" = fixed"))?;
    fixture.job.candidate = Some(
        candidate::build(
            &fixture.job,
            &fixture.root.path().join("candidate"),
            &fixture.root.path().join("downloads"),
            &settings,
            &BTreeMap::new(),
            watch::channel(false).1,
        )
        .await?,
    );
    fixture.job.status = Status::Failed;
    let path = fixture.root.path().join("events.sqlite");
    let journal =
        intercat_daemon::journal::Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32]))
            .await?;
    let db = rusqlite::Connection::open(path)?;
    for track in &fixture.job.tracks {
        db.execute(
            "INSERT INTO tracks VALUES(?1,?2,?3,'ready')",
            rusqlite::params![track.id, track.label, track.title],
        )?;
    }
    db.execute(
        "INSERT INTO integration_jobs VALUES(?1,?2,1,?3)",
        rusqlite::params![
            fixture.job.id.to_string(),
            fixture.job.session_id,
            serde_json::to_string(&fixture.job)?
        ],
    )?;
    db.execute(
        "INSERT INTO integration_settings VALUES(?1,?2,?3)",
        rusqlite::params![
            fixture.job.id.to_string(),
            i64::try_from(fixture.job.revision)?,
            serde_json::to_string(&settings)?
        ],
    )?;
    Ok((fixture, journal))
}

fn repair_request(f: &Fixture) -> intercat_daemon::repair_run::Request {
    intercat_daemon::repair_run::Request {
        op_id: Uuid::now_v7(),
        job_id: f.job.id,
        revision: f.job.revision,
        track: f.job.tracks[0].id.clone(),
        purpose: intercat_daemon::repair_run::Purpose::Check,
    }
}

async fn advance_repair(
    journal: &intercat_daemon::journal::Journal,
    run: &intercat_daemon::repair_run::Run,
    event: intercat_daemon::repair_run::Event,
) -> Result<intercat_daemon::repair_run::Run> {
    journal
        .repair_transition(Uuid::now_v7(), run.id, run.revision, event)
        .await
}

async fn project_secret_fixture(
    journal: &intercat_daemon::journal::Journal,
    store: &intercat_daemon::project_secrets::Store,
    revision: u64,
    value: Option<&str>,
) -> Result<()> {
    use intercat_daemon::{
        project_secrets::{Descriptor, Entry, Load, Prepare},
        secrets::Key,
    };
    let key = Key::new([7; 32]);
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let project_id = journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let fingerprints = value
        .map(|value| {
            vec![Descriptor {
                name: "DEMO_TOKEN".into(),
                byte_len: value.len(),
                tag: hex(&key.tag(value.as_bytes())),
            }]
        })
        .unwrap_or_default();
    let catalog = journal
        .prepare_project_secrets(Prepare {
            op_id: Uuid::now_v7(),
            project_id: project_id.clone(),
            revision,
            fingerprints,
            key_check: hex(&key.check()),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    let entries = value
        .map(|value| {
            vec![Entry {
                name: "DEMO_TOKEN".into(),
                value: value.into(),
            }]
        })
        .unwrap_or_default();
    store.publish(
        journal
            .activate_project_secrets(Load {
                project_id,
                revision,
                entries,
            })
            .await?,
    );
    Ok(())
}

#[tokio::test]
async fn repair_start_rejects_stale_secret_hosts_and_requeues_without_losing_its_session()
-> Result<()> {
    use intercat_daemon::{
        project_secrets::Store,
        repair_run::{Command, Control, Event, Outcome, Phase},
    };
    let (f, journal) = repair_run_fixture(false).await?;
    journal
        .configure_project(Uuid::now_v7(), project::inspect(f.source.clone()).await?)
        .await?;
    let store = Store::default();
    project_secret_fixture(&journal, &store, 1, Some("dummy-repair-secret")).await?;
    let mut run = journal.repair_start(repair_request(&f)).await?;
    run = advance_repair(&journal, &run, Event::Prepare).await?;
    run = advance_repair(&journal, &run, Event::WorkspaceReady).await?;
    run = advance_repair(
        &journal,
        &run,
        Event::HostStarted {
            host: intercat_daemon::process::Identity {
                pid: 42,
                started_seconds: 1,
                started_micros: 1,
            },
            nonce: Uuid::now_v7(),
            secret_revision: Some(0),
        },
    )
    .await?;
    run = advance_repair(
        &journal,
        &run,
        Event::SessionReady {
            session: "repair-session".into(),
        },
    )
    .await?;
    let operation = Uuid::now_v7();
    let rejected = journal
        .repair_transition(
            operation,
            run.id,
            run.revision,
            Event::ReserveStart {
                command: Uuid::now_v7(),
            },
        )
        .await
        .unwrap_err();
    assert!(rejected.is::<intercat_daemon::journal::StartSuperseded>());
    assert!(journal.operation(operation).await?.is_none());
    run = advance_repair(
        &journal,
        &run,
        Event::HostClosed {
            outcome: Outcome::Reconfigure,
        },
    )
    .await?;
    assert_eq!(run.phase, Phase::Queued);
    assert_eq!(run.session.as_deref(), Some("repair-session"));
    assert!(run.workspace_ready);
    run = advance_repair(&journal, &run, Event::Prepare).await?;
    run = advance_repair(
        &journal,
        &run,
        Event::HostStarted {
            host: intercat_daemon::process::Identity {
                pid: 42,
                started_seconds: 2,
                started_micros: 1,
            },
            nonce: Uuid::now_v7(),
            secret_revision: Some(1),
        },
    )
    .await?;
    run = advance_repair(
        &journal,
        &run,
        Event::SessionReady {
            session: "repair-session".into(),
        },
    )
    .await?;
    journal.hold_project_secrets().await?;
    assert!(
        advance_repair(
            &journal,
            &run,
            Event::ReserveStart {
                command: Uuid::now_v7()
            }
        )
        .await
        .is_err()
    );
    project_secret_fixture(&journal, &store, 1, Some("dummy-repair-secret")).await?;
    run = advance_repair(
        &journal,
        &run,
        Event::ReserveStart {
            command: Uuid::now_v7(),
        },
    )
    .await?;
    assert_eq!(run.phase, Phase::Running);
    assert!(
        advance_repair(
            &journal,
            &run,
            Event::HostClosed {
                outcome: Outcome::Reconfigure
            }
        )
        .await
        .is_err()
    );
    run = journal
        .repair_control(Command {
            op_id: Uuid::now_v7(),
            id: run.id,
            revision: run.revision,
            action: Control::Stop,
        })
        .await?;
    run = advance_repair(
        &journal,
        &run,
        Event::HostClosed {
            outcome: Outcome::Stopped,
        },
    )
    .await?;
    assert_eq!(run.phase, Phase::Held);
    assert!(run.secret_revision.is_none());
    f.unchanged()?;
    Ok(())
}

async fn captured_repair(
    journal: &intercat_daemon::journal::Journal,
    mut run: intercat_daemon::repair_run::Run,
) -> Result<intercat_daemon::repair_run::Run> {
    use intercat_daemon::repair_run::{Event, Outcome};
    run = advance_repair(journal, &run, Event::Prepare).await?;
    run = advance_repair(journal, &run, Event::WorkspaceReady).await?;
    run = advance_repair(
        journal,
        &run,
        Event::HostStarted {
            secret_revision: None,
            host: intercat_daemon::process::Identity {
                pid: 42,
                started_seconds: 1,
                started_micros: 1,
            },
            nonce: Uuid::now_v7(),
        },
    )
    .await?;
    run = advance_repair(
        journal,
        &run,
        Event::SessionReady {
            session: "closed-test-session".into(),
        },
    )
    .await?;
    let command = Uuid::now_v7();
    run = advance_repair(journal, &run, Event::ReserveStart { command }).await?;
    run = advance_repair(
        journal,
        &run,
        Event::Started {
            command,
            turn: Uuid::now_v7(),
        },
    )
    .await?;
    advance_repair(
        journal,
        &run,
        Event::HostClosed {
            outcome: Outcome::Completed,
        },
    )
    .await
}

async fn held_repair_session(
    f: &Fixture,
    journal: &intercat_daemon::journal::Journal,
) -> Result<intercat_daemon::repair_run::Run> {
    use intercat_daemon::repair_run::{Event, Outcome};
    let mut run = journal.repair_start(repair_request(f)).await?;
    run = advance_repair(journal, &run, Event::Prepare).await?;
    run = advance_repair(journal, &run, Event::WorkspaceReady).await?;
    run = advance_repair(
        journal,
        &run,
        Event::HostStarted {
            secret_revision: None,
            host: intercat_daemon::process::Identity {
                pid: 42,
                started_seconds: 1,
                started_micros: 1,
            },
            nonce: Uuid::now_v7(),
        },
    )
    .await?;
    run = advance_repair(
        journal,
        &run,
        Event::SessionReady {
            session: "held-repair-session".into(),
        },
    )
    .await?;
    advance_repair(
        journal,
        &run,
        Event::HostClosed {
            outcome: Outcome::Stopped,
        },
    )
    .await
}

#[tokio::test]
async fn repair_followup_preserves_its_session_and_replays_once_without_storing_secrets()
-> Result<()> {
    use intercat_daemon::{
        repair_run::{FollowUp, Phase},
        repair_service,
    };
    let (f, journal) = repair_run_fixture(false).await?;
    let held = held_repair_session(&f, &journal).await?;
    let runtime = repair_service::Runtime::start(
        journal.clone(),
        f.root.path().join("integration"),
        f.root.path().join("data"),
        f.root.path().join("rpc"),
        std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
        integration::Service::new(journal.clone(), f.root.path().join("integration")),
        Default::default(),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    assert!(
        runtime
            .service
            .followup(FollowUp {
                op_id: Uuid::now_v7(),
                id: held.id,
                revision: held.revision,
                text: format!("Use this token: {token}"),
                selection: None,
            })
            .await
            .is_err()
    );
    let request = FollowUp {
        op_id: Uuid::now_v7(),
        id: held.id,
        revision: held.revision,
        text: "Keep the existing edits and fix the remaining assertion.".into(),
        selection: None,
    };
    let queued = runtime.service.followup(request.clone()).await?;
    assert_eq!(queued.phase, Phase::Queued);
    assert_eq!(queued.session, held.session);
    assert_eq!(queued.instruction.as_deref(), Some(request.text.as_str()));
    assert_eq!(
        runtime.service.followup(request.clone()).await?.revision,
        queued.revision
    );
    let mut changed = request.clone();
    changed.text = "A different request".into();
    assert!(runtime.service.followup(changed).await.is_err());
    assert!(
        runtime
            .service
            .followup(FollowUp {
                op_id: Uuid::now_v7(),
                ..request
            })
            .await
            .is_err()
    );
    runtime.shutdown().await;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    for table in ["events", "operations"] {
        let count: i64 = db.query_row(
            &format!("SELECT count(*) FROM {table} WHERE instr(payload,?1)>0"),
            [token],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0);
    }
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_timeline_retains_its_owner_separately_from_work_after_restart() -> Result<()> {
    let (f, journal) = repair_run_fixture(false).await?;
    let held = held_repair_session(&f, &journal).await?;
    let stamp = intercat_daemon::clock::Stamp::now();
    for session in ["work-session", held.session.as_deref().unwrap()] {
        for (id, kind) in [("request", "userMessage"), ("edit", "toolCall")] {
            let value = serde_json::json!({"method":"item/updated","params":{"sessionId":session,"item":{"itemId":id,"turnId":"test-turn","kind":kind,"revision":1,"status":"completed","tool":"edit_file","args":"{\"path\":\"file.txt\"}"}}});
            journal
                .try_msp(
                    held.track.clone(),
                    session.into(),
                    stamp.clone(),
                    value,
                    None,
                    None,
                )?
                .await??;
        }
    }
    let timeline = journal
        .timeline_window(intercat_daemon::timeline::Window {
            tracks: vec![held.track.clone()],
            start: 0,
            end: i64::MAX,
            since: None,
        })
        .await?;
    let clips = timeline["clips"].as_array().unwrap();
    let repair = clips
        .iter()
        .find(|clip| clip["sessionId"] == held.session.as_deref().unwrap())
        .unwrap();
    let work = clips
        .iter()
        .find(|clip| clip["sessionId"] == "work-session")
        .unwrap();
    assert_eq!(repair["repairRunId"], held.id.to_string());
    assert!(work["repairRunId"].is_null());
    assert_ne!(repair["id"], work["id"]);
    let repair_id = repair["id"].as_str().unwrap().to_owned();
    assert_eq!(
        timeline["marks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|mark| mark["repairRunId"] == held.id.to_string())
            .count(),
        1
    );
    let wrong = serde_json::json!({"method":"turn/started","params":{"sessionId":held.session,"turnId":"other-turn"}});
    assert!(
        journal
            .try_msp(
                f.job.tracks[1].id.clone(),
                held.session.clone().unwrap(),
                stamp,
                wrong,
                None,
                None
            )?
            .await?
            .is_err()
    );
    drop(journal);
    let journal = intercat_daemon::journal::Journal::open(
        f.root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    assert_eq!(
        journal.clip_detail(repair_id).await?["clip"]["repairRunId"],
        held.id.to_string()
    );
    f.unchanged()?;
    Ok(())
}

fn scheduler_candidate(f: &Fixture) -> Result<PathBuf> {
    let root = f.root.path().join("integration");
    let parent = root.join(f.job.id.to_string());
    fs::create_dir_all(&parent)?;
    fs::rename(
        f.root.path().join("candidate"),
        parent.join(f.job.candidate.as_ref().unwrap().generation.to_string()),
    )?;
    Ok(root)
}

async fn wait_repair(
    journal: &intercat_daemon::journal::Journal,
    id: Uuid,
    ready: impl Fn(&intercat_daemon::repair_run::Run) -> bool,
) -> Result<intercat_daemon::repair_run::Run> {
    tokio::time::timeout(std::time::Duration::from_secs(150), async {
        loop {
            let snapshot = journal.snapshot().await?;
            let run: intercat_daemon::repair_run::Run = serde_json::from_value(
                snapshot["repairRuns"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|run| run["id"] == id.to_string())
                    .unwrap()
                    .clone(),
            )?;
            if ready(&run) {
                return Ok(run);
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await?
}

#[tokio::test]
async fn repair_scheduler_requires_configuration_and_records_preparation_failure() -> Result<()> {
    use intercat_daemon::{repair_run::Phase, repair_service, runtime::Config};
    let (f, journal) = repair_run_fixture(false).await?;
    let runtime = repair_service::Runtime::start(
        journal.clone(),
        scheduler_candidate(&f)?,
        f.root.path().join("data"),
        f.root.path().join("rpc"),
        std::sync::Arc::new(tokio::sync::Semaphore::new(4)),
        integration::Service::new(journal.clone(), f.root.path().join("integration")),
        Default::default(),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    assert!(runtime.service.start(repair_request(&f)).await.is_err());
    assert_eq!(
        journal.snapshot().await?["repairRuns"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    journal
        .set_worker_config(
            Uuid::now_v7(),
            Config {
                source: f.root.path().join("missing"),
                binary: f.root.path().join("missing"),
                credentials: f.root.path().join("missing-auth"),
                path: "/usr/bin:/bin".into(),
            },
        )
        .await?;
    let run = runtime.service.start(repair_request(&f)).await?;
    let result = wait_repair(&journal, run.id, |run| run.phase == Phase::Failed).await;
    runtime.shutdown().await;
    let failed = result?;
    assert!(failed.host.is_none() && failed.start_command.is_none());
    f.unchanged()?;
    Ok(())
}

async fn invalid_worker(f: &Fixture, journal: &intercat_daemon::journal::Journal) -> Result<()> {
    journal
        .set_worker_config(
            Uuid::now_v7(),
            intercat_daemon::runtime::Config {
                source: f.root.path().join("missing"),
                binary: f.root.path().join("missing"),
                credentials: f.root.path().join("missing-auth"),
                path: "/usr/bin:/bin".into(),
            },
        )
        .await
        .map(|_| ())
}

#[tokio::test]
async fn repair_preparation_rebuilds_incomplete_initialization_after_stop() -> Result<()> {
    use intercat_daemon::{
        repair_run::{Command, Control, Phase},
        repair_service,
    };
    let (f, journal) = repair_run_fixture(false).await?;
    let root = scheduler_candidate(&f)?;
    let run = journal.repair_start(repair_request(&f)).await?;
    let held = journal
        .repair_control(Command {
            op_id: Uuid::now_v7(),
            id: run.id,
            revision: run.revision,
            action: Control::Stop,
        })
        .await?;
    let working = root
        .join(f.job.id.to_string())
        .join(format!("repair-{}", held.id));
    fs::create_dir_all(working.join("repo"))?;
    fs::write(
        working.join("repo/partial-initialization"),
        "not an editable workspace",
    )?;
    invalid_worker(&f, &journal).await?;
    let runtime = repair_service::Runtime::start(
        journal.clone(),
        root,
        f.root.path().join("data"),
        f.root.path().join("rpc"),
        std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        integration::Service::new(journal.clone(), f.root.path().join("integration")),
        Default::default(),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    runtime
        .service
        .control(Command {
            op_id: Uuid::now_v7(),
            id: held.id,
            revision: held.revision,
            action: Control::Resume,
        })
        .await?;
    let result = wait_repair(&journal, held.id, |run| run.phase == Phase::Failed).await;
    runtime.shutdown().await;
    let failed = result?;
    assert!(failed.workspace_ready);
    assert_eq!(
        failed.failure,
        Some(intercat_daemon::repair_run::Failure::Host)
    );
    assert_eq!(fs::read_to_string(working.join("repo/file.txt"))?, "A\n");
    assert!(!working.join("repo/partial-initialization").exists());
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_preparation_keeps_a_ready_workspaces_edits_on_resume() -> Result<()> {
    use intercat_daemon::{
        repair,
        repair_run::{Command, Control, Event, Outcome, Phase},
        repair_service,
    };
    let (f, journal) = repair_run_fixture(false).await?;
    let root = scheduler_candidate(&f)?;
    let mut run = journal.repair_start(repair_request(&f)).await?;
    run = advance_repair(&journal, &run, Event::Prepare).await?;
    let (_, job, settings) = journal.repair_input(run.id).await?;
    let working = root
        .join(f.job.id.to_string())
        .join(format!("repair-{}", run.id));
    let workspace = repair::Workspace::prepare(
        &job,
        &root
            .join(f.job.id.to_string())
            .join(f.job.candidate.as_ref().unwrap().generation.to_string()),
        &working,
        &run.track,
        &settings,
        watch::channel(false).1,
    )
    .await?;
    fs::write(
        workspace.checkout.workdir.join("file.txt"),
        "preserve held edits\n",
    )?;
    run = advance_repair(&journal, &run, Event::WorkspaceReady).await?;
    run = journal
        .repair_control(Command {
            op_id: Uuid::now_v7(),
            id: run.id,
            revision: run.revision,
            action: Control::Stop,
        })
        .await?;
    run = advance_repair(
        &journal,
        &run,
        Event::HostClosed {
            outcome: Outcome::Stopped,
        },
    )
    .await?;
    invalid_worker(&f, &journal).await?;
    let runtime = repair_service::Runtime::start(
        journal.clone(),
        root,
        f.root.path().join("data"),
        f.root.path().join("rpc"),
        std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        integration::Service::new(journal.clone(), f.root.path().join("integration")),
        Default::default(),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    runtime
        .service
        .control(Command {
            op_id: Uuid::now_v7(),
            id: run.id,
            revision: run.revision,
            action: Control::Resume,
        })
        .await?;
    let result = wait_repair(&journal, run.id, |run| run.phase == Phase::Failed).await;
    runtime.shutdown().await;
    assert!(result?.workspace_ready);
    assert_eq!(
        fs::read_to_string(working.join("repo/file.txt"))?,
        "preserve held edits\n"
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_preparation_does_not_remove_a_foreign_symlink_target() -> Result<()> {
    use intercat_daemon::{
        repair_run::{Command, Control, Phase},
        repair_service,
    };
    let (f, journal) = repair_run_fixture(false).await?;
    let root = scheduler_candidate(&f)?;
    let run = journal.repair_start(repair_request(&f)).await?;
    let foreign = f.root.path().join("foreign");
    fs::create_dir(&foreign)?;
    fs::write(foreign.join("keep"), "untouched")?;
    let working = root
        .join(f.job.id.to_string())
        .join(format!("repair-{}", run.id));
    std::os::unix::fs::symlink(&foreign, &working)?;
    invalid_worker(&f, &journal).await?;
    let runtime = repair_service::Runtime::start(
        journal.clone(),
        root,
        f.root.path().join("data"),
        f.root.path().join("rpc"),
        std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        integration::Service::new(journal.clone(), f.root.path().join("integration")),
        Default::default(),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    let result = wait_repair(&journal, run.id, |run| run.phase == Phase::Failed).await;
    runtime.shutdown().await;
    let failed = result?;
    assert!(!failed.workspace_ready);
    assert_eq!(fs::read_to_string(foreign.join("keep"))?, "untouched");
    assert!(fs::symlink_metadata(working)?.file_type().is_symlink());
    journal
        .repair_control(Command {
            op_id: Uuid::now_v7(),
            id: failed.id,
            revision: failed.revision,
            action: Control::Discard,
        })
        .await?;
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
#[ignore = "paid dummy Muse turn with stored authentication"]
async fn real_repair_scheduler_shares_capacity_stops_follows_up_and_admits_check() -> Result<()> {
    use intercat_daemon::{
        host,
        repair_run::{FollowUp, Phase},
        repair_service,
        runtime::Config,
    };
    let (f, journal) = repair_run_fixture(false).await?;
    let mut evidence = RepairEvidence::new(&f, "scheduler")?;
    let root = scheduler_candidate(&f)?;
    let binary = host::prepare_binary(
        &PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        &f.root.path().join("muse"),
    )?;
    journal
        .set_worker_config(
            Uuid::now_v7(),
            Config {
                source: binary.clone(),
                binary,
                credentials: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?),
                path: "/usr/bin:/bin:/usr/sbin:/sbin".into(),
            },
        )
        .await?;
    let rpc = f.root.path().join("rpc");
    fs::create_dir(&rpc)?;
    fs::set_permissions(&rpc, fs::Permissions::from_mode(0o700))?;
    let capacity = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let runtime = repair_service::Runtime::start(
        journal.clone(),
        root.clone(),
        f.root.path().join("data"),
        rpc,
        capacity.clone(),
        integration::Service::new(journal.clone(), f.root.path().join("integration")),
        Default::default(),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    let service = runtime.service.clone();
    let request = repair_request(&f);
    let run = service.start(request.clone()).await?;
    let result = async {
        let require = |condition: bool, message: &str| -> Result<()> {
            if condition {
                Ok(())
            } else {
                Err(message.to_owned().into())
            }
        };
        require(
            service.start(request).await?.id == run.id,
            "Start replay changed the repair ID.",
        )?;
        let waiting = wait_repair(&journal, run.id, |run| {
            run.session_ready || run.phase == Phase::Failed
        })
        .await?;
        require(
            waiting.phase == Phase::Preparing,
            "The repair did not wait for shared capacity.",
        )?;
        require(
            waiting.start_command.is_none() && waiting.turn.is_none(),
            "Paid work started without shared capacity.",
        )?;
        let original_host = waiting.host.unwrap();
        let edit = root
            .join(f.job.id.to_string())
            .join(format!("repair-{}", run.id))
            .join("repo/keep-me.txt");
        fs::write(&edit, "Preserve this held edit.\n")?;
        let stop = Uuid::now_v7();
        service.stop(stop, run.id).await?;
        service.stop(stop, run.id).await?;
        let held = wait_repair(&journal, run.id, |run| {
            run.phase == Phase::Held || run.phase == Phase::Failed
        })
        .await?;
        require(
            held.phase == Phase::Held,
            "The repair did not stop while waiting.",
        )?;
        require(
            held.start_command.is_none() && held.turn.is_none(),
            "Stopping a queued turn dispatched paid work.",
        )?;
        require(
            !intercat_daemon::process::is_alive(original_host),
            "The stopped host survived.",
        )?;
        service
            .followup(FollowUp {
                op_id: Uuid::now_v7(),
                id: run.id,
                revision: held.revision,
                text: "Fix file.txt to contain exactly fixed followed by a newline. Use the file editing tool. Keep keep-me.txt and other.txt unchanged. Complete the original task and give a brief final report.".into(),
                selection: None,
            })
            .await?;
        let resumed = wait_repair(&journal, run.id, |run| {
            run.session_ready || run.phase == Phase::Failed
        })
        .await?;
        require(
            resumed.phase == Phase::Preparing,
            "The held repair did not reopen.",
        )?;
        require(
            resumed.session == waiting.session,
            "Resume changed the repair session.",
        )?;
        require(
            fs::read_to_string(&edit)? == "Preserve this held edit.\n",
            "Resume overwrote the held edit.",
        )?;
        capacity.add_permits(1);
        let done = tokio::time::timeout(std::time::Duration::from_secs(150), async {
            loop {
                let current = journal.repair_input(run.id).await;
                let current = match current {
                    Ok((run, _, _)) => run,
                    Err(_) => {
                        let snapshot = journal.snapshot().await?;
                        serde_json::from_value(
                            snapshot["repairRuns"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .find(|item| item["id"] == run.id.to_string())
                                .unwrap()
                                .clone(),
                        )?
                    }
                };
                if matches!(
                    current.phase,
                    Phase::Done | Phase::Failed | Phase::Held | Phase::NeedsRecovery
                ) {
                    break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(current);
                }
                for approval in &current.approvals {
                    let subject: serde_json::Value = serde_json::from_str(&approval.subject)?;
                    let command = subject["command"]
                        .as_str()
                        .ok_or("Unexpected repair approval subject.")?;
                    // Fixture-only approval, never a production policy rule.
                    let allowed = [
                        "test \"$(cat file.txt)\" = fixed",
                        "echo PASS",
                        "echo",
                        "cat file.txt",
                        "printf 'fixed' > file.txt",
                        "printf 'fixed\\n' > file.txt",
                    ];
                    require(
                        command
                            .split(';')
                            .flat_map(|part| part.split("&&"))
                            .all(|part| allowed.contains(&part.trim())),
                        "The dummy worker requested an unexpected command.",
                    )?;
                    service
                        .answer(
                            Uuid::now_v7(),
                            run.id,
                            approval.id.clone(),
                            approval.requirement.clone(),
                            true,
                        )
                        .await?;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await??;
        require(
            done.phase == Phase::Done,
            "The repair did not finish applying its candidate.",
        )?;
        let snapshot = journal.snapshot().await?;
        let job: Job = serde_json::from_value(snapshot["integration"].clone())?;
        require(
            job.status == Status::Checked,
            "The rebuilt candidate did not pass Check.",
        )?;
        require(
            job.candidate.as_ref().unwrap().generation == run.job_revision,
            "The candidate used the wrong generation.",
        )?;
        require(
            Repository::open(&f.source)?
                .reference_target(&job.plan.as_ref().unwrap().target_ref)?
                .is_none(),
            "The repair published a user branch.",
        )?;
        f.unchanged()?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>((done, job))
    }
    .await;
    runtime.shutdown().await;
    evidence.save()?;
    let (done, job) = result?;
    fs::write(
        evidence.target.join("result.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"run":done,"job":job,"sourceUnchanged":true,"sharedCapacity":true,"stoppedBeforePaidStart":true}),
        )?,
    )?;
    Ok(())
}

#[tokio::test]
async fn repair_application_rechecks_and_atomically_releases_the_candidate() -> Result<()> {
    use intercat_daemon::{repair, repair_application, repair_run::Phase};
    let (f, journal) = repair_run_fixture(false).await?;
    let run = captured_repair(&journal, journal.repair_start(repair_request(&f)).await?).await?;
    let (_, job, settings) = journal.repair_input(run.id).await?;
    let workspace = repair::Workspace::prepare(
        &job,
        &f.root.path().join("candidate"),
        &f.root.path().join("repair"),
        &run.track,
        &settings,
        watch::channel(false).1,
    )
    .await?;
    fs::write(workspace.checkout.workdir.join("file.txt"), "fixed\n")?;
    let done = repair_application::apply(
        &journal,
        &run,
        std::sync::Arc::new(workspace),
        f.root.path().join("candidate"),
        f.root.path().join(job.revision.to_string()),
        f.root.path().join("downloads"),
        watch::channel(false).1,
    )
    .await?;
    assert_eq!(done.phase, Phase::Done);
    let snapshot = journal.snapshot().await?;
    let current: Job = serde_json::from_value(snapshot["integration"].clone())?;
    assert_eq!(current.status, Status::Checked);
    let candidate = current.candidate.as_ref().unwrap();
    assert_eq!(candidate.state, State::Checked);
    assert_eq!(candidate.generation, job.revision);
    assert_eq!(candidate.revision, job.revision);
    assert_eq!(
        candidate.repair.as_ref().unwrap().requested_track,
        run.track
    );
    assert_eq!(
        Repository::open(&f.source)?
            .reference_target(&format!("refs/intercat/{}/staging", job.slug))?,
        Some(candidate.head.parse()?)
    );
    // A successful repair no longer holds the editing lock.
    let reopened = journal
        .integration_command(integration::Command {
            op_id: Uuid::now_v7(),
            action: integration::Action::Select {
                id: job.id,
                revision: job.revision,
                track: job.tracks[0].id.clone(),
                included: true,
            },
        })
        .await?;
    assert_eq!(reopened.status, Status::Building);
    f.unchanged()?;
    Ok(())
}

async fn apply_fixture_edits(
    f: &Fixture,
    journal: &intercat_daemon::journal::Journal,
    files: &[(&str, &str)],
) -> Result<Job> {
    let run = captured_repair(journal, journal.repair_start(repair_request(f)).await?).await?;
    let (_, job, settings) = journal.repair_input(run.id).await?;
    let workspace = intercat_daemon::repair::Workspace::prepare(
        &job,
        &f.root.path().join("candidate"),
        &f.root.path().join("repair"),
        &run.track,
        &settings,
        watch::channel(false).1,
    )
    .await?;
    for (name, content) in files {
        fs::write(workspace.checkout.workdir.join(name), content)?;
    }
    let done = intercat_daemon::repair_application::apply(
        journal,
        &run,
        std::sync::Arc::new(workspace),
        f.root.path().join("candidate"),
        f.root.path().join(job.revision.to_string()),
        f.root.path().join("downloads"),
        watch::channel(false).1,
    )
    .await?;
    assert_eq!(done.phase, intercat_daemon::repair_run::Phase::Done);
    Ok(serde_json::from_value(
        journal.snapshot().await?["integration"].clone(),
    )?)
}

async fn rebuild_fixture(
    f: &Fixture,
    journal: &intercat_daemon::journal::Journal,
    action: integration::Action,
) -> Result<Job> {
    let service = integration::Service::new(journal.clone(), f.root.path().join("integration"));
    let result = async {
        let dispatched = service
            .command(integration::Command {
                op_id: Uuid::now_v7(),
                action,
            })
            .await?;
        assert_eq!(dispatched.status, Status::Building);
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                let job: Job =
                    serde_json::from_value(journal.snapshot().await?["integration"].clone())?;
                if !matches!(
                    job.status,
                    Status::Preparing | Status::Building | Status::Staging
                ) {
                    break Ok(job);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await?
    }
    .await;
    service.shutdown().await;
    result
}

#[tokio::test]
async fn repaired_candidate_keeps_its_tree_when_the_commit_message_changes() -> Result<()> {
    let (f, journal) = repair_run_fixture(false).await?;
    let repaired = apply_fixture_edits(&f, &journal, &[("file.txt", "fixed\n")]).await?;
    assert_eq!(repaired.status, Status::Checked);
    let original = repaired.candidate.as_ref().unwrap();
    let rebuilt = rebuild_fixture(
        &f,
        &journal,
        integration::Action::EditMessage {
            id: repaired.id,
            revision: repaired.revision,
            track: repaired.tracks[0].id.clone(),
            message: "Explain the repaired behavior".into(),
        },
    )
    .await?;
    assert_eq!(rebuilt.status, Status::Checked, "{:?}", rebuilt.error);
    let changed = rebuilt.candidate.as_ref().unwrap();
    assert_eq!(changed.tree, original.tree);
    assert_ne!(changed.head, original.head);
    assert!(
        changed.commits[0]
            .message
            .starts_with("Explain the repaired behavior\n")
    );
    assert!(
        changed
            .check
            .as_ref()
            .is_some_and(|check| check.code == Some(0))
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repaired_candidate_keeps_failed_check_edits_when_retrying() -> Result<()> {
    let (f, journal) = repair_run_fixture(false).await?;
    let repaired = apply_fixture_edits(&f, &journal, &[("file.txt", "almost\n")]).await?;
    assert_eq!(repaired.status, Status::Failed);
    let original = repaired.candidate.as_ref().unwrap();
    let rebuilt = rebuild_fixture(
        &f,
        &journal,
        integration::Action::Prepare {
            id: repaired.id,
            revision: repaired.revision,
        },
    )
    .await?;
    let changed = rebuilt.candidate.as_ref().unwrap();
    assert_eq!(changed.state, State::CheckFailed);
    assert_eq!(changed.tree, original.tree);
    let repo = Repository::open(&f.source)?;
    assert_eq!(
        repo.saved_bytes(&repo.tree_files(changed.tree.parse()?)?["file.txt"])?,
        b"almost\n"
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repaired_candidate_can_exclude_and_reinclude_the_fixed_track() -> Result<()> {
    let (f, journal) = repair_run_fixture(false).await?;
    let repaired = apply_fixture_edits(&f, &journal, &[("file.txt", "fixed\n")]).await?;
    let original = repaired.candidate.as_ref().unwrap();
    let excluded = rebuild_fixture(
        &f,
        &journal,
        integration::Action::Select {
            id: repaired.id,
            revision: repaired.revision,
            track: repaired.tracks[0].id.clone(),
            included: false,
        },
    )
    .await?;
    assert_eq!(excluded.candidate.as_ref().unwrap().commits.len(), 1);
    assert_eq!(
        excluded.candidate.as_ref().unwrap().commits[0].track,
        repaired.tracks[1].id
    );
    assert!(excluded.contributions.contains_key(&repaired.tracks[0].id));
    let included = rebuild_fixture(
        &f,
        &journal,
        integration::Action::Select {
            id: repaired.id,
            revision: excluded.revision,
            track: repaired.tracks[0].id.clone(),
            included: true,
        },
    )
    .await?;
    assert_eq!(included.status, Status::Checked, "{:?}", included.error);
    assert_eq!(included.candidate.as_ref().unwrap().tree, original.tree);
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repaired_secret_tree_stays_private_when_its_message_is_rebuilt() -> Result<()> {
    let (f, journal) = repair_run_fixture(false).await?;
    let token = concat!("token = ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H\n");
    let repaired = apply_fixture_edits(
        &f,
        &journal,
        &[("file.txt", "fixed\n"), ("private.txt", token)],
    )
    .await?;
    assert_eq!(
        repaired.candidate.as_ref().unwrap().state,
        State::SecretFound
    );
    let blocked = repaired
        .candidate
        .as_ref()
        .unwrap()
        .blocked_commit
        .as_ref()
        .unwrap();
    let original = repaired.contributions.get(&blocked.track).unwrap();
    let private = Repository::open(&original.source)?;
    let secret = private.tree_files(blocked.tree.parse()?)?["private.txt"].clone();
    assert!(Repository::open(&f.source)?.saved_bytes(&secret).is_err());
    let rebuilt = rebuild_fixture(
        &f,
        &journal,
        integration::Action::EditMessage {
            id: repaired.id,
            revision: repaired.revision,
            track: blocked.track.clone(),
            message: "Revise the message without dropping the edit".into(),
        },
    )
    .await?;
    let candidate = rebuilt.candidate.as_ref().unwrap();
    assert_eq!(candidate.state, State::SecretFound);
    assert_eq!(
        candidate.blocked_commit.as_ref().unwrap().tree,
        blocked.tree
    );
    assert_ne!(
        candidate.generation,
        repaired.candidate.as_ref().unwrap().generation
    );
    assert!(Repository::open(&f.source)?.saved_bytes(&secret).is_err());
    assert_eq!(repaired.kept_secrets.len(), 0);
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_application_rejects_forged_results_and_survives_restart_without_replay()
-> Result<()> {
    use intercat_daemon::repair_run::{Command, Control, Phase};
    let (f, journal) = repair_run_fixture(false).await?;
    rusqlite::Connection::open(f.root.path().join("events.sqlite"))?.execute(
        "INSERT INTO settings VALUES('session',?1)",
        [serde_json::json!({"id":f.job.session_id}).to_string()],
    )?;
    let run = captured_repair(&journal, journal.repair_start(repair_request(&f)).await?).await?;
    assert!(
        journal
            .repair_begin_apply(run.id, run.revision - 1)
            .await
            .is_err()
    );
    let (applying, input, _) = journal.repair_begin_apply(run.id, run.revision).await?;
    assert_eq!(applying.phase, Phase::Applying);
    let service = integration::Service::new(journal.clone(), f.root.path().join("never-build"));
    let opened = service
        .command(integration::Command {
            op_id: Uuid::now_v7(),
            action: integration::Action::Open { tracks: None },
        })
        .await?;
    assert_eq!(opened.status, Status::Staging);
    service.shutdown().await;
    assert!(!f.root.path().join("never-build").exists());
    assert!(
        journal
            .repair_begin_apply(run.id, run.revision)
            .await
            .is_err()
    );
    let mut forged = input.candidate.clone().unwrap();
    forged.revision = input.revision;
    forged.generation = input.revision;
    assert!(
        journal
            .repair_applied(applying.id, applying.revision, Ok(forged))
            .await
            .is_err()
    );
    assert!(
        journal
            .repair_control(Command {
                op_id: Uuid::now_v7(),
                id: run.id,
                revision: applying.revision,
                action: Control::Discard
            })
            .await
            .is_err()
    );
    assert!(
        journal
            .integration_command(integration::Command {
                op_id: Uuid::now_v7(),
                action: integration::Action::Refresh {
                    id: input.id,
                    revision: input.revision
                }
            })
            .await
            .is_err()
    );
    drop(journal);
    let journal = intercat_daemon::journal::Journal::open(
        f.root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let (recovered, job, _) = journal.repair_input(run.id).await?;
    assert_eq!(recovered.phase, Phase::NeedsRecovery);
    assert_eq!(job.status, Status::Failed);
    assert_eq!(job.candidate.unwrap().head, input.candidate.unwrap().head);
    assert!(
        journal
            .repair_begin_apply(run.id, recovered.revision)
            .await
            .is_err()
    );
    assert!(
        journal
            .repair_applied(run.id, recovered.revision, Err("late result".into()))
            .await
            .is_err()
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_run_reserves_the_candidate_and_never_changes_the_main_worker() -> Result<()> {
    use intercat_daemon::repair_run::{Command, Control, Phase};
    let (f, journal) = repair_run_fixture(false).await?;
    let main = intercat_daemon::runtime::WorkerState {
        secret_revision: None,
        track_id: f.job.tracks[0].id.clone(),
        phase: "working".into(),
        session_id: Some("main-session".into()),
        identity: None,
        detail: None,
        active_turn: Some(Uuid::now_v7()),
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    journal.worker_state(main.clone()).await?;
    let request = repair_request(&f);
    let run = journal.repair_start(request.clone()).await?;
    assert_eq!(run.phase, Phase::Queued);
    assert_eq!(journal.repair_start(request.clone()).await?.id, run.id);
    let (_, job, settings) = journal.repair_input(run.id).await?;
    assert_eq!(job.revision, f.job.revision + 1);
    assert_eq!(job.candidate.as_ref().unwrap().revision, job.revision);
    assert_eq!(
        settings.hash()?,
        job.candidate.as_ref().unwrap().settings_hash
    );
    let mut duplicate = request.clone();
    duplicate.op_id = Uuid::now_v7();
    duplicate.revision = job.revision;
    assert!(journal.repair_start(duplicate).await.is_err());
    assert!(
        journal
            .integration_command(integration::Command {
                op_id: Uuid::now_v7(),
                action: integration::Action::Refresh {
                    id: job.id,
                    revision: job.revision
                }
            })
            .await
            .is_err()
    );
    let stop = Command {
        op_id: Uuid::now_v7(),
        id: run.id,
        revision: run.revision,
        action: Control::Stop,
    };
    let held = journal.repair_control(stop.clone()).await?;
    assert_eq!(held.phase, Phase::Held);
    assert_eq!(journal.repair_control(stop).await?.revision, held.revision);
    let current = journal.snapshot().await?;
    assert_eq!(current["workers"][0], serde_json::to_value(main)?);
    assert_eq!(current["repairRuns"][0]["id"], run.id.to_string());
    let discarded = journal
        .repair_control(Command {
            op_id: Uuid::now_v7(),
            id: held.id,
            revision: held.revision,
            action: Control::Discard,
        })
        .await?;
    assert_eq!(discarded.phase, Phase::Discarded);
    journal
        .integration_command(integration::Command {
            op_id: Uuid::now_v7(),
            action: integration::Action::Discard {
                id: job.id,
                revision: job.revision,
            },
        })
        .await?;
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_run_stop_during_start_retains_the_returned_turn_without_resuming() -> Result<()> {
    use intercat_daemon::repair_run::{Command, Control, Event, Outcome, Phase};
    let (f, journal) = repair_run_fixture(false).await?;
    let run = journal.repair_start(repair_request(&f)).await?;
    let preparing = advance_repair(&journal, &run, Event::Prepare).await?;
    assert!(
        advance_repair(
            &journal,
            &preparing,
            Event::ReserveStart {
                command: Uuid::now_v7()
            }
        )
        .await
        .is_err()
    );
    let preparing = advance_repair(&journal, &preparing, Event::WorkspaceReady).await?;
    let hosted = advance_repair(
        &journal,
        &preparing,
        Event::HostStarted {
            secret_revision: None,
            host: intercat_daemon::process::Identity {
                pid: 42,
                started_seconds: 1,
                started_micros: 2,
            },
            nonce: Uuid::now_v7(),
        },
    )
    .await?;
    assert!(
        advance_repair(
            &journal,
            &hosted,
            Event::ReserveStart {
                command: Uuid::now_v7()
            }
        )
        .await
        .is_err()
    );
    let ready = advance_repair(
        &journal,
        &hosted,
        Event::SessionReady {
            session: "fix-session".into(),
        },
    )
    .await?;
    let command = Uuid::now_v7();
    let starting = advance_repair(&journal, &ready, Event::ReserveStart { command }).await?;
    let stopping = journal
        .repair_control(Command {
            op_id: Uuid::now_v7(),
            id: starting.id,
            revision: starting.revision,
            action: Control::Stop,
        })
        .await?;
    assert!(
        journal
            .repair_control(Command {
                op_id: Uuid::now_v7(),
                id: stopping.id,
                revision: stopping.revision,
                action: Control::Discard
            })
            .await
            .is_err()
    );
    assert!(
        advance_repair(
            &journal,
            &starting,
            Event::Started {
                command,
                turn: Uuid::now_v7()
            }
        )
        .await
        .is_err()
    );
    let turn = Uuid::now_v7();
    let acknowledged =
        advance_repair(&journal, &stopping, Event::Started { command, turn }).await?;
    assert_eq!(acknowledged.phase, Phase::Stopping);
    assert_eq!(acknowledged.turn, Some(turn));
    assert!(
        advance_repair(
            &journal,
            &acknowledged,
            Event::ReserveStart {
                command: Uuid::now_v7()
            }
        )
        .await
        .is_err()
    );
    let held = advance_repair(
        &journal,
        &acknowledged,
        Event::HostClosed {
            outcome: Outcome::Stopped,
        },
    )
    .await?;
    assert!(held.host.is_none());
    let resumed = journal
        .repair_control(Command {
            op_id: Uuid::now_v7(),
            id: held.id,
            revision: held.revision,
            action: Control::Resume,
        })
        .await?;
    assert_eq!(resumed.phase, Phase::Queued);
    assert!(resumed.turn.is_none() && resumed.start_command.is_none());
    assert_eq!(resumed.session.as_deref(), Some("fix-session"));
    Ok(())
}

#[tokio::test]
async fn repair_run_recovery_retains_unknown_dispatch_and_does_not_replay_it() -> Result<()> {
    use intercat_daemon::repair_run::{Command, Control, Event, Outcome, Phase};
    let (f, journal) = repair_run_fixture(false).await?;
    let request = repair_request(&f);
    let queued = journal.repair_start(request.clone()).await?;
    drop(journal);
    let journal = intercat_daemon::journal::Journal::open(
        f.root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    assert_eq!(
        journal.repair_start(request.clone()).await?.phase,
        Phase::Queued
    );
    let preparing = advance_repair(&journal, &queued, Event::Prepare).await?;
    let preparing = advance_repair(&journal, &preparing, Event::WorkspaceReady).await?;
    let hosted = advance_repair(
        &journal,
        &preparing,
        Event::HostStarted {
            secret_revision: None,
            host: intercat_daemon::process::Identity {
                pid: 43,
                started_seconds: 2,
                started_micros: 3,
            },
            nonce: Uuid::now_v7(),
        },
    )
    .await?;
    let ready = advance_repair(
        &journal,
        &hosted,
        Event::SessionReady {
            session: "unknown-session".into(),
        },
    )
    .await?;
    let command = Uuid::now_v7();
    let starting = advance_repair(&journal, &ready, Event::ReserveStart { command }).await?;
    drop(journal);
    let journal = intercat_daemon::journal::Journal::open(
        f.root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let (recovered, _, _) = journal.repair_input(starting.id).await?;
    assert_eq!(recovered.phase, Phase::NeedsRecovery);
    assert_eq!(recovered.start_command, Some(command));
    assert_eq!(recovered.host, starting.host);
    assert_eq!(
        journal.repair_start(request).await?.phase,
        Phase::NeedsRecovery
    );
    assert!(
        journal
            .repair_control(Command {
                op_id: Uuid::now_v7(),
                id: recovered.id,
                revision: recovered.revision,
                action: Control::Resume
            })
            .await
            .is_err()
    );
    let held = advance_repair(
        &journal,
        &recovered,
        Event::HostClosed {
            outcome: Outcome::Stopped,
        },
    )
    .await?;
    assert_eq!(held.phase, Phase::Held);
    Ok(())
}

#[tokio::test]
async fn repair_run_rejects_wrong_failures_and_operation_reuse() -> Result<()> {
    use intercat_daemon::repair_run::{Command, Control, Purpose};
    let (f, journal) = repair_run_fixture(false).await?;
    let mut request = repair_request(&f);
    request.purpose = Purpose::RemoveSecret {
        finding: "missing".into(),
    };
    assert!(journal.repair_start(request.clone()).await.is_err());
    request.purpose = Purpose::Check;
    request.track = "missing".into();
    assert!(journal.repair_start(request.clone()).await.is_err());
    request.track = f.job.tracks[0].id.clone();
    let run = journal.repair_start(request.clone()).await?;
    request.track = f.job.tracks[1].id.clone();
    assert!(journal.repair_start(request).await.is_err());
    assert!(
        journal
            .repair_control(Command {
                op_id: Uuid::now_v7(),
                id: run.id,
                revision: 0,
                action: Control::Stop
            })
            .await
            .is_err()
    );
    let held = journal
        .repair_control(Command {
            op_id: Uuid::now_v7(),
            id: run.id,
            revision: run.revision,
            action: Control::Stop,
        })
        .await?;
    let (_, job, _) = journal.repair_input(held.id).await?;
    assert!(
        journal
            .integration_command(integration::Command {
                op_id: Uuid::now_v7(),
                action: integration::Action::Prepare {
                    id: job.id,
                    revision: job.revision
                }
            })
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn repair_run_remove_is_bound_to_the_current_finding_and_its_owner() -> Result<()> {
    use intercat_daemon::repair_run::{Phase, Purpose};
    let (f, journal) = repair_run_fixture(true).await?;
    let candidate = f.job.candidate.as_ref().unwrap();
    assert_eq!(candidate.state, State::SecretFound);
    let finding = candidate.secret_scans.last().unwrap().findings[0]
        .id
        .clone();
    let mut request = repair_request(&f);
    request.purpose = Purpose::RemoveSecret {
        finding: finding.clone(),
    };
    request.track = f.job.tracks[1].id.clone();
    assert!(journal.repair_start(request.clone()).await.is_err());
    request.track = f.job.tracks[0].id.clone();
    let run = journal.repair_start(request).await?;
    assert_eq!(run.phase, Phase::Queued);
    let (_, job, _) = journal.repair_input(run.id).await?;
    assert!(
        journal
            .integration_command(integration::Command {
                op_id: Uuid::now_v7(),
                action: integration::Action::KeepSecret {
                    id: job.id,
                    revision: job.revision,
                    finding
                }
            })
            .await
            .is_err()
    );
    let snapshot = serde_json::to_string(&journal.snapshot().await?)?;
    assert!(!snapshot.contains(concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H")));
    Ok(())
}

#[tokio::test]
async fn repair_host_rejects_an_unverified_program_before_starting_it() -> Result<()> {
    use intercat_daemon::{
        repair_host,
        repair_run::{Event, Failure, Phase},
        runtime::Config,
    };
    let (f, journal) = repair_run_fixture(false).await?;
    let mut run = journal.repair_start(repair_request(&f)).await?;
    run = advance_repair(&journal, &run, Event::Prepare).await?;
    let (_, job, settings) = journal.repair_input(run.id).await?;
    let workspace = intercat_daemon::repair::Workspace::prepare(
        &job,
        &f.root.path().join("candidate"),
        &f.root.path().join("unverified-workspace"),
        &run.track,
        &settings,
        watch::channel(false).1,
    )
    .await?;
    let binary = f.root.path().join("unverified");
    let marker = f.root.path().join("executed");
    executable(
        &binary,
        &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )?;
    let config = Config {
        source: binary.clone(),
        binary,
        credentials: f.root.path().join("unused-auth"),
        path: "/usr/bin:/bin".into(),
    };
    let wrong = intercat_daemon::repair::Workspace::prepare(
        &job,
        &f.root.path().join("candidate"),
        &f.root.path().join("wrong-owner"),
        &job.tracks[1].id,
        &settings,
        watch::channel(false).1,
    )
    .await?;
    assert!(
        repair_host::open(
            &journal,
            &mut run,
            &wrong,
            repair_host::Launch {
                one_time: None,
                project_secrets: Default::default(),
                worker: &config,
                directory: &f.root.path().join("wrong-host"),
                rpc: &f.root.path().join("rpc")
            }
        )
        .await
        .is_err()
    );
    assert_eq!(run.phase, Phase::Preparing);
    assert!(!f.root.path().join("wrong-host").exists());
    let result = repair_host::open(
        &journal,
        &mut run,
        &workspace,
        repair_host::Launch {
            one_time: None,
            project_secrets: Default::default(),
            worker: &config,
            directory: &f.root.path().join("host"),
            rpc: &f.root.path().join("rpc"),
        },
    )
    .await;
    assert!(result.is_err());
    assert!(!marker.exists());
    assert_eq!(run.phase, Phase::Failed);
    assert_eq!(run.failure, Some(Failure::Host));
    assert!(run.host.is_none());
    Ok(())
}

#[tokio::test]
async fn repair_host_does_not_launch_if_stop_was_saved_during_preparation() -> Result<()> {
    use intercat_daemon::{
        repair_host,
        repair_run::{Command, Control, Event, Phase},
        runtime::Config,
    };
    let (f, journal) = repair_run_fixture(false).await?;
    let mut run = journal.repair_start(repair_request(&f)).await?;
    run = advance_repair(&journal, &run, Event::Prepare).await?;
    let (_, job, settings) = journal.repair_input(run.id).await?;
    let workspace = intercat_daemon::repair::Workspace::prepare(
        &job,
        &f.root.path().join("candidate"),
        &f.root.path().join("stopped-workspace"),
        &run.track,
        &settings,
        watch::channel(false).1,
    )
    .await?;
    journal
        .repair_control(Command {
            op_id: Uuid::now_v7(),
            id: run.id,
            revision: run.revision,
            action: Control::Stop,
        })
        .await?;
    let config = Config {
        source: f.root.path().join("never-read"),
        binary: f.root.path().join("never-executed"),
        credentials: f.root.path().join("never-read-auth"),
        path: "/usr/bin:/bin".into(),
    };
    let host_dir = f.root.path().join("never-created");
    assert!(
        repair_host::open(
            &journal,
            &mut run,
            &workspace,
            repair_host::Launch {
                one_time: None,
                project_secrets: Default::default(),
                worker: &config,
                directory: &host_dir,
                rpc: &f.root.path().join("rpc")
            }
        )
        .await
        .is_err()
    );
    assert_eq!(run.phase, Phase::Held);
    assert!(!host_dir.exists());
    assert!(run.host.is_none());
    Ok(())
}

#[tokio::test]
#[ignore = "requires the pinned Muse binary and saved authentication; no model turn"]
async fn real_repair_host_registers_its_session_and_closes_without_stopping_another_host()
-> Result<()> {
    use intercat_daemon::{
        host::{self, Host},
        repair_host,
        repair_run::{Command, Control, Event, Outcome, Phase},
        runtime::Config,
    };
    let binary = PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?);
    let credentials = PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?);
    assert_eq!(host::hash(&binary)?, host::BINARY_SHA256);
    let (f, journal) = repair_run_fixture(false).await?;
    let mut run = journal.repair_start(repair_request(&f)).await?;
    run = advance_repair(&journal, &run, Event::Prepare).await?;
    let (_, job, settings) = journal.repair_input(run.id).await?;
    let workspace = intercat_daemon::repair::Workspace::prepare(
        &job,
        &f.root.path().join("candidate"),
        &f.root.path().join("repair"),
        &run.track,
        &settings,
        watch::channel(false).1,
    )
    .await?;
    let rpc = f.root.path().join("rpc");
    fs::create_dir(&rpc)?;
    fs::set_permissions(&rpc, fs::Permissions::from_mode(0o700))?;
    let config = Config {
        source: binary.clone(),
        binary,
        credentials,
        path: std::env::var("PATH")?,
    };
    let main = Host::spawn(
        &config.binary,
        &f.root.path().join("main-host"),
        &f.source,
        std::ffi::OsStr::new(&config.path),
        Some(&config.credentials),
        host::Boundary {
            profile: intercat_daemon::host::Profile::Worker,
            secrets: &[],
            rpc: &rpc,
            denied: &[],
        },
    )
    .await?;
    main.connection.client.initialize().await?;
    let main_identity = main.identity;
    let denied = vec![f.source.clone(), f.root.path().join("candidate")];
    let host_a = f.root.path().join("fix-host-a");
    let host_b = f.root.path().join("fix-host-b");
    let mut competing = run.clone();
    let (a, b) = tokio::join!(
        repair_host::open(
            &journal,
            &mut run,
            &workspace,
            repair_host::Launch {
                one_time: None,
                project_secrets: Default::default(),
                worker: &config,
                directory: &host_a,
                rpc: &rpc
            }
        ),
        repair_host::open(
            &journal,
            &mut competing,
            &workspace,
            repair_host::Launch {
                one_time: None,
                project_secrets: Default::default(),
                worker: &config,
                directory: &host_b,
                rpc: &rpc
            }
        ),
    );
    let (fix, host_dir) = match (a, b) {
        (Ok(host), Err(_)) => (host, host_a),
        (Err(_), Ok(host)) => {
            run = competing;
            (host, host_b)
        }
        _ => panic!("Exactly one host must own the repair Run."),
    };
    let fix_identity = fix.identity();
    assert_ne!(main_identity, fix_identity);
    assert_eq!(run.phase, Phase::Preparing);
    assert_eq!(run.host, Some(fix_identity));
    assert_eq!(
        journal.repair_input(run.id).await?.0.host,
        Some(fix_identity)
    );
    assert!(run.session_ready && run.session.is_some());
    let session = run.session.clone();
    assert!(run.start_command.is_none() && run.turn.is_none());
    let permission_settings: serde_json::Value =
        serde_json::from_slice(&fs::read(host_dir.join("config/muse/settings.json"))?)?;
    for path in denied {
        assert_eq!(
            permission_settings["permissions"]["profiles"]["intercat-worker"]["filesystem"]
                [path.canonicalize()?.to_str().unwrap()],
            "deny"
        );
    }
    run = fix.close(&journal, Outcome::Stopped).await?;
    assert_eq!(run.phase, Phase::Held);
    assert!(run.host.is_none() && !run.session_ready);
    assert!(!intercat_daemon::process::is_alive(fix_identity));
    assert!(intercat_daemon::process::is_alive(main_identity));
    fs::write(
        workspace.checkout.workdir.join("resume-proof.txt"),
        "Keep the held repair edits.\n",
    )?;
    drop(workspace);
    let workspace = intercat_daemon::repair::Workspace::resume(
        &job,
        &f.root.path().join("candidate"),
        &f.root.path().join("repair"),
        &run.track,
        &settings,
    )
    .await?;
    assert_eq!(
        fs::read_to_string(workspace.checkout.workdir.join("resume-proof.txt"))?,
        "Keep the held repair edits.\n"
    );
    run = journal
        .repair_control(Command {
            op_id: Uuid::now_v7(),
            id: run.id,
            revision: run.revision,
            action: Control::Resume,
        })
        .await?;
    run = advance_repair(&journal, &run, Event::Prepare).await?;
    let resumed = repair_host::open(
        &journal,
        &mut run,
        &workspace,
        repair_host::Launch {
            one_time: None,
            project_secrets: Default::default(),
            worker: &config,
            directory: &host_dir,
            rpc: &rpc,
        },
    )
    .await?;
    assert_eq!(run.session, session);
    assert_ne!(resumed.identity(), fix_identity);
    run = resumed.close(&journal, Outcome::Stopped).await?;
    assert_eq!(run.phase, Phase::Held);
    assert!(intercat_daemon::process::is_alive(main_identity));
    main.shutdown().await?;
    assert!(!intercat_daemon::process::is_alive(main_identity));
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let turns: i64 = db.query_row(
        "SELECT count(*) FROM operations WHERE kind='turn.start'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(turns, 0);
    let sessions: i64 = db.query_row(
        "SELECT count(*) FROM operations WHERE kind='session/start' AND state='complete'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(sessions, 1);
    let resumed_sessions: i64 = db.query_row(
        "SELECT count(*) FROM operations WHERE kind='session/resume' AND state='complete'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(resumed_sessions, 1);
    for directory in [
        f.root.path().join("main-host"),
        f.root.path().join("fix-host-a"),
        f.root.path().join("fix-host-b"),
    ] {
        assert!(fs::symlink_metadata(directory.join("config/muse/auth.json")).is_err());
    }
    f.unchanged()?;
    Ok(())
}

struct RepairEvidence {
    source: PathBuf,
    target: PathBuf,
    saved: bool,
}

#[tokio::test]
#[ignore = "pinned Muse and saved authentication; no model turn"]
async fn real_repair_waits_for_project_secrets_and_apply_stops_only_its_host() -> Result<()> {
    use intercat_daemon::{
        host::{self, Host},
        repair_run::Phase,
        runtime::Config,
    };
    let (f, journal) = repair_run_fixture(false).await?;
    let mut evidence = RepairEvidence::new(&f, "project-apply")?;
    journal
        .configure_project(Uuid::now_v7(), project::inspect(f.source.clone()).await?)
        .await?;
    let binary = host::prepare_binary(
        &PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        &f.root.path().join("private-muse"),
    )?;
    let config = Config {
        source: binary.clone(),
        binary,
        credentials: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?),
        path: "/usr/bin:/bin:/usr/sbin:/sbin".into(),
    };
    journal
        .set_worker_config(Uuid::now_v7(), config.clone())
        .await?;
    let rpc = f.root.path().join("rpc");
    fs::create_dir(&rpc)?;
    let unrelated = Host::spawn(
        &config.binary,
        &f.root.path().join("unrelated-host"),
        &f.source,
        std::ffi::OsStr::new(&config.path),
        Some(&config.credentials),
        host::Boundary {
            profile: intercat_daemon::host::Profile::Worker,
            secrets: &[],
            rpc: &rpc,
            denied: &[],
        },
    )
    .await?;
    unrelated.connection.client.initialize().await?;
    let other = unrelated.identity;
    let secrets = intercat_daemon::project_secrets::Store::default();
    let root = scheduler_candidate(&f)?;
    let integration = intercat_daemon::integration::Service::new(journal.clone(), root.clone());
    let runtime = intercat_daemon::repair_service::Runtime::start(
        journal.clone(),
        root,
        f.root.path().join("data"),
        rpc,
        std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
        integration.clone(),
        secrets.clone(),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    let result=async {
        let run=journal.repair_start(repair_request(&f)).await?;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let waiting=journal.repair_input(run.id).await?.0;
        assert_eq!(waiting.phase,Phase::Queued);assert!(waiting.host.is_none());
        project_secret_fixture(&journal,&secrets,1,Some("dummy-project-apply-secret")).await?;
        let ready=tokio::time::timeout(std::time::Duration::from_secs(30),async {
            loop {let run=journal.repair_input(run.id).await?.0;if run.session_ready{return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(run);}else if run.phase==Phase::Failed{return Err("Repair host failed.".into());}tokio::time::sleep(std::time::Duration::from_millis(20)).await;}
        }).await??;
        assert_eq!(ready.secret_revision,Some(1));
        assert!(journal.project_uses().await?.iter().any(|usage|usage.host.repair_run_id==Some(ready.id)));
        let identity=ready.host.unwrap();
        project_secret_fixture(&journal,&secrets,2,None).await?;
        runtime.service.apply_project_secrets(2).await?;
        let held=tokio::time::timeout(std::time::Duration::from_secs(20),async {
            loop {let current=journal.repair_input(run.id).await?.0;if current.phase==Phase::Held{return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(current);}tokio::time::sleep(std::time::Duration::from_millis(20)).await;}
        }).await??;
        assert!(!intercat_daemon::process::is_alive(identity));
        assert!(intercat_daemon::process::is_alive(other));
        assert!(held.host.is_none());assert_eq!(held.session,ready.session);
        assert!(journal.project_uses().await?.is_empty());
        f.unchanged()?;
        fs::write(evidence.target.join("result.json"),serde_json::to_vec_pretty(&serde_json::json!({"waitedForKeychain":true,"repairHostStopped":true,"unrelatedHostPreserved":true,"sourceUnchanged":true,"modelTurns":0,"ready":ready,"held":held}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    runtime.shutdown().await;
    unrelated.shutdown().await?;
    integration.shutdown().await;
    evidence.save()?;
    let db = rusqlite::Connection::open(evidence.target.join("events.sqlite"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM operations WHERE kind='turn.start'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    result
}

#[tokio::test]
#[ignore = "two paid dummy Muse turns with saved authentication"]
async fn real_repair_uses_project_secrets_and_resumes_without_a_removed_value() -> Result<()> {
    use intercat_daemon::{
        host, repair_execution, repair_host,
        repair_run::{Command, Control, Event, Phase},
        runtime::Config,
    };
    use sha2::{Digest, Sha256};
    let (f, journal) = repair_run_fixture(false).await?;
    let mut evidence = RepairEvidence::new(&f, "project-secrets")?;
    journal
        .configure_project(Uuid::now_v7(), project::inspect(f.source.clone()).await?)
        .await?;
    let secrets = intercat_daemon::project_secrets::Store::default();
    let value = format!("dummy-repair-project-{}", Uuid::now_v7());
    project_secret_fixture(&journal, &secrets, 1, Some(&value)).await?;
    let binary = host::prepare_binary(
        &PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        &f.root.path().join("private-muse"),
    )?;
    let config = Config {
        source: binary.clone(),
        binary,
        credentials: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?),
        path: "/usr/bin:/bin:/usr/sbin:/sbin".into(),
    };
    let rpc = f.root.path().join("rpc");
    fs::create_dir(&rpc)?;
    let mut run = journal.repair_start(repair_request(&f)).await?;
    run = advance_repair(&journal, &run, Event::Prepare).await?;
    let (_, job, settings) = journal.repair_input(run.id).await?;
    let workspace = std::sync::Arc::new(
        intercat_daemon::repair::Workspace::prepare(
            &job,
            &f.root.path().join("candidate"),
            &f.root.path().join("repair"),
            &run.track,
            &settings,
            watch::channel(false).1,
        )
        .await?,
    );
    let digest = Sha256::digest(value.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    fs::write(
        workspace.checkout.workdir.join("verify_token.py"),
        format!(
            "import os,hashlib\nfrom pathlib import Path\nv=os.environ.get('DEMO_TOKEN','')\nassert hashlib.sha256(v.encode()).hexdigest()=='{digest}'\nprint(v)\nPath('verified.txt').write_text('verified\\n')\n"
        ),
    )?;
    fs::write(
        workspace.checkout.workdir.join("verify_absent.py"),
        "import os\nfrom pathlib import Path\nassert 'DEMO_TOKEN' not in os.environ\nPath('absent.txt').write_text('absent\\n')\nprint('absent')\n",
    )?;
    let mut session = None;
    let mut identities = Vec::new();
    for pass in 1..=2 {
        if pass == 2 {
            project_secret_fixture(&journal, &secrets, 2, None).await?;
            run = journal
                .repair_control(Command {
                    op_id: Uuid::now_v7(),
                    id: run.id,
                    revision: run.revision,
                    action: Control::Resume,
                })
                .await?;
            run = advance_repair(&journal, &run, Event::Prepare).await?;
        }
        let mut hosted = repair_host::open(
            &journal,
            &mut run,
            &workspace,
            repair_host::Launch {
                one_time: None,
                worker: &config,
                directory: &f.root.path().join(format!("host-{pass}")),
                rpc: &rpc,
                project_secrets: secrets.clone(),
            },
        )
        .await?;
        assert_eq!(run.secret_revision, Some(pass));
        if pass == 1 {
            session = run.session.clone();
        } else {
            assert_eq!(run.session, session);
        }
        identities.push(hosted.identity());
        let (port, runner) = repair_execution::channel(&mut hosted, &run)?;
        let writer = journal.clone();
        let working = workspace.clone();
        let store = secrets.clone();
        let (_stop, shutdown) = watch::channel(false);
        let script = if pass == 1 {
            "verify_token.py"
        } else {
            "verify_absent.py"
        };
        let instruction = format!(
            "This is a disposable test repository. Run exactly /usr/bin/python3 {script} once using the bash tool, report its output, then end with the exact standalone line 'Waiting for you: Verification complete.' Do not inspect other files, edit files, use network services, or run any other command."
        );
        let mut running = tokio::spawn(async move {
            runner
                .execute(
                    hosted,
                    writer,
                    &working,
                    repair_execution::Task {
                        instruction,
                        capacity: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
                        shutdown,
                        project_secrets: store,
                    },
                )
                .await
        });
        let observed = tokio::time::timeout(std::time::Duration::from_secs(120), async {
            let mut allowed = std::collections::HashSet::new();
            loop {
                if running.is_finished() {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                }
                let current = journal.repair_input(run.id).await?.0;
                for card in current.approvals {
                    if !allowed.insert((card.id.clone(), card.requirement.to_string())) {
                        continue;
                    }
                    if card.subject != format!("/usr/bin/python3 {script}") {
                        return Err("Unexpected repair command.".into());
                    }
                    port.answer(Uuid::now_v7(), card.id, card.requirement, true)
                        .await?;
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
        })
        .await;
        if !matches!(&observed, Ok(Ok(()))) {
            let _ = port.stop(Uuid::now_v7());
        }
        run = tokio::time::timeout(std::time::Duration::from_secs(35), &mut running).await???;
        observed??;
        assert_eq!(run.phase, Phase::Held);
        assert!(!intercat_daemon::process::is_alive(
            *identities.last().unwrap()
        ));
        assert_eq!(
            fs::read_to_string(workspace.checkout.workdir.join(if pass == 1 {
                "verified.txt"
            } else {
                "absent.txt"
            }))?
            .trim(),
            if pass == 1 { "verified" } else { "absent" }
        );
    }
    assert_ne!(identities[0], identities[1]);
    f.unchanged()?;
    evidence.save()?;
    let bytes = fs::read(evidence.target.join("events.sqlite"))?;
    assert!(
        !bytes
            .windows(value.len())
            .any(|window| window == value.as_bytes())
    );
    let db = rusqlite::Connection::open(evidence.target.join("events.sqlite"))?;
    let turns: i64 = db.query_row(
        "SELECT count(*) FROM operations WHERE kind='turn.start'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(turns, 2);
    let masked: i64 = db.query_row(
        "SELECT count(*) FROM events WHERE payload LIKE '%<secret:DEMO_TOKEN>%'",
        [],
        |row| row.get(0),
    )?;
    assert!(masked > 0);
    fs::write(
        evidence.target.join("result.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"schema":intercat_daemon::journal::SCHEMA_VERSION,"turns":turns,"sameSession":true,"differentHosts":identities,"removedValueAbsent":true,"maskedEvents":masked,"rawOccurrences":0,"sourceUnchanged":true,"run":run}),
        )?,
    )?;
    Ok(())
}
impl RepairEvidence {
    fn new(f: &Fixture, name: &str) -> Result<Self> {
        let parent = PathBuf::from(std::env::var("INTERCAT_REPAIR_OUTPUT")?);
        let target = parent.join(format!("{name}-{}", Uuid::now_v7()));
        fs::create_dir_all(&target)?;
        Ok(Self {
            source: f.root.path().join("events.sqlite"),
            target,
            saved: false,
        })
    }
    fn save(&mut self) -> Result<()> {
        if !self.saved {
            let db = rusqlite::Connection::open(&self.source)?;
            db.busy_timeout(std::time::Duration::from_secs(5))?;
            db.execute(
                "VACUUM INTO ?1",
                [self.target.join("events.sqlite").to_str().unwrap()],
            )?;
            self.saved = true;
        }
        Ok(())
    }
}
impl Drop for RepairEvidence {
    fn drop(&mut self) {
        let _ = self.save();
    }
}

struct LiveRepair {
    run: intercat_daemon::repair_run::Run,
    workspace: std::sync::Arc<intercat_daemon::repair::Workspace>,
    hosted: intercat_daemon::repair_host::Hosted,
    config: intercat_daemon::runtime::Config,
}
async fn live_repair(
    f: &Fixture,
    journal: &intercat_daemon::journal::Journal,
) -> Result<LiveRepair> {
    use intercat_daemon::{host, repair_host, repair_run::Event, runtime::Config};
    let binary = PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?);
    let credentials = PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?);
    let private = host::prepare_binary(&binary, &f.root.path().join("private-muse"))?;
    let mut run = journal.repair_start(repair_request(f)).await?;
    run = advance_repair(journal, &run, Event::Prepare).await?;
    let (_, job, settings) = journal.repair_input(run.id).await?;
    let workspace = std::sync::Arc::new(
        intercat_daemon::repair::Workspace::prepare(
            &job,
            &f.root.path().join("candidate"),
            &f.root.path().join("repair"),
            &run.track,
            &settings,
            watch::channel(false).1,
        )
        .await?,
    );
    let rpc = f.root.path().join("rpc");
    fs::create_dir(&rpc)?;
    fs::set_permissions(&rpc, fs::Permissions::from_mode(0o700))?;
    let config = Config {
        source: private.clone(),
        binary: private,
        credentials,
        path: "/usr/bin:/bin:/usr/sbin:/sbin".into(),
    };
    let hosted = repair_host::open(
        journal,
        &mut run,
        &workspace,
        repair_host::Launch {
            one_time: None,
            project_secrets: Default::default(),
            worker: &config,
            directory: &f.root.path().join("host"),
            rpc: &rpc,
        },
    )
    .await?;
    Ok(LiveRepair {
        run,
        workspace,
        hosted,
        config,
    })
}

#[tokio::test]
#[ignore = "paid dummy-fixture Muse turn; requires explicit authorization and output directory"]
async fn real_repair_stop_interrupts_its_shell_and_preserves_another_host() -> Result<()> {
    use intercat_daemon::{
        host::{self, Host},
        repair_execution,
        repair_run::Phase,
    };
    let (f, journal) = repair_run_fixture(false).await?;
    let mut evidence = RepairEvidence::new(&f, "stop")?;
    let mut live = live_repair(&f, &journal).await?;
    let main = Host::spawn(
        &live.config.binary,
        &f.root.path().join("main-host"),
        &f.source,
        std::ffi::OsStr::new(&live.config.path),
        Some(&live.config.credentials),
        host::Boundary {
            profile: intercat_daemon::host::Profile::Worker,
            secrets: &[],
            rpc: &f.root.path().join("rpc"),
            denied: &[],
        },
    )
    .await?;
    main.connection.client.initialize().await?;
    let main_identity = main.identity;
    let repair_identity = live.hosted.identity();
    let mut tracker = intercat_daemon::process::Tracker::new(
        live.run.host_nonce.unwrap().to_string(),
        repair_identity,
    )?;
    let (port, runner) = repair_execution::channel(&mut live.hosted, &live.run)?;
    let (_shutdown, shutdown) = watch::channel(false);
    let workspace = live.workspace.clone();
    let writer = journal.clone();
    let mut running = tokio::spawn(async move {
        runner.execute(live.hosted,writer,&workspace,repair_execution::Task{project_secrets: Default::default(),instruction:"This is a disposable test repository. Work only in this workspace. Run exactly this bash command:\n/usr/bin/touch stop-marker; /bin/sleep 60\nWait for it to finish before your final answer. Do not edit other files, commit, or use network services.".into(),capacity:std::sync::Arc::new(tokio::sync::Semaphore::new(1)),shutdown}).await
    });
    let observed = tokio::time::timeout(std::time::Duration::from_secs(110), async {
        loop {
            if live.workspace.checkout.workdir.join("stop-marker").exists() {
                let children = tracker
                    .refresh()?
                    .processes
                    .into_iter()
                    .map(|p| p.identity)
                    .filter(|id| *id != repair_identity && intercat_daemon::process::is_alive(*id))
                    .collect::<Vec<_>>();
                if children.iter().any(|id| {
                    libproc::proc_pid::name(id.pid as i32).is_ok_and(|name| name == "sleep")
                }) {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(children);
                }
            }
            if running.is_finished() {
                return Err("The repair ended before its shell could be stopped.".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;
    let _ = port.stop(Uuid::now_v7());
    let completed =
        tokio::time::timeout(std::time::Duration::from_secs(35), &mut running).await???;
    let other_alive = intercat_daemon::process::is_alive(main_identity);
    main.shutdown().await?;
    evidence.save()?;
    let children = observed??;
    assert_eq!(completed.phase, Phase::Held, "{:?}", completed.output);
    assert!(other_alive);
    assert!(!intercat_daemon::process::is_alive(repair_identity));
    assert!(
        children
            .iter()
            .all(|id| !intercat_daemon::process::is_alive(*id))
    );
    f.unchanged()?;
    fs::write(
        evidence.target.join("result.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"run":completed,"children":children,"otherHostPreserved":other_alive,"sourceUnchanged":true}),
        )?,
    )?;
    Ok(())
}

#[tokio::test]
#[ignore = "paid dummy-fixture Muse turn; requires explicit authorization and output directory"]
async fn real_repair_push_waits_for_its_own_approval_and_denial_preserves_remote() -> Result<()> {
    use intercat_daemon::repair_execution;
    let (f, journal) = repair_run_fixture(false).await?;
    let mut evidence = RepairEvidence::new(&f, "approval")?;
    let mut live = live_repair(&f, &journal).await?;
    let remote = f.root.path().join("remote.git");
    git(
        f.root.path(),
        &["init", "--bare", "-q", remote.to_str().unwrap()],
        None,
    );
    let before = git(&remote, &["for-each-ref", "--format=%(refname)"], None);
    let run_id = live.run.id;
    let (port, runner) = repair_execution::channel(&mut live.hosted, &live.run)?;
    let (_shutdown, shutdown) = watch::channel(false);
    let workspace = live.workspace.clone();
    let writer = journal.clone();
    let instruction = format!(
        "This is a disposable local Git test. Use bash to run exactly this command:\ngit push '{}' HEAD:refs/heads/fixture\nThe remote is a disposable local bare repository. Wait for that command's result; do not substitute another operation or write to the remote in another way.",
        remote.display()
    );
    let mut running = tokio::spawn(async move {
        runner
            .execute(
                live.hosted,
                writer,
                &workspace,
                repair_execution::Task {
                    project_secrets: Default::default(),
                    instruction,
                    capacity: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
                    shutdown,
                },
            )
            .await
    });
    let card = tokio::time::timeout(std::time::Duration::from_secs(110), async {
        loop {
            let run = journal.repair_input(run_id).await?.0;
            if let Some(card) = run.approvals.first() {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(card.clone());
            }
            if running.is_finished() {
                return Err("The repair ended without an approval card.".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await;
    let denied = if let Ok(Ok(card)) = &card {
        Some(
            port.answer(
                Uuid::now_v7(),
                card.id.clone(),
                card.requirement.clone(),
                false,
            )
            .await,
        )
    } else {
        None
    };
    let _ = port.stop(Uuid::now_v7());
    let completed =
        tokio::time::timeout(std::time::Duration::from_secs(35), &mut running).await???;
    evidence.save()?;
    let card = card??;
    denied.unwrap()?;
    assert_eq!(card.reason.kind, "push");
    assert_eq!(
        git(&remote, &["for-each-ref", "--format=%(refname)"], None),
        before
    );
    f.unchanged()?;
    fs::write(
        evidence.target.join("result.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"run":completed,"approval":card,"remoteUnchanged":true,"sourceUnchanged":true}),
        )?,
    )?;
    Ok(())
}

#[tokio::test]
#[ignore = "paid dummy-fixture Muse turn; requires explicit authorization and output directory"]
async fn real_repair_turn_fixes_candidate_and_rechecks_without_changing_original() -> Result<()> {
    use intercat_daemon::{
        host, repair_execution, repair_host,
        repair_run::{Event, Phase},
        runtime::Config,
    };
    let binary = PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?);
    let credentials = PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?);
    let (f, journal) = repair_run_fixture(false).await?;
    let mut evidence = RepairEvidence::new(&f, "fix")?;
    let private = host::prepare_binary(&binary, &f.root.path().join("private-muse"))?;
    let mut run = journal.repair_start(repair_request(&f)).await?;
    run = advance_repair(&journal, &run, Event::Prepare).await?;
    let (_, job, settings) = journal.repair_input(run.id).await?;
    let workspace = std::sync::Arc::new(
        intercat_daemon::repair::Workspace::prepare(
            &job,
            &f.root.path().join("candidate"),
            &f.root.path().join("repair"),
            &run.track,
            &settings,
            watch::channel(false).1,
        )
        .await?,
    );
    let rpc = f.root.path().join("rpc");
    fs::create_dir(&rpc)?;
    fs::set_permissions(&rpc, fs::Permissions::from_mode(0o700))?;
    let config = Config {
        source: private.clone(),
        binary: private,
        credentials,
        path: "/usr/bin:/bin:/usr/sbin:/sbin".into(),
    };
    let mut hosted = repair_host::open(
        &journal,
        &mut run,
        &workspace,
        repair_host::Launch {
            one_time: None,
            project_secrets: Default::default(),
            worker: &config,
            directory: &f.root.path().join("host"),
            rpc: &rpc,
        },
    )
    .await?;
    let (port, runner) = repair_execution::channel(&mut hosted, &run)?;
    let (_shutdown, shutdown) = watch::channel(false);
    let writer = journal.clone();
    let working = workspace.clone();
    let mut running = tokio::spawn(async move {
        runner.execute(hosted,writer,&working,repair_execution::Task{project_secrets: Default::default(),instruction:"This is a disposable test repository. First send a Japanese progress update of about 100 characters describing the planned edit and verification. Work only in this workspace. Replace file.txt with exactly the single line fixed followed by a newline. Use the file editing tools. Keep other.txt unchanged. Do not make Git commits, start background processes, or access network services. Finish with an English report describing what changed, why, how it was verified, and any remaining work.".into(),capacity:std::sync::Arc::new(tokio::sync::Semaphore::new(1)),shutdown}).await
    });
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let mut previews = Vec::new();
    let observed = async {
        loop {
            tokio::select! {
                result = &mut running => break result,
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                    if let Some(preview) = port.progress() {
                        let completed_items: i64 = db.query_row("SELECT count(*) FROM events WHERE kind='msp.applied' AND json_extract(payload,'$.method')='item/completed' AND json_extract(payload,'$.params.item.itemId')=?1", [&preview.item_id], |row| row.get(0)).unwrap();
                        let value = serde_json::json!({"preview":preview,"completedItems":completed_items});
                        if previews.last() != Some(&value) && previews.len() < 64 { previews.push(value); }
                    }
                }
            }
        }
    };
    let completed = match tokio::time::timeout(std::time::Duration::from_secs(150), observed).await
    {
        Ok(result) => result??,
        Err(_) => {
            let _ = port.stop(Uuid::now_v7());
            let _ = tokio::time::timeout(std::time::Duration::from_secs(35), &mut running).await;
            evidence.save()?;
            fs::write(
                evidence.target.join("previews.json"),
                serde_json::to_vec_pretty(&previews)?,
            )?;
            return Err("The dummy repair turn timed out.".into());
        }
    };
    evidence.save()?;
    fs::write(
        evidence.target.join("previews.json"),
        serde_json::to_vec_pretty(&previews)?,
    )?;
    assert!(
        previews.iter().any(|sample| {
            let text = sample["preview"]["text"].as_str().unwrap();
            let complete: rusqlite::Result<String> = db.query_row("SELECT json_extract(payload,'$.params.item.text') FROM events WHERE kind='msp.applied' AND json_extract(payload,'$.method')='item/completed' AND json_extract(payload,'$.params.item.itemId')=?1 ORDER BY seq DESC LIMIT 1", [sample["preview"]["itemId"].as_str().unwrap()], |row| row.get(0));
            sample["completedItems"] == 0 && complete.is_ok_and(|full| full.starts_with(text) && full.len() > text.len())
        }),
        "No partial preview arrived before item completion"
    );
    assert_eq!(completed.phase, Phase::Captured, "{:?}", completed.output);
    assert!(
        completed
            .output
            .as_ref()
            .is_some_and(|text| !text.is_empty())
    );
    assert_eq!(
        fs::read_to_string(workspace.checkout.workdir.join("file.txt"))?,
        "fixed\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.checkout.workdir.join("other.txt"))?,
        "B\n"
    );
    let plan = workspace
        .finish(
            &job,
            &f.root.path().join("candidate"),
            &settings,
            watch::channel(false).1,
        )
        .await?;
    let rebuilt = workspace
        .rebuild(
            &job,
            &plan,
            &f.root.path().join("rebuilt"),
            &f.root.path().join("downloads"),
            &settings,
            watch::channel(false).1,
        )
        .await?;
    assert_eq!(rebuilt.state, State::Checked, "{:?}", rebuilt.reason);
    assert_eq!(rebuilt.tree, plan.tree);
    f.unchanged()?;
    fs::write(
        evidence.target.join("result.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"run":completed,"candidate":rebuilt,"sourceUnchanged":true}),
        )?,
    )?;
    Ok(())
}

#[tokio::test]
async fn repair_replays_later_contributions_without_putting_them_in_the_owners_commit() -> Result<()>
{
    let mut f = Fixture::new(&[]).await?;
    f.track("A", &[("file.txt", Some("broken\n"), false)])?;
    f.track("B", &[("other.txt", Some("B contribution\n"), false)])?;
    let settings = f.settings(Some("test \"$(cat file.txt)\" = fixed"))?;
    let (repair, candidate_root) = repair_fixture(&mut f, &settings, 0).await?;
    let denied = repair
        .checkout
        .run_check(
            &format!(
                "/bin/cat '{}' '{}'",
                f.source.join("file.txt").display(),
                candidate_root.join("repo/file.txt").display()
            ),
            watch::channel(false).1,
            std::time::Duration::from_secs(5),
        )
        .await?;
    assert!(!denied.success());
    assert!(denied.stdout.is_empty());
    let old_index = fs::read(candidate_root.join("repo/.git/index"))?;
    fs::write(repair.checkout.workdir.join("file.txt"), "fixed\n")?;
    let reopened = intercat_daemon::repair::Workspace::resume(
        &f.job,
        &candidate_root,
        &repair.checkout.root,
        &f.job.tracks[0].id,
        &settings,
    )
    .await?;
    assert_eq!(
        fs::read_to_string(reopened.checkout.workdir.join("file.txt"))?,
        "fixed\n"
    );
    let plan = repair
        .finish(&f.job, &candidate_root, &settings, watch::channel(false).1)
        .await?;
    plan.validate(&f.job, &settings)?;
    assert_eq!(plan.applied_track, f.job.tracks[0].id);
    assert_eq!(plan.steps.len(), 2);
    let repo = Repository::open(&repair.checkout.workdir)?;
    let a = repo.tree_files(plan.steps[0].tree.parse()?)?;
    let b = repo.tree_files(plan.steps[1].tree.parse()?)?;
    assert_eq!(repo.saved_bytes(&a["file.txt"])?, b"fixed\n");
    assert_eq!(repo.saved_bytes(&a["other.txt"])?, b"other\n");
    assert_eq!(repo.saved_bytes(&b["file.txt"])?, b"fixed\n");
    assert_eq!(repo.saved_bytes(&b["other.txt"])?, b"B contribution\n");
    assert_eq!(plan.steps.last().unwrap().tree, plan.tree);
    assert_eq!(fs::read(candidate_root.join("repo/.git/index"))?, old_index);
    assert_eq!(
        fs::read_to_string(candidate_root.join("repo/file.txt"))?,
        "broken\n"
    );
    assert!(
        Repository::open(&f.source)?
            .tree_files(plan.tree.parse()?)
            .is_err()
    );
    let rebuilt = repair
        .rebuild(
            &f.job,
            &plan,
            &f.root.path().join("rebuilt"),
            &f.root.path().join("downloads"),
            &settings,
            watch::channel(false).1,
        )
        .await?;
    assert_eq!(rebuilt.state, State::Checked, "{:?}", rebuilt.reason);
    assert_eq!(rebuilt.tree, plan.tree);
    assert_eq!(rebuilt.check.as_ref().unwrap().tree, plan.tree);
    assert_eq!(rebuilt.commits.len(), 2);
    assert!(
        rebuilt
            .secret_scans
            .iter()
            .all(|scan| scan.findings.is_empty())
    );
    assert_eq!(rebuilt.commits[0].author, "Worker A");
    assert_eq!(rebuilt.commits[1].parent, rebuilt.commits[0].sha);
    assert_eq!(
        rebuilt.repair.as_ref().unwrap().applied_track,
        f.job.tracks[0].id
    );
    let mut reviewed = f.job.clone();
    reviewed.status = Status::Checked;
    reviewed.candidate = Some(rebuilt);
    let intent = intercat_daemon::publication::intent(&reviewed, Uuid::now_v7())?;
    assert_eq!(
        intent.refs[0].new,
        reviewed.candidate.as_ref().unwrap().head
    );
    assert_eq!(
        intent
            .refs
            .iter()
            .filter(|r| r.name.contains("/integration/"))
            .count(),
        2
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_moves_to_the_first_later_commit_that_contains_its_dependency() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    f.track("A", &[("file.txt", Some("A\n"), false)])?;
    f.track("B", &[("api.txt", Some("old API\n"), false)])?;
    f.track("C", &[("other.txt", Some("C\n"), false)])?;
    let settings = f.settings(Some("exit 1"))?;
    let (repair, candidate_root) = repair_fixture(&mut f, &settings, 0).await?;
    fs::write(repair.checkout.workdir.join("api.txt"), "fixed API\n")?;
    let plan = repair
        .finish(&f.job, &candidate_root, &settings, watch::channel(false).1)
        .await?;
    assert_eq!(plan.requested_track, f.job.tracks[0].id);
    assert_eq!(plan.applied_track, f.job.tracks[1].id);
    assert_eq!(plan.steps[0].tree, plan.basis.commits[0].tree);
    let repo = Repository::open(&repair.checkout.workdir)?;
    let b = repo.tree_files(plan.steps[1].tree.parse()?)?;
    assert_eq!(repo.saved_bytes(&b["api.txt"])?, b"fixed API\n");
    assert_eq!(repo.saved_bytes(&b["other.txt"])?, b"other\n");
    let c = repo.tree_files(plan.tree.parse()?)?;
    assert_eq!(repo.saved_bytes(&c["other.txt"])?, b"C\n");
    let rebuilt = repair
        .rebuild(
            &f.job,
            &plan,
            &f.root.path().join("rebuilt"),
            &f.root.path().join("downloads"),
            &settings,
            watch::channel(false).1,
        )
        .await?;
    assert_eq!(rebuilt.state, State::CheckFailed);
    assert_eq!(rebuilt.tree, plan.tree);
    assert_eq!(rebuilt.commits.len(), 3);
    assert_eq!(rebuilt.commits[0].sha, plan.basis.commits[0].sha);
    assert_ne!(rebuilt.commits[1].sha, plan.basis.commits[1].sha);
    assert_ne!(rebuilt.commits[2].sha, plan.basis.commits[2].sha);
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_captures_deletion_binary_mode_and_rename_without_importing_objects() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    f.track("A", &[("file.txt", Some("A\n"), false)])?;
    f.track("B", &[("βeta.txt", Some("remove me\n"), false)])?;
    let settings = f.settings(Some("exit 1"))?;
    let (repair, candidate_root) = repair_fixture(&mut f, &settings, 0).await?;
    let dir = &repair.checkout.workdir;
    fs::remove_file(dir.join("βeta.txt"))?;
    fs::rename(dir.join("file.txt"), dir.join("renamed.txt"))?;
    fs::write(dir.join("data.bin"), [0, 255, 1, 2])?;
    executable(&dir.join("run.sh"), "#!/bin/sh\nexit 0\n")?;
    fs::write(dir.join(".env"), "EXCLUDED=fixture\n")?;
    let plan = repair
        .finish(&f.job, &candidate_root, &settings, watch::channel(false).1)
        .await?;
    assert_eq!(plan.applied_track, f.job.tracks[1].id);
    assert!(plan.left_out.contains(&".env".to_owned()));
    let repo = Repository::open(dir)?;
    let files = repo.tree_files(plan.tree.parse()?)?;
    assert!(!files.contains_key("file.txt"));
    assert!(!files.contains_key("βeta.txt"));
    assert!(!files.contains_key(".env"));
    assert_eq!(repo.saved_bytes(&files["renamed.txt"])?, b"A\n");
    assert_eq!(repo.saved_bytes(&files["data.bin"])?, [0, 255, 1, 2]);
    assert_eq!(files["run.sh"].mode, 0o100755);
    assert!(
        Repository::open(&f.source)?
            .tree_files(plan.tree.parse()?)
            .is_err()
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_of_a_secret_blocked_commit_never_exposes_its_objects_to_the_project() -> Result<()>
{
    let mut f = Fixture::new(&[]).await?;
    let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    f.track(
        "A",
        &[("fixture.txt", Some(&format!("token = {token}\n")), false)],
    )?;
    let settings = f.settings(None)?;
    let (repair, candidate_root) = repair_fixture(&mut f, &settings, 0).await?;
    let blocked = f
        .job
        .candidate
        .as_ref()
        .unwrap()
        .blocked_commit
        .as_ref()
        .unwrap();
    assert!(
        Repository::open(&f.source)?
            .commit_details(blocked.sha.parse()?)
            .is_err()
    );
    assert!(
        Repository::open(&repair.checkout.workdir)?
            .commit_details(f.base.parse()?)
            .is_err()
    );
    fs::write(
        repair.checkout.workdir.join("fixture.txt"),
        "token = process.env.TOKEN\n",
    )?;
    let plan = repair
        .finish(&f.job, &candidate_root, &settings, watch::channel(false).1)
        .await?;
    assert_eq!(plan.basis.head, blocked.sha);
    assert_eq!(plan.applied_track, f.job.tracks[0].id);
    assert!(!serde_json::to_string(&plan)?.contains(token));
    let repo = Repository::open(&repair.checkout.workdir)?;
    let files = repo.tree_files(plan.tree.parse()?)?;
    assert!(!String::from_utf8(repo.saved_bytes(&files["fixture.txt"])?)?.contains(token));
    assert!(
        Repository::open(&f.source)?
            .commit_details(blocked.sha.parse()?)
            .is_err()
    );
    assert!(
        Repository::open(&f.source)?
            .tree_files(plan.tree.parse()?)
            .is_err()
    );
    let rebuilt = repair
        .rebuild(
            &f.job,
            &plan,
            &f.root.path().join("rebuilt"),
            &f.root.path().join("downloads"),
            &settings,
            watch::channel(false).1,
        )
        .await?;
    assert_eq!(rebuilt.state, State::Checked, "{:?}", rebuilt.reason);
    assert_eq!(rebuilt.tree, plan.tree);
    assert_eq!(rebuilt.commits.len(), 1);
    assert!(
        rebuilt
            .secret_scans
            .iter()
            .all(|scan| scan.findings.is_empty())
    );
    assert!(
        Repository::open(&f.source)?
            .commit_details(blocked.sha.parse()?)
            .is_err()
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_does_not_rerun_hooks_of_the_unchanged_prefix() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    executable(
        &f.source.join(".git/hooks/pre-commit"),
        "#!/bin/sh\nif [ \"$(git log -1 --format=%s)\" = base ]; then\n/usr/bin/uuidgen > build-id.txt\ngit add build-id.txt\nfi\n",
    )?;
    f.track("A", &[("file.txt", Some("A\n"), false)])?;
    f.track("B", &[("api.txt", Some("broken\n"), false)])?;
    let settings = f.settings(Some("test \"$(cat api.txt)\" = fixed"))?;
    let (repair, candidate_root) = repair_fixture(&mut f, &settings, 1).await?;
    let build_id = fs::read(repair.checkout.workdir.join("build-id.txt"))?;
    fs::write(repair.checkout.workdir.join("api.txt"), "fixed\n")?;
    let plan = repair
        .finish(&f.job, &candidate_root, &settings, watch::channel(false).1)
        .await?;
    let rebuilt = repair
        .rebuild(
            &f.job,
            &plan,
            &f.root.path().join("rebuilt"),
            &f.root.path().join("downloads"),
            &settings,
            watch::channel(false).1,
        )
        .await?;
    assert_eq!(rebuilt.state, State::Checked, "{:?}", rebuilt.reason);
    assert_eq!(rebuilt.commits[0].sha, plan.basis.commits[0].sha);
    assert_eq!(
        fs::read(f.root.path().join("rebuilt/repo/build-id.txt"))?,
        build_id
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_requires_review_when_a_rebuilt_hook_changes_its_tree() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    executable(
        &f.source.join(".git/hooks/pre-commit"),
        "#!/bin/sh\nif [ \"$(git log -1 --format=%s)\" = base ]; then\n/usr/bin/uuidgen > build-id.txt\ngit add build-id.txt\nfi\n",
    )?;
    f.track("A", &[("file.txt", Some("broken\n"), false)])?;
    f.track("B", &[("other.txt", Some("B\n"), false)])?;
    let settings = f.settings(Some("test \"$(cat file.txt)\" = fixed"))?;
    let (repair, candidate_root) = repair_fixture(&mut f, &settings, 0).await?;
    fs::write(repair.checkout.workdir.join("file.txt"), "fixed\n")?;
    let plan = repair
        .finish(&f.job, &candidate_root, &settings, watch::channel(false).1)
        .await?;
    let rebuilt = repair
        .rebuild(
            &f.job,
            &plan,
            &f.root.path().join("rebuilt"),
            &f.root.path().join("downloads"),
            &settings,
            watch::channel(false).1,
        )
        .await?;
    assert_eq!(rebuilt.state, State::RepairChanged);
    assert_ne!(rebuilt.tree, plan.tree);
    assert!(rebuilt.check.is_none());
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_removing_a_whole_contribution_omits_the_empty_commit() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    f.track("A", &[("file.txt", Some("A\n"), false)])?;
    f.track("B", &[("broken.txt", Some("remove me\n"), false)])?;
    let settings = f.settings(Some("test ! -e broken.txt"))?;
    let (repair, candidate_root) = repair_fixture(&mut f, &settings, 1).await?;
    fs::remove_file(repair.checkout.workdir.join("broken.txt"))?;
    let plan = repair
        .finish(&f.job, &candidate_root, &settings, watch::channel(false).1)
        .await?;
    let rebuilt = repair
        .rebuild(
            &f.job,
            &plan,
            &f.root.path().join("rebuilt"),
            &f.root.path().join("downloads"),
            &settings,
            watch::channel(false).1,
        )
        .await?;
    assert_eq!(rebuilt.state, State::Checked, "{:?}", rebuilt.reason);
    assert_eq!(rebuilt.tree, plan.tree);
    assert_eq!(rebuilt.commits.len(), 1);
    assert_eq!(rebuilt.commits[0].sha, plan.basis.commits[0].sha);
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_keeps_its_remaining_trees_when_a_new_secret_is_explicitly_kept() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    f.track("A", &[("file.txt", Some("broken\n"), false)])?;
    f.track("B", &[("other.txt", Some("B\n"), false)])?;
    let settings = f.settings(Some("test \"$(cat file.txt)\" = fixed"))?;
    let (repair, candidate_root) = repair_fixture(&mut f, &settings, 0).await?;
    fs::write(repair.checkout.workdir.join("file.txt"), "fixed\n")?;
    fs::write(
        repair.checkout.workdir.join("fixture.txt"),
        concat!("token = ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H\n"),
    )?;
    let plan = repair
        .finish(&f.job, &candidate_root, &settings, watch::channel(false).1)
        .await?;
    let rebuilt_root = f.root.path().join("rebuilt");
    let blocked = repair
        .rebuild(
            &f.job,
            &plan,
            &rebuilt_root,
            &f.root.path().join("downloads"),
            &settings,
            watch::channel(false).1,
        )
        .await?;
    assert_eq!(blocked.state, State::SecretFound);
    let blocked_sha = blocked.blocked_commit.as_ref().unwrap().sha.clone();
    f.job.kept_secrets = blocked
        .secret_scans
        .last()
        .unwrap()
        .findings
        .iter()
        .map(|f| f.id.clone())
        .collect();
    f.job.candidate = Some(blocked);
    f.job.revision += 1;
    let rebuilt = candidate::build(
        &f.job,
        &rebuilt_root,
        &f.root.path().join("downloads"),
        &settings,
        &f.job.messages,
        watch::channel(false).1,
    )
    .await?;
    assert_eq!(rebuilt.state, State::Checked, "{:?}", rebuilt.reason);
    assert_eq!(rebuilt.tree, plan.tree);
    assert_eq!(rebuilt.commits[0].sha, blocked_sha);
    assert_eq!(rebuilt.commits.len(), 2);
    assert_eq!(rebuilt.commits[1].tree, plan.steps[1].tree);
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_of_a_secret_continues_with_tracks_not_built_before_the_block() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    f.track(
        "A",
        &[(
            "fixture.txt",
            Some(concat!(
                "token = ghp_",
                "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H\n"
            )),
            false,
        )],
    )?;
    f.track("B", &[("other.txt", Some("B\n"), false)])?;
    let settings = f.settings(None)?;
    let (repair, candidate_root) = repair_fixture(&mut f, &settings, 0).await?;
    fs::write(
        repair.checkout.workdir.join("fixture.txt"),
        "token = process.env.TOKEN\n",
    )?;
    let plan = repair
        .finish(&f.job, &candidate_root, &settings, watch::channel(false).1)
        .await?;
    assert_eq!(plan.steps.len(), 1);
    let rebuilt = repair
        .rebuild(
            &f.job,
            &plan,
            &f.root.path().join("rebuilt"),
            &f.root.path().join("downloads"),
            &settings,
            watch::channel(false).1,
        )
        .await?;
    assert_eq!(rebuilt.state, State::Checked, "{:?}", rebuilt.reason);
    assert_eq!(rebuilt.commits.len(), 2);
    assert_eq!(rebuilt.commits[0].tree, plan.tree);
    let repo = Repository::open(&f.source)?;
    let files = repo.tree_files(rebuilt.tree.parse()?)?;
    assert_eq!(repo.saved_bytes(&files["other.txt"])?, b"B\n");
    assert_eq!(
        repo.saved_bytes(&files["fixture.txt"])?,
        b"token = process.env.TOKEN\n"
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn repair_rejects_stale_inputs_unchanged_results_and_changed_heads() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    f.track("A", &[("file.txt", Some("A\n"), false)])?;
    let settings = f.settings(Some("exit 1"))?;
    let (repair, candidate_root) = repair_fixture(&mut f, &settings, 0).await?;
    assert!(
        repair
            .finish(&f.job, &candidate_root, &settings, watch::channel(false).1)
            .await
            .is_err()
    );
    fs::write(repair.checkout.workdir.join("file.txt"), "fixed\n")?;
    let plan = repair
        .finish(&f.job, &candidate_root, &settings, watch::channel(false).1)
        .await?;
    let mut stale = f.job.clone();
    stale.revision += 1;
    assert!(plan.validate(&stale, &settings).is_err());
    assert!(
        repair
            .finish(&stale, &candidate_root, &settings, watch::channel(false).1)
            .await
            .is_err()
    );
    let mut changed = settings.clone();
    changed.command = candidate::CheckCommand::Disabled;
    assert!(plan.validate(&f.job, &changed).is_err());
    let mut tampered = plan.clone();
    tampered.steps[0].tree = plan.basis.tree.clone();
    assert!(
        repair
            .rebuild(
                &f.job,
                &tampered,
                &f.root.path().join("tampered-rebuild"),
                &f.root.path().join("downloads"),
                &settings,
                watch::channel(false).1
            )
            .await
            .is_err()
    );
    assert!(!f.root.path().join("tampered-rebuild").exists());
    let (_, cancelled) = watch::channel(true);
    assert!(
        repair
            .finish(&f.job, &candidate_root, &settings, cancelled)
            .await
            .is_err()
    );
    git(
        &repair.checkout.workdir,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "foreign",
        ],
        None,
    );
    assert!(
        repair
            .finish(&f.job, &candidate_root, &settings, watch::channel(false).1)
            .await
            .is_err()
    );
    assert!(
        intercat_daemon::repair::Workspace::resume(
            &f.job,
            &candidate_root,
            &repair.checkout.root,
            &f.job.tracks[0].id,
            &settings
        )
        .await
        .is_err()
    );
    f.unchanged()?;
    Ok(())
}
struct Fixture {
    root: tempfile::TempDir,
    source: PathBuf,
    job: Job,
    index: Vec<u8>,
    base: String,
}
impl Fixture {
    async fn new(files: &[(&str, &str, bool)]) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        fs::create_dir(&source)?;
        git(&source, &["init", "-q", "-b", "main"], None);
        git(&source, &["config", "user.name", "Fixture"], None);
        git(
            &source,
            &["config", "user.email", "fixture@example.invalid"],
            None,
        );
        fs::write(source.join("file.txt"), "base\n")?;
        fs::write(source.join("other.txt"), "other\n")?;
        fs::write(
            source.join(".gitignore"),
            "ignored.fixture\n.hook-count\ncheck-ran\n",
        )?;
        for (name, text, exec) in files {
            let path = source.join(name);
            fs::create_dir_all(path.parent().unwrap())?;
            if *exec {
                executable(&path, text)?;
            } else {
                fs::write(path, text)?;
            }
        }
        git(&source, &["add", "."], None);
        git(&source, &["commit", "-qm", "base"], None);
        let base = git(&source, &["rev-parse", "HEAD"], None);
        let index = fs::read(source.join(".git/index"))?;
        let project = project::inspect(source.clone()).await?;
        let session = Uuid::now_v7();
        let slug = "candidate-fixture";
        let repo = Repository::open(&source)?;
        let marker = repo.write_blob(format!("Intercat session {session}\n").as_bytes())?;
        repo.sync_objects(&[marker])?;
        repo.compare_and_swap(&format!("refs/intercat/{slug}/session"), None, marker)?;
        let job = Job {
            id: Uuid::now_v7(),
            revision: 1,
            status: Status::Preparing,
            opened_at: 0,
            session_id: session.to_string(),
            slug: slug.into(),
            session_name: "Fixture session".into(),
            project,
            tracks: vec![],
            plan: None,
            error: None,
            candidate: None,
            messages: std::collections::BTreeMap::new(),
            publication: None,
            kept_secrets: Default::default(),
            contributions: Default::default(),
        };
        Ok(Self {
            root,
            source,
            job,
            index,
            base,
        })
    }
    fn track(&mut self, label: &str, changes: &[(&str, Option<&str>, bool)]) -> Result<()> {
        let repo = Repository::open(&self.source)?;
        let temporary = tempfile::tempdir()?;
        let index = temporary.path().join("index");
        git(&self.source, &["read-tree", &self.base], Some(&index));
        for (name, text, exec) in changes {
            if let Some(text) = text {
                let blob = repo.write_blob(text.as_bytes())?;
                let info = format!("{},{blob},{name}", if *exec { "100755" } else { "100644" });
                git(
                    &self.source,
                    &["update-index", "--add", "--cacheinfo", &info],
                    Some(&index),
                );
            } else {
                git(
                    &self.source,
                    &["update-index", "--force-remove", name],
                    Some(&index),
                );
            }
        }
        let tree: ObjectId = git(&self.source, &["write-tree"], Some(&index)).parse()?;
        let sha = repo.checkpoint_commit(tree, self.base.parse()?, "fixture checkpoint")?;
        let turn = Uuid::now_v7();
        let reference = format!(
            "refs/intercat/{}/tracks/{label}-checkpoints/{turn}",
            self.job.slug
        );
        repo.sync_objects(&[sha])?;
        repo.compare_and_swap(&reference, None, sha)?;
        self.job.tracks.push(Track {
            id: Uuid::now_v7().to_string(),
            label: label.into(),
            title: format!("Work on {label}"),
            initial_base: self.base.clone(),
            checkpoint: Some(intercat_daemon::checkpoint::Checkpoint {
                turn_id: turn,
                sha: sha.to_string(),
                reference,
                unmerged_paths: vec![],
                live_processes: false,
            }),
            checkpoint_count: 1,
            last_clip: Some(self.job.tracks.len() as i64 + 1),
            included: true,
            integration_count: 0,
        });
        Ok(())
    }
    fn settings(&mut self, command: Option<&str>) -> Result<Settings> {
        self.job.plan = Some(integration::freeze(&self.job)?);
        self.job.plan = Some(integration::plan(&self.job)?);
        Ok(Settings {
            scanner: intercat_daemon::secret_scan::Engine::capture()?,
            npm: None,
            registry_origins: vec!["https://registry.npmjs.org/".into()],
            command: command
                .map(|command| candidate::CheckCommand::Run(command.into()))
                .unwrap_or(candidate::CheckCommand::Disabled),
            timeout_ms: 10000,
            path: "/usr/bin:/bin:/usr/sbin:/sbin".into(),
            toolchain: vec![],
            checkout: Configuration::capture(&intercat_git::Repository::worktree_source(
                &self.source,
                None,
            )?)?,
            skip_hooks: false,
        })
    }
    async fn build(&self, settings: &Settings) -> Result<candidate::Candidate> {
        candidate::build(
            &self.job,
            &self.root.path().join(format!("build-{}", Uuid::now_v7())),
            &self.root.path().join("downloads"),
            settings,
            &BTreeMap::new(),
            watch::channel(false).1,
        )
        .await
    }
    fn unchanged(&self) -> Result<()> {
        assert_eq!(git(&self.source, &["rev-parse", "HEAD"], None), self.base);
        assert_eq!(fs::read(self.source.join(".git/index"))?, self.index);
        assert_eq!(fs::read_to_string(self.source.join("file.txt"))?, "base\n");
        assert_eq!(
            Repository::open(&self.source)?
                .reference_target(&format!("refs/heads/intercat/{}", self.job.slug))?,
            None
        );
        Ok(())
    }
}

#[tokio::test]
async fn creates_one_real_commit_per_track_with_author_trailer_and_matching_check_tree()
-> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    f.track(
        "A",
        &[
            ("file.txt", Some("A\n"), false),
            ("ignored.fixture", Some("forced\n"), false),
        ],
    )?;
    f.track("B", &[("other.txt", Some("B\n"), false)])?;
    let settings = f.settings(Some(
        "test \"$(cat file.txt)\" = A && test \"$(cat other.txt)\" = B && test -f ignored.fixture",
    ))?;
    let result = f.build(&settings).await?;
    assert_eq!(result.state, State::Checked, "{result:?}");
    assert_eq!(result.commits.len(), 2);
    assert_eq!(result.commits[0].parent, f.base);
    assert_eq!(result.commits[1].parent, result.commits[0].sha);
    assert_eq!(result.commits[0].author, "Worker A");
    assert_eq!(result.commits[1].author, "Worker B");
    assert!(
        result.commits[0]
            .message
            .contains("Intercat: Fixture session · candidate-fixture/A:1")
    );
    assert_eq!(result.check.as_ref().unwrap().tree, result.tree);
    assert_eq!(
        result.check.as_ref().unwrap().settings_hash,
        settings.hash()?
    );
    let repo = Repository::open(&f.source)?;
    assert_eq!(repo.tree_id(result.head.parse()?)?.to_string(), result.tree);
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn hook_build_id_and_commit_message_are_part_of_the_checked_candidate() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    executable(
        &f.source.join(".git/hooks/pre-commit"),
        "#!/bin/sh\nprintf x >> build-id.txt\ngit add build-id.txt\n",
    )?;
    executable(
        &f.source.join(".git/hooks/commit-msg"),
        "#!/bin/sh\nprintf '\\nHook reviewed\\n' >> \"$1\"\n",
    )?;
    f.track("A", &[("file.txt", Some("A\n"), false)])?;
    let settings = f.settings(Some("test \"$(cat build-id.txt)\" = x"))?;
    let result = f.build(&settings).await?;
    assert_eq!(result.state, State::Checked, "{result:?}");
    assert_eq!(result.commits.len(), 1);
    assert!(result.commits[0].message.ends_with("Hook reviewed\n"));
    assert_eq!(
        git(
            &f.source,
            &["show", &format!("{}:build-id.txt", result.head)],
            None
        ),
        "x"
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn tracked_hooks_use_the_candidate_version_and_deleted_hooks_stay_deleted() -> Result<()> {
    let old = "#!/bin/sh\nprintf old > version.txt\ngit add version.txt\n";
    let new = "#!/bin/sh\nprintf new > version.txt\ngit add version.txt\n";
    let mut f = Fixture::new(&[(".githooks/pre-commit", old, true)]).await?;
    git(&f.source, &["config", "core.hooksPath", ".githooks"], None);
    f.track(
        "A",
        &[
            ("file.txt", Some("A\n"), false),
            (".githooks/pre-commit", Some(new), true),
        ],
    )?;
    let settings = f.settings(Some("test \"$(cat version.txt)\" = new"))?;
    let result = f.build(&settings).await?;
    assert_eq!(result.state, State::Checked, "{result:?}");
    assert_eq!(
        fs::read_to_string(f.source.join(".githooks/pre-commit"))?,
        old
    );
    let mut deleted = Fixture::new(&[(".githooks/pre-commit", old, true)]).await?;
    git(
        &deleted.source,
        &["config", "core.hooksPath", ".githooks"],
        None,
    );
    deleted.track(
        "A",
        &[
            ("file.txt", Some("A\n"), false),
            (".githooks/pre-commit", None, false),
        ],
    )?;
    let settings = deleted.settings(Some("test ! -e version.txt"))?;
    assert_eq!(deleted.build(&settings).await?.state, State::Checked);
    f.unchanged()?;
    deleted.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn generated_husky_helpers_keep_their_relative_layout() -> Result<()> {
    let mut f = Fixture::new(&[(
        ".husky/pre-commit",
        "#!/bin/sh\nprintf husky > version.txt\ngit add version.txt\n",
        true,
    )])
    .await?;
    git(&f.source, &["config", "core.hooksPath", ".husky/_"], None);
    fs::create_dir(f.source.join(".husky/_"))?;
    executable(
        &f.source.join(".husky/_/pre-commit"),
        "#!/bin/sh\n. \"$(dirname \"$0\")/h\"\n",
    )?;
    fs::write(
        f.source.join(".husky/_/h"),
        "#!/bin/sh\nsh \"$(dirname \"$0\")/../pre-commit\"\n",
    )?;
    f.track("A", &[("file.txt", Some("A\n"), false)])?;
    let settings = f.settings(Some("test \"$(cat version.txt)\" = husky"))?;
    let result = f.build(&settings).await?;
    assert_eq!(result.state, State::Checked, "{result:?}");
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn hook_rejection_and_outside_hooks_stop_before_check_and_skip_is_explicit() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    f.track("A", &[("file.txt", Some("A\n"), false)])?;
    executable(
        &f.source.join(".git/hooks/pre-commit"),
        "#!/bin/sh\nexit 42\n",
    )?;
    let settings = f.settings(Some("exit 0"))?;
    let rejected = f.build(&settings).await?;
    assert_eq!(rejected.state, State::HookRejected);
    assert!(rejected.check.is_none());
    let outside = f.root.path().join("outside-hooks");
    fs::create_dir(&outside)?;
    executable(&outside.join("pre-commit"), "#!/bin/sh\nexit 42\n")?;
    git(
        &f.source,
        &["config", "core.hooksPath", outside.to_str().unwrap()],
        None,
    );
    let mut settings = f.settings(Some("exit 0"))?;
    let blocked = f.build(&settings).await?;
    assert_eq!(blocked.state, State::HooksOutside);
    assert!(blocked.check.is_none());
    let original_hash = settings.hash()?;
    settings.skip_hooks = true;
    assert_ne!(settings.hash()?, original_hash);
    let skipped = f.build(&settings).await?;
    assert_eq!(skipped.state, State::Checked);
    assert!(skipped.hooks_skipped);
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn check_failure_and_check_mutation_are_bound_to_the_candidate_tree() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    f.track("A", &[("file.txt", Some("A\n"), false)])?;
    let settings = f.settings(Some("exit 7"))?;
    let failed = f.build(&settings).await?;
    assert_eq!(failed.state, State::CheckFailed);
    assert_eq!(failed.check.as_ref().unwrap().code, Some(7));
    let mut settings = settings;
    settings.command = candidate::CheckCommand::Run("printf rewritten > file.txt".into());
    let mutated = f.build(&settings).await?;
    assert_eq!(mutated.state, State::CheckMutated);
    assert_eq!(mutated.check.as_ref().unwrap().code, Some(0));
    assert_eq!(
        git(
            &f.source,
            &["show", &format!("{}:file.txt", mutated.head)],
            None
        ),
        "A"
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn a_post_commit_change_is_caught_before_check_and_new_secret_paths_are_excluded()
-> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    f.track(
        "A",
        &[
            ("file.txt", Some("A\n"), false),
            (".env", Some("TEST_SECRET=fixture\n"), false),
        ],
    )?;
    let settings = f.settings(Some("test ! -e .env"))?;
    let clean = f.build(&settings).await?;
    assert_eq!(clean.state, State::Checked, "{clean:?}");
    assert_eq!(clean.left_out, vec![".env"]);
    executable(
        &f.source.join(".git/hooks/post-commit"),
        "#!/bin/sh\nprintf unstaged > file.txt\n",
    )?;
    let settings = f.settings(Some("exit 13"))?;
    let dirty = f.build(&settings).await?;
    assert_eq!(dirty.state, State::DirtyBeforeCheck);
    assert!(dirty.check.is_none());
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn dependencies_hold_the_candidate_instead_of_running_a_check_without_installation()
-> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    f.track(
        "A",
        &[(
            "package.json",
            Some(r#"{"dependencies":{"not-installed":"1.0.0"}}"#),
            false,
        )],
    )?;
    let settings = f.settings(Some("exit 0"))?;
    let result = f.build(&settings).await?;
    assert_eq!(result.state, State::EnvFailed);
    assert!(result.commits.is_empty());
    assert!(result.check.is_none());
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn a_hook_cannot_add_a_secret_path_after_the_staging_filter() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    executable(
        &f.source.join(".git/hooks/pre-commit"),
        "#!/bin/sh\nprintf TEST=fixture > .env\ngit add .env\n",
    )?;
    f.track("A", &[("file.txt", Some("A\n"), false)])?;
    let settings = f.settings(Some("exit 0"))?;
    let result = f.build(&settings).await?;
    assert_eq!(result.state, State::SecretFound);
    assert!(result.check.is_none());
    let probe = f.root.path().join("secret-probe");
    fs::write(&probe, "TEST=fixture")?;
    let blob = git(&f.source, &["hash-object", probe.to_str().unwrap()], None);
    let exists = Command::new("/usr/bin/git")
        .current_dir(&f.source)
        .args(["cat-file", "-e", &blob])
        .output()?;
    assert!(
        !exists.status.success(),
        "A rejected hook secret must not be imported into the source object store."
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn automatic_check_uses_the_manifest_after_hooks_have_changed_it() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    executable(
        &f.source.join(".git/hooks/pre-commit"),
        "#!/bin/sh\nprintf '%s' '{\"scripts\":{\"test\":\"exit 7\"}}' > package.json\ngit add package.json\n",
    )?;
    f.track("A", &[("file.txt", Some("A\n"), false)])?;
    let mut settings = f.settings(None)?;
    settings.command = candidate::CheckCommand::Auto;
    let path = std::env::var("PATH")?;
    settings.path = path.clone();
    let node = std::env::split_paths(&path)
        .map(|dir| dir.join("node"))
        .find(|p| p.is_file())
        .unwrap()
        .canonicalize()?;
    settings
        .toolchain
        .push(node.parent().unwrap().parent().unwrap().into());
    let result = f.build(&settings).await?;
    assert_eq!(result.state, State::CheckFailed, "{result:?}");
    assert_eq!(result.check.unwrap().command.as_deref(), Some("npm test"));
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn content_secret_stops_before_check_and_before_import_even_without_a_check() -> Result<()> {
    let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    let mut f = Fixture::new(&[]).await?;
    f.track(
        "A",
        &[("fixture.txt", Some(&format!("token = '{token}'\n")), false)],
    )?;
    let settings = f.settings(None)?;
    let result = f.build(&settings).await?;
    assert_eq!(result.state, State::SecretFound, "{result:?}");
    assert!(result.check.is_none());
    assert!(result.commits.is_empty());
    assert!(
        !Command::new("/usr/bin/git")
            .current_dir(&f.source)
            .args(["cat-file", "-e", &result.secret_scans[0].binding.commit])
            .output()?
            .status
            .success()
    );
    assert!(
        result.secret_scans[0]
            .findings
            .iter()
            .any(|finding| finding.path == "fixture.txt")
    );
    assert!(!serde_json::to_string(&result)?.contains(token));
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn hook_final_message_is_scanned_and_check_logs_are_redacted() -> Result<()> {
    let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
    let mut f = Fixture::new(&[]).await?;
    executable(
        &f.source.join(".git/hooks/commit-msg"),
        &format!("#!/bin/sh\nprintf '\\n{token}\\n' >> \"$1\"\n"),
    )?;
    f.track("A", &[("file.txt", Some("changed\n"), false)])?;
    let mut settings = f.settings(None)?;
    let rejected = f.build(&settings).await?;
    assert_eq!(rejected.state, State::SecretFound, "{rejected:?}");
    assert!(
        rejected.secret_scans[0]
            .findings
            .iter()
            .any(|finding| finding.path == "commit-message.txt")
    );
    assert!(!serde_json::to_string(&rejected)?.contains(token));
    settings.skip_hooks = true;
    settings.command = candidate::CheckCommand::Run(format!("printf '%s\\n' '{token}'; exit 1"));
    let failed = f.build(&settings).await?;
    assert_eq!(failed.state, State::CheckFailed, "{failed:?}");
    assert!(failed.check.as_ref().unwrap().stdout.contains("[redacted]"));
    assert!(!serde_json::to_string(&failed)?.contains(token));
    f.unchanged()?;
    Ok(())
}

async fn held_secret_repair(
    journal: &intercat_daemon::journal::Journal,
    f: &Fixture,
) -> Result<intercat_daemon::repair_run::Run> {
    fixture_secret_repair(journal, f, false).await
}
async fn fixture_secret_repair(
    journal: &intercat_daemon::journal::Journal,
    f: &Fixture,
    prompt: bool,
) -> Result<intercat_daemon::repair_run::Run> {
    use intercat_daemon::repair_run::{Event, Outcome};
    let mut run = journal.repair_start(repair_request(f)).await?;
    run = advance_repair(journal, &run, Event::Prepare).await?;
    run = advance_repair(journal, &run, Event::WorkspaceReady).await?;
    run = advance_repair(
        journal,
        &run,
        Event::HostStarted {
            host: intercat_daemon::process::Identity {
                pid: 999991,
                started_seconds: 1,
                started_micros: 0,
            },
            nonce: Uuid::now_v7(),
            secret_revision: None,
        },
    )
    .await?;
    run = advance_repair(
        journal,
        &run,
        Event::SessionReady {
            session: "repair-secret-session".into(),
        },
    )
    .await?;
    let command = Uuid::now_v7();
    run = advance_repair(journal, &run, Event::ReserveStart { command }).await?;
    run = advance_repair(
        journal,
        &run,
        Event::Started {
            command,
            turn: Uuid::now_v7(),
        },
    )
    .await?;
    if prompt {
        let question = intercat_daemon::questions::Prompt::parse(
            serde_json::json!({"sessionId":run.session,"userInputId":"repair-secret-prompt","turnId":run.turn,"itemId":"item","toolCallId":"call","toolName":"request_user_input","viewCursor":"1","questions":[{"id":"token","header":"Token","question":"Provide DEMO_TOKEN","selection":{"mode":"single"},"options":[{"label":"Use token"},{"label":"Stop"}]}]}),
        )?;
        return advance_repair(
            journal,
            &run,
            Event::Presentation {
                approvals: vec![],
                prompts: vec![question],
                output: None,
            },
        )
        .await;
    }
    run = advance_repair(
        journal,
        &run,
        Event::Presentation {
            approvals: vec![],
            prompts: vec![],
            output: Some("Waiting for you: Provide DEMO_TOKEN.".into()),
        },
    )
    .await?;
    advance_repair(
        journal,
        &run,
        Event::HostClosed {
            outcome: Outcome::Stopped,
        },
    )
    .await
}
async fn repair_secret_request(
    journal: &intercat_daemon::journal::Journal,
    run: &intercat_daemon::repair_run::Run,
    value: &str,
) -> Result<intercat_daemon::secret_input::Request> {
    use intercat_daemon::{
        secret_input::{Owner, Question, Request},
        secrets::{Key, Registration},
    };
    let session = run.session.clone().unwrap();
    journal
        .register_fingerprint(Registration {
            op_id: Uuid::now_v7(),
            track_id: run.track.clone(),
            session_id: session.clone(),
            name: "DEMO_TOKEN".into(),
            byte_len: value.len(),
            tag: Key::new([7; 32])
                .tag(value.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
            key_check: Key::new([7; 32])
                .check()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        })
        .await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    Ok(Request {
        op_id: Uuid::now_v7(),
        owner: Owner::Repair { run_id: run.id },
        track_id: run.track.clone(),
        session_id: session,
        turn_id: run.turn.unwrap(),
        question: Question::Report {
            id: run.id.to_string(),
            revision: run.revision as i64,
        },
        name: "DEMO_TOKEN".into(),
        value: value.into(),
    })
}
fn repair_secret_checkpoint(
    run: &intercat_daemon::repair_run::Run,
    request: Uuid,
) -> intercat_daemon::checkpoint::Checkpoint {
    intercat_daemon::checkpoint::Checkpoint {
        turn_id: run.turn.unwrap(),
        sha: "a".repeat(40),
        reference: format!("refs/intercat/repairs/{}-secrets/{}", run.id, request),
        unmerged_paths: vec![],
        live_processes: false,
    }
}
#[tokio::test]
async fn repair_secret_binds_the_report_and_host_without_queueing_work_input() -> Result<()> {
    use intercat_daemon::{repair_run::Event, secret_input::Owner};
    let (f, journal) = repair_run_fixture(false).await?;
    let held = held_secret_repair(&journal, &f).await?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    db.execute(
        "INSERT INTO msp_positions VALUES(?1,?2,NULL)",
        rusqlite::params![held.session, held.track],
    )?;
    let request = repair_secret_request(&journal, &held, "dummy-repair-secret").await?;
    let install = journal.prepare_secret(request).await?.unwrap();
    // Sharing a track does not share a delivery lock or destination.
    assert!(
        journal
            .repair_control(intercat_daemon::repair_run::Command {
                op_id: Uuid::now_v7(),
                id: held.id,
                revision: held.revision,
                action: intercat_daemon::repair_run::Control::Resume,
            })
            .await
            .is_err()
    );
    assert!(
        journal
            .repair_followup(intercat_daemon::repair_run::FollowUp {
                op_id: Uuid::now_v7(),
                id: held.id,
                revision: held.revision,
                text: "Unrelated follow-up".into(),
                selection: None,
            })
            .await
            .is_err()
    );
    let mut ordinary = repair_secret_request(&journal, &held, "dummy-repair-secret").await?;
    ordinary.owner = Owner::Work;
    ordinary.question = intercat_daemon::secret_input::Question::Msp {
        id: "work-prompt".into(),
        revision: "a".repeat(64),
    };
    assert!(journal.prepare_secret(ordinary).await?.is_some());
    journal.secret_accepted(install.target.op_id).await?;
    journal
        .queue_repair_secret(
            install.target.clone(),
            repair_secret_checkpoint(&held, install.target.op_id),
        )
        .await?;
    let mut run = journal.repair_input(held.id).await?.0;
    assert_eq!(run.secret_input, Some(install.target.op_id));
    assert_eq!(run.session, held.session);
    assert_eq!(run.instruction, held.instruction);
    run = advance_repair(&journal, &run, Event::Prepare).await?;
    let nonce = Uuid::now_v7();
    run = advance_repair(
        &journal,
        &run,
        Event::HostStarted {
            host: intercat_daemon::process::Identity {
                pid: 999992,
                started_seconds: 2,
                started_micros: 0,
            },
            nonce,
            secret_revision: None,
        },
    )
    .await?;
    run = advance_repair(
        &journal,
        &run,
        Event::SessionReady {
            session: held.session.clone().unwrap(),
        },
    )
    .await?;
    assert!(
        advance_repair(
            &journal,
            &run,
            Event::ReserveStart {
                command: Uuid::now_v7()
            }
        )
        .await
        .is_err()
    );
    assert!(
        journal
            .secret_ready(install.target.clone(), Uuid::now_v7().to_string())
            .await
            .is_err()
    );
    journal
        .secret_ready(install.target.clone(), nonce.to_string())
        .await?;
    run = advance_repair(
        &journal,
        &run,
        Event::ReserveStart {
            command: Uuid::now_v7(),
        },
    )
    .await?;
    assert_eq!(run.phase, intercat_daemon::repair_run::Phase::Running);
    assert_eq!(
        db.query_row("SELECT count(*) FROM utterances", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    assert!(
        !journal
            .snapshot()
            .await?
            .to_string()
            .contains("dummy-repair-secret")
    );
    journal.secret_expired(nonce.to_string()).await?;
    assert!(
        journal.snapshot().await?["secretInputs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["opId"] == install.target.op_id.to_string()
                && entry["state"] == "expired")
    );
    Ok(())
}
#[tokio::test]
async fn repair_secret_cancellation_and_restart_leave_the_report_held() -> Result<()> {
    for restart in [false, true] {
        let (f, journal) = repair_run_fixture(false).await?;
        let held = held_secret_repair(&journal, &f).await?;
        rusqlite::Connection::open(f.root.path().join("events.sqlite"))?.execute(
            "INSERT INTO msp_positions VALUES(?1,?2,NULL)",
            rusqlite::params![held.session, held.track],
        )?;
        let request = repair_secret_request(&journal, &held, "dummy-repair-secret").await?;
        let install = journal.prepare_secret(request).await?.unwrap();
        journal.secret_accepted(install.target.op_id).await?;
        journal
            .queue_repair_secret(
                install.target.clone(),
                repair_secret_checkpoint(&held, install.target.op_id),
            )
            .await?;
        if restart {
            drop(journal);
            let reopened = intercat_daemon::journal::Journal::open(
                f.root.path().join("events.sqlite"),
                intercat_daemon::secrets::Key::new([7; 32]),
            )
            .await?;
            assert_eq!(
                reopened.repair_input(held.id).await?.0.phase,
                intercat_daemon::repair_run::Phase::Held
            );
            assert_eq!(
                reopened.snapshot().await?["secretInputs"][0]["state"],
                "failed"
            );
        } else {
            journal.secret_failed(install.target.op_id).await?;
            assert_eq!(
                journal.repair_input(held.id).await?.0.phase,
                intercat_daemon::repair_run::Phase::Held
            );
            assert!(
                journal
                    .queue_repair_secret(
                        install.target.clone(),
                        repair_secret_checkpoint(&held, install.target.op_id)
                    )
                    .await
                    .is_err()
            );
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "three paid dummy Muse turns with saved authentication"]
async fn real_repair_report_secret_resumes_same_session_and_expires() -> Result<()> {
    real_repair_secret(false).await
}

#[tokio::test]
#[ignore = "three paid dummy Muse turns with saved authentication"]
async fn real_repair_prompt_secret_waits_for_settlement_and_resumes() -> Result<()> {
    real_repair_secret(true).await
}

async fn real_repair_secret(msp_question: bool) -> Result<()> {
    use intercat_daemon::{
        host,
        repair_run::{Command, Control, FollowUp, Phase},
        runtime::Config,
    };
    use sha2::{Digest, Sha256};
    let (f, journal) = repair_run_fixture(false).await?;
    let mut evidence = RepairEvidence::new(
        &f,
        if msp_question {
            "prompt-secret"
        } else {
            "report-secret"
        },
    )?;
    let root = scheduler_candidate(&f)?;
    let binary = host::prepare_binary(
        &PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        &f.root.path().join("private-muse"),
    )?;
    journal
        .set_worker_config(
            Uuid::now_v7(),
            Config {
                source: binary.clone(),
                binary,
                credentials: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?),
                path: "/usr/bin:/bin:/usr/sbin:/sbin".into(),
            },
        )
        .await?;
    let mut run = journal.repair_start(repair_request(&f)).await?;
    run = journal
        .repair_control(Command {
            op_id: Uuid::now_v7(),
            id: run.id,
            revision: run.revision,
            action: Control::Stop,
        })
        .await?;
    // Resolve the actual checkout path from the prepared host below; the task
    // uses only this relative script name, never a value in a prompt.
    let instruction = if msp_question {
        "This is a disposable test. Call request_user_input now to ask the user for DEMO_TOKEN, with two options: Use token / Stop. Wait for the user input; do not answer the question yourself. Do not run commands or edit files yet. After the user provides the token, run exactly /usr/bin/python3 verify_token.py once using bash, report its output, and end with the exact standalone line Waiting for you: Verification complete. Do not inspect other files, run other commands, or use network services."
    } else {
        "This is a disposable test. Do not run commands or edit files yet. Ask for DEMO_TOKEN using the exact final line 'Waiting for you: Provide DEMO_TOKEN to run verify_token.py.' After the user provides it, run exactly /usr/bin/python3 verify_token.py once using bash, report its output, and end with 'Waiting for you: Verification complete.' Do not run other commands, inspect other files, or use network services."
    };
    run = journal
        .repair_followup(FollowUp {
            op_id: Uuid::now_v7(),
            id: run.id,
            revision: run.revision,
            text: instruction.into(),
            selection: None,
        })
        .await?;
    let rpc = f.root.path().join("rpc");
    fs::create_dir(&rpc)?;
    let integration = integration::Service::new(journal.clone(), root.clone());
    let capacity = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
    let runtime = intercat_daemon::repair_service::Runtime::start(
        journal.clone(),
        root.clone(),
        f.root.path().join("data"),
        rpc,
        capacity.clone(),
        integration.clone(),
        Default::default(),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    let value = format!("dummy-repair-report-{}", Uuid::now_v7());
    let result=async {
        let ready=wait_repair(&journal,run.id,|run|run.session_ready||run.phase==Phase::Failed).await?;
        assert_eq!(ready.phase,Phase::Preparing);
        // The checkout implementation supplies its own fixed private directory.
        let actual=root.join(run.job_id.to_string()).join(format!("repair-{}",run.id));
        let workspace=intercat_daemon::repair::Workspace::resume(&journal.repair_input(run.id).await?.1,&root.join(run.job_id.to_string()).join(f.job.candidate.as_ref().unwrap().generation.to_string()),&actual,&run.track,&journal.repair_input(run.id).await?.2).await?;
        let work=&workspace.checkout.workdir;
        let digest=Sha256::digest(value.as_bytes()).iter().map(|b|format!("{b:02x}")).collect::<String>();
        fs::write(work.join("verify_token.py"),format!("import os,hashlib\nfrom pathlib import Path\nv=os.environ.get('DEMO_TOKEN','')\nassert hashlib.sha256(v.encode()).hexdigest()=='{digest}'\nprint(v)\nPath('verified.txt').write_text('verified\\n')\n"))?;
        fs::write(work.join("verify_absent.py"),"import os\nfrom pathlib import Path\nassert 'DEMO_TOKEN' not in os.environ\nPath('absent.txt').write_text('absent\\n')\nprint('absent')\n")?;
        capacity.add_permits(1);
        let first=wait_repair(&journal,run.id,|run|if msp_question { !run.prompts.is_empty() || matches!(run.phase,Phase::Failed|Phase::Held) } else { matches!(run.phase,Phase::Held|Phase::Failed) }).await?;
        assert_eq!(first.phase,if msp_question {Phase::Running} else {Phase::Held});
        if !msp_question {assert!(first.output.as_ref().unwrap().contains("Waiting for you:"));}
        let first_session=first.session.clone();
        let mut request=repair_secret_request(&journal,&first,&value).await?;
        if msp_question {let prompt=&first.prompts[0];request.question=intercat_daemon::secret_input::Question::Msp {id:prompt.request.user_input_id.clone(),revision:prompt.revision.clone()};}
        let id=request.op_id;
        let service=runtime.service.clone();
        let caller=tokio::spawn(async move {service.install_secret(request).await});
        // Losing the caller after durable admission cannot strand the value.
        for _ in 0..200 {
            if journal.snapshot().await?["secretInputs"].as_array().unwrap().iter().any(|entry|entry["opId"]==id.to_string()) {break;}
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        caller.abort();let _=caller.await;
        let mut observed_second=false;
        for pass in 0..2 {
            let script=if pass==0 {"verify_token.py"} else {"verify_absent.py"};
            let result=tokio::time::timeout(std::time::Duration::from_secs(140),async {
                let mut allowed=std::collections::HashSet::new();
                loop {
                    let current=journal.repair_input(run.id).await?.0;
                    if pass==0&&current.turn!=first.turn {observed_second=true;}
                    if observed_second && !current.prompts.is_empty() {return Err("The resumed repair asked for the supplied secret again.".into());}
                    if (pass==1||observed_second)&&current.phase==Phase::Held {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(current);}
                    if matches!(current.phase,Phase::Failed|Phase::NeedsRecovery) {return Err(format!("Repair failed: {:?}",current.failure).into());}
                    for card in current.approvals {
                        if allowed.insert((card.id.clone(),card.requirement.to_string())) {
                            if card.subject!=format!("/usr/bin/python3 {script}") {return Err(format!("Unexpected repair command: {}",card.subject).into());}
                            runtime.service.answer(Uuid::now_v7(),run.id,card.id,card.requirement,true).await?;
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                }
            }).await??;
            assert_eq!(result.session,first_session);
            if pass==0 {
                assert_eq!(fs::read_to_string(work.join("verified.txt"))?.trim(),"verified");
                assert!(result.checkpoint.is_some());
                assert_eq!(journal.snapshot().await?["secretInputs"][0]["state"],"expired");
                journal.repair_followup(FollowUp {op_id:Uuid::now_v7(),id:run.id,revision:result.revision,text:"Run exactly /usr/bin/python3 verify_absent.py once using bash, report its output, and end with 'Waiting for you: Verification complete.' Do not run other commands or inspect other files.".into(),selection:None}).await?;
            } else {assert_eq!(fs::read_to_string(work.join("absent.txt"))?.trim(),"absent");}
        }
        f.unchanged()?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let _ = runtime.service.stop(Uuid::now_v7(), run.id).await;
    runtime.shutdown().await;
    integration.shutdown().await;
    evidence.save()?;
    let db = rusqlite::Connection::open(evidence.target.join("events.sqlite"))?;
    let turns: i64 = db.query_row(
        "SELECT count(*) FROM operations WHERE kind='turn.start'",
        [],
        |r| r.get(0),
    )?;
    let masked: i64 = db.query_row(
        "SELECT count(*) FROM events WHERE payload LIKE '%<secret:DEMO_TOKEN>%'",
        [],
        |r| r.get(0),
    )?;
    let raw: i64 = db.query_row(
        "SELECT count(*) FROM events WHERE instr(payload,?1)>0",
        [&value],
        |r| r.get(0),
    )?;
    let unchanged = f.unchanged();
    fs::write(
        evidence.target.join("result.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"schema":intercat_daemon::journal::SCHEMA_VERSION,"mspQuestion":msp_question,"turns":turns,"maskedEvents":masked,"rawOccurrences":raw,"sameSession":result.is_ok().then_some(true),"removedValueAbsent":result.is_ok(),"sourceUnchanged":unchanged.is_ok(),"error":result.as_ref().err().map(ToString::to_string)}),
        )?,
    )?;
    result?;
    unchanged?;
    assert_eq!(turns, 3);
    assert!(masked > 0);
    assert_eq!(raw, 0);
    assert!(
        !fs::read(evidence.target.join("events.sqlite"))?
            .windows(value.len())
            .any(|bytes| bytes == value.as_bytes())
    );
    Ok(())
}

#[tokio::test]
async fn repair_stop_before_secret_acceptance_invalidates_the_pending_destination() -> Result<()> {
    use intercat_daemon::repair_run::{Command, Control};
    let (f, journal) = repair_run_fixture(false).await?;
    let held = held_secret_repair(&journal, &f).await?;
    rusqlite::Connection::open(f.root.path().join("events.sqlite"))?.execute(
        "INSERT INTO msp_positions VALUES(?1,?2,NULL)",
        rusqlite::params![held.session, held.track],
    )?;
    let request = repair_secret_request(&journal, &held, "dummy-repair-stop-race").await?;
    let install = journal.prepare_secret(request).await?.unwrap();
    let stopped = journal
        .repair_control(Command {
            op_id: Uuid::now_v7(),
            id: held.id,
            revision: held.revision,
            action: Control::Stop,
        })
        .await?;
    assert!(stopped.revision > held.revision);
    assert!(journal.secret_accepted(install.target.op_id).await.is_err());
    journal.secret_failed(install.target.op_id).await?;
    assert!(
        journal
            .queue_repair_secret(
                install.target.clone(),
                repair_secret_checkpoint(&held, install.target.op_id)
            )
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn repair_secret_host_failure_keeps_checkpoint_and_requires_reentry() -> Result<()> {
    let (f, journal) = repair_run_fixture(false).await?;
    let held = held_secret_repair(&journal, &f).await?;
    let root = scheduler_candidate(&f)?;
    let (_, job, settings) = journal.repair_input(held.id).await?;
    let parent = root.join(job.id.to_string());
    let workspace = intercat_daemon::repair::Workspace::prepare(
        &job,
        &parent.join(job.candidate.as_ref().unwrap().generation.to_string()),
        &parent.join(format!("repair-{}", held.id)),
        &held.track,
        &settings,
        watch::channel(false).1,
    )
    .await?;
    fs::write(
        workspace.checkout.workdir.join("retained.txt"),
        "keep these private repair edits\n",
    )?;
    invalid_worker(&f, &journal).await?;
    rusqlite::Connection::open(f.root.path().join("events.sqlite"))?.execute(
        "INSERT INTO msp_positions VALUES(?1,?2,NULL)",
        rusqlite::params![held.session, held.track],
    )?;
    let request = repair_secret_request(&journal, &held, "dummy-repair-host-failure").await?;
    let service = integration::Service::new(journal.clone(), root.clone());
    let runtime = intercat_daemon::repair_service::Runtime::start(
        journal.clone(),
        root,
        f.root.path().join("data"),
        f.root.path().join("rpc"),
        std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        service.clone(),
        Default::default(),
        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
    );
    let result = async {
        runtime.service.install_secret(request).await?;
        let failed = wait_repair(&journal, held.id, |run| {
            run.phase == intercat_daemon::repair_run::Phase::Failed
        })
        .await?;
        assert!(failed.host.is_none());
        assert_eq!(failed.session, held.session);
        let checkpoint = failed.checkpoint.unwrap();
        assert_eq!(
            intercat_git::Repository::open(&workspace.checkout.workdir)?
                .reference_target(&checkpoint.reference)?
                .unwrap()
                .to_string(),
            checkpoint.sha
        );
        assert_eq!(
            fs::read_to_string(workspace.checkout.workdir.join("retained.txt"))?,
            "keep these private repair edits\n"
        );
        for _ in 0..100 {
            if journal.snapshot().await?["secretInputs"][0]["state"] == "failed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            journal.snapshot().await?["secretInputs"][0]["state"],
            "failed"
        );
        assert!(
            journal.snapshot().await?["utterances"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    runtime.shutdown().await;
    service.shutdown().await;
    result
}

#[tokio::test]
async fn repair_secret_retry_captures_restored_edits_in_a_new_immutable_snapshot() -> Result<()> {
    let (f, journal) = repair_run_fixture(false).await?;
    let run = journal.repair_start(repair_request(&f)).await?;
    let (_, job, settings) = journal.repair_input(run.id).await?;
    let workspace = intercat_daemon::repair::Workspace::prepare(
        &job,
        &f.root.path().join("candidate"),
        &f.root.path().join("retry-checkpoint"),
        &run.track,
        &settings,
        watch::channel(false).1,
    )
    .await?;
    let turn = Uuid::now_v7();
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    let save = |request| {
        intercat_daemon::repair_checkpoint::save(
            workspace.snapshot_workspace(run.id),
            request,
            turn,
            None,
            &journal,
        )
    };
    fs::write(
        workspace.checkout.workdir.join("restored.txt"),
        "before restoration\n",
    )?;
    let before = save(first).await?;
    fs::write(
        workspace.checkout.workdir.join("restored.txt"),
        "after restoration\n",
    )?;
    let after = save(second).await?;
    assert_ne!(before.sha, after.sha);
    assert_ne!(before.reference, after.reference);
    assert_eq!(save(first).await?.sha, before.sha);
    let repo = intercat_git::Repository::open(&workspace.checkout.workdir)?;
    assert_eq!(
        repo.reference_target(&before.reference)?
            .unwrap()
            .to_string(),
        before.sha
    );
    assert_eq!(
        repo.reference_target(&after.reference)?
            .unwrap()
            .to_string(),
        after.sha
    );
    assert_eq!(
        fs::read_to_string(workspace.checkout.workdir.join("restored.txt"))?,
        "after restoration\n"
    );
    Ok(())
}

#[tokio::test]
async fn repair_prompt_secret_needs_a_confirmed_settlement_before_restarting() -> Result<()> {
    use intercat_daemon::repair_run::{Event, Outcome};
    for cancel in [false, true] {
        let (f, journal) = repair_run_fixture(false).await?;
        let run = fixture_secret_repair(&journal, &f, true).await?;
        rusqlite::Connection::open(f.root.path().join("events.sqlite"))?.execute(
            "INSERT INTO msp_positions VALUES(?1,?2,NULL)",
            rusqlite::params![run.session, run.track],
        )?;
        let mut request =
            repair_secret_request(&journal, &run, "dummy-repair-prompt-secret").await?;
        let prompt = &run.prompts[0];
        request.question = intercat_daemon::secret_input::Question::Msp {
            id: prompt.request.user_input_id.clone(),
            revision: prompt.revision.clone(),
        };
        let install = journal.prepare_secret(request).await?.unwrap();
        journal.secret_accepted(install.target.op_id).await?;
        let current = journal.repair_input(run.id).await?.0;
        assert_eq!(current.secret_input, Some(install.target.op_id));
        assert!(journal.secret_settled(install.target.op_id).await.is_err());
        assert!(
            journal
                .queue_repair_secret(
                    install.target.clone(),
                    repair_secret_checkpoint(&run, install.target.op_id)
                )
                .await
                .is_err()
        );
        let held = advance_repair(
            &journal,
            &current,
            Event::HostClosed {
                outcome: Outcome::Stopped,
            },
        )
        .await?;
        assert!(
            journal
                .queue_repair_secret(
                    install.target.clone(),
                    repair_secret_checkpoint(&run, install.target.op_id)
                )
                .await
                .is_err()
        );
        if cancel {
            journal.secret_failed(install.target.op_id).await?;
            assert!(journal.secret_settled(install.target.op_id).await.is_err());
        } else {
            journal.secret_settled(install.target.op_id).await?;
            assert_eq!(
                journal.snapshot().await?["secretInputs"][0]["state"],
                "settled"
            );
            journal
                .queue_repair_secret(
                    install.target.clone(),
                    repair_secret_checkpoint(&run, install.target.op_id),
                )
                .await?;
            let queued = journal.repair_input(run.id).await?.0;
            assert_eq!(queued.phase, intercat_daemon::repair_run::Phase::Queued);
            assert_eq!(queued.session, held.session);
        }
    }
    Ok(())
}

#[tokio::test]
async fn attachments_from_hooks_and_checks_never_enter_candidate_commits() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    executable(
        &f.source.join(".git/hooks/pre-commit"),
        "#!/bin/sh\nmkdir -p .intercat/attachments\nprintf hook > .intercat/attachments/hook.txt\nprintf x >> hook-count.txt\ngit add -f .intercat/attachments/hook.txt hook-count.txt\n",
    )?;
    f.track(
        "A",
        &[
            ("file.txt", Some("A\n"), false),
            (
                ".gitignore",
                Some(".intercat/*\n!.intercat/attachments/\n!.intercat/attachments/**\n"),
                false,
            ),
        ],
    )?;
    let settings=f.settings(Some("mkdir -p .intercat/attachments; printf check > .intercat/attachments/check.txt; test \"$(cat file.txt)\" = A"))?;
    let result = f.build(&settings).await?;
    assert_eq!(result.state, State::Checked, "{result:?}");
    let repo = Repository::open(&f.source)?;
    let files = repo.tree_files(result.tree.parse()?)?;
    assert!(
        !files
            .keys()
            .any(|path| path.starts_with(".intercat/attachments/"))
    );
    assert_eq!(repo.saved_bytes(files.get("file.txt").unwrap())?, b"A\n");
    assert_eq!(
        repo.saved_bytes(files.get("hook-count.txt").unwrap())?,
        b"x"
    );
    assert!(
        result
            .left_out
            .iter()
            .any(|path| path == ".intercat/attachments/hook.txt")
    );
    let commit = repo.commit_details(result.head.parse()?)?;
    assert_eq!(commit.author_name, "Worker A");
    assert!(commit.message.contains("Intercat: Fixture session"));
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn attachment_name_collisions_keep_project_code_through_hooks_and_commit_checks() -> Result<()>
{
    let mut f = Fixture::new(&[(
        ".intercat/attachments/code.txt",
        "original project code",
        false,
    )])
    .await?;
    executable(
        &f.source.join(".git/hooks/pre-commit"),
        "#!/bin/sh\nmkdir -p .intercat-attachments-2 .intercat/attachments\nprintf copy > .intercat-attachments-2/hook.txt\nprintf code > .intercat/attachments/new-code.txt\ngit add -f .intercat-attachments-2/hook.txt .intercat/attachments/new-code.txt\n",
    )?;
    f.track(
        "A",
        &[(
            ".intercat/attachments/code.txt",
            Some("changed project code"),
            false,
        )],
    )?;
    let settings = f.settings(Some(
        "mkdir -p .intercat-attachments-2; printf copy > .intercat-attachments-2/check.txt",
    ))?;
    let result = f.build(&settings).await?;
    assert_eq!(result.state, State::Checked, "{result:?}");
    let repo = Repository::open(&f.source)?;
    let files = repo.tree_files(result.tree.parse()?)?;
    assert_eq!(
        repo.saved_bytes(files.get(".intercat/attachments/code.txt").unwrap())?,
        b"changed project code"
    );
    assert!(files.contains_key(".intercat/attachments/new-code.txt"));
    assert!(
        !files
            .keys()
            .any(|path| path.starts_with(".intercat-attachments-"))
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn attachment_filter_does_not_hide_a_check_that_creates_a_new_secret_file() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    f.track("A", &[("file.txt", Some("A\n"), false)])?;
    let settings = f.settings(Some("printf 'LOCAL=fixture' > .env"))?;
    let result = f.build(&settings).await?;
    assert_eq!(result.state, State::CheckMutated, "{result:?}");
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn attachment_filter_does_not_hide_check_changes_to_unregistered_directories() -> Result<()> {
    let mut f = Fixture::new(&[]).await?;
    f.track("A", &[("file.txt", Some("A\n"), false)])?;
    let settings = f.settings(Some(
        "mkdir -p .intercat-attachments-3; printf user > .intercat-attachments-3/user.txt",
    ))?;
    let result = f.build(&settings).await?;
    assert_eq!(result.state, State::CheckMutated, "{result:?}");
    f.unchanged()?;
    Ok(())
}
