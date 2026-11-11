use super::*;

// A public phrase in the SDK's local report skill is registered as a dummy
// secret. Echo returns that skill body without putting the value in turn input.
const DUMMY_SECRET: &str = "Do not invent a checkpoint SHA";
const SECRET_NAME: &str = "DUMMY_SHARED_TOKEN";

struct Guard(Arc<Mutex<crate::process::Tracker>>);
impl Drop for Guard {
    fn drop(&mut self) {
        if let Ok(mut tracker) = self.0.lock() {
            let _ = tracker.stop_confirmed();
        }
    }
}

struct Prepared {
    f: Fixture,
    job: Job,
    request: Request,
    shared: Checkpoint,
    summary: crate::shared_merge::Summary,
    run: crate::repair_run::Run,
    root: PathBuf,
    workspace: Arc<crate::run_workspace::Workspace>,
    memory: Arc<crate::host_memory::Pool>,
    secrets: crate::project_secrets::Store,
    host_directory: PathBuf,
    echo: i64,
    guard: Guard,
}

async fn dummy_secrets(f: &Fixture) -> Result<crate::project_secrets::Store> {
    let project = f.journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .ok_or("Missing dummy project")?
        .to_owned();
    let key = crate::secrets::Key::new([7; 32]);
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let catalog = f
        .journal
        .prepare_project_secrets(crate::project_secrets::Prepare {
            op_id: Uuid::now_v7(),
            project_id: project.clone(),
            revision: 1,
            fingerprints: vec![crate::project_secrets::Descriptor {
                name: SECRET_NAME.into(),
                byte_len: DUMMY_SECRET.len(),
                tag: hex(&key.tag(DUMMY_SECRET.as_bytes())),
            }],
            key_check: hex(&key.check()),
        })
        .await?;
    f.journal.confirm_fingerprints(catalog.current).await?;
    let secrets = crate::project_secrets::Store::default();
    secrets.publish(
        f.journal
            .activate_project_secrets(crate::project_secrets::Load {
                project_id: project,
                revision: 1,
                entries: vec![crate::project_secrets::Entry {
                    name: SECRET_NAME.into(),
                    value: DUMMY_SECRET.into(),
                }],
            })
            .await?,
    );
    Ok(secrets)
}

async fn prepare() -> Result<(Prepared, crate::repair_host::Hosted)> {
    use crate::repair_run::Event;
    let (f, job, request, shared, _later) = nested(true, 1).await?;
    let secrets = dummy_secrets(&f).await?;
    let pending = queue_shared(&f, &request).await?;
    let summary = pending
        .shared_input
        .as_ref()
        .ok_or("Missing shared input")?
        .clone();
    let input = crate::shared_merge::read(&f.journal, &summary).await?;
    let queued = f
        .journal
        .repair_start(crate::repair_run::Request {
            op_id: input.automatic_operation(),
            owner: Owner::Shared {
                id: summary.id,
                revision: summary.revision,
            },
            track: summary.track.clone(),
            purpose: Purpose::Merge,
        })
        .await?;
    let mut run = f
        .journal
        .repair_transition(Uuid::now_v7(), queued.id, queued.revision, Event::Prepare)
        .await?;
    let root = f
        .root
        .path()
        .join("integration")
        .join(summary.id.to_string())
        .join(format!("repair-{}", run.id));
    let (_, source) = f.journal.repair_source(run.id).await?;
    let workspace = Arc::new(
        crate::run_workspace::Workspace::open_source(
            source,
            &root,
            &run,
            tokio::sync::watch::channel(false).1,
        )
        .await?,
    );
    fs::write(
        workspace.checkout().workdir.join("parent.txt"),
        "Own chosen A plus shared B SDK resolution\n",
    )?;
    workspace
        .checkout()
        .git(
            &["add".into(), "--".into(), "parent.txt".into()],
            true,
            tokio::sync::watch::channel(false).1,
        )
        .await?
        .require_success("Stage own shared echo resolution")?;
    let binary = crate::host::prepare_binary(
        &PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        &f.root.path().join("muse"),
    )?;
    let credentials = f.root.path().join("own-shared-dummy-auth.json");
    fs::write(&credentials, r#"{"schema_version":1}"#)?;
    let config = crate::runtime::Config {
        source: binary.clone(),
        binary,
        credentials,
        path: std::env::var("PATH")?,
    };
    let memory = crate::host_memory::Pool::fixed(crate::host_memory::Sample {
        free_bytes: 8 * 1024 * 1024 * 1024,
        pressured: false,
    });
    let host_directory = f.root.path().join("repair-hosts").join(run.id.to_string());
    let rpc = f.root.path().join("own-shared-rpc");
    fs::create_dir(&rpc)?;
    let mut hosted = crate::host::OFFLINE_ECHO
        .scope(
            (),
            crate::repair_host::open(
                &f.journal,
                &mut run,
                &workspace,
                crate::repair_host::Launch {
                    memory: memory.clone(),
                    worker: &config,
                    directory: &host_directory,
                    rpc: &rpc,
                    project_secrets: secrets.clone(),
                    one_time: None,
                },
            ),
        )
        .await?;
    let identity = hosted.identity();
    let guard = Guard(hosted.host_mut().tracker.clone());
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let echo:i64=db.query_row("SELECT count(*) FROM events WHERE kind='worker.session' AND json_extract(payload,'$.result.session.providerId')='echo'",[],|row|row.get(0))?;
    assert!(
        echo > 0,
        "Never send the dummy shared turn to a paid provider"
    );
    let nonce = hosted.nonce();
    assert_eq!(run.secret_revision, Some(1));
    let environment = std::process::Command::new("/bin/ps")
        .args(["eww", "-p", &identity.pid.to_string(), "-o", "command="])
        .output()?;
    assert!(environment.status.success());
    let expected = format!("{SECRET_NAME}={DUMMY_SECRET}");
    assert!(
        String::from_utf8_lossy(&environment.stdout).contains(&expected),
        "The dummy value must reach only the separately owned native host environment"
    );
    let uses = f.journal.project_uses().await?;
    let usage = uses
        .iter()
        .find(|entry| entry.host.nonce == nonce)
        .ok_or("Missing shared host secret provenance")?;
    assert_eq!(usage.host.repair_run_id, Some(run.id));
    assert_eq!(usage.host.track_id, summary.track);
    assert_eq!(usage.host.revision, 1);
    assert_eq!(usage.host.fingerprints[0].name, SECRET_NAME);
    assert!(!serde_json::to_string(&uses)?.contains(DUMMY_SECRET));
    drop(db);
    Ok((
        Prepared {
            f,
            job,
            request,
            shared,
            summary,
            run,
            root,
            workspace,
            memory,
            secrets,
            host_directory,
            echo,
            guard,
        },
        hosted,
    ))
}

#[tokio::test]
#[ignore = "native Muse echo transport for a shared checkpoint; dummy auth only, no paid provider"]
async fn a_native_shared_merge_releases_its_host_and_requeues_the_exact_shared_source() -> Result<()>
{
    let (prepared, mut hosted) = prepare().await?;
    let Prepared {
        f,
        job,
        request,
        shared,
        summary,
        run,
        root,
        workspace,
        memory,
        secrets,
        host_directory,
        echo,
        guard: _guard,
    } = prepared;
    let identity = hosted.identity();
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let capacity = Arc::new(crate::scheduler::Capacity::new(1));
    let (_, runner) = crate::repair_execution::channel(&mut hosted, &run)?;
    let captured = crate::host::OFFLINE_ECHO
        .scope(
            (),
            runner.execute(
                hosted,
                f.journal.clone(),
                &workspace,
                crate::repair_execution::Task {
                    instruction: "INTERCAT_OWN_SHARED_CHECKPOINT_ECHO".into(),
                    capacity: capacity.clone(),
                    shutdown: tokio::sync::watch::channel(false).1,
                    project_secrets: secrets,
                },
            ),
        )
        .await?;
    assert_eq!(captured.phase, RunPhase::Captured);
    let output = captured
        .output
        .as_deref()
        .ok_or("Missing real echo output")?;
    assert!(output.contains(&format!("<secret:{SECRET_NAME}>")));
    assert!(!output.contains(DUMMY_SECRET));
    let mut query =
        db.prepare("SELECT payload FROM events UNION ALL SELECT payload FROM operations")?;
    for raw in query.query_map([], |row| row.get::<_, String>(0))? {
        assert!(
            !raw?.contains(DUMMY_SECRET),
            "Raw dummy value must not enter event or operation payloads"
        );
    }
    drop(query);

    assert!(!crate::process::is_alive(identity));
    assert_eq!(capacity.snapshot().active, 0);
    assert_eq!(memory.snapshot().hosts, 0);
    let done = crate::repair_application::apply_source(
        &f.journal,
        &f.review,
        &captured,
        workspace,
        f.root.path().join("integration"),
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    assert_eq!(done.phase, RunPhase::Done);
    let state = f.journal.shared_context(summary.id).await?;
    assert_eq!(state.phase, Phase::Pending);
    assert_eq!(state.solutions.len(), 1);
    assert_eq!(state.input.projection.source.sha, shared.sha);
    resume_shared(&f, &request).await?;
    let prepared = f.action(Action::Open { tracks: None }).await?;
    f.action(Action::Prepare {
        id: prepared.id,
        revision: prepared.revision,
    })
    .await?;
    let checked = f.settled(job.id).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    assert_eq!(
        fs::read_to_string(f.workspaces[3].path.join("parent.txt"))?,
        "Own chosen A plus shared B SDK resolution\n"
    );
    assert_eq!(
        fs::read_to_string(f.workspaces[2].path.join("parent.txt"))?,
        "Own shared B edit\n"
    );
    let monitor = crate::repair_process_service::Monitor::start(
        f.journal.clone(),
        f.root.path().join("integration"),
        f.root.path().into(),
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if f.journal.repair_run(run.id).await?.remaining.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await??;
    monitor.shutdown().await;
    assert!(!root.exists());
    assert!(!host_directory.exists());
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    let output = PathBuf::from(std::env::var("INTERCAT_M4_SHARED_OUTPUT")?);
    fs::create_dir_all(&output)?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    fs::write(
        output.join("verification.json"),
        serde_json::to_vec_pretty(
            &json!({"verified":true,"schema":crate::journal::SCHEMA_VERSION,"provider":"echo","paidTurns":0,"modelResolution":false,"stagedDummyResolution":true,"owner":"shared","sourceCheckpoint":1,"hostClosedBeforeApplication":true,"capacityReleased":true,"memoryLeaseReleased":true,"candidateChecked":true,"originalSourceUnchanged":true,"descendantResolutionsEmpty":true,"privateFilesRemoved":true,"echoSessions":echo,"dummyProjectSecretInNativeEnvironment":true,"secretProvenanceBoundToSharedRun":true,"sdkSkillOutputRedacted":true,"rawSecretAbsentFromJournalPayloads":true}),
        )?,
    )?;
    f.review.shutdown().await;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum StopAt {
    Capacity,
    Running,
}

#[tokio::test]
#[ignore = "native shared merge Stop while capacity is occupied; local echo and dummy secret only"]
async fn a_native_shared_merge_stops_while_waiting_for_capacity() -> Result<()> {
    stop_native(StopAt::Capacity).await
}

#[tokio::test]
#[ignore = "native shared merge Stop after a confirmed SDK turn start; local echo and dummy secret only"]
async fn a_native_shared_merge_stops_its_own_running_turn() -> Result<()> {
    stop_native(StopAt::Running).await
}

async fn stop_native(at: StopAt) -> Result<()> {
    let (prepared, mut hosted) = prepare().await?;
    let identity = hosted.identity();
    let nonce = hosted.nonce();
    let original_workers = prepared.f.journal.snapshot().await?["workers"].clone();
    let original_child = fs::read(prepared.f.workspaces[3].path.join("parent.txt"))?;
    let capacity = Arc::new(crate::scheduler::Capacity::new(1));
    let occupied = match at {
        StopAt::Capacity => Some(
            capacity
                .request(
                    "own dummy occupied slot".into(),
                    crate::scheduler::Priority::User,
                )
                .acquire()
                .await,
        ),
        StopAt::Running => None,
    };
    let (port, runner) = crate::repair_execution::channel(&mut hosted, &prepared.run)?;
    let (_shutdown, shutdown) = tokio::sync::watch::channel(false);
    let writer = prepared.f.journal.clone();
    let files = prepared.workspace.clone();
    let slots = capacity.clone();
    let secrets = prepared.secrets.clone();
    let mut running = tokio::spawn(async move {
        crate::host::OFFLINE_ECHO
            .scope(
                (),
                runner.execute(
                    hosted,
                    writer,
                    &files,
                    crate::repair_execution::Task {
                        instruction: "INTERCAT_OWN_SHARED_STOP_ECHO ".repeat(100),
                        capacity: slots,
                        shutdown,
                        project_secrets: secrets,
                    },
                ),
            )
            .await
    });
    let boundary = tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let current = prepared.f.journal.repair_run(prepared.run.id).await?;
            let observed = match at {
                StopAt::Capacity => capacity
                    .snapshot()
                    .waiting
                    .iter()
                    .any(|waiting| waiting.id == prepared.run.id.to_string()),
                StopAt::Running => current.phase == RunPhase::Running && current.turn.is_some(),
            };
            if observed {
                break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(current);
            }
            if running.is_finished() {
                return Err("The native shared Run ended before its Stop boundary".into());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let observed = match boundary {
        Ok(Ok(run)) => run,
        failure => {
            port.stop(Uuid::now_v7())?;
            let _ = tokio::time::timeout(Duration::from_secs(35), &mut running).await;
            return Err(format!("The shared Stop boundary was unavailable: {failure:?}").into());
        }
    };
    assert!(crate::process::is_alive(identity));
    let command = Uuid::now_v7();
    port.stop(command)?;
    let held = tokio::time::timeout(Duration::from_secs(35), &mut running).await???;
    assert_eq!(held.phase, RunPhase::Held);
    assert!(held.host.is_none());
    assert!(!crate::process::is_alive(identity));
    assert_eq!(prepared.memory.snapshot().hosts, 0);
    drop(occupied);
    assert_eq!(capacity.snapshot().active, 0);
    assert!(capacity.snapshot().waiting.is_empty());
    assert!(
        prepared.root.exists(),
        "Stop retains the private edits for Resume or Discard"
    );
    assert_eq!(
        prepared.f.journal.snapshot().await?["workers"],
        original_workers
    );
    assert_eq!(
        fs::read(prepared.f.workspaces[3].path.join("parent.txt"))?,
        original_child
    );
    let state = prepared
        .f
        .journal
        .shared_context(prepared.summary.id)
        .await?;
    assert_eq!(state.phase, Phase::NeedsMerge);
    assert!(state.solutions.is_empty());
    assert!(
        prepared
            .f
            .journal
            .project_uses()
            .await?
            .iter()
            .all(|usage| usage.host.nonce != nonce)
    );
    let operation = prepared
        .f
        .journal
        .operation(command)
        .await?
        .ok_or("Missing Stop command")?;
    assert_eq!(operation["kind"], "repair.control");
    assert_eq!(operation["payload"]["request"]["id"], held.id.to_string());
    assert_eq!(operation["payload"]["request"]["action"], "stop");
    let db = rusqlite::Connection::open(prepared.f.root.path().join("events.sqlite"))?;
    let starts: i64 = db.query_row(
        "SELECT count(*) FROM operations WHERE kind='turn.start'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(
        starts,
        match at {
            StopAt::Capacity => 0,
            StopAt::Running => 1,
        }
    );
    let mut query =
        db.prepare("SELECT payload FROM events UNION ALL SELECT payload FROM operations")?;
    for raw in query.query_map([], |row| row.get::<_, String>(0))? {
        assert!(!raw?.contains(DUMMY_SECRET));
    }
    drop(query);
    assert_eq!(
        git(&prepared.f.source, &["rev-parse", "HEAD"])?,
        prepared.f.head
    );
    assert_eq!(
        fs::read(prepared.f.source.join(".git/index"))?,
        prepared.f.index
    );
    let suffix = match at {
        StopAt::Capacity => "capacity",
        StopAt::Running => "running",
    };
    let output = PathBuf::from(std::env::var("INTERCAT_M4_SHARED_OUTPUT")?).join(suffix);
    fs::create_dir_all(&output)?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    fs::write(
        output.join("verification.json"),
        serde_json::to_vec_pretty(&json!({
            "verified":true,"schema":crate::journal::SCHEMA_VERSION,"owner":"shared",
            "provider":"echo","paidTurns":0,"boundary":suffix,"observedPhase":observed.phase,
            "observedTurn":observed.turn,"starts":starts,"held":true,"hostClosed":true,
            "memoryReleased":true,"capacityReleased":true,"secretUsageClosed":true,
            "privateEditsHeld":true,"originalWorkersUnchanged":true,"sharedResultNotApplied":true,
            "originalHeadAndIndexUnchanged":true,"rawSecretAbsentFromJournalPayloads":true,
        }))?,
    )?;
    prepared.f.review.shutdown().await;
    Ok(())
}
