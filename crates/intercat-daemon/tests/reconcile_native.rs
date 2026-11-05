//! Explicitly invoked paid verification, confined to a disposable Git project.
use intercat_daemon::{
    Result,
    history::Context,
    host::{self, Boundary, Host},
    journal::Journal,
    msp::Event,
    reconcile::{self, Expected},
    recording,
};
use serde_json::json;
use std::{fs, io::Write, path::Path, process::Command, time::Duration};
use uuid::Uuid;

fn git(path: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("/usr/bin/git")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Intercat fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()?;
    if !output.status.success() {
        return Err("The disposable Git fixture could not be prepared.".into());
    }
    Ok(())
}

#[tokio::test]
#[ignore = "read-only view contract check; requires a recorded dummy session and the pinned Muse"]
async fn inspect_recorded_view_event_keys_without_a_model_turn() -> Result<()> {
    let root = tempfile::tempdir()?;
    let binary = std::path::PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?);
    let credentials = std::path::PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?);
    let rpc = root.path().join("rpc");
    fs::create_dir(&rpc)?;
    let host = Host::spawn(
        &binary,
        &root.path().join("host"),
        root.path(),
        &std::env::var_os("PATH").ok_or("Missing PATH")?,
        Some(&credentials),
        Boundary {
            profile: intercat_daemon::host::Profile::Worker,
            secrets: &[],
            rpc: &rpc,
            denied: &[],
        },
    )
    .await?;
    let result=async {
        host.connection.client.initialize().await?;
        let page=host.connection.client.request("view/page",json!({"sessionId":std::env::var("INTERCAT_RECONCILE_SESSION")?,"limit":200,"maxBytes":1048576})).await?;
        fs::write(std::env::var("INTERCAT_VIEW_OUTPUT")?,serde_json::to_vec_pretty(&page)?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    host.shutdown().await?;
    result
}

#[tokio::test]
#[ignore = "one paid dummy Muse turn; requires stored authentication and an explicit output directory"]
async fn real_pending_approval_is_reconciled_without_approving_or_starting_again() -> Result<()> {
    verify_pending_recovery(false).await
}

#[tokio::test]
#[ignore = "one paid dummy Muse turn and SIGKILL of its owned host; requires saved authentication"]
async fn killed_host_restores_terminal_turn_without_replaying_approval() -> Result<()> {
    verify_pending_recovery(true).await
}

async fn verify_pending_recovery(kill_host: bool) -> Result<()> {
    let target = std::path::PathBuf::from(std::env::var("INTERCAT_RECONCILE_OUTPUT")?)
        .join(Uuid::now_v7().to_string());
    fs::create_dir_all(&target)?;
    let root = tempfile::tempdir()?;
    let project = root.path().join("project");
    fs::create_dir(&project)?;
    git(&project, &["init", "-q", "-b", "main"])?;
    fs::write(
        project.join("README.txt"),
        "Disposable Intercat reconciliation fixture.\n",
    )?;
    git(&project, &["add", "README.txt"])?;
    git(&project, &["commit", "-qm", "fixture"])?;
    let remote = root.path().join("remote");
    fs::create_dir(&remote)?;
    git(&remote, &["init", "--bare", "-q"])?;
    git(
        &project,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    )?;
    let binary = std::path::PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?);
    assert_eq!(host::hash(&binary)?, host::BINARY_SHA256);
    let credentials = std::path::PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?);
    let database = root.path().join("events.sqlite");
    let journal = Journal::open(&database, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let denied = vec![std::env::current_dir()?];
    let rpc = root.path().join("rpc");
    fs::create_dir(&rpc)?;
    let mut host = Host::spawn(
        &binary,
        &root.path().join("host"),
        &project,
        &std::env::var_os("PATH").ok_or("Missing PATH")?,
        Some(&credentials),
        Boundary {
            profile: intercat_daemon::host::Profile::Worker,
            secrets: &[],
            rpc: &rpc,
            denied: &denied,
        },
    )
    .await?;
    let mut session = None::<String>;
    let mut turn = None::<Uuid>;
    let result=async {
        host.connection.client.initialize().await?;
        let started=host.connection.client.request("session/start",json!({"commandId":Uuid::now_v7(),"workspaceRoot":project,"approvalMode":"promptUnmatched","providerId":"meta","modelId":"muse-spark-1.3-contributor","recoveryMode":"on"})).await?;
        let id=started["session"]["sessionId"].as_str().ok_or("No fixture session")?.to_owned();session=Some(id.clone());
        let command=Uuid::now_v7();let params=json!({"commandId":command,"sessionId":id,"reasoningEffort":"minimal","input":[{"type":"text","text":"This is a disposable local test repository. Use the bash tool to run exactly `git push origin main`. Do not inspect or change any files. Wait for the approval result. Do not try another command or tool."}]});
        journal.intent(&command.to_string(),"turn.start",params.clone()).await?;
        fs::write(target.join("attempt.json"),serde_json::to_vec_pretty(&json!({"sessionId":id,"commandId":command,"attemptedModelTurns":1}))?)?;
        let response=host.connection.client.request("turn/start",params).await?;turn=Some(response["turnId"].as_str().ok_or("No fixture turn")?.parse()?);
        let approval=tokio::time::timeout(Duration::from_secs(100),async {
            loop {
                let Some(Event::Frame(received))=host.connection.next_event().await else {return Err("Fixture host closed before approval.".into());};
                let value=received.frame;
                journal.try_msp("track".into(),id.clone(),received.stamp,recording::sanitize(value.clone(),&[]),Some(received.lease),received.source)?.await??;
                if value["params"]["sessionId"]==id && matches!(value["method"].as_str(),Some("approval/request"|"approval/requested")){return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(value["params"]["approvalId"].clone());}
                if value["method"]=="turn/completed" {return Err("Fixture finished without exercising approval.".into());}
            }
        }).await??;
        let state=reconcile::prepare(&mut host.connection,Context{journal:&journal,track:"track",session:&id,scanner:None},Expected{turn,..Expected::default()}).await?;
        assert_eq!(state.active_turn,turn);assert!(state.pending_approvals>0);
        let pending=host.connection.client.request("approval/listPending",json!({"sessionId":id})).await?;
        assert!(pending["approvals"].as_array().is_some_and(|requests|requests.iter().any(|request|request["approvalId"]==approval)));
        let output=Command::new("/usr/bin/git").arg("-C").arg(&remote).args(["for-each-ref","--format=%(refname)"]).output()?;
        assert!(output.stdout.is_empty(),"The fixture push must never be approved");
        let mut evidence=json!({"sessionId":id,"turnId":turn,"pendingApprovals":state.pending_approvals,"approvalUnchanged":true,"remoteUnchanged":true,"additionalModelTurns":1});
        if kill_host {
            let began=std::time::Instant::now();
            let signal=host.tracker.lock().unwrap().terminate(host.identity,true)?;
            assert!(matches!(signal,intercat_daemon::process::SignalResult::Sent));
            let replacement=Host::spawn(&binary,&root.path().join("resumed-host"),&project,&std::env::var_os("PATH").ok_or("Missing PATH")?,Some(&credentials),Boundary{profile: intercat_daemon::host::Profile::Worker,
secrets:&[],rpc:&rpc,denied:&denied}).await?;
            let lost=std::mem::replace(&mut host,replacement);
            let mut lost_frames=fs::File::create(target.join("lost-host-frames.jsonl"))?;
            for frame in lost.shutdown().await? {
                writeln!(lost_frames,"{}",recording::sanitize(frame,&[]))?;
            }
            host.connection.client.initialize().await?;
            let resumed=host.connection.client.request("session/resume",json!({"commandId":Uuid::now_v7(),"sessionId":id,"excludeItems":true})).await?;
            assert_eq!(resumed["session"]["sessionId"],id);
            assert_eq!(resumed["session"]["permissionState"]["profileId"],"intercat-worker");
            intercat_daemon::history::recover(&mut host.connection,Context{journal:&journal,track:"track",session:&id,scanner:None},resumed["viewCursor"].as_str().ok_or("No resumed cursor")?,None).await?;
            let recovered=reconcile::prepare(&mut host.connection,Context{journal:&journal,track:"track",session:&id,scanner:None},Expected{turn,checkpoint_pending:true,..Expected::default()}).await?;
            assert!(recovered.active_turn.is_none());
            assert_eq!(recovered.resume_turn,turn);
            assert_eq!(recovered.pending_approvals,0);
            assert!(recovered.prompts.is_empty());
            let local=journal.reconcile_local(id.clone()).await?;
            assert!(local.view.turns[&turn.unwrap()].terminal.is_some());
            let elapsed=began.elapsed();
            assert!(elapsed<Duration::from_secs(20),"Recovery took {elapsed:?}");
            evidence["hostKilled"]=json!(true);
            evidence["recoveryMs"]=json!(elapsed.as_millis());
            evidence["terminal"]=json!(local.view.turns[&turn.unwrap()].terminal);
            evidence["recoveredPendingApprovals"]=json!(0);
        }
        fs::write(target.join("result.json"),serde_json::to_vec_pretty(&evidence)?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    if let Some(id) = &session {
        let _ = host.connection.client.interrupt_now(id, turn);
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(Event::Frame(received)) = host.connection.next_event().await {
                let ended = received.frame["method"] == "turn/completed";
                journal
                    .try_msp(
                        "track".into(),
                        id.clone(),
                        received.stamp,
                        recording::sanitize(received.frame, &[]),
                        Some(received.lease),
                        received.source,
                    )?
                    .await??;
                if ended {
                    break;
                }
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await;
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            host.connection.client.request(
                "session/leave",
                json!({"commandId":Uuid::now_v7(),"sessionId":id,"stop":true}),
            ),
        )
        .await;
    }
    let shutdown = host.shutdown().await;
    let mut log = fs::File::create(target.join("final-frames.jsonl"))?;
    if let Ok(final_frames) = &shutdown {
        for frame in final_frames {
            writeln!(log, "{}", recording::sanitize(frame.clone(), &[]))?;
        }
    }
    let db = rusqlite::Connection::open(database)?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute(
        "VACUUM INTO ?1",
        [target.join("events.sqlite").to_str().unwrap()],
    )?;
    if let Err(error) = &result {
        fs::write(target.join("failure.txt"), error.to_string())?;
    }
    shutdown?;
    result
}
