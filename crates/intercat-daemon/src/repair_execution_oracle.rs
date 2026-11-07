//! Offline evidence for the actual repair loop with scripted MSP peers.
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

async fn prepare(session: Option<&str>) -> Result<Prepared> {
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
    if let Some(session) = session {
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
    }
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

async fn reply_through_repair_loop(mode: crate::approval::ReplyMode) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut prepared = prepare(Some("repair-reply")).await?;
    let op = Uuid::now_v7();
    let turn = Uuid::now_v7();
    let text = "Yes, run it, but keep the original folder.";
    let response = crate::approval::Response::Reply {
        text: text.into(),
        mode,
    };
    let requirement = json!({"approvalId":"reply-approval","sourceIndex":0});
    let request = json!({"sessionId":"repair-reply","turnId":turn,"approvalId":"reply-approval","itemId":"reply-item","rawArgs":"{}",
        "subject":{"kind":"shell","workspaceRoot":prepared.workspace.checkout.workdir,"command":"git push origin main","stages":[{"argv":["git","push","origin","main"],"argvComplete":true}]},
        "currentRequirementId":requirement,"availableChoices":[{"scope":"once","decision":"approved","choiceId":"yes"},{"scope":"once","decision":"denied","choiceId":"no","acceptsFeedback":mode==crate::approval::ReplyMode::Feedback}]});
    let (client, server) = tokio::io::duplex(1024 * 1024);
    let (read, write) = tokio::io::split(client);
    let mut connection = crate::msp::Connection::new(read, write);
    let (port, mut runner) = channel_on(&prepared.run, connection.client.clone())?;
    let (interrupted, interruption) = oneshot::channel();
    let (finished, finish) = oneshot::channel();
    let journal = prepared.journal.clone();
    let peer = tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(server);
        let mut lines = BufReader::new(read).lines();
        let start: Value =
            serde_json::from_str(&lines.next_line().await?.ok_or("Missing repair start")?)?;
        assert_eq!(start["method"], "turn/start");
        write.write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":start["id"],"result":{"commandId":start["params"]["commandId"],"disposition":"started","turnId":turn}})).as_bytes()).await?;
        write
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","method":"approval/request","params":request})
                )
                .as_bytes(),
            )
            .await?;
        let command: Value =
            serde_json::from_str(&lines.next_line().await?.ok_or("Missing repair response")?)?;
        if mode == crate::approval::ReplyMode::Feedback {
            assert_eq!(command["method"], "approval/decide");
            assert_eq!(command["params"]["choiceId"], "no");
            assert_eq!(command["params"]["feedback"], text);
            assert!(command["params"].get("scope").is_none());
            assert!(command["params"].get("response").is_none());
            let saved = journal
                .operation(op)
                .await?
                .ok_or("Reply was not saved before its send")?;
            assert_eq!(saved["kind"], "approval/decide");
            assert_eq!(saved["state"], "prepared");
            assert_eq!(saved["payload"]["request"], command["params"]);
            write.write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":command["id"],"result":{"commandId":op,"approvalId":"reply-approval","status":"accepted","terminal":true}})).as_bytes()).await?;
        } else {
            assert_eq!(command["method"], "turn/interrupt");
            let _ = interrupted.send(());
        }
        write.write_all(format!("{}\n", json!({"jsonrpc":"2.0","method":"turn/completed","params":{"sessionId":"repair-reply","turnId":turn,"terminal":"interrupted"}})).as_bytes()).await?;
        finish.await?;
        let mut commands = vec![start, command];
        while let Ok(Ok(Some(line))) =
            tokio::time::timeout(Duration::from_millis(100), lines.next_line()).await
        {
            commands.push(serde_json::from_str(&line)?);
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(commands)
    });
    let journal = prepared.journal.clone();
    let database = prepared.root.path().join("db");
    let id = prepared.run.id;
    let sent_response = response.clone();
    let ui = tokio::spawn(async move {
        let mut changes = journal.subscribe();
        loop {
            changes.borrow_and_update();
            let run = journal.repair_input(id).await?.0;
            if !run.approvals.is_empty() {
                assert_eq!(run.approvals[0].reply_mode, Some(mode));
                break;
            }
            changes.changed().await?;
        }
        let lock = rusqlite::Connection::open(database)?;
        if mode == crate::approval::ReplyMode::Restart {
            lock.execute_batch("BEGIN IMMEDIATE")?;
        }
        let mut answer = tokio::spawn(async move {
            port.answer(
                op,
                "reply-approval".into(),
                json!({"approvalId":"reply-approval","sourceIndex":0}),
                sent_response,
            )
            .await
        });
        if mode == crate::approval::ReplyMode::Restart {
            tokio::time::timeout(Duration::from_secs(5), interruption).await??;
            assert!(
                tokio::time::timeout(Duration::from_millis(50), &mut answer)
                    .await
                    .is_err()
            );
            lock.execute_batch("COMMIT")?;
        }
        answer.await??;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let scanner = prepared
        .journal
        .repair_input(id)
        .await?
        .2
        .scanner
        .start(&[])?;
    let (_shutdown, shutdown) = watch::channel(false);
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        runner.execute_inner(
            SessionIo {
                connection: &mut connection,
                tracker: Arc::new(Mutex::new(crate::process::Tracker::new(
                    "repair-reply".into(),
                    prepared.identity,
                )?)),
            },
            &prepared.journal,
            &prepared.workspace,
            &mut prepared.run,
            &scanner,
            Task {
                instruction: "Offline repair reply verification".into(),
                capacity: Arc::new(Semaphore::new(1)),
                shutdown,
                project_secrets: prepared.secrets.clone(),
            },
        ),
    )
    .await??;
    assert!(matches!(outcome, Outcome::Stopped));
    ui.await??;
    let _ = finished.send(());
    let commands = peer.await??;
    assert_eq!(
        commands
            .iter()
            .filter(|frame| frame["method"] == "turn/start")
            .count(),
        1
    );
    assert_eq!(
        commands
            .iter()
            .filter(|frame| frame["method"] == "turn/interrupt")
            .count(),
        usize::from(mode == crate::approval::ReplyMode::Restart)
    );
    let scope = crate::secret_input::Owner::Repair { run_id: id }.key(&prepared.run.track);
    if mode == crate::approval::ReplyMode::Feedback {
        let control = json!({"scope":scope,"approval":"reply-approval","requirement":requirement,"response":response});
        assert!(completed_request(&prepared.journal, op, "approval/decide", &control).await?);
        let mut wrong = control;
        wrong["scope"] = json!(
            crate::secret_input::Owner::Repair {
                run_id: Uuid::now_v7()
            }
            .key("")
        );
        assert!(
            completed_request(&prepared.journal, op, "approval/decide", &wrong)
                .await
                .is_err()
        );
    } else {
        let journal = prepared.journal.clone();
        let mut confirmation =
            tokio::spawn(async move { crate::approval_reply::confirm(&journal, op).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut confirmation)
                .await
                .is_err()
        );
        prepared.journal.advance_approval_replies().await?;
        assert_eq!(
            prepared.journal.operation(op).await?.unwrap()["state"],
            "prepared"
        );
        assert_eq!(
            prepared.journal.repair_input(id).await?.0.phase,
            Phase::Stopping
        );
        let current = prepared.journal.repair_input(id).await?.0;
        prepared
            .journal
            .repair_transition(
                Uuid::now_v7(),
                id,
                current.revision,
                Event::HostClosed {
                    outcome: Outcome::Stopped,
                },
            )
            .await?;
        prepared.journal.advance_approval_replies().await?;
        confirmation.await??;
        let queued = prepared.journal.repair_input(id).await?.0;
        assert_eq!(queued.phase, Phase::Queued);
        assert_eq!(queued.instruction.as_deref(), Some(text));
        assert!(queued.host.is_none());
        assert_eq!(
            prepared.journal.operation(op).await?.unwrap()["state"],
            "complete"
        );
        prepared.journal.advance_approval_replies().await?;
        assert_eq!(
            prepared.journal.repair_input(id).await?.0.revision,
            queued.revision
        );
        assert!(
            crate::approval::replay(
                &prepared.journal,
                op,
                &scope,
                "reply-approval",
                &requirement,
                &response
            )
            .await?
        );
        assert!(
            crate::approval::replay(
                &prepared.journal,
                op,
                "repair:another-run",
                "reply-approval",
                &requirement,
                &response
            )
            .await
            .is_err()
        );
    }
    let source = prepared.root.path().join("source");
    assert_eq!(git(&source, &["rev-parse", "HEAD"])?, prepared.source_head);
    assert_eq!(fs::read(source.join(".git/index"))?, prepared.source_index);
    assert_eq!(fs::read_to_string(source.join("file.txt"))?, "base\n");
    prepared.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn repair_reply_sends_a_durable_denial_with_feedback() -> Result<()> {
    reply_through_repair_loop(crate::approval::ReplyMode::Feedback).await
}

#[tokio::test]
async fn repair_reply_stops_before_the_writer_and_queues_only_after_host_closure() -> Result<()> {
    reply_through_repair_loop(crate::approval::ReplyMode::Restart).await
}

#[tokio::test]
#[ignore = "two paid dummy Muse turns; masks feedback only in the test transport"]
async fn real_repair_reply_closes_its_host_before_starting_the_followup() -> Result<()> {
    let prepared = prepare(None).await?;
    let output = PathBuf::from(std::env::var("INTERCAT_REPAIR_OUTPUT")?)
        .join(format!("native-reply-{}", Uuid::now_v7()));
    fs::create_dir_all(&output)?;
    let binary = crate::host::prepare_binary(
        &PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        &prepared.root.path().join("muse"),
    )?;
    let config = crate::runtime::Config {
        source: binary.clone(),
        binary,
        credentials: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?),
        path: "/usr/bin:/bin:/usr/sbin:/sbin".into(),
    };
    prepared
        .journal
        .set_worker_config(Uuid::now_v7(), config.clone())
        .await?;
    let rpc = prepared.root.path().join("rpc");
    fs::create_dir(&rpc)?;
    let remote = prepared.root.path().join("remote.git");
    git(
        prepared.root.path(),
        &["init", "--bare", "-q", remote.to_str().unwrap()],
    )?;
    let workspace = Arc::new(prepared.workspace);
    let mut run = prepared.run;
    let reply = "Do not push. Set file.txt to exactly fixed followed by a newline with the file editing tool. Preserve other files. Do not make Git commits. End with a brief report.";
    let op = Uuid::now_v7();
    let mut hosted = crate::repair_host::open(
        &prepared.journal,
        &mut run,
        &workspace,
        crate::repair_host::Launch {
            worker: &config,
            directory: &prepared.root.path().join("native-host"),
            rpc: &rpc,
            project_secrets: prepared.secrets.clone(),
            one_time: None,
        },
    )
    .await?;
    // Only the receiving test connection changes the optional capability.
    // Both host launches still verify and use the pinned Muse binary.
    hosted.host_mut().connection.hide_approval_feedback = true;
    let first_host = hosted.identity();
    let (port, runner) = channel(&mut hosted, &run)?;
    let editor = crate::editor::Manager::new(
        prepared.root.path(),
        &prepared.root.path().join("editor.log"),
    )?;
    let data = prepared.root.path().join("data");
    let work = crate::runtime::Runtime::start(
        data.clone(),
        rpc.clone(),
        prepared.journal.clone(),
        editor,
        Arc::new(Semaphore::new(1)),
        Arc::new(crate::dispatch_control::Gate::new(true)),
    );
    let repairs = crate::repair_service::Runtime::start(
        prepared.journal.clone(),
        prepared.root.path().join("integration"),
        data,
        rpc,
        Arc::new(Semaphore::new(1)),
        prepared.service.clone(),
        prepared.secrets.clone(),
        Arc::new(crate::dispatch_control::Gate::new(false)),
    );
    let result = async {
        let instruction = format!("This is a disposable local approval test. Before editing files, use bash to run exactly this command once:\ngit push '{}' HEAD:refs/heads/fixture\nWait for its result. Do not write to the remote another way and do not make Git commits. End with a brief report.", remote.display());
        let (_shutdown, shutdown) = watch::channel(false);
        let writer = prepared.journal.clone();
        let files = workspace.clone();
        let secrets = prepared.secrets.clone();
        fs::write(output.join("attempt.json"), serde_json::to_vec_pretty(&json!({"maximumPaidTurns":2,"previousPaidTurns":201,"automaticFollowup":true}))?)?;
        let mut running = tokio::spawn(async move { runner.execute(hosted, writer, &files, Task { instruction, capacity: Arc::new(Semaphore::new(1)), shutdown, project_secrets: secrets }).await });
        let card = tokio::time::timeout(Duration::from_secs(120), async {
            loop {
                let current = prepared.journal.repair_input(run.id).await?.0;
                if let Some(card) = current.approvals.first() { return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(card.clone()); }
                if running.is_finished() { return Err("The native repair ended before approval.".into()); }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await;
        let card = match card {
            Ok(Ok(card)) => card,
            failure => {
                port.stop(Uuid::now_v7())?;
                let _ = tokio::time::timeout(Duration::from_secs(35), &mut running).await;
                return Err(format!("Native approval was unavailable: {failure:?}").into());
            }
        };
        assert_eq!(card.reason.kind, "push");
        assert_eq!(card.reply_mode, Some(crate::approval::ReplyMode::Restart));
        fs::write(output.join("approval.json"), serde_json::to_vec_pretty(&card)?)?;
        port.answer(op, card.id, card.requirement, crate::approval::Response::Reply { text: reply.into(), mode: crate::approval::ReplyMode::Restart }).await?;
        let held = tokio::time::timeout(Duration::from_secs(45), &mut running).await???;
        assert_eq!(held.phase, Phase::Held);
        assert!(!crate::process::is_alive(first_host));
        let mut next_host = None;
        let completed = tokio::time::timeout(Duration::from_secs(150), async {
            loop {
                let current = prepared.journal.repair_input(run.id).await?.0;
                if let Some(host) = current.host && host != first_host { next_host = Some(host); }
                if matches!(current.phase, Phase::Done | Phase::Failed) || current.phase == Phase::Held && current.turn != held.turn { return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(current); }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await??;
        assert_eq!(completed.phase, Phase::Done, "{:?}", completed.output);
        let next_host = next_host.ok_or("The automatic follow-up never launched a host.")?;
        assert_ne!(first_host, next_host);
        assert!(!crate::process::is_alive(next_host));
        assert_eq!(held.session, completed.session);
        crate::approval_reply::confirm(&prepared.journal, op).await?;
        assert_eq!(fs::read_to_string(workspace.checkout.workdir.join("file.txt"))?, "fixed\n");
        assert!(git(&remote, &["for-each-ref", "--format=%(refname)"])?.is_empty());
        assert_eq!(git(&prepared.root.path().join("source"), &["rev-parse", "HEAD"])?, prepared.source_head);
        assert_eq!(fs::read(prepared.root.path().join("source/.git/index"))?, prepared.source_index);
        let db = rusqlite::Connection::open(prepared.root.path().join("db"))?;
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'", [], |row| row.get::<_, i64>(0))?, 2);
        let checked = prepared.journal.snapshot().await?;
        assert_eq!(checked["integration"]["status"], "checked");
        fs::write(output.join("result.json"), serde_json::to_vec_pretty(&json!({"runs":[held,completed],"hosts":[first_host,next_host],"sameSession":true,"sourceUnchanged":true,"remoteUnchanged":true,"paidTurns":2,"automaticFollowup":true,"candidateStatus":"checked","capabilityFixture":"acceptsFeedback hidden in test transport"}))?)?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }.await;
    repairs.shutdown().await;
    work.shutdown().await;
    prepared.service.shutdown().await;
    let db = rusqlite::Connection::open(prepared.root.path().join("db"))?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    result
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
    let mut prepared = prepare(Some(&fixture.session)).await?;
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
                    port.answer(
                        Uuid::now_v7(),
                        card.id,
                        card.requirement,
                        crate::approval::Response::Deny,
                    )
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
