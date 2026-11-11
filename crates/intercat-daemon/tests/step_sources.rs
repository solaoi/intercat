use intercat_daemon::{
    Result, journal::Journal, preparation::Workspace, runtime::WorkerState, step_capture::Service,
    step_sources,
};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Stdio};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

fn git(path: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("/usr/bin/git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
struct Fixture {
    root: tempfile::TempDir,
    journal: Journal,
    workspace: Workspace,
    session: String,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("workspace");
        fs::create_dir(&path)?;
        let path = path.canonicalize()?;
        git(&path, &["init", "-q", "--template=", "-b", "main"]);
        for (name, body) in [
            ("keep.txt", "before\n"),
            ("same.txt", "unchanged\n"),
            ("remove.txt", "remove\n"),
            ("run.sh", "true\n"),
        ] {
            fs::write(path.join(name), body)?;
        }
        git(&path, &["add", "."]);
        git(
            &path,
            &[
                "-c",
                "user.name=Dummy",
                "-c",
                "user.email=dummy@example.invalid",
                "commit",
                "-qm",
                "baseline",
            ],
        );
        let workspace = Workspace {
            denied_roots: Vec::new(),
            attachment_roots: vec![".intercat/attachments".into()],
            track_id: Uuid::now_v7().to_string(),
            path,
            reference: "refs/intercat/dummy/tracks/A".into(),
            base: String::new(),
            state: "ready".into(),
            error: None,
        };
        let workspace = Workspace {
            base: git(&workspace.path, &["rev-parse", "HEAD"]),
            ..workspace
        };
        let journal = Journal::open(
            root.path().join("db"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        let db = rusqlite::Connection::open(root.path().join("db"))?;
        db.execute(
            "INSERT INTO tracks VALUES(?1,'A','Dummy','waiting')",
            [&workspace.track_id],
        )?;
        journal.workspace_result(workspace.clone()).await?;
        let session = Uuid::now_v7().to_string();
        journal
            .worker_state(WorkerState {
                track_id: workspace.track_id.clone(),
                phase: "ready".into(),
                session_id: Some(session.clone()),
                secret_revision: None,
                identity: None,
                detail: None,
                active_turn: None,
                approvals: vec![],
                output: None,
                checkpoint_pending: false,
                checkpoint: None,
                prompts: vec![],
            })
            .await?;
        Ok(Self {
            root,
            journal,
            workspace,
            session,
        })
    }
    fn request(&self, event: &str, turn: &str, calls: &[&str]) -> Value {
        let mut value = json!({"hook_event_name":event,"session_id":self.session,"turn_id":turn,"cwd":"/untrusted/input/path","tool_input":{"text":"RAW_TOOL_INPUT_MUST_NOT_BE_STORED"}});
        if event == "PreToolUse" {
            value["tool_use_id"] = json!(calls[0])
        } else {
            value["tool_calls"]=json!(calls.iter().map(|id|json!({"tool_use_id":id,"tool_response":"RAW_RESPONSE_MUST_NOT_BE_STORED"})).collect::<Vec<_>>())
        }
        value
    }
}
async fn hook(service: &Service, request: Value) -> Result<Value> {
    let command =
        service.hooks()[request["hook_event_name"].as_str().unwrap()][0]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .to_owned();
    let mut child = tokio::process::Command::new("/bin/sh")
        .args(["-c", &command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(&request)?)
        .await?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
        .await??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
}

#[tokio::test]
async fn a_batch_keeps_its_exact_net_changes_without_raw_tool_text_or_checkout_changes()
-> Result<()> {
    let f = Fixture::new().await?;
    let service = Service::start(
        f.journal.clone(),
        f.workspace.track_id.clone(),
        &f.workspace.path,
    )?;
    let turn = Uuid::now_v7().to_string();
    let index = fs::read(f.workspace.path.join(".git/index"))?;
    assert_eq!(
        hook(&service, f.request("PreToolUse", &turn, &["call-a"])).await?,
        json!({})
    );
    fs::write(f.workspace.path.join("keep.txt"), "after\n")?;
    fs::write(f.workspace.path.join("same.txt"), "unchanged\n")?;
    fs::write(f.workspace.path.join("empty.txt"), "")?;
    fs::write(f.workspace.path.join("binary.bin"), [0, 255, 1])?;
    fs::remove_file(f.workspace.path.join("remove.txt"))?;
    fs::set_permissions(
        f.workspace.path.join("run.sh"),
        fs::Permissions::from_mode(0o755),
    )?;
    assert_eq!(
        hook(&service, f.request("PreToolUse", &turn, &["call-b"])).await?,
        json!({})
    );
    fs::write(f.workspace.path.join(".env"), "DUMMY_PRIVATE_VALUE\n")?;
    fs::create_dir_all(f.workspace.path.join(".intercat/attachments"))?;
    fs::write(
        f.workspace.path.join(".intercat/attachments/ref.txt"),
        "DUMMY_REFERENCE\n",
    )?;
    assert_eq!(
        hook(
            &service,
            f.request("PostToolBatch", &turn, &["call-b", "call-a"])
        )
        .await?,
        json!({})
    );
    let id = step_sources::identity(&f.session, &turn, &["call-a".into(), "call-b".into()]);
    let saved = f.journal.step_source(id).await?.unwrap();
    assert_eq!(
        saved.paths.as_deref().unwrap(),
        vec![
            "binary.bin",
            "empty.txt",
            "keep.txt",
            "remove.txt",
            "run.sh"
        ]
    );
    assert_eq!(fs::read(f.workspace.path.join(".git/index"))?, index);
    assert_eq!(
        git(&f.workspace.path, &["rev-parse", "HEAD"]),
        f.workspace.base
    );
    fs::write(f.workspace.path.join("later.txt"), "later work\n")?;
    assert_eq!(
        hook(
            &service,
            f.request("PostToolBatch", &turn, &["call-a", "call-b"])
        )
        .await?,
        json!({})
    );
    let replay = f.journal.step_source(id).await?.unwrap();
    assert_eq!(serde_json::to_value(&saved)?, serde_json::to_value(replay)?);
    let db = rusqlite::Connection::open(f.root.path().join("db"))?;
    let record: String = db.query_row(
        "SELECT record FROM step_file_sources WHERE id=?1",
        [id.to_string()],
        |row| row.get(0),
    )?;
    assert!(
        !record.contains("RAW_")
            && !record.contains("DUMMY_PRIVATE_VALUE")
            && !record.contains("DUMMY_REFERENCE")
    );
    let reopened = Journal::open(
        f.root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    assert_eq!(reopened.step_source(id).await?.unwrap().paths, saved.paths);
    let path = service.directory().to_owned();
    drop(service);
    assert!(!path.exists());
    Ok(())
}

#[tokio::test]
async fn foreign_sessions_and_changed_workspaces_do_not_acquire_a_source() -> Result<()> {
    let f = Fixture::new().await?;
    let service = Service::start(
        f.journal.clone(),
        f.workspace.track_id.clone(),
        &f.workspace.path,
    )?;
    let turn = Uuid::now_v7().to_string();
    let mut foreign = f.request("PreToolUse", &turn, &["call-foreign"]);
    foreign["session_id"] = json!(Uuid::now_v7());
    assert_eq!(hook(&service, foreign).await?, json!({}));
    assert_eq!(
        hook(&service, f.request("PreToolUse", &turn, &["call-main"])).await?,
        json!({})
    );
    let mut changed = f.workspace.clone();
    changed.attachment_roots.clear();
    f.journal.workspace_result(changed).await?;
    assert_eq!(
        hook(&service, f.request("PostToolBatch", &turn, &["call-main"])).await?["continue"],
        false
    );
    let id = step_sources::identity(&f.session, &turn, &["call-main".into()]);
    assert!(f.journal.step_source(id).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn a_wrong_capability_cannot_capture_or_block_the_next_valid_hook() -> Result<()> {
    use tokio::io::AsyncReadExt;
    let f = Fixture::new().await?;
    let service = Service::start(
        f.journal.clone(),
        f.workspace.track_id.clone(),
        &f.workspace.path,
    )?;
    let mut stream =
        tokio::net::UnixStream::connect(service.directory().join("capture.sock")).await?;
    stream
        .write_all(format!("{}\n{{}}", "0".repeat(64)).as_bytes())
        .await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    assert!(response.is_empty());
    let turn = Uuid::now_v7().to_string();
    assert_eq!(
        hook(&service, f.request("PreToolUse", &turn, &["real-call"])).await?,
        json!({})
    );
    assert_eq!(
        hook(&service, f.request("PostToolBatch", &turn, &["real-call"])).await?,
        json!({})
    );
    assert!(
        f.journal
            .step_source(step_sources::identity(
                &f.session,
                &turn,
                &["real-call".into()]
            ))
            .await?
            .unwrap()
            .paths
            .unwrap()
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn owned_capture_hooks_stay_out_of_the_timeline_but_other_hooks_remain_visible() -> Result<()>
{
    let f = Fixture::new().await?;
    let file = f.root.path().join("owned-settings.json");
    fs::write(&file, "{}")?;
    f.journal
        .register_step_hook(f.workspace.track_id.clone(), file.clone())
        .await?;
    let turn = Uuid::now_v7();
    for (id, key) in [
        ("own", format!("{}:pre_tool_use:def-1", file.display())),
        (
            "other",
            format!("{}other:pre_tool_use:def-1", file.display()),
        ),
    ] {
        f.journal.try_msp(f.workspace.track_id.clone(),f.session.clone(),intercat_daemon::clock::Stamp::now(),json!({"method":"item/completed","params":{"sessionId":f.session,"item":{"itemId":id,"turnId":turn,"kind":"hookRun","hookKey":key,"event":"preToolUse","status":"completed","revision":1}}}),None,None)?.await??;
    }
    let db = rusqlite::Connection::open(f.root.path().join("db"))?;
    let items: Vec<String> = db
        .prepare("SELECT item FROM timeline_items")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(items, vec!["other"]);
    Ok(())
}

#[tokio::test]
async fn a_new_turn_never_uses_an_interrupted_turns_before_state() -> Result<()> {
    let f = Fixture::new().await?;
    let service = Service::start(
        f.journal.clone(),
        f.workspace.track_id.clone(),
        &f.workspace.path,
    )?;
    for number in 0..10 {
        let turn = Uuid::now_v7().to_string();
        assert_eq!(
            hook(
                &service,
                f.request("PreToolUse", &turn, &["interrupted-call"])
            )
            .await?,
            json!({})
        );
        fs::write(
            f.workspace.path.join("keep.txt"),
            format!("interrupted {number}\n"),
        )?;
    }
    let turn = Uuid::now_v7().to_string();
    assert_eq!(
        hook(&service, f.request("PreToolUse", &turn, &["latest-call"])).await?,
        json!({})
    );
    fs::write(f.workspace.path.join("latest.txt"), "latest turn only\n")?;
    assert_eq!(
        hook(
            &service,
            f.request("PostToolBatch", &turn, &["latest-call"])
        )
        .await?,
        json!({})
    );
    let saved = f
        .journal
        .step_source(step_sources::identity(
            &f.session,
            &turn,
            &["latest-call".into()],
        ))
        .await?
        .unwrap();
    assert_eq!(saved.paths.as_deref().unwrap(), vec!["latest.txt"]);
    Ok(())
}

#[tokio::test]
async fn duplicate_tool_identities_cannot_be_saved_as_a_valid_batch() -> Result<()> {
    let f = Fixture::new().await?;
    let service = Service::start(
        f.journal.clone(),
        f.workspace.track_id.clone(),
        &f.workspace.path,
    )?;
    let turn = Uuid::now_v7().to_string();
    hook(&service, f.request("PreToolUse", &turn, &["duplicate"])).await?;
    fs::write(f.workspace.path.join("duplicate.txt"), "dummy change\n")?;
    assert_eq!(
        hook(
            &service,
            f.request("PostToolBatch", &turn, &["duplicate", "duplicate"])
        )
        .await?["continue"],
        false
    );
    assert!(
        f.journal
            .step_source(step_sources::identity(
                &f.session,
                &turn,
                &["duplicate".into()]
            ))
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn restart_restores_the_before_state_without_including_later_work() -> Result<()> {
    let f = Fixture::new().await?;
    let service = Service::start(
        f.journal.clone(),
        f.workspace.track_id.clone(),
        &f.workspace.path,
    )?;
    let turn = Uuid::now_v7().to_string();
    assert_eq!(
        hook(
            &service,
            f.request("PreToolUse", &turn, &["before-restart"])
        )
        .await?,
        json!({})
    );
    fs::write(
        f.workspace.path.join("keep.txt"),
        "changed before restart\n",
    )?;
    drop(service);
    let journal = Journal::open(
        f.root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let service = Service::start(
        journal.clone(),
        f.workspace.track_id.clone(),
        &f.workspace.path,
    )?;
    assert_eq!(
        hook(
            &service,
            f.request("PostToolBatch", &turn, &["before-restart"])
        )
        .await?,
        json!({})
    );
    let source = journal
        .step_source(step_sources::identity(
            &f.session,
            &turn,
            &["before-restart".into()],
        ))
        .await?
        .unwrap();
    assert_eq!(source.paths.as_deref().unwrap(), ["keep.txt"]);
    assert_eq!(
        hook(&service, f.request("PreToolUse", &turn, &["after-restart"])).await?,
        json!({})
    );
    fs::write(f.workspace.path.join("later.txt"), "later work\n")?;
    assert_eq!(
        hook(
            &service,
            f.request("PostToolBatch", &turn, &["after-restart"])
        )
        .await?,
        json!({})
    );
    let source = journal
        .step_source(step_sources::identity(
            &f.session,
            &turn,
            &["after-restart".into()],
        ))
        .await?
        .unwrap();
    assert_eq!(source.paths.as_deref().unwrap(), ["later.txt"]);
    Ok(())
}

#[tokio::test]
#[ignore = "one paid dummy Muse turn through the production host and capture service"]
async fn native_tool_batches_are_saved_before_the_next_batch_runs() -> Result<()> {
    use intercat_daemon::{
        host::{Boundary, Host, Profile},
        msp::Event,
    };
    let mut f = Fixture::new().await?;
    let binary = std::path::PathBuf::from(std::env::var("INTERCAT_MUSE_BINARY")?);
    let auth = std::path::PathBuf::from(std::env::var("INTERCAT_MUSE_AUTH")?);
    let directory = std::path::PathBuf::from(std::env::var("INTERCAT_STEP_OUTPUT")?);
    fs::create_dir_all(&directory)?;
    let rpc = f.root.path().join("rpc");
    fs::create_dir(&rpc)?;
    let capture = Service::start(
        f.journal.clone(),
        f.workspace.track_id.clone(),
        &f.workspace.path,
    )?;
    let mut host = Host::spawn(
        &binary,
        &f.root.path().join("host"),
        &f.workspace.path,
        std::ffi::OsStr::new(&std::env::var("PATH")?),
        Some(&auth),
        Boundary {
            profile: Profile::Worker,
            secrets: &[],
            rpc: &rpc,
            denied: &[],
            steps: Some(capture),
        },
    )
    .await?;
    let result=async {
        host.connection.client.initialize().await?;
        let opened=host.connection.client.request("session/start",json!({"commandId":Uuid::now_v7(),"workspaceRoot":f.workspace.path,"providerId":"meta","approvalMode":"promptUnmatched"})).await?;
        f.session=opened["session"]["sessionId"].as_str().unwrap().to_owned();
        f.journal.worker_state(WorkerState{track_id:f.workspace.track_id.clone(),phase:"ready".into(),session_id:Some(f.session.clone()),secret_revision:None,identity:Some(host.identity),detail:None,active_turn:None,approvals:vec![],output:None,checkpoint_pending:false,checkpoint:None,prompts:vec![]}).await?;
        let commands=["printf FIRST > keep.txt; : > empty.txt", "printf SECOND > keep.txt; printf NEXT > second.txt"];
        let prompt=format!("Disposable file capture test. Do exactly two sequential bash calls. First execute exactly: {}. After that completes, execute exactly: {}. Do not combine the calls or use other tools. Then reply DONE.",commands[0],commands[1]);
        let prepared=f.journal.screen_start(json!({"commandId":Uuid::now_v7(),"sessionId":f.session,"reasoningEffort":"minimal","input":[{"type":"text","text":prompt}]})).await?;
        let started=host.connection.client.start(prepared,intercat_daemon::msp::StartGuard::Stopped(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)))).await?;
        let turn=started["turnId"].as_str().unwrap().to_owned();
        let mut decided=std::collections::BTreeSet::new();let mut call_ids=std::collections::BTreeSet::new();let mut hook_frames=0;
        tokio::time::timeout(std::time::Duration::from_secs(90),async {
            loop {
                let Some(event)=host.connection.next_event().await else{return Err("Host closed".into())};
                let frame=match event {Event::Frame(received)=>received.frame,Event::Closed(reason)=>return Err(reason.into())};
                let p=&frame["params"];if p["sessionId"]!=f.session {continue}
                if p["item"]["kind"]=="hookRun" {hook_frames+=1;}
                f.journal.try_msp(f.workspace.track_id.clone(),f.session.clone(),intercat_daemon::clock::Stamp::now(),frame.clone(),None,None)?.await??;
                if matches!(frame["method"].as_str(),Some("approval/request"|"approval/requested")) {
                    let key=(p["approvalId"].to_string(),p["currentRequirementId"].to_string());if !decided.insert(key){continue}
                    let allowed=p["subject"]["kind"]=="shell" && p["subject"]["workspaceRoot"]==f.workspace.path.to_string_lossy().as_ref() && p["subject"]["command"].as_str().is_some_and(|value|commands.contains(&value));
                    let choice=p["availableChoices"].as_array().unwrap().iter().find(|c|c["scope"]=="once" && c["decision"]==if allowed{"approved"}else{"abort"}).unwrap();
                    host.connection.client.request("approval/decide",json!({"commandId":Uuid::now_v7(),"sessionId":f.session,"approvalId":p["approvalId"],"requirementId":p["currentRequirementId"],"choiceId":choice["choiceId"]})).await?;
                }
                if frame["method"]=="item/completed" && p["item"]["kind"]=="toolCall" {
                    call_ids.insert(p["item"]["callId"].as_str().unwrap().to_owned());
                }
                if frame["method"]=="turn/completed" && p["turnId"]==turn {break}
            }
            Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
        }).await??;
        let db=rusqlite::Connection::open(f.root.path().join("db"))?;
        let records=db.prepare("SELECT record FROM step_file_sources WHERE session=?1 AND turn=?2 ORDER BY rowid")?.query_map(rusqlite::params![f.session,turn],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let sources=records.iter().map(|record|serde_json::from_str::<step_sources::Source>(record)).collect::<std::result::Result<Vec<_>,_>>()?;
        assert!(hook_frames>0);let visible:i64=db.query_row("SELECT count(*) FROM timeline_items WHERE json_extract(record,'$.kind')='hookRun'",[],|row|row.get(0))?;assert_eq!(visible,0);
        assert_eq!(sources.len(),2);
        assert_eq!(sources[0].paths.as_deref().unwrap(),vec!["empty.txt","keep.txt"]);
        assert_eq!(sources[1].paths.as_deref().unwrap(),vec!["keep.txt","second.txt"]);
        let bound:i64=db.query_row("SELECT count(*) FROM step_source_clips b JOIN step_file_sources s ON s.id=b.source WHERE s.session=?1 AND s.turn=?2",rusqlite::params![f.session,turn],|row|row.get(0))?;
        assert_eq!(bound,2);
        for source in &sources {
            let clip:String=db.query_row("SELECT clip FROM step_source_clips WHERE source=?1",[source.id.to_string()],|row|row.get(0))?;
            let calls=db.prepare("SELECT json_extract(record,'$.callId') FROM timeline_items WHERE clip=?1 AND json_extract(record,'$.kind')='toolCall'")?.query_map([&clip],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<std::collections::BTreeSet<_>>>()?;
            assert_eq!(calls,source.calls.iter().cloned().collect());
        }
        assert_eq!(sources.iter().flat_map(|source|source.calls.iter().cloned()).collect::<std::collections::BTreeSet<_>>(),call_ids);
        fs::write(directory.join("result.json"),serde_json::to_vec_pretty(&json!({"sources":sources,"paidTurns":1,"ownedHookFramesHidden":hook_frames,"schema":intercat_daemon::journal::SCHEMA_VERSION}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let cleanup = host.shutdown().await;
    result?;
    cleanup?;
    Ok(())
}
