//! Opt-in offline evidence: the actual repair loop with a scripted MSP peer.
use super::*;
use crate::{
    decision::{Decision, Kind, Outcome as DecisionOutcome, Target},
    integration, project,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[path = "../tests/support/approval_fixture.rs"]
mod fixture;

fn git(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/git")
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

struct Prepared {
    root: tempfile::TempDir,
    journal: Journal,
    service: integration::Service,
    secrets: crate::project_secrets::Store,
    workspace: crate::repair::Workspace,
    run: Run,
    identity: crate::process::Identity,
    source_head: String,
    source_index: Vec<u8>,
}

async fn prepare(session: &str) -> Result<Prepared> {
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    fs::create_dir(&source)?;
    git(&source, &["init", "-q", "-b", "main"])?;
    fs::write(source.join("file.txt"), "base\n")?;
    git(&source, &["add", "."])?;
    git(
        &source,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "base",
        ],
    )?;
    let source_head = git(&source, &["rev-parse", "HEAD"])?;
    let source_index = fs::read(source.join(".git/index"))?;
    let journal = Journal::open(root.path().join("db"), crate::secrets::Key::new([7; 32])).await?;
    journal
        .configure_project(Uuid::now_v7(), project::inspect(source).await?)
        .await?;
    let project_id = journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let catalog = journal
        .prepare_project_secrets(crate::project_secrets::Prepare {
            op_id: Uuid::now_v7(),
            project_id: project_id.clone(),
            revision: 0,
            fingerprints: vec![],
            key_check: crate::secrets::Key::new([7; 32])
                .check()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    let secrets = crate::project_secrets::Store::default();
    secrets.publish(
        journal
            .activate_project_secrets(crate::project_secrets::Load {
                project_id,
                revision: 0,
                entries: vec![],
            })
            .await?,
    );
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Offline repair probe".into(),
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
            DecisionOutcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: Some("when-told".into()),
                command: None,
                confidence: 1.0,
                inference_ms: 0.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let prep =
        crate::preparation::Preparation::start(&root.path().join("workspaces"), journal.clone())?;
    let mut changes = journal.subscribe();
    let workspace = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            changes.borrow_and_update();
            let snapshot = journal.snapshot().await?;
            if let Some(value) = snapshot["workspaces"]
                .as_array()
                .and_then(|list| list.first())
            {
                if value["state"] == "failed" {
                    return Err("Probe workspace preparation failed".into());
                }
                if value["state"] == "ready" {
                    let workspace =
                        serde_json::from_value::<crate::preparation::Workspace>(value.clone())?;
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(workspace);
                }
            }
            changes.changed().await?;
        }
    })
    .await??;
    prep.shutdown().await;
    fs::write(workspace.path.join("file.txt"), "candidate\n")?;
    crate::checkpoint::save(
        workspace.clone(),
        Uuid::now_v7(),
        None,
        false,
        journal.clone(),
    )
    .await?;
    let service = integration::Service::new(journal.clone(), root.path().join("integration"));
    let job = service
        .command(integration::Command {
            op_id: Uuid::now_v7(),
            action: integration::Action::Open { tracks: None },
        })
        .await?;
    service
        .command(integration::Command {
            op_id: Uuid::now_v7(),
            action: integration::Action::Prepare {
                id: job.id,
                revision: job.revision,
            },
        })
        .await?;
    let job = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            changes.borrow_and_update();
            let job: integration::Job =
                serde_json::from_value(journal.snapshot().await?["integration"].clone())?;
            if !matches!(
                job.status,
                integration::Status::Building
                    | integration::Status::Staging
                    | integration::Status::Committing
            ) {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
            }
            changes.changed().await?;
        }
    })
    .await??;
    if job.status != integration::Status::Checked {
        return Err(format!("Probe candidate failed: {:?}", job.error).into());
    }
    let candidate = job.candidate.as_ref().unwrap();
    let commit = &candidate.commits[0];
    let change = commit
        .changes
        .iter()
        .find(|c| c.path == "file.txt")
        .unwrap();
    let mut run = journal
        .repair_start(crate::repair_run::Request {
            op_id: Uuid::now_v7(),
            job_id: job.id,
            revision: job.revision,
            track: workspace.track_id,
            purpose: crate::repair_run::Purpose::Edit {
                text: "Offline protocol verification".into(),
                selection: crate::repair_run::Selection {
                    tree: candidate.tree.clone(),
                    path: "file.txt".into(),
                    side: crate::repair_run::Side::After,
                    object: change.after.as_ref().unwrap().object.clone(),
                    start_line: 1,
                    end_line: 1,
                },
            },
        })
        .await?;
    run = journal
        .repair_transition(Uuid::now_v7(), run.id, run.revision, Event::Prepare)
        .await?;
    let (_, job, settings) = journal.repair_input(run.id).await?;
    let parent = root.path().join("integration").join(job.id.to_string());
    let candidate_root = parent.join(job.candidate.as_ref().unwrap().generation.to_string());
    let workspace = crate::repair::Workspace::prepare(
        &job,
        &candidate_root,
        &parent.join(format!("repair-{}", run.id)),
        &run.track,
        &settings,
        watch::channel(false).1,
    )
    .await?;
    run = journal
        .repair_transition(Uuid::now_v7(), run.id, run.revision, Event::WorkspaceReady)
        .await?;
    let identity = crate::process::identity(std::process::id())?;
    run = journal
        .repair_transition(
            Uuid::now_v7(),
            run.id,
            run.revision,
            Event::HostStarted {
                host: identity,
                nonce: Uuid::now_v7(),
                secret_revision: Some(0),
            },
        )
        .await?;
    run = journal
        .repair_transition(
            Uuid::now_v7(),
            run.id,
            run.revision,
            Event::SessionReady {
                session: session.into(),
            },
        )
        .await?;
    Ok(Prepared {
        root,
        journal,
        service,
        secrets,
        workspace,
        run,
        identity,
        source_head,
        source_index,
    })
}

#[tokio::test]
#[ignore = "offline fake-host oracle; set INTERCAT_REPAIR_ORACLE to a prepared case directory"]
async fn approvals_match_the_pinned_sdk() -> Result<()> {
    let directory = PathBuf::from(
        std::env::var_os("INTERCAT_REPAIR_ORACLE").ok_or("Missing fixture directory")?,
    );
    let fixture: fixture::Fixture =
        serde_json::from_slice(&fs::read(directory.join("fixture.json"))?)?;
    let _: muse_code_sdk::msp::ApprovalRequestParams =
        serde_json::from_value(fixture.request.clone())?;
    let _: muse_code_sdk::msp::ApprovalUpdatedParams =
        serde_json::from_value(fixture.update.clone())?;
    assert!(fixture.workspace.is_absolute() && fixture.workspace.is_dir());
    assert_eq!(
        fixture.request["subject"]["workspaceRoot"],
        fixture.workspace.to_string_lossy().as_ref()
    );
    let mut prepared = prepare(&fixture.session).await?;
    let (client, server) = tokio::io::duplex(1024 * 1024);
    let (read, write) = tokio::io::split(client);
    let mut connection = crate::msp::Connection::new(read, write);
    let (port, mut runner) = channel_on(&prepared.run, connection.client.clone())?;
    let journal = prepared.journal.clone();
    let id = prepared.run.id;
    let ui = tokio::spawn(async move {
        let mut changes = journal.subscribe();
        let mut answered = std::collections::BTreeSet::new();
        while changes.changed().await.is_ok() {
            let run = journal.repair_input(id).await?.0;
            for card in run.approvals {
                if card.can_deny
                    && !card.can_allow
                    && answered.insert(format!("{}:{}", card.id, card.requirement))
                {
                    port.answer(Uuid::now_v7(), card.id, card.requirement, false)
                        .await?;
                }
            }
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let server = tokio::spawn(fixture::serve(server, fixture, true));
    let tracker = Arc::new(Mutex::new(crate::process::Tracker::new(
        "repair-oracle".into(),
        prepared.identity,
    )?));
    let scanner = prepared
        .journal
        .repair_input(prepared.run.id)
        .await?
        .2
        .scanner
        .start(&[])?;
    let (_shutdown_sender, shutdown) = watch::channel(false);
    let result = runner
        .execute_inner(
            SessionIo {
                connection: &mut connection,
                tracker,
            },
            &prepared.journal,
            &prepared.workspace,
            &mut prepared.run,
            &scanner,
            Task {
                instruction: "Offline protocol verification".into(),
                capacity: Arc::new(Semaphore::new(1)),
                shutdown,
                project_secrets: prepared.secrets.clone(),
            },
        )
        .await;
    ui.abort();
    while let Some(task) = runner.policies.join_next().await {
        task??;
    }
    let mut output = server.await??;
    let mut operations = Vec::new();
    for frame in output["sent"].as_array().unwrap() {
        let id = frame["params"]["commandId"].as_str().unwrap().parse()?;
        operations.push(
            prepared
                .journal
                .operation(id)
                .await?
                .ok_or("Missing repair decision")?,
        );
    }
    output["operations"] = json!(operations);
    let mut starts = Vec::new();
    for frame in output["starts"].as_array().unwrap() {
        let id = frame["params"]["commandId"].as_str().unwrap().parse()?;
        starts.push(
            prepared
                .journal
                .operation(id)
                .await?
                .ok_or("Missing repair start")?,
        );
    }
    output["startOperations"] = json!(starts);
    output["workerError"] = json!(result.err().map(|e| e.to_string()));
    output["run"] = serde_json::to_value(&prepared.run)?;
    let source = prepared.root.path().join("source");
    assert_eq!(git(&source, &["rev-parse", "HEAD"])?, prepared.source_head);
    assert_eq!(fs::read(source.join(".git/index"))?, prepared.source_index);
    assert_eq!(fs::read_to_string(source.join("file.txt"))?, "base\n");
    output["sourceUnchanged"] = json!(true);
    fs::write(directory.join("repair.json"), serde_json::to_vec(&output)?)?;
    prepared.service.shutdown().await;
    assert!(
        output["complete"] == true,
        "Repair did not send the expected decisions"
    );
    Ok(())
}
