use super::*;
use intercat_daemon::{
    git_tool::GitTool,
    push_inspect::Inspector,
    push_job::{Action as PushAction, Command as PushCommand, Phase},
    push_lfs::Tool,
    push_review::{Choice, Request},
    push_review_service, push_service,
};
use std::{os::unix::fs::PermissionsExt, sync::Arc};
use tokio::sync::watch;
async fn committed(f: &Fixture) -> Result<Job> {
    fs::write(
        f.workspaces[0].path.join("file.txt"),
        "Own generated Push change\n",
    )?;
    f.save(0, None).await?;
    let open = f.open().await?;
    f.action(Action::Prepare {
        id: open.id,
        revision: open.revision,
    })
    .await?;
    let checked = finished(&f.journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    f.action(commit_action(&checked)).await?;
    let done = finished(&f.journal).await?;
    assert_eq!(done.status, Status::Done);
    Ok(done)
}
struct PushFixture {
    reviews: push_review_service::Service,
    configuration: push_service::Configuration,
    remotes: Vec<PathBuf>,
}
async fn setup(f: &Fixture, count: usize, seed: bool) -> Result<PushFixture> {
    let home = f._root.path().join("push-home");
    fs::create_dir(&home)?;
    let git_tool = GitTool::system().await?;
    let inspector = Arc::new(Inspector::new(
        git_tool.clone(),
        home.clone(),
        home.join(".config"),
        false,
    )?);
    let lfs = Arc::new(Tool::open(
        git_tool.clone(),
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tools/git-lfs-3.8.1/git-lfs"),
    )?);
    let mut remotes = Vec::new();
    for index in 0..count {
        let remote = f._root.path().join(format!("remote-{index}.git"));
        git(
            f._root.path(),
            &["init", "--bare", "-q", remote.to_str().unwrap()],
        );
        let url = reqwest::Url::from_file_path(&remote).unwrap().to_string();
        if index == 0 {
            git(&f.source, &["remote", "add", "origin", &url]);
        }
        git(
            &f.source,
            &["config", "--add", "remote.origin.pushurl", &url],
        );
        if seed {
            git(
                &f.source,
                &[
                    "push",
                    "--no-verify",
                    &url,
                    &format!("{}:refs/heads/dummy-base", f.head),
                ],
            );
        }
        remotes.push(remote);
    }
    let root = f._root.path().join("pushes");
    fs::create_dir(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    Ok(PushFixture {
        reviews: push_review_service::Service::new(
            f.journal.clone(),
            inspector.clone(),
            lfs.clone(),
        ),
        configuration: push_service::Configuration {
            git: git_tool,
            inspector,
            lfs,
            root,
            path: "/usr/bin:/bin:/usr/sbin:/sbin".into(),
            agent: None,
        },
        remotes,
    })
}
async fn review(f: &Fixture, push: &PushFixture) -> Result<intercat_daemon::push_review::Review> {
    let folder = f.journal.snapshot().await?["project"]["folders"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    push.reviews
        .open(
            Request {
                op_id: Uuid::now_v7(),
                choices: vec![Choice {
                    folder_id: folder,
                    remote: "origin".into(),
                    target_ref: None,
                    through: None,
                }],
            },
            watch::channel(false).1,
        )
        .await
}
async fn settled(
    service: &push_service::Service,
    id: Uuid,
) -> Result<intercat_daemon::push_job::Job> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let job = service.job(id).await?;
            if !job.active() {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}
fn command(action: PushAction) -> PushCommand {
    PushCommand {
        op_id: Uuid::now_v7(),
        action,
    }
}
#[tokio::test]
async fn confirmed_push_retains_partial_success_and_retries_only_remaining_destinations()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let push = setup(&f, 2, true).await?;
    let committed = committed(&f).await?;
    let review = review(&f, &push).await?;
    let sha = review.members[0].target.sha.clone();
    let reference = review.members[0].target.target_ref.clone();
    // Keep a known base ref, but make this destination's target diverge.
    let tree = git(&f.source, &["rev-parse", &format!("{}^{{tree}}", f.head)]);
    let divergent = git(
        &f.source,
        &[
            "-c",
            "user.name=Dummy",
            "-c",
            "user.email=dummy@example.invalid",
            "commit-tree",
            &tree,
            "-p",
            &f.head,
            "-m",
            "Own divergent receiver history",
        ],
    );
    let second_url = reqwest::Url::from_file_path(&push.remotes[1])
        .unwrap()
        .to_string();
    git(
        &f.source,
        &[
            "push",
            "--no-verify",
            &second_url,
            &format!("{divergent}:{reference}"),
        ],
    );
    let service = push_service::Service::new(f.journal.clone(), push.configuration).await?;
    let confirm = command(PushAction::Confirm {
        review: review.request.op_id,
    });
    let job = service.command(confirm.clone()).await?;
    let partial = settled(&service, job.id).await?;
    assert_eq!(partial.recipients[0].phase, Phase::Done, "{partial:?}");
    assert_eq!(partial.recipients[1].phase, Phase::Uncertain, "{partial:?}");
    assert_eq!(git(&push.remotes[0], &["rev-parse", &reference]), sha);
    assert_eq!(
        Repository::open(&push.remotes[1])?.reference_target(&reference)?,
        Some(divergent.parse()?)
    );
    let timeline = f
        .journal
        .timeline_window(intercat_daemon::timeline::Window {
            tracks: vec![],
            start: 0,
            end: i64::MAX,
            since: None,
        })
        .await?;
    let point = timeline["marks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|mark| mark["commit"]["sha"] == sha)
        .unwrap();
    assert_eq!(point["commit"]["pushed"], false);
    let first = serde_json::to_value(&partial.recipients[0])?;
    let replay = service.command(confirm).await?;
    assert_eq!(replay.id, job.id);
    assert!(!replay.active());
    git(&push.remotes[1], &["update-ref", "-d", &reference]);
    let held = push.remotes[0].with_extension("held");
    fs::rename(&push.remotes[0], &held)?;
    let retry = service
        .command(command(PushAction::Retry {
            id: job.id,
            revision: replay.revision,
        }))
        .await?;
    let done = settled(&service, retry.id).await?;
    assert!(done.done(), "{done:?}");
    assert_eq!(serde_json::to_value(&done.recipients[0])?, first);
    assert_eq!(done.recipients[1].attempt, 1);
    assert_eq!(git(&push.remotes[1], &["rev-parse", &reference]), sha);
    fs::rename(&held, &push.remotes[0])?;
    assert_eq!(sha, committed.candidate.unwrap().head);
    f.source_unchanged()?;
    service.shutdown().await;
    f.service.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn push_stops_for_unknown_history_and_changed_known_value_bindings() -> Result<()> {
    for seeded in [false, true] {
        let f = Fixture::new(1).await?;
        let push = setup(&f, 1, seeded).await?;
        committed(&f).await?;
        if seeded {
            let file = f.source.join(".env.test");
            fs::write(
                &file,
                "DUMMY_SCAN_VALUE=own-generated-unrelated-value-583\n",
            )?;
            f.journal
                .change_check_environment(intercat_daemon::check_environment::Change {
                    op_id: Uuid::now_v7(),
                    revision: 0,
                    file: Some(file),
                })
                .await?;
        }
        let review = review(&f, &push).await?;
        let reference = review.members[0].target.target_ref.clone();
        let service = push_service::Service::new(f.journal.clone(), push.configuration).await?;
        let job = service
            .command(command(PushAction::Confirm {
                review: review.request.op_id,
            }))
            .await?;
        let stopped = settled(&service, job.id).await?;
        assert_eq!(stopped.recipients[0].phase, Phase::Failed, "{stopped:?}");
        assert!(
            stopped.recipients[0]
                .reason
                .as_deref()
                .unwrap()
                .starts_with("Unchecked commit"),
            "{stopped:?}"
        );
        assert!(!stopped.recipients[0].scans.is_empty());
        assert!(
            Repository::open(&push.remotes[0])?
                .reference_target(&reference)?
                .is_none()
        );
        f.source_unchanged()?;
        service.shutdown().await;
        f.service.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn interrupted_send_is_queried_after_database_reopen_before_any_retry() -> Result<()> {
    let f = Fixture::new(1).await?;
    let push = setup(&f, 1, true).await?;
    committed(&f).await?;
    let review = review(&f, &push).await?;
    let sha = review.members[0].target.sha.clone();
    let reference = review.members[0].target.target_ref.clone();
    let configuration = push.configuration.clone();
    let remote = push.remotes[0].clone();
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    // Simulate process loss after receive-pack has changed the ref, before the
    // result or its uncertain outcome can be saved. No mock transport is used.
    db.execute_batch("CREATE TRIGGER dummy_lost_receipt BEFORE UPDATE ON push_jobs WHEN json_extract(NEW.record,'$.recipients[0].phase') IN ('done','uncertain') BEGIN SELECT RAISE(ABORT,'dummy lost process'); END;")?;
    let service = push_service::Service::new(f.journal.clone(), push.configuration).await?;
    let confirm = command(PushAction::Confirm {
        review: review.request.op_id,
    });
    let job = service.command(confirm.clone()).await?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if Repository::open(&remote)?.reference_target(&reference)? == Some(sha.parse()?) {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            let status = service.job(job.id).await?;
            assert!(status.active(), "{status:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    service.shutdown().await;
    assert_eq!(
        service.job(job.id).await?.recipients[0].phase,
        Phase::Sending
    );
    db.execute_batch("DROP TRIGGER dummy_lost_receipt;")?;
    drop(db);
    f.source_unchanged()?;
    f.service.shutdown().await;
    drop(service);
    drop(push.reviews);
    let Fixture {
        _root,
        journal,
        service,
        source: _,
        project_secrets: _,
        workspaces: _,
        index: _,
        head: _,
    } = f;
    drop(service);
    drop(journal);
    let journal = Journal::open(
        _root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let db = rusqlite::Connection::open(_root.path().join("events.sqlite"))?;
    db.execute_batch("CREATE TRIGGER dummy_refuse_resend BEFORE UPDATE ON push_jobs WHEN json_extract(NEW.record,'$.recipients[0].phase')='sending' BEGIN SELECT RAISE(ABORT,'dummy resend forbidden'); END;")?;
    let service = push_service::Service::new(journal, configuration).await?;
    let recovered = service.job(job.id).await?;
    assert_eq!(recovered.recipients[0].phase, Phase::Uncertain);
    assert!(!recovered.active());
    let retry = service
        .command(command(PushAction::Retry {
            id: job.id,
            revision: recovered.revision,
        }))
        .await?;
    let done = settled(&service, retry.id).await?;
    assert!(done.done(), "{done:?}");
    assert!(!done.recipients[0].receipt.as_ref().unwrap().changed);
    let replay = service.command(confirm).await?;
    assert_eq!(replay.id, job.id);
    assert!(replay.done());
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn cancelling_during_offline_pre_push_stops_current_and_queued_destinations() -> Result<()> {
    let f = Fixture::new(1).await?;
    let push = setup(&f, 2, true).await?;
    committed(&f).await?;
    let hook = f.source.join(".git/hooks/pre-push");
    fs::write(&hook, "#!/bin/sh\nsleep 5\n")?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700))?;
    let review = review(&f, &push).await?;
    let reference = review.members[0].target.target_ref.clone();
    let service = push_service::Service::new(f.journal.clone(), push.configuration).await?;
    let job = service
        .command(command(PushAction::Confirm {
            review: review.request.op_id,
        }))
        .await?;
    let hooking = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let job = service.job(job.id).await?;
            if job.recipients[0].phase == Phase::Hooks {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
            }
            assert!(job.active(), "{job:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    service
        .command(command(PushAction::Cancel {
            id: job.id,
            revision: hooking.revision,
        }))
        .await?;
    let stopped = settled(&service, job.id).await?;
    assert!(
        stopped
            .recipients
            .iter()
            .all(|recipient| recipient.phase == Phase::Cancelled),
        "{stopped:?}"
    );
    for remote in push.remotes {
        assert!(
            Repository::open(&remote)?
                .reference_target(&reference)?
                .is_none()
        );
    }
    f.source_unchanged()?;
    service.shutdown().await;
    f.service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn push_rpc_requires_authentication_and_confirmation_before_the_local_send() -> Result<()> {
    if std::env::var_os("INTERCAT_DUMMY_PUSH_RPC").is_none() {
        let root = tempfile::tempdir()?;
        let home = root.path().join("home");
        fs::create_dir(&home)?;
        let project = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()?;
        let mut child=tokio::process::Command::new(std::env::current_exe()?)
            .args(["--exact","push_jobs::push_rpc_requires_authentication_and_confirmation_before_the_local_send","--nocapture"])
            .env_clear().env("HOME",home).env("PATH","/usr/bin:/bin:/usr/sbin:/sbin")
            .env("INTERCAT_DUMMY_PUSH_RPC","own-dummy-only")
            .env("INTERCAT_GITLEAKS",project.join(".tools/gitleaks-8.30.1/gitleaks"))
            .env("INTERCAT_GIT_LFS",project.join(".tools/git-lfs-3.8.1/git-lfs"))
            .kill_on_drop(true).spawn()?;
        assert!(
            tokio::time::timeout(Duration::from_secs(60), child.wait())
                .await??
                .success()
        );
        return Ok(());
    }
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixStream,
    };
    async fn call(
        stream: &mut BufReader<UnixStream>,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        stream
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await?;
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_line(&mut line)).await??;
        Ok(serde_json::from_str(&line)?)
    }
    let f = Fixture::new(1).await?;
    let push = setup(&f, 1, true).await?;
    let committed = committed(&f).await?;
    f.service.shutdown().await;
    let folder = committed.project.id.clone();
    let reference = format!("refs/heads/intercat/{}", committed.slug);
    let token = "c9".repeat(32);
    let editor = intercat_daemon::editor::Manager::new(
        f._root.path(),
        &f._root.path().join("dummy-editor.log"),
    )?;
    let rpc_root = f._root.path().join("rpc");
    fs::create_dir(&rpc_root)?;
    fs::set_permissions(&rpc_root, fs::Permissions::from_mode(0o700))?;
    let _lock = intercat_daemon::lock::DaemonLock::acquire(&rpc_root)?
        .ok_or("Dummy RPC directory locked.")?;
    let server = intercat_daemon::rpc::Server::bind(
        &rpc_root,
        intercat_daemon::rpc::Token::parse(&token)?,
        f.journal.clone(),
        editor.clone(),
        &f._root.path().join("workspaces"),
    )?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let mut stream = BufReader::new(UnixStream::connect(rpc_root.join("rpc.sock")).await?);
    assert_eq!(
        call(&mut stream, "push.remotes", json!({"folderId":folder})).await?["error"]["code"],
        -32001
    );
    let mut stream = BufReader::new(UnixStream::connect(rpc_root.join("rpc.sock")).await?);
    assert!(
        call(&mut stream, "rpc.authenticate", json!({"token":token})).await?["error"].is_null()
    );
    assert_eq!(
        call(&mut stream, "push.remotes", json!({"folderId":folder})).await?["result"],
        json!(["origin"])
    );
    let id = Uuid::now_v7();
    let opened=call(&mut stream,"push.review",json!({"opId":id,"choices":[{"folderId":folder,"remote":"origin","targetRef":null,"through":null}]})).await?;
    assert!(opened["error"].is_null(), "{opened}");
    let sha = committed.candidate.as_ref().unwrap().head.clone();
    assert_eq!(
        opened["result"]["review"]["members"][0]["target"]["sha"],
        sha
    );
    assert_eq!(opened["result"]["commits"][&folder], json!([sha]));
    assert!(
        Repository::open(&push.remotes[0])?
            .reference_target(&reference)?
            .is_none()
    );
    let invalid = call(
        &mut stream,
        "push.command",
        json!({"opId":Uuid::now_v7(),"action":{"action":"send","url":"file:///dummy-never-sent"}}),
    )
    .await?;
    assert_eq!(invalid["error"]["code"], -32602);
    let confirmed = call(
        &mut stream,
        "push.command",
        json!({"opId":Uuid::now_v7(),"action":{"action":"confirm","review":id}}),
    )
    .await?;
    assert!(confirmed["error"].is_null(), "{confirmed}");
    let job_id = confirmed["result"]["id"].as_str().unwrap().parse()?;
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let job = f.journal.push_job(job_id).await?;
            if !job.active() {
                assert!(job.done(), "{job:?}");
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    assert_eq!(
        Repository::open(&push.remotes[0])?.reference_target(&reference)?,
        Some(sha.parse()?)
    );
    let window = f
        .journal
        .timeline_window(intercat_daemon::timeline::Window {
            tracks: vec![],
            start: 0,
            end: i64::MAX,
            since: None,
        })
        .await?;
    let marks = window["marks"].as_array().unwrap();
    let mark = marks
        .iter()
        .find(|mark| mark["commit"]["sha"] == sha)
        .expect("Committed point missing from timeline");
    assert_eq!(mark["commit"]["folderId"], folder);
    assert_eq!(mark["commit"]["jobId"], committed.id.to_string());
    assert!(mark["inputId"].is_null());
    assert_eq!(mark["commit"]["pushed"], true);
    let saved = call(&mut stream, "push.presentation", json!({"id":id})).await?;
    assert_eq!(saved["result"]["review"], opened["result"]["review"]);
    f.source_unchanged()?;
    let _ = stop.send(());
    task.await??;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
async fn pr_open_keeps_committed_sources_and_requires_a_matching_push_receipt_without_network()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let push = setup(&f, 1, true).await?;
    committed(&f).await?;
    // GitHub is only a logical URL here. Review resolution is offline and no
    // sender is started. Its prior receipt is a controlled local fixture.
    git(
        &f.source,
        &["config", "--unset-all", "remote.origin.pushurl"],
    );
    git(
        &f.source,
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/own-generated-dummy/dummy-only.git",
        ],
    );
    let review = review(&f, &push).await?;
    let member = &review.members[0];
    let request = intercat_daemon::pull_request::Open {
        op_id: Uuid::now_v7(),
        push_review: review.request.op_id,
        choices: vec![intercat_daemon::pull_request::Choice {
            folder_id: member.folder.id.clone(),
            destination: member.target.destinations[0].fingerprint.clone(),
            base: "main".into(),
        }],
    };
    assert!(f.journal.open_pull(request.clone()).await.is_err());
    assert!(f.journal.operation(request.op_id).await?.is_none());
    let mut prior = f
        .journal
        .push_command(command(PushAction::Confirm {
            review: review.request.op_id,
        }))
        .await?;
    prior.recipients[0].phase = Phase::Done;
    prior.recipients[0].receipt = Some(intercat_daemon::push_wire::Receipt {
        sha: member.target.sha.clone(),
        reference: member.target.target_ref.clone(),
        destination: member.target.destinations[0].clone(),
        changed: true,
    });
    let db = rusqlite::Connection::open(f._root.path().join("events.sqlite"))?;
    db.execute(
        "UPDATE push_jobs SET record=?2 WHERE id=?1",
        rusqlite::params![prior.id.to_string(), serde_json::to_string(&prior)?],
    )?;
    drop(db);
    let opened = f.journal.open_pull(request.clone()).await?;
    assert_eq!(opened.members.len(), 1);
    assert_eq!(
        opened.members[0].phase,
        intercat_daemon::pull_request::Phase::Drafting
    );
    assert!(!opened.members[0].sources.commits.is_empty());
    assert!(opened.confirmed.is_none());
    assert!(opened.members[0].result.is_none());
    let replay = f.journal.open_pull(request.clone()).await?;
    assert_eq!(
        serde_json::to_value(&replay)?,
        serde_json::to_value(&opened)?
    );
    let mut wrong = request;
    wrong.choices[0].base = "another-dummy-base".into();
    assert!(f.journal.open_pull(wrong).await.is_err());
    let state = f.journal.snapshot().await?;
    assert_eq!(state["pullRequests"][0]["id"], opened.id.to_string());
    assert!(
        state["pullRequests"][0]["members"][0]
            .get("sources")
            .is_none()
    );
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn committed_work_proposal_prepares_a_frozen_review_without_sending_and_replays_its_receipt()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let committed = committed(&f).await?;
    let state = f.journal.snapshot().await?;
    let proposal = &state["proposal"];
    assert_eq!(proposal["kind"], "publication", "{proposal}");
    assert_eq!(
        state["editorEntries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["scene"] == "commit"
                && entry["line"] == "Committed. Push when you like.")
            .count(),
        1
    );
    assert_eq!(proposal["text"], "Changes committed · Push and PR?");
    let request = intercat_daemon::proposals::Command {
        op_id: Uuid::now_v7(),
        proposal_id: serde_json::from_value(proposal["id"].clone())?,
        action: intercat_daemon::proposals::Action::OpenPr,
    };
    let response = f.journal.answer_proposal(request.clone()).await?;
    assert_eq!(f.journal.answer_proposal(request).await?, response);
    let state = f.journal.snapshot().await?;
    assert!(state["proposal"].is_null());
    assert_eq!(
        state["editorEntries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["scene"] == "commit")
            .count(),
        1
    );
    assert!(state["pushes"].as_array().unwrap().is_empty());
    assert!(state["pullRequests"].as_array().unwrap().is_empty());
    let word = state["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|word| word["id"] == response["inputId"])
        .unwrap();
    assert_eq!(word["state"], "placed");
    assert_eq!(word["presentation"]["command"], "open-pr");
    assert_eq!(
        word["presentation"]["publication"]["folders"],
        serde_json::json!([committed.project.id])
    );
    assert_eq!(
        word["presentation"]["publication"]["through"][&committed.project.id],
        committed.candidate.unwrap().head
    );
    let word_id: Uuid = serde_json::from_value(word["id"].clone())?;
    let receipt: Uuid = serde_json::from_value(word["presentation"]["receiptId"].clone())?;
    f.journal
        .resolve_input(
            receipt,
            word_id.to_string(),
            word["revision"].as_i64().unwrap(),
            intercat_daemon::decision::Resolution::Presented,
        )
        .await?;
    let state = f.journal.snapshot().await?;
    assert_eq!(
        state["utterances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|word| word["id"] == response["inputId"])
            .unwrap()["state"],
        "landed"
    );
    assert!(state["pushes"].as_array().unwrap().is_empty());
    assert!(state["pullRequests"].as_array().unwrap().is_empty());
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}
#[tokio::test]
async fn publication_proposal_can_be_left_for_later_without_creating_work() -> Result<()> {
    let f = Fixture::new(1).await?;
    committed(&f).await?;
    let state = f.journal.snapshot().await?;
    let before = state["utterances"].clone();
    let request = intercat_daemon::proposals::Command {
        op_id: Uuid::now_v7(),
        proposal_id: serde_json::from_value(state["proposal"]["id"].clone())?,
        action: intercat_daemon::proposals::Action::Later,
    };
    f.journal.answer_proposal(request).await?;
    let state = f.journal.snapshot().await?;
    assert!(state["proposal"].is_null());
    assert_eq!(state["utterances"], before);
    assert!(state["pushes"].as_array().unwrap().is_empty());
    assert!(state["pullRequests"].as_array().unwrap().is_empty());
    f.source_unchanged()?;
    f.service.shutdown().await;
    Ok(())
}
