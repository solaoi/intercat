//! Shared admission on real, isolated Muse hosts using the offline echo provider.
#[path = "runtime_native_fresh.rs"]
mod fresh_handoff;
#[path = "two_ways_native.rs"]
mod two_ways;
use super::*;
use crate::decision::{Decision, Kind, Outcome, Target};
use std::time::Duration;

struct QueueFixture {
    root: tempfile::TempDir,
    journal: Journal,
    service: Service,
    config: Config,
    source_head: String,
    editor: crate::editor::Manager,
    memory: Arc<crate::host_memory::Pool>,
    capacity: Arc<crate::scheduler::Capacity>,
    words: Vec<(String, String)>,
    stop: watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _service_stop: watch::Sender<bool>,
}

impl QueueFixture {
    async fn new() -> Result<Self> {
        Self::with_initial_instruction("QUEUE_DUMMY_0").await
    }

    async fn with_initial_instruction(instruction: &str) -> Result<Self> {
        let (root, runner, _, mut service, _, service_stop) =
            reconnect_fixture(instruction).await?;
        let journal = runner.journal.clone();
        let first = journal.snapshot().await?["utterances"][0]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let mut words = vec![(first, runner.state.track_id.clone())];
        let source = root.path().join("source");
        std::fs::create_dir(&source)?;
        native_git(&source, &["init", "-q", "-b", "main"])?;
        std::fs::write(source.join("README.txt"), "Agent-created queue fixture.\n")?;
        native_git(&source, &["add", "."])?;
        native_git(&source, &["commit", "-qm", "fixture"])?;
        let source_head = native_git(&source, &["rev-parse", "HEAD"])?;
        journal
            .configure_project(
                Uuid::now_v7(),
                crate::project::inspect(Uuid::now_v7(), "Queue fixture".into(), vec![source])
                    .await?,
            )
            .await?;
        for index in 1..5 {
            journal
                .stage_utterance(
                    Uuid::now_v7(),
                    format!("QUEUE_DUMMY_{index}"),
                    Target::New,
                    None,
                    vec![],
                )
                .await?;
            let word = journal.claim_input().await?.unwrap();
            let id = word.id.clone();
            journal
                .apply_input(
                    word.id,
                    word.revision,
                    Outcome::Classified(Decision {
                        kind: Kind::Fork,
                        track_id: None,
                        boundary: Some("when-told".into()),
                        command: None,
                        confidence: 1.,
                        inference_ms: 1.,
                        timed_out: false,
                        scores: json!({}),
                    }),
                )
                .await?;
            let track = journal.read_utterance(id.clone()).await?["decision"]["trackId"]
                .as_str()
                .unwrap()
                .to_owned();
            words.push((id, track));
        }
        let project = journal.snapshot().await?["projectSecrets"]["projectId"]
            .as_str()
            .unwrap()
            .to_owned();
        journal
            .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
            .await?;
        let key = crate::secrets::Key::new([7; 32]);
        let catalog = journal
            .prepare_project_secrets(crate::project_secrets::Prepare {
                op_id: Uuid::now_v7(),
                project_id: project.clone(),
                revision: 0,
                fingerprints: vec![],
                key_check: key
                    .check()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            })
            .await?;
        journal.confirm_fingerprints(catalog.current).await?;
        service
            .activate_project_secrets(crate::project_secrets::Load {
                project_id: project,
                revision: 0,
                entries: vec![],
            })
            .await?;
        let preparation = crate::preparation::Preparation::start(
            &root.path().join("workspaces"),
            journal.clone(),
        )?;
        let prepared = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let snapshot = journal.snapshot().await?;
                if snapshot["workspaces"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|workspace| workspace["state"] == "ready")
                    .count()
                    == words.len()
                {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?;
        preparation.shutdown().await;
        prepared?;
        let binary = PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?).canonicalize()?;
        assert_eq!(host::hash(&binary)?, host::BINARY_SHA256);
        let credentials = root.path().join("dummy-auth.json");
        std::fs::write(&credentials, r#"{"schema_version":1}"#)?;
        let config = Config {
            source: binary.clone(),
            binary,
            credentials,
            path: std::env::var("PATH")?,
        };
        let editor = native_editor(root.path()).await?;
        let capacity = Arc::new(crate::scheduler::Capacity::new(1));
        service.capacity = capacity.clone();
        let memory = crate::host_memory::Pool::fixed(crate::host_memory::Sample {
            free_bytes: 8 * 1024 * 1024 * 1024,
            pressured: false,
        });
        let (stop, _) = watch::channel(false);
        Ok(Self {
            root,
            journal,
            service,
            config,
            source_head,
            editor,
            memory,
            capacity,
            words,
            stop,
            tasks: vec![],
            _service_stop: service_stop,
        })
    }

    async fn launch(&mut self, indices: &[usize]) -> Result<()> {
        for &index in indices {
            let track = self.words[index].1.clone();
            self.launch_worker(&track, true).await?;
        }
        self.wait_workers(indices, false).await
    }

    async fn launch_worker(&mut self, track: &str, offline_echo: bool) -> Result<()> {
        let previous = self.journal.snapshot().await?["workers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|worker| worker["trackId"] == track)
            .cloned()
            .map(serde_json::from_value)
            .transpose()?;
        let (port, commands) =
            crate::execution::Port::new(Arc::new(crate::dispatch_control::Gate::new(false)));
        self.service
            .ports
            .lock()
            .unwrap()
            .insert(track.to_owned(), port.clone());
        let launch = Launch {
            host_memory: self.memory.clone(),
            config: self.config.clone(),
            data: self.root.path().into(),
            rpc: self.root.path().join("rpc"),
            journal: self.journal.clone(),
            workspace: serde_json::to_value(self.journal.workspace(track.to_owned()).await?)?,
            previous,
            stopping: self.stop.subscribe(),
            execution: crate::execution::Context {
                project_secrets: self.service.project_secrets(),
                port,
                commands,
                editor: self.editor.clone(),
                capacity: self.capacity.clone(),
            },
        };
        self.tasks.push(if offline_echo {
            tokio::spawn(host::OFFLINE_ECHO.scope((), run_host(launch)))
        } else {
            tokio::spawn(run_host(launch))
        });
        Ok(())
    }

    async fn wait_workers(&self, indices: &[usize], checkpoint: bool) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let snapshot = self.journal.snapshot().await?;
                let workers = snapshot["workers"].as_array().unwrap();
                for worker in workers {
                    if matches!(
                        worker["phase"].as_str(),
                        Some("failed" | "lost" | "needs-recovery")
                    ) {
                        return Err(format!("Queue worker failed: {worker}").into());
                    }
                }
                if indices.iter().all(|&index| {
                    workers.iter().any(|worker| {
                        worker["trackId"] == self.words[index].1
                            && if checkpoint {
                                !worker["checkpoint"].is_null()
                                    && worker["checkpointPending"] == false
                            } else {
                                worker["phase"] == "ready"
                            }
                    })
                }) {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?
    }

    async fn start(&self, index: usize) -> Result<()> {
        let id = self.words[index].0.clone();
        let revision = self.journal.read_utterance(id.clone()).await?["revision"]
            .as_i64()
            .unwrap();
        self.service
            .start_held_input(Uuid::now_v7(), id, revision)
            .await?;
        Ok(())
    }

    async fn queued(&self, indices: &[usize]) -> Result<Value> {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let scheduling = self.capacity.snapshot();
                if scheduling
                    .waiting
                    .iter()
                    .map(|entry| &entry.id)
                    .eq(indices.iter().map(|&i| &self.words[i].0))
                {
                    assert!(
                        scheduling
                            .waiting
                            .iter()
                            .enumerate()
                            .all(|(index, entry)| entry.position == index + 1)
                    );
                    let mut snapshot = self.journal.snapshot().await?;
                    snapshot["scheduling"] = serde_json::to_value(scheduling)?;
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(snapshot);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?
    }

    async fn stop_hosts(&mut self) -> Result<()> {
        self.stop.send_replace(true);
        for task in self.tasks.drain(..) {
            tokio::time::timeout(Duration::from_secs(20), task).await??;
        }
        self.service.ports.lock().unwrap().clear();
        assert_eq!(self.memory.snapshot().hosts, 0);
        Ok(())
    }
}

#[tokio::test]
#[ignore = "native offline echo: nested repository card, exclusion, Stop and the next checkpoint"]
async fn native_nested_repository_exclusion_preserves_index_and_next_checkpoint() -> Result<()> {
    use crate::checkpoint_scope::{Exclude, Issue};
    use std::fs;
    let mut fixture = QueueFixture::new().await?;
    let output = PathBuf::from(std::env::var("INTERCAT_M4_CHECKPOINT_OUTPUT")?);
    fs::create_dir_all(&output)?;
    async fn issue(journal: &Journal, track: &str) -> Result<Issue> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let snapshot = journal.snapshot().await?;
                if let Some(issue) = snapshot["checkpointIssues"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|issue| issue["trackId"] == track)
                {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                        serde_json::from_value(issue.clone())?,
                    );
                }
                if snapshot["workers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|worker| {
                        worker["trackId"] == track
                            && matches!(
                                worker["phase"].as_str(),
                                Some("lost" | "failed" | "needs-recovery")
                            )
                    })
                {
                    return Err(format!(
                        "Worker failed before the checkpoint card: {}",
                        snapshot["workers"]
                    )
                    .into());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?
    }
    let result = async {
        fixture.launch(&[0]).await?;
        let track = fixture.words[0].1.clone();
        let workspace = fixture.journal.workspace(track.clone()).await?;
        let empty = workspace.path.join("vendor/empty");
        let staged = workspace.path.join("vendor/z[staged] repo");
        for root in [&empty,&staged] {fs::create_dir_all(root)?;native_git(root,&["init","-q"])?;}
        fs::write(staged.join("dummy.txt"),"DUMMY_NESTED_FILE\n")?;
        native_git(&staged,&["add","."])?;
        native_git(&staged,&["commit","-qm","Dummy nested commit"])?;
        native_git(&workspace.path,&["--literal-pathspecs","add","vendor/z[staged] repo"])?;
        let index = workspace.path.join(native_git(&workspace.path,&["rev-parse","--git-path","index"])?);
        let original_index = fs::read(&index)?;
        fixture.start(0).await?;
        let first = issue(&fixture.journal,&track).await?;
        assert_eq!(first.path,"vendor/empty");
        assert_eq!(fixture.capacity.snapshot().active,0);
        assert!(native_git(&workspace.path,&["for-each-ref","--format=%(refname)", &format!("{}-checkpoints",workspace.reference)])?.is_empty());
        let stale = Exclude{op_id:Uuid::now_v7(),issue_id:first.id,revision:first.revision+1};
        assert!(fixture.service.exclude_checkpoint(stale).await.is_err());
        assert!(fixture.journal.capture_scope(workspace.clone()).await?.context.exclusions.is_empty());
        let command = Exclude{op_id:Uuid::now_v7(),issue_id:first.id,revision:first.revision};
        fixture.journal.checkpoint_scope(crate::checkpoint_scope::Change::Exclude(command.clone())).await?;
        assert_eq!(fixture.journal.operation(command.op_id).await?.unwrap()["state"],"prepared");
        fixture.service.exclude_checkpoint(command.clone()).await?;
        let second = issue(&fixture.journal,&track).await?;
        assert_eq!(second.path,"vendor/z[staged] repo");
        assert_ne!(first.id,second.id);
        fixture.service.exclude_checkpoint(command).await?;
        assert_eq!(issue(&fixture.journal,&track).await?.id,second.id);
        fixture.service.stop(Uuid::now_v7(),&track).await?;
        fixture.service.exclude_checkpoint(Exclude{op_id:Uuid::now_v7(),issue_id:second.id,revision:second.revision}).await?;
        fixture.wait_workers(&[0],true).await?;
        let status = fixture.journal.snapshot().await?;
        assert_eq!(status["workers"][0]["phase"],"held");
        assert!(status["checkpointIssues"].as_array().unwrap().is_empty());
        let previous: crate::checkpoint::Checkpoint = serde_json::from_value(status["workers"][0]["checkpoint"].clone())?;
        fs::write(workspace.path.join(".gitignore"),"vendor/empty/\nvendor/z\\[staged\\] repo/\n")?;
        fs::write(workspace.path.join("after.txt"),"DUMMY_NEXT_CHECKPOINT\n")?;
        let next = crate::checkpoint::save(workspace.clone(),Uuid::now_v7(),Some(previous),false,fixture.journal.clone()).await?;
        let files = intercat_git::Repository::open(&workspace.path)?.saved_files(next.sha.parse()?)?;
        assert!(files.contains_key("after.txt"));
        assert!(!files.keys().any(|path|path.starts_with("vendor/")));
        assert_eq!(fs::read(&index)?,original_index);
        assert_eq!(fs::read_to_string(staged.join("dummy.txt"))?,"DUMMY_NESTED_FILE\n");
        assert!(empty.join(".git/HEAD").is_file());
        assert_eq!(fixture.journal.capture_scope(workspace.clone()).await?.context.exclusions,vec!["vendor/empty","vendor/z[staged] repo"]);
        assert_eq!(native_git(&fixture.root.path().join("source"),&["rev-parse","HEAD"])?,fixture.source_head);
        let db = rusqlite::Connection::open_with_flags(fixture.root.path().join("db"),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        assert_eq!(db.query_row("SELECT count(*) FROM checkpoint_exclusions WHERE track=?1",[track],|row|row.get::<_,i64>(0))?,2);
        fs::write(output.join("verification.json"),serde_json::to_vec_pretty(&json!({"verified":true,"paidTurns":0,"unbornAndStagedNestedRepositories":true,"staleDecisionRefused":true,"decisionReplayKeepsNextCard":true,"preparedDecisionReplay":true,"literalExcludedPath":true,"ignoredExclusionStillSaves":true,"stopPreserved":true,"nextCheckpointExcludesStagedGitlink":true,"originalIndexAndFilesUnchanged":true,"exclusionsSaved":true}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    fixture.stop_hosts().await?;
    result
}

#[tokio::test]
#[ignore = "native offline echo: frozen branch instructions reach initial dispatch after change, rename and removal"]
async fn native_branches_dispatch_the_chosen_instruction_snapshot() -> Result<()> {
    let output = PathBuf::from(std::env::var("INTERCAT_M4_BRANCH_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let mut fixture = QueueFixture::new().await?;
    async fn saved(journal: &Journal, track: &str, previous: Option<Uuid>) -> Result<WorkerState> {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let snapshot = journal.snapshot().await?;
                if let Some(value) = snapshot["workers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|worker| worker["trackId"] == track)
                {
                    let worker: WorkerState = serde_json::from_value(value.clone())?;
                    if matches!(worker.phase.as_str(), "failed" | "lost" | "needs-recovery") {
                        return Err(worker.detail.unwrap_or(worker.phase).into());
                    }
                    if worker.active_turn.is_none()
                        && !worker.checkpoint_pending
                        && worker
                            .checkpoint
                            .as_ref()
                            .is_some_and(|checkpoint| Some(checkpoint.turn_id) != previous)
                    {
                        return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(worker);
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?
    }
    let result = async {
        let source = fixture.root.path().join("source");
        let parent = fixture.words[0].1.clone();
        let workspace = fixture.journal.workspace(parent.clone()).await?;
        std::fs::write(workspace.path.join("AGENTS.md"), "DUMMY_OLD_BRANCH_RULE\n")?;
        std::fs::write(source.join("AGENTS.md"), "DUMMY_OLD_BRANCH_RULE\n")?;
        fixture.launch(&[0]).await?;
        fixture.start(0).await?;
        fixture.wait_workers(&[0], true).await?;
        let checkpoint = saved(&fixture.journal, &parent, None).await?.checkpoint.unwrap();
        eprintln!("branch instructions: parent checkpoint saved");
        let parent_session = fixture.journal.snapshot().await?["workers"][0]["sessionId"].as_str().unwrap().to_owned();
        let secret = "DUMMY_BRANCH_RULE_SECRET_f7b92";
        let key = crate::secrets::Key::new([7; 32]);
        let hex = |bytes: &[u8]| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        let catalog = fixture.journal.register_fingerprint(crate::secrets::Registration {
            op_id: Uuid::now_v7(), track_id: parent.clone(), session_id: parent_session.clone(), name: "RULE_TOKEN".into(),
            byte_len: secret.len(), tag: hex(&key.tag(secret.as_bytes())), key_check: hex(&key.check()),
        }).await?;
        fixture.journal.confirm_fingerprints(catalog.current).await?;
        fixture.service.reload_fingerprints(&parent, &parent_session).await?;
        let mut verified = Vec::new();
        for (case, current_name) in [("changed", Some("AGENTS.md")), ("renamed", Some("CLAUDE.md")), ("removed", None), ("checkpoint", Some("AGENTS.md"))] {
            let as_of = case == "checkpoint";
            for name in ["AGENTS.md", "CLAUDE.md"] {
                match std::fs::remove_file(source.join(name)) {
                    Ok(()) => {}, Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}, Err(error) => return Err(error.into()),
                }
            }
            if let Some(name) = current_name { std::fs::write(source.join(name), format!("DUMMY_CURRENT_BRANCH_{case}\nRULE_TOKEN={secret}\n"))?; }
            let operation = Uuid::now_v7();
            let prepared = host::OFFLINE_ECHO.scope((), crate::branch_run::prepare(
                fixture.memory.clone(), &fixture.journal, fixture.config.clone(), fixture.root.path(), &fixture.root.path().join("rpc"), fixture.stop.subscribe(),
                crate::branch::Request { op_id: operation, parent_track_id: parent.clone(), point: crate::branch_start::Selection::Turn { turn_id: checkpoint.turn_id }, instructions: if as_of { crate::branch::InstructionMode::Checkpoint } else { crate::branch::InstructionMode::Current } },
            )).await?;
            let prepared: crate::branch::Branch = serde_json::from_value(prepared["branch"].clone())?;
            assert!(prepared.instructions.as_ref().is_some_and(|snapshot| snapshot.changed.is_empty() == as_of));
            fixture.journal.confirm_fingerprints(fixture.journal.fingerprint_catalog().await?.current).await?;
            let branch = fixture.service.activate_branch(operation).await?;
            eprintln!("branch instructions: {case} activated");
            // Changing the project after confirmation cannot replace the saved
            // instruction files when activation or dispatch is retried.
            if let Some(name) = current_name { std::fs::write(source.join(name), "DUMMY_LATER_PROJECT_RULE\n")?; }
            let index = fixture.words.len();
            fixture.words.push((String::new(), branch.track_id.clone()));
            fixture.launch(&[index]).await?;
            branch_input(&fixture.journal, &branch.track_id, &format!("DUMMY_CHILD_REQUEST_{case}"), vec![]).await?;
            let completed = saved(&fixture.journal, &branch.track_id, Some(checkpoint.turn_id)).await?;
            eprintln!("branch instructions: {case} checkpoint saved");
            assert!(!completed.checkpoint_pending);
            let repo = intercat_git::Repository::open(&branch.workspace.path)?;
            let saved_files = repo.saved_files(completed.checkpoint.as_ref().unwrap().sha.parse()?)?;
            let instruction_paths = saved_files.keys().filter(|path| crate::instructions::is_instruction(path)).cloned().collect::<Vec<_>>();
            if as_of { assert_eq!(instruction_paths, vec!["AGENTS.md"]); assert!(String::from_utf8(repo.saved_bytes(&saved_files["AGENTS.md"])?)?.contains("DUMMY_OLD_BRANCH_RULE")); }
            else if let Some(name) = current_name { assert_eq!(instruction_paths, vec![name]); assert!(String::from_utf8(repo.saved_bytes(&saved_files[name])?)?.contains(&format!("DUMMY_CURRENT_BRANCH_{case}"))); }
            else { assert!(instruction_paths.is_empty()); }
            let db = rusqlite::Connection::open(fixture.root.path().join("db"))?;
            let raw: String = db.query_row("SELECT payload FROM operations WHERE kind='turn.start' AND json_extract(payload,'$.sessionId')=?1 ORDER BY rowid LIMIT 1", [branch.session_id.as_ref().unwrap()], |row| row.get(0))?;
            let params: Value = serde_json::from_str(&raw)?;
            let argument = params["input"][0]["arguments"].as_str().ok_or("No explicit project instructions.")?;
            assert!(argument.contains("It replaces previous project instructions"));
            assert_eq!(argument.contains("DUMMY_OLD_BRANCH_RULE"), as_of);
            assert!(!argument.contains("DUMMY_LATER_PROJECT_RULE"));
            assert!(!raw.contains(secret));
            if current_name.is_some() && !as_of { assert!(argument.contains("<secret:RULE_TOKEN>")); }
            assert!(argument.contains(&format!("DUMMY_CHILD_REQUEST_{case}")));
            if as_of { assert!(!argument.contains(&format!("DUMMY_CURRENT_BRANCH_{case}"))); }
            else if current_name.is_some() { assert!(argument.contains(&format!("DUMMY_CURRENT_BRANCH_{case}"))); }
            else { assert!(argument.contains("\"rules\": []")); }
            assert_eq!(params["workspaceRoots"][0], branch.workspace.path.to_str().unwrap());
            let recorded: String = db.query_row("SELECT record FROM branches WHERE id=?1", [operation.to_string()], |row| row.get(0))?;
            assert!(!recorded.contains("DUMMY_CURRENT_BRANCH_"));
            verified.push(json!({"case":case,"branch":branch.track_id,"origin":branch.origin,"initialSnapshot":true,"immutableChoice":true,"requestPreserved":true,"workspaceBound":true,"branchRecordContainsOnlyMetadata":true,"initialInstructionsScreened":true,"instructionChangesSaved":true}));
        }
        assert_eq!(native_git(&source, &["rev-parse", "HEAD"])?, fixture.source_head);
        std::fs::write(output.join("verification.json"), serde_json::to_vec_pretty(&json!({"verified":true,"cases":verified,"paidTurns":0,"schema":crate::journal::SCHEMA_VERSION}))?)?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }.await;
    if let Err(error) = &result {
        std::fs::write(output.join("failure.txt"), error.to_string())?;
    }
    fixture.stop_hosts().await?;
    fixture.editor.stop().await;
    result
}

#[tokio::test]
#[ignore = "native offline echo: shared queue, cancellation and persisted order after reopening"]
async fn native_sessions_share_queue_and_reopen_held_requests_in_saved_order() -> Result<()> {
    let output = PathBuf::from(std::env::var("INTERCAT_M3_QUEUE_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let mut fixture = QueueFixture::new().await?;
    let result = async {
        let occupied = fixture.capacity.request("test-running-slot".into(), crate::scheduler::Priority::User).acquire().await;
        fixture.launch(&[4, 3, 2, 1, 0]).await?;
        let original_sessions: HashMap<String, String> = fixture.journal.snapshot().await?["workers"]
            .as_array().unwrap().iter().map(|worker| (
                worker["trackId"].as_str().unwrap().to_owned(), worker["sessionId"].as_str().unwrap().to_owned(),
            )).collect();
        assert_eq!(original_sessions.values().collect::<HashSet<_>>().len(), 5);
        for index in [4, 3, 2, 1, 0] { fixture.start(index).await?; }
        let before = fixture.queued(&[0, 1, 2, 3, 4]).await?;
        fixture.journal.cut_input(Uuid::now_v7(), fixture.words[1].0.clone()).await?;
        let after_cut = fixture.queued(&[0, 2, 3, 4]).await?;
        for index in [2, 3] {
            fixture.service.stop_captured(Uuid::now_v7(), &fixture.words[index].1, None).await?;
        }
        let after_stop = fixture.queued(&[0, 4]).await?;
        drop(occupied);
        fixture.wait_workers(&[0, 4], true).await?;
        assert_eq!(fixture.capacity.snapshot().active, 0);
        assert_eq!(fixture.journal.read_utterance(fixture.words[1].0.clone()).await?["state"], "cut");
        for index in [2, 3] {
            assert_eq!(fixture.journal.read_utterance(fixture.words[index].0.clone()).await?["state"], "held");
        }
        fixture.stop_hosts().await?;
        fixture.journal = Journal::open(fixture.root.path().join("db"), crate::secrets::Key::new([7; 32])).await?;
        fixture.service.journal = fixture.journal.clone();
        let project = fixture.journal.snapshot().await?["projectSecrets"].clone();
        fixture.service.activate_project_secrets(crate::project_secrets::Load {
            project_id: project["projectId"].as_str().unwrap().into(),
            revision: project["revision"].as_u64().unwrap(), entries: vec![],
        }).await?;
        fixture.capacity = Arc::new(crate::scheduler::Capacity::new(1));
        fixture.service.capacity = fixture.capacity.clone();
        fixture.stop = watch::channel(false).0;
        let occupied = fixture.capacity.request("test-reopened-slot".into(), crate::scheduler::Priority::User).acquire().await;
        fixture.launch(&[3, 2]).await?;
        for worker in fixture.journal.snapshot().await?["workers"].as_array().unwrap() {
            if [fixture.words[2].1.as_str(), fixture.words[3].1.as_str()].contains(&worker["trackId"].as_str().unwrap()) {
                assert_eq!(worker["sessionId"], original_sessions[worker["trackId"].as_str().unwrap()]);
            }
        }
        for index in [3, 2] { fixture.start(index).await?; }
        let reopened = fixture.queued(&[2, 3]).await?;
        drop(occupied);
        fixture.wait_workers(&[2, 3], true).await?;
        fixture.stop_hosts().await?;
        let db = rusqlite::Connection::open(fixture.root.path().join("db"))?;
        let mut query = db.prepare("SELECT track_id,payload FROM events WHERE kind='msp.applied' AND json_extract(payload,'$.method') IN ('turn/started','turn/completed') ORDER BY seq")?;
        let events = query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut active = HashSet::new(); let mut started = vec![]; let mut seen = HashSet::new(); let mut maximum = 0;
        for (track, raw) in events {
            let event: Value = serde_json::from_str(&raw)?;
            let key = (event["params"]["sessionId"].as_str().unwrap().to_owned(), event["params"]["turnId"].as_str().unwrap().to_owned());
            if event["method"] == "turn/started" {
                if seen.insert(key.clone()) { started.push(track); active.insert(key); maximum = maximum.max(active.len()); }
            } else { active.remove(&key); }
        }
        assert_eq!(started, [0, 4, 2, 3].map(|i| fixture.words[i].1.clone()));
        assert_eq!(maximum, 1); assert!(active.is_empty());
        let echo: i64 = db.query_row("SELECT count(*) FROM events WHERE kind='msp.received' AND json_extract(payload,'$.result.session.providerId')='echo'", [], |row| row.get(0))?;
        let mut providers = Vec::new();
        for entry in walkdir::WalkDir::new(fixture.root.path().join("hosts")).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_file() && entry.file_name() == "settings.json" {
                let settings: Value = serde_json::from_slice(&std::fs::read(entry.path())?)?;
                providers.push(settings["provider"].clone());
            }
        }
        std::fs::write(output.join("providers.json"), serde_json::to_vec_pretty(&json!({"echoSessionReplies":echo,"hostProviders":providers}))?)?;
        if echo < 5 || providers.len() != 7 || providers.iter().any(|provider| provider != "echo") {
            return Err(format!("Every fresh session and host must use echo: {echo} session replies, {providers:?}").into());
        }
        let invalid: i64 = db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start' AND json_type(payload,'$.ifBusy') IS NOT NULL", [], |row| row.get(0))?;
        assert_eq!(invalid, 0);
        let source = fixture.root.path().join("source");
        assert_eq!(native_git(&source, &["rev-parse", "HEAD"])?, fixture.source_head);
        assert!(native_git(&source, &["status", "--porcelain"])?.is_empty());
        let evidence = json!({"provider":"echo","paidTurns":0,"distinctSessions":original_sessions.len(),"startOrder":started,"maximumRunning":maximum,"sourceUnchanged":true,"before":before,"afterCut":after_cut,"afterStop":after_stop,"reopened":reopened});
        std::fs::write(output.join("result.json"), serde_json::to_vec_pretty(&evidence)?)?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }.await;
    let stopped = fixture.stop_hosts().await;
    fixture.editor.stop().await;
    if let Err(error) = &result {
        let path = fixture.root.keep();
        std::fs::write(
            output.join("failure.json"),
            serde_json::to_vec_pretty(&json!({"error":error.to_string(),"fixture":path}))?,
        )?;
    }
    result?;
    stopped
}

#[tokio::test]
#[ignore = "native offline echo: Start fresh boundary holds dispatch until exact cancellation"]
async fn native_fresh_boundary_blocks_future_dispatch_without_interrupting_completed_work()
-> Result<()> {
    use crate::fresh::{Change, Phase, Request};
    let output = PathBuf::from(std::env::var("INTERCAT_M4_FRESH_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let mut fixture = QueueFixture::new()
        .await
        .map_err(|error| format!("Create own echo fixture: {error}"))?;
    let reports =
        crate::report_analysis::Service::start(fixture.journal.clone(), fixture.editor.clone());
    let result = async {
        fixture.launch(&[0]).await.map_err(|error|format!("Launch own echo worker: {error}"))?;
        fixture.start(0).await.map_err(|error|format!("Start own initial turn: {error}"))?;
        fixture.wait_workers(&[0],true).await.map_err(|error|format!("Save own initial turn: {error}"))?;
        let track = fixture.words[0].1.clone();
        let before = fixture.journal.snapshot().await?;
        let worker = before["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==track).unwrap();
        let session = worker["sessionId"].as_str().unwrap().to_owned();
        let checkpoint = worker["checkpoint"].clone();
        let request = Request {op_id:Uuid::now_v7(),track_id:track.clone(),session_id:session.clone()};
        let pending = fixture.service.prepare_fresh(request.clone()).await?;
        assert_eq!(pending.phase,Phase::WaitingForTurn);
        let captured = tokio::time::timeout(Duration::from_secs(10),async {
            loop {
                match fixture.journal.fresh(Change::Capture {id:request.op_id,revision:pending.revision}).await {
                    Ok(job) => return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(job),
                    Err(error) if error.to_string().contains("admitted turn") => tokio::time::sleep(Duration::from_millis(20)).await,
                    Err(error) => return Err(error),
                }
            }
        }).await.map_err(|error|format!("Capture source boundary: {error}"))??;
        assert_eq!(captured.phase,Phase::Captured);
        assert_eq!(serde_json::to_value(&captured.boundary.as_ref().unwrap().checkpoint)?,checkpoint);
        let word = Uuid::now_v7();
        fixture.journal.stage_utterance(word,"OWN_FRESH_FUTURE".into(),Target::Lock(track.clone()),None,vec![]).await?;
        let input = fixture.journal.claim_input().await?.unwrap();
        fixture.journal.apply_input(input.id,input.revision,Outcome::Classified(Decision {kind:Kind::Extend,track_id:Some(track.clone()),boundary:Some("now".into()),command:None,confidence:1.,inference_ms:1.,timed_out:false,scores:json!({})})).await?;
        let future = fixture.journal.read_utterance(word.to_string()).await?;
        assert_eq!(future["state"],"placed");
        assert_eq!(future["decision"]["trackId"],track);
        assert!(fixture.service.start_turn(Uuid::now_v7(),&track).await.is_err());
        let db = rusqlite::Connection::open(fixture.root.path().join("db"))?;
        // Wait for the real actor to observe the new journal revision. No
        // arbitrary delay is used as evidence that dispatch did not occur.
        tokio::time::timeout(Duration::from_secs(10),async {
            loop {
                let state = fixture.journal.snapshot().await?;
                let current = state["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==track).unwrap();
                if current["phase"]=="held" { return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(state); }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.map_err(|error|format!("Observe source held: {error}"))??;
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start' AND json_extract(payload,'$.sessionId')=?1",[&session],|row|row.get::<_,i64>(0))?,1);
        let observed = fixture.journal.snapshot().await?;
        assert_eq!(observed["freshTracks"][0]["phase"],"captured");
        assert_eq!(fixture.journal.read_utterance(word.to_string()).await?["state"],"placed");
        fixture.service.cancel_fresh(Uuid::now_v7(),request.op_id,captured.revision).await?;
        assert_eq!(fixture.journal.read_utterance(word.to_string()).await?["state"],"held");
        let new_word=Uuid::now_v7();
        fixture.journal.stage_utterance(new_word,"OWN_WARM_NEW_WORD_ONLY".into(),Target::Lock(track.clone()),None,vec![]).await?;
        let input=fixture.journal.claim_input().await?.unwrap();
        assert_eq!(input.id,new_word.to_string());
        fixture.journal.apply_input(input.id,input.revision,Outcome::Classified(Decision{kind:Kind::Extend,track_id:Some(track.clone()),boundary:Some("now".into()),command:None,confidence:1.,inference_ms:0.,timed_out:false,scores:json!({})})).await?;
        // Cancellation binds the live Port to this exact saved local hold. A
        // new word resumes it without Start, leaving old queued work held.
        let resumed=tokio::time::timeout(Duration::from_secs(30),async {
            loop {
                let state=fixture.journal.snapshot().await?;
                let current=state["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==track).unwrap();
                if matches!(current["phase"].as_str(),Some("failed"|"lost"|"needs-recovery")){return Err(format!("Own warm resume failed: {current}").into());}
                if current["checkpointPending"]==false && current["activeTurn"].is_null() && fixture.journal.read_utterance(new_word.to_string()).await?["state"]=="landed"{return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(current.clone());}
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await??;
        assert_eq!(resumed["sessionId"],session);
        assert_eq!(resumed["identity"],worker["identity"]);
        assert_eq!(fixture.journal.read_utterance(word.to_string()).await?["state"],"held");
        fixture.service.start_turn(Uuid::now_v7(),&track).await?;
        tokio::time::timeout(Duration::from_secs(20),async {
            loop {
                let state = fixture.journal.snapshot().await?;
                let current = state["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==track).unwrap();
                if current["checkpointPending"]==false && !current["checkpoint"].is_null() && current["checkpoint"]["sha"]!=resumed["checkpoint"]["sha"] {
                    assert_eq!(current["sessionId"],session);
                    return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(state);
                }
                if matches!(current["phase"].as_str(),Some("failed"|"lost"|"needs-recovery")) { return Err(format!("Own echo worker failed: {current}").into()); }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.map_err(|error|format!("Resume after exact cancellation: {error}"))??;
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start' AND json_extract(payload,'$.sessionId')=?1",[&session],|row|row.get::<_,i64>(0))?,3);
        let inputs=db.prepare("SELECT u.id FROM operations o JOIN utterances u ON u.dispatch_command=o.id WHERE o.kind='turn.start' AND json_extract(o.payload,'$.sessionId')=?1 ORDER BY o.rowid")?.query_map([&session],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(inputs,vec![fixture.words[0].0.clone(),new_word.to_string(),word.to_string()]);
        let echo:i64=db.query_row("SELECT count(*) FROM events WHERE kind='msp.received' AND json_extract(payload,'$.result.session.providerId')='echo'",[],|row|row.get(0))?;
        let mut providers=Vec::new();
        for entry in walkdir::WalkDir::new(fixture.root.path().join("hosts")).follow_links(false) {
            let entry=entry?;
            if entry.file_type().is_file() && entry.file_name()=="settings.json" {
                let settings:Value=serde_json::from_slice(&std::fs::read(entry.path())?)?;
                providers.push(settings["provider"].clone());
            }
        }
        if echo!=1 || providers!=[json!("echo")] { return Err(format!("Own host must use offline echo: {echo} replies, {providers:?}").into()); }
        assert_eq!(native_git(&fixture.root.path().join("source"),&["rev-parse","HEAD"])?,fixture.source_head);
        let proof=json!({"provider":"echo","paidTurns":0,"echoSessionReplies":echo,"hostProviders":providers,"before":before,"captured":observed,"futureStayedPlaced":true,"oldCheckpointPreservedUntilCancellation":true,"turnsBeforeCancellation":1,"turnsAfterCancellation":3,"warmNewWordResumedSameHost":true,"oldWordNeededExplicitStart":true,"inputs":inputs,"sameSessionResumed":true,"sourceUnchanged":true,"schema":crate::journal::SCHEMA_VERSION});
        std::fs::write(output.join("result.json"),serde_json::to_vec_pretty(&proof)?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let final_state = fixture.journal.snapshot().await;
    reports.shutdown().await;
    let stopped = fixture.stop_hosts().await;
    fixture.editor.stop().await;
    if let Ok(state) = final_state {
        std::fs::write(
            output.join("last-state.json"),
            serde_json::to_vec_pretty(&state)?,
        )?;
    }
    result?;
    stopped
}
