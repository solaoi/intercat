//! Shared admission on real, isolated Muse hosts using the offline echo provider.
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
        let (root, runner, _, mut service, _, service_stop) =
            reconnect_fixture("QUEUE_DUMMY_0").await?;
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
            let track = &self.words[index].1;
            let previous = self.journal.snapshot().await?["workers"]
                .as_array()
                .unwrap()
                .iter()
                .find(|worker| worker["trackId"] == *track)
                .cloned()
                .map(serde_json::from_value)
                .transpose()?;
            let (port, commands) =
                crate::execution::Port::new(Arc::new(crate::dispatch_control::Gate::new(false)));
            self.service
                .ports
                .lock()
                .unwrap()
                .insert(track.clone(), port.clone());
            let launch = Launch {
                host_memory: self.memory.clone(),
                config: self.config.clone(),
                data: self.root.path().into(),
                rpc: self.root.path().join("rpc"),
                journal: self.journal.clone(),
                workspace: serde_json::to_value(self.journal.workspace(track.clone()).await?)?,
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
            self.tasks
                .push(tokio::spawn(host::OFFLINE_ECHO.scope((), run_host(launch))));
        }
        self.wait_workers(indices, false).await
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
#[ignore = "native offline echo: shared queue, cancellation and persisted order after reopening"]
async fn native_sessions_share_queue_and_reopen_held_requests_in_saved_order() -> Result<()> {
    let mut fixture = QueueFixture::new().await?;
    let output = PathBuf::from(std::env::var("INTERCAT_M3_QUEUE_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
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
