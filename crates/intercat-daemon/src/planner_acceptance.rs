//! Acceptance fixtures contain only agent-created disposable repositories.
use super::*;
use crate::{
    attachment_store::Attachment,
    checkpoint::Checkpoint,
    decision::Target,
    journal::Journal,
    plans::{GenerationState, Plan},
    request_sources::Submission,
    secrets::Key,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

const PRIVATE: &str = "dummy-private-planner-reference-token";
fn git(path: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("/usr/bin/git")
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=Dummy",
            "-c",
            "user.email=dummy@localhost",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).to_string().into());
    }
    Ok(String::from_utf8(out.stdout)?.trim().into())
}
fn commit(
    repo: &Repository,
    parent: intercat_git::ObjectId,
    text: &str,
) -> Result<intercat_git::ObjectId> {
    repo.checkpoint_commit(
        repo.write_tree(&[
            ("basis.txt", repo.write_blob(text.as_bytes())?),
            (".env", repo.write_blob(b"DUMMY_ENV_VALUE=not_for_planner")?),
        ])?,
        parent,
        "Dummy saved tree",
    )
}
struct Fixture {
    root: tempfile::TempDir,
    journal: Journal,
    plan: Plan,
    project: PathBuf,
    expected: Vec<BasedOn>,
    attachments: Vec<Attachment>,
    originals: Vec<PathBuf>,
}
impl Fixture {
    async fn new(active_workers: bool) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let project = root.path().join("project");
        fs::create_dir(&project)?;
        git(&project, &["init", "-q", "-b", "main"])?;
        fs::write(project.join("basis.txt"), "MAIN_BASE: orange turtle")?;
        git(&project, &["add", "basis.txt"])?;
        git(&project, &["commit", "-qm", "Dummy base"])?;
        let configured = crate::project::inspect(
            Uuid::now_v7(),
            "Dummy planning".into(),
            vec![project.clone()],
        )
        .await?;
        let folder = configured.folders[0].id();
        let repo = Repository::open(&project)?;
        let base = repo.head_commit()?;
        let session_head = commit(&repo, base, "SESSION_SAVED: silver oak")?;
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        journal
            .configure_project(Uuid::now_v7(), configured)
            .await?;
        let key = Key::new([7; 32]);
        let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        rusqlite::Connection::open(root.path().join("db"))?.execute(
            "INSERT INTO msp_positions VALUES('dummy-mask-session','dummy-mask-track',NULL)",
            [],
        )?;
        let catalog = journal
            .register_fingerprint(crate::secrets::Registration {
                op_id: Uuid::now_v7(),
                track_id: "dummy-mask-track".into(),
                session_id: "dummy-mask-session".into(),
                name: "PLANNER_TEST_TOKEN".into(),
                byte_len: PRIVATE.len(),
                tag: hex(&key.tag(PRIVATE.as_bytes())),
                key_check: hex(&key.check()),
            })
            .await?;
        journal.confirm_fingerprints(catalog.current).await?;
        let file = root.path().join("specification.txt");
        fs::write(
            &file,
            "Create exactly two independent jobs. Job one uses new:1 and writes one.txt containing exactly DUMMY_one. Job two uses new:2 and writes two.txt containing exactly DUMMY_two. Each job must read both attached references. Include the saved session's color and subject in both jobs' constraints. If active track roots are supplied, include both active tracks' saved colors and subjects too. During planning, read only the saved roots, never the source checkouts. During execution, each worker writes in its own assigned worktree, not in the Planner snapshots. Each worker must use file read/edit tools only, never shell commands, network, agents or commits. Each worker must end its final message with this exact standalone footer, including its reason: Result: done — created the requested file. Do not implement work while planning.",
        )?;
        let plain = journal
            .import_attachment(Uuid::now_v7(), file.clone())
            .await?;
        let paste = root.path().join("pasted.txt");
        fs::write(
            &paste,
            format!(
                "Use the literal suffix DUMMY_REFERENCE_OK as context in both item instructions. Diagnostic token (ignore): {PRIVATE}"
            ),
        )?;
        let pasted = journal
            .import_pasted_text(Uuid::now_v7(), paste.clone())
            .await?;
        fs::write(&file, "CHANGED_ORIGINAL: black panther")?;
        fs::remove_file(&paste)?;
        let attachments = vec![plain, pasted];
        journal.stage_document(Submission{op_id:Uuid::now_v7(),text:"Read both attached references and the supplied saved roots, then make exactly the two jobs described by the specification. Do not implement work.".into(),target:Target::New,selected_track_id:None,attachment_ids:attachments.iter().map(|a|a.id).collect(),sources:None,execute_allowed:false,plan_item:None}).await?;
        let state = journal.snapshot().await?;
        let session = state["session"]["id"].as_str().unwrap();
        let slug = state["session"]["slug"].as_str().unwrap();
        let marker = repo.write_blob(format!("Intercat session {session}\n").as_bytes())?;
        repo.compare_and_swap(&format!("refs/intercat/{slug}/session"), None, marker)?;
        repo.compare_and_swap(&format!("refs/heads/intercat/{slug}"), None, session_head)?;
        let staging = commit(&repo, session_head, "STAGING_ONLY: copper hedgehog")?;
        repo.compare_and_swap(&format!("refs/intercat/{slug}/staging"), None, staging)?;
        journal
            .bind_session(session.into(), folder.clone(), slug.into())
            .await?;
        let mut expected = vec![BasedOn {
            folder_id: folder.clone(),
            track_id: None,
            sha: session_head.to_string(),
        }];
        if active_workers {
            for (label, saved, active, has_checkpoint) in [
                ("A", "A_SAVED: amber otter", true, true),
                ("B", "B_SAVED: violet heron", true, true),
                ("C", "IDLE_ONLY: brown mole", false, true),
                ("D", "NO_CHECKPOINT: yellow seal", true, false),
            ] {
                let track = Uuid::now_v7().to_string();
                let path = root.path().join(format!("work-{label}"));
                let sha = commit(&repo, session_head, saved)?;
                git(
                    &project,
                    &[
                        "worktree",
                        "add",
                        "--quiet",
                        "--detach",
                        path.to_str().unwrap(),
                        &sha.to_string(),
                    ],
                )?;
                fs::write(
                    path.join("basis.txt"),
                    format!("UNSAVED_{label}: magenta fox"),
                )?;
                let db = rusqlite::Connection::open(root.path().join("db"))?;
                db.execute(
                    "INSERT INTO tracks VALUES(?1,?2,'Dummy worker','working')",
                    rusqlite::params![track, label],
                )?;
                crate::folders::bind(&db, &track, &folder)?;
                journal
                    .workspace_result(crate::preparation::Workspace {
                        denied_roots: Vec::new(),
                        track_id: track.clone(),
                        path,
                        reference: format!("refs/intercat/{slug}/tracks/{label}"),
                        base: session_head.to_string(),
                        state: "ready".into(),
                        error: None,
                        attachment_roots: vec![],
                    })
                    .await?;
                let checkpoint = has_checkpoint.then(|| Checkpoint {
                    turn_id: Uuid::now_v7(),
                    sha: sha.to_string(),
                    reference: format!("refs/intercat/{slug}/checkpoints/{label}"),
                    unmerged_paths: vec![],
                    live_processes: false,
                });
                journal
                    .worker_state(crate::runtime::WorkerState {
                        track_id: track.clone(),
                        phase: if active { "working" } else { "ready" }.into(),
                        session_id: Some(Uuid::now_v7().to_string()),
                        secret_revision: Some(0),
                        identity: None,
                        detail: None,
                        active_turn: active.then(Uuid::now_v7),
                        approvals: vec![],
                        output: None,
                        checkpoint_pending: false,
                        checkpoint,
                        prompts: vec![],
                    })
                    .await?;
                if active && has_checkpoint {
                    expected.push(BasedOn {
                        folder_id: folder.clone(),
                        track_id: Some(track),
                        sha: sha.to_string(),
                    });
                }
            }
        }
        fs::write(project.join("basis.txt"), "UNSAVED_MAIN: scarlet whale")?;
        let plan = journal.plans().await?.pop().unwrap();
        Ok(Self {
            root,
            journal,
            plan,
            project,
            expected,
            attachments,
            originals: vec![file, paste],
        })
    }
}
fn assert_content(draft: &Draft, active: bool) -> Result<()> {
    assert_eq!(draft.items.len(), 2);
    assert!(
        draft
            .items
            .iter()
            .all(|item| crate::privacy::display_length(&item.title) <= 30)
    );
    let text = serde_json::to_string(draft)?.to_lowercase();
    for value in [
        "dummy_one",
        "dummy_two",
        "dummy_reference_ok",
        "silver",
        "oak",
    ] {
        assert!(text.contains(value), "Missing {value}");
    }
    if active {
        for value in ["amber", "otter", "violet", "heron"] {
            assert!(text.contains(value), "Missing {value}");
        }
    }
    for excluded in [
        PRIVATE,
        "scarlet",
        "whale",
        "magenta",
        "copper",
        "hedgehog",
        "brown",
        "mole",
        "yellow seal",
        "black panther",
        "dummy_env_value",
    ] {
        assert!(
            !text.contains(excluded),
            "Included excluded dummy content {excluded}"
        );
    }
    Ok(())
}
#[tokio::test]
async fn saved_roots_select_the_session_and_only_active_workers_last_checkpoints() -> Result<()> {
    let f = Fixture::new(true).await?;
    f.journal
        .begin_plan(f.plan.id, f.plan.generation.id, f.plan.rev)
        .await?;
    let (snapshot, based_on, context, revision) =
        prepare(&f.journal, &f.plan, f.root.path()).await?;
    assert_eq!(based_on, f.expected);
    assert!(revision.is_none());
    assert_eq!(snapshot.roots.len(), 3);
    let index = fs::read_to_string(context["savedFiles"].as_str().unwrap())?;
    let paths = index
        .lines()
        .map(serde_json::from_str::<PathBuf>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(paths.len(), 3);
    assert!(paths.iter().all(|path| path.starts_with(snapshot.path())
        && path.file_name().unwrap() == "basis.txt"
        && path.is_file()));
    assert!(!index.contains(".env") && !index.contains("work-C") && !index.contains("work-D"));
    for (index, words) in ["silver oak", "amber otter", "violet heron"]
        .iter()
        .enumerate()
    {
        assert!(fs::read_to_string(snapshot.roots[index].join("basis.txt"))?.contains(words));
        assert!(!snapshot.roots[index].join(".env").exists());
        assert!(!snapshot.roots[index].join(".git").exists());
        assert_eq!(context["roots"][index]["sha"], based_on[index].sha);
    }
    assert!(f.journal.plan_based_on(&f.plan, based_on).await?);
    assert_eq!(f.journal.plans().await?[0].based_on, f.expected);
    assert!(fs::read_to_string(f.project.join("basis.txt"))?.contains("UNSAVED_MAIN"));
    Ok(())
}
#[tokio::test]
async fn planner_receives_sealed_file_bytes_and_masked_paste_with_original_identities() -> Result<()>
{
    let f = Fixture::new(false).await?;
    let (mut snapshot, _, context, _) = prepare(&f.journal, &f.plan, f.root.path()).await?;
    snapshot.roots.insert(0, snapshot.path().to_path_buf());
    let prepared = f
        .journal
        .planner_start(
            &f.plan,
            Uuid::now_v7(),
            &snapshot.roots,
            snapshot.path().join("references"),
            PROMPT,
            context,
            None,
        )
        .await?;
    let text = prepared.params["input"][0]["text"].as_str().unwrap();
    assert!(!text.contains(PRIVATE));
    let tail: serde_json::Value = serde_json::from_str(text.split_once("\n\n").unwrap().1)?;
    for (index, reference) in tail["references"].as_array().unwrap().iter().enumerate() {
        assert_eq!(reference["id"], f.attachments[index].id.to_string());
        let path = Path::new(reference["path"].as_str().unwrap());
        let content = fs::read_to_string(path)?;
        assert!(!content.contains(PRIVATE));
        assert!(!content.contains("CHANGED_ORIGINAL"));
        if index == 1 {
            assert!(content.contains("<secret:PLANNER_TEST_TOKEN>"));
        }
        assert_eq!(
            path.file_name().unwrap().to_str().unwrap(),
            crate::attachment_store::copy_name(&f.attachments[index])
        );
    }
    snapshot.freeze()?;
    assert!(fs::write(snapshot.roots[1].join("basis.txt"), "bad").is_err());
    assert!(fs::write(snapshot.path().join("new.txt"), "bad").is_err());
    assert_eq!(f.journal.plans().await?[0].attachments, f.attachments);
    assert!(!f.originals[1].exists());
    Ok(())
}
#[tokio::test]
async fn changed_paste_fingerprints_stop_before_any_planner_copy_is_created() -> Result<()> {
    let f = Fixture::new(false).await?;
    let extra = "DUMMY_REFERENCE_OK";
    let key = Key::new([7; 32]);
    let hex = |b: &[u8]| b.iter().map(|v| format!("{v:02x}")).collect::<String>();
    let catalog = f
        .journal
        .register_fingerprint(crate::secrets::Registration {
            op_id: Uuid::now_v7(),
            track_id: "dummy-mask-track".into(),
            session_id: "dummy-mask-session".into(),
            name: "NEW_PLANNER_TOKEN".into(),
            byte_len: extra.len(),
            tag: hex(&key.tag(extra.as_bytes())),
            key_check: hex(&key.check()),
        })
        .await?;
    f.journal.confirm_fingerprints(catalog.current).await?;
    let (snapshot, _, context, _) = prepare(&f.journal, &f.plan, f.root.path()).await?;
    let references = snapshot.path().join("references");
    let error = f
        .journal
        .planner_start(
            &f.plan,
            Uuid::now_v7(),
            &snapshot.roots,
            references.clone(),
            PROMPT,
            context,
            None,
        )
        .await
        .err()
        .ok_or("Expected privacy review")?;
    assert!(
        error
            .downcast_ref::<crate::attachment_screening::ReviewRequired>()
            .is_some()
    );
    assert!(!references.exists());
    Ok(())
}
#[tokio::test]
#[ignore = "one paid dummy Planner turn; requires pinned Muse and INTERCAT_M3_PLANNER_OUTPUT"]
async fn real_planner_reads_attachments_and_multiple_saved_worker_roots() -> Result<()> {
    let f = Fixture::new(true).await?;
    f.journal
        .begin_plan(f.plan.id, f.plan.generation.id, f.plan.rev)
        .await?;
    let config = Config {
        source: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
        binary: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
        credentials: std::env::var("INTERCAT_MUSE_AUTH")?.into(),
        path: std::env::var("PATH")?,
    };
    let rpc = f.root.path().join("rpc");
    fs::create_dir(&rpc)?;
    let guard = f
        .journal
        .plan_guard(&f.plan, Arc::new(crate::dispatch_control::Gate::new(false)))
        .await?
        .ok_or("Missing Planner guard")?;
    let (_stop, stopping) = watch::channel(false);
    let (draft, based_on) = run(
        crate::host_memory::Pool::new(),
        &f.journal,
        &config,
        f.root.path(),
        &rpc,
        &f.plan,
        guard,
        stopping,
    )
    .await?;
    let output = PathBuf::from(std::env::var("INTERCAT_M3_PLANNER_OUTPUT")?);
    fs::create_dir_all(&output)?;
    fs::write(
        output.join("draft.json"),
        serde_json::to_vec_pretty(&json!({"draft":draft,"basedOn":based_on}))?,
    )?;
    assert_content(&draft, true)?;
    assert_eq!(based_on, f.expected);
    f.journal
        .generated_plan(
            f.plan.id,
            f.plan.generation.id,
            f.plan.rev,
            crate::plan_store::Generated::Draft(draft.clone()),
            based_on.clone(),
        )
        .await?;
    let ready = f.journal.plans().await?.pop().unwrap();
    assert_eq!(ready.generation.state, GenerationState::Ready);
    assert_eq!(ready.attachments, f.attachments);
    let output = PathBuf::from(std::env::var("INTERCAT_M3_PLANNER_OUTPUT")?);
    fs::create_dir_all(&output)?;
    fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(
            &json!({"draft":draft,"basedOn":based_on,"plan":ready,"savedRootsOnly":true,"maskedPaste":true,"paidTurns":1}),
        )?,
    )?;
    Ok(())
}

async fn editor(root: &Path) -> Result<crate::editor::Manager> {
    let source = root.join("editor/intercat_editor");
    fs::create_dir_all(&source)?;
    fs::write(source.join("__init__.py"), "")?;
    fs::write(
        source.join("server.py"),
        r#"import json
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
class Handler(BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_GET(self):
  body=b'{"accepting":true,"modelLoaded":true}';self.send_response(200);self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
 def do_POST(self):
  self.rfile.read(int(self.headers['Content-Length']));body=b'{"tokens":10,"cachePrefix":false,"stages":{"question":{"selected":"0"}},"inferenceMs":1}';self.send_response(200);self.send_header('X-Intercat-Inference','started');self.send_header('Content-Length',str(len(body)));self.end_headers();self.wfile.write(body)
server=ThreadingHTTPServer(('127.0.0.1',0),Handler)
print(json.dumps({'host':'127.0.0.1','port':server.server_port}),flush=True)
server.serve_forever()
"#,
    )?;
    let model = root.join("dummy-model");
    fs::create_dir(&model)?;
    let editor = crate::editor::Manager::new(&root.join("editor"), &root.join("editor.log"))?;
    editor
        .configure(crate::editor::Config {
            python: "/usr/bin/python3".into(),
            model,
        })
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !editor.status().model_loaded {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(editor)
}
fn config() -> Result<Config> {
    Ok(Config {
        source: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
        binary: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
        credentials: std::env::var("INTERCAT_MUSE_AUTH")?.into(),
        path: std::env::var("PATH")?,
    })
}
#[tokio::test]
#[ignore = "three paid dummy turns: read-only Planner and two workers reading the same references"]
async fn real_planner_references_reach_each_worker_with_original_ids_and_masked_paste() -> Result<()>
{
    let f = Fixture::new(false).await?;
    let output = PathBuf::from(std::env::var("INTERCAT_M3_PLANNER_OUTPUT")?);
    fs::create_dir_all(&output)?;
    let config = config()?;
    let rpc = f.root.path().join("rpc");
    fs::create_dir(&rpc)?;
    f.journal
        .begin_plan(f.plan.id, f.plan.generation.id, f.plan.rev)
        .await?;
    let guard = f
        .journal
        .plan_guard(&f.plan, Arc::new(crate::dispatch_control::Gate::new(false)))
        .await?
        .ok_or("Missing Planner guard")?;
    let (_stop, stopping) = watch::channel(false);
    let (draft, based_on) = run(
        crate::host_memory::Pool::new(),
        &f.journal,
        &config,
        f.root.path(),
        &rpc,
        &f.plan,
        guard,
        stopping,
    )
    .await?;
    fs::write(
        output.join("draft.json"),
        serde_json::to_vec_pretty(
            &json!({"draft":draft,"basedOn":based_on,"attachments":f.attachments}),
        )?,
    )?;
    assert_content(&draft, false)?;
    assert_eq!(based_on, f.expected);
    f.journal
        .generated_plan(
            f.plan.id,
            f.plan.generation.id,
            f.plan.rev,
            crate::plan_store::Generated::Draft(draft),
            based_on,
        )
        .await?;
    f.journal.set_worker_config(Uuid::now_v7(), config).await?;
    let preparation = crate::preparation::Preparation::start(
        &f.root.path().join("workspaces"),
        f.journal.clone(),
    )?;
    let editor = editor(f.root.path()).await?;
    let runtime = crate::runtime::Runtime::start(
        crate::host_memory::Pool::new(),
        f.root.path().into(),
        rpc,
        f.journal.clone(),
        editor.clone(),
        Arc::new(crate::scheduler::Capacity::new(2)),
        Arc::new(crate::dispatch_control::Gate::new(false)),
    );
    let result=async {
        let project_id=f.journal.snapshot().await?["projectSecrets"]["projectId"].as_str().unwrap().to_owned();
        let key=Key::new([7;32]);
        let catalog=f.journal.prepare_project_secrets(crate::project_secrets::Prepare{op_id:Uuid::now_v7(),project_id:project_id.clone(),revision:0,fingerprints:vec![],key_check:key.check().iter().map(|b|format!("{b:02x}")).collect()}).await?;
        f.journal.confirm_fingerprints(catalog.current).await?;
        runtime.service.activate_project_secrets(crate::project_secrets::Load{project_id,revision:0,entries:vec![]}).await?;
        let ready=f.journal.plans().await?.pop().unwrap();
        let command=|action|crate::plan_store::Command{op_id:Uuid::now_v7(),plan_id:ready.id,revision:ready.rev,action};
        f.journal.plan_command(command(crate::plan_store::Action::Review)).await?;
        f.journal.plan_command(command(crate::plan_store::Action::Start)).await?;
        let snapshot=tokio::time::timeout(std::time::Duration::from_secs(180),async {
            loop {
                f.journal.advance_plans().await?;
                let snapshot=f.journal.snapshot().await?;
                fs::write(output.join("latest.json"),serde_json::to_vec_pretty(&snapshot)?)?;
                if snapshot["workers"].as_array().unwrap().iter().any(|w|!w["approvals"].as_array().unwrap().is_empty()||!w["prompts"].as_array().unwrap().is_empty()) {return Err::<_,Box<dyn std::error::Error+Send+Sync>>("Dummy reference work requested an unexpected approval.".into());}
                let plan=f.journal.plans().await?.pop().unwrap();
                if plan.items.iter().any(|item|item.state==crate::plans::ItemState::Failed) {return Err("A dummy reference item failed.".into());}
                if plan.state==crate::plans::State::Done {break Ok(snapshot);}
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }).await??;
        let plan=f.journal.plans().await?.pop().unwrap();
        assert_eq!(plan.attachments,f.attachments);
        let db=rusqlite::Connection::open(f.root.path().join("db"))?;
        for item in &plan.items {
            let workspace=f.journal.workspace(item.track_ref.clone()).await?;
            let first=item.instruction.contains("one.txt");
            assert_eq!(fs::read_to_string(workspace.path.join(if first{"one.txt"}else{"two.txt"}))?.trim(),if first{"DUMMY_one"}else{"DUMMY_two"});
            for original in &f.attachments {
                let path=workspace.path.join(&workspace.attachment_roots[0]).join(crate::attachment_store::copy_name(original));
                let bytes=fs::read(&path)?;
                assert_eq!(crate::attachment_store::digest(&bytes),original.sha256);
                assert!(!String::from_utf8_lossy(&bytes).contains(PRIVATE));
            }
            let payload:String=db.query_row("SELECT o.payload FROM utterances u JOIN operations o ON o.id=u.dispatch_command WHERE u.id=?1",[item.utterance_id.unwrap().to_string()],|row|row.get(0))?;
            assert!(!payload.contains(PRIVATE));
            for original in &f.attachments {assert!(payload.contains(&crate::attachment_store::copy_name(original)));}
        }
        fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"snapshot":snapshot,"attachments":f.attachments,"originalIdsAndHashesPreserved":true,"workerCopiesVerified":2,"maskedPaste":true,"paidTurns":3}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    runtime.shutdown().await;
    preparation.shutdown().await;
    editor.stop().await;
    result
}

#[tokio::test]
async fn tampered_attachment_stops_planning_before_it_is_made_readable() -> Result<()> {
    let f = Fixture::new(false).await?;
    let (snapshot, _, context, _) = prepare(&f.journal, &f.plan, f.root.path()).await?;
    let file = f
        .journal
        .attachment_store()
        .root
        .join(f.attachments[0].id.to_string());
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
    fs::write(file, "tampered dummy attachment")?;
    let references = snapshot.path().join("references");
    let error = f
        .journal
        .planner_start(
            &f.plan,
            Uuid::now_v7(),
            &snapshot.roots,
            references.clone(),
            PROMPT,
            context,
            None,
        )
        .await
        .err()
        .ok_or("Expected corrupt attachment failure")?;
    let message = error.to_string().to_lowercase();
    assert!(message.contains("attachment") || message.contains("original"));
    assert!(!references.exists());
    Ok(())
}
#[tokio::test]
async fn a_superseded_planner_cannot_replace_the_new_attempts_saved_sources() -> Result<()> {
    let f = Fixture::new(false).await?;
    f.journal
        .begin_plan(f.plan.id, f.plan.generation.id, f.plan.rev)
        .await?;
    let (snapshot, based_on, _, _) = prepare(&f.journal, &f.plan, f.root.path()).await?;
    f.journal
        .plan_command(crate::plan_store::Command {
            op_id: Uuid::now_v7(),
            plan_id: f.plan.id,
            revision: f.plan.rev,
            action: crate::plan_store::Action::Retry,
        })
        .await?;
    assert!(!f.journal.plan_based_on(&f.plan, based_on).await?);
    assert!(f.journal.plans().await?[0].based_on.is_empty());
    let path = snapshot.path().to_path_buf();
    drop(snapshot);
    assert!(!path.exists());
    Ok(())
}

pub(super) fn record_frame(frame: &serde_json::Value) -> Result<()> {
    let Ok(path) = std::env::var("INTERCAT_M3_PLANNER_TRACE") else {
        return Ok(());
    };
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(
        file,
        "{}",
        json!({"receivedAt":crate::journal::timestamp(),"frame":frame})
    )?;
    Ok(())
}

#[tokio::test]
async fn local_reader_fixture_starts_and_stops_before_native_worker_tests() -> Result<()> {
    let root = tempfile::tempdir()?;
    let reader = editor(root.path()).await?;
    assert!(reader.status().model_loaded);
    reader.stop().await;
    Ok(())
}
