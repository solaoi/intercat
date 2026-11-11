//! Native SDK transport for the Restore owner; only offline echo and dummy auth.
use super::*;
const DUMMY_SECRET: &str = "Do not invent a checkpoint SHA";
const SECRET_NAME: &str = "DUMMY_RESTORE_TOKEN";
struct Guard(Arc<Mutex<crate::process::Tracker>>);
impl Drop for Guard {
    fn drop(&mut self) {
        if let Ok(mut tracker) = self.0.lock() {
            let _ = tracker.stop_confirmed();
        }
    }
}
struct Prepared {
    run: crate::repair_run::Run,
    workspace: Arc<crate::run_workspace::Workspace>,
    hosted: crate::repair_host::Hosted,
    memory: Arc<crate::host_memory::Pool>,
    directory: PathBuf,
    guard: Guard,
}
async fn secrets(f: &Fixture) -> Result<crate::project_secrets::Store> {
    let project = f.journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
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
    let store = crate::project_secrets::Store::default();
    store.publish(
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
    Ok(store)
}
async fn prepare(
    f: &Fixture,
    request: &RestoreRequest,
    store: crate::project_secrets::Store,
    text: &str,
) -> Result<Prepared> {
    let error = advance(f, request)
        .await
        .err()
        .ok_or("Expected an own Restore conflict")?;
    assert!(error.is::<crate::path_restore::MergeRequired>(), "{error}");
    let pending = f.journal.restore_path(request.clone()).await?;
    let summary = pending.merge_input.as_ref().unwrap();
    let queued = f
        .journal
        .repair_start(crate::repair_run::Request {
            op_id: Uuid::now_v7(),
            owner: Owner::Restore {
                id: summary.id,
                revision: summary.revision,
            },
            track: summary.track.clone(),
            purpose: Purpose::Merge,
        })
        .await?;
    let mut run = f
        .journal
        .repair_transition(
            Uuid::now_v7(),
            queued.id,
            queued.revision,
            crate::repair_run::Event::Prepare,
        )
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
    fs::write(workspace.checkout().workdir.join("parent.txt"), text)?;
    workspace
        .checkout()
        .git(
            &["add".into(), "--".into(), "parent.txt".into()],
            true,
            tokio::sync::watch::channel(false).1,
        )
        .await?
        .require_success("Stage own dummy Restore resolution")?;
    let binary = crate::host::prepare_binary(
        &PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        &f.root.path().join("muse"),
    )?;
    let auth = f.root.path().join("own-restore-dummy-auth.json");
    fs::write(&auth, r#"{"schema_version":1}"#)?;
    let config = crate::runtime::Config {
        source: binary.clone(),
        binary,
        credentials: auth,
        path: std::env::var("PATH")?,
    };
    let memory = crate::host_memory::Pool::fixed(crate::host_memory::Sample {
        free_bytes: 8 * 1024 * 1024 * 1024,
        pressured: false,
    });
    let directory = f.root.path().join("repair-hosts").join(run.id.to_string());
    let rpc = f.root.path().join("own-sdk-rpc");
    fs::create_dir_all(&rpc)?;
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
                    directory: &directory,
                    rpc: &rpc,
                    project_secrets: store,
                    one_time: None,
                },
            ),
        )
        .await?;
    let guard = Guard(hosted.host_mut().tracker.clone());
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let provider:String=db.query_row("SELECT json_extract(payload,'$.result.session.providerId') FROM events WHERE kind='worker.session' ORDER BY seq DESC LIMIT 1",[],|row|row.get(0))?;
    assert_eq!(
        provider, "echo",
        "Never submit the Restore fixture to a paid provider"
    );
    assert_eq!(run.secret_revision, Some(1));
    Ok(Prepared {
        run,
        workspace,
        hosted,
        memory,
        directory,
        guard,
    })
}
fn masked(db: &rusqlite::Connection) -> Result<()> {
    for raw in db
        .prepare("SELECT payload FROM events UNION ALL SELECT payload FROM operations")?
        .query_map([], |row| row.get::<_, String>(0))?
    {
        assert!(
            !raw?.contains(DUMMY_SECRET),
            "Raw dummy secret must not enter records"
        );
    }
    Ok(())
}
#[tokio::test]
#[ignore = "native Muse echo: both Restore merge stages, secret masks and host release; dummy auth only"]
async fn a_native_restore_merge_releases_each_host_before_applying_the_original_worker()
-> Result<()> {
    let (f, request) = conflicted_restore().await?;
    let store = secrets(&f).await?;
    let before = f.journal.snapshot().await?["workers"].clone();
    let original = fs::read(f.workspaces[0].path.join("parent.txt"))?;
    for text in [
        "Own initial base\n",
        "Own native restored worker and chosen branch\n",
    ] {
        let Prepared {
            run,
            workspace,
            mut hosted,
            memory,
            directory,
            guard: _guard,
        } = prepare(&f, &request, store.clone(), text).await?;
        let identity = hosted.identity();
        let nonce = hosted.nonce();
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
                        instruction: "INTERCAT_OWN_RESTORE_STAGE_ECHO".into(),
                        capacity: capacity.clone(),
                        shutdown: tokio::sync::watch::channel(false).1,
                        project_secrets: store.clone(),
                    },
                ),
            )
            .await?;
        assert_eq!(captured.phase, RunPhase::Captured);
        assert!(
            captured
                .output
                .as_ref()
                .unwrap()
                .contains(&format!("<secret:{SECRET_NAME}>"))
        );
        assert!(!captured.output.as_ref().unwrap().contains(DUMMY_SECRET));
        assert!(!crate::process::is_alive(identity));
        assert_eq!(memory.snapshot().hosts, 0);
        assert_eq!(capacity.snapshot().active, 0);
        assert_eq!(f.journal.snapshot().await?["workers"], before);
        assert_eq!(fs::read(f.workspaces[0].path.join("parent.txt"))?, original);
        let mut tracker = crate::process::Tracker::restore_host(&directory, Some(identity))?;
        assert!(tracker.refresh()?.is_empty());
        assert!(
            f.journal
                .project_uses()
                .await?
                .iter()
                .all(|usage| usage.host.nonce != nonce)
        );
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
        assert_eq!(fs::read(f.workspaces[0].path.join("parent.txt"))?, original);
        masked(&rusqlite::Connection::open(
            f.root.path().join("events.sqlite"),
        )?)?;
    }
    let restored = advance(&f, &request)
        .await?
        .ok_or("Missing final own native Restore")?;
    let done = f
        .journal
        .change_path_restore(crate::path_restore_store::Change::Complete(
            restored.operation,
        ))
        .await?;
    assert_eq!(
        done.restored.as_ref().unwrap().track_id,
        f.workspaces[0].track_id
    );
    assert_eq!(
        fs::read_to_string(f.workspaces[0].path.join("parent.txt"))?,
        "Own native restored worker and chosen branch\n"
    );
    assert_eq!(f.journal.snapshot().await?["workers"], before);
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    Ok(())
}
enum StopAt {
    Capacity,
    Running,
}
async fn stop(at: StopAt) -> Result<()> {
    let (f, request) = conflicted_restore().await?;
    let store = secrets(&f).await?;
    let before = f.journal.snapshot().await?["workers"].clone();
    let original = fs::read(f.workspaces[0].path.join("parent.txt"))?;
    let Prepared {
        run,
        workspace,
        mut hosted,
        memory,
        directory,
        guard: _guard,
    } = prepare(&f, &request, store.clone(), "Own initial base\n").await?;
    let identity = hosted.identity();
    let nonce = hosted.nonce();
    let capacity = Arc::new(crate::scheduler::Capacity::new(1));
    let occupied = match at {
        StopAt::Capacity => Some(
            capacity
                .request("own occupied slot".into(), crate::scheduler::Priority::User)
                .acquire()
                .await,
        ),
        StopAt::Running => None,
    };
    let (port, runner) = crate::repair_execution::channel(&mut hosted, &run)?;
    let writer = f.journal.clone();
    let files = workspace.clone();
    let slots = capacity.clone();
    let mut task = tokio::spawn(crate::host::OFFLINE_ECHO.scope((), async move {
        runner
            .execute(
                hosted,
                writer,
                &files,
                crate::repair_execution::Task {
                    instruction: "INTERCAT_OWN_RESTORE_STOP_ECHO ".repeat(100),
                    capacity: slots,
                    shutdown: tokio::sync::watch::channel(false).1,
                    project_secrets: store,
                },
            )
            .await
    }));
    let observed = tokio::time::timeout(Duration::from_secs(35), async {
        loop {
            let current = f.journal.repair_run(run.id).await?;
            if match at {
                StopAt::Capacity => capacity
                    .snapshot()
                    .waiting
                    .iter()
                    .any(|waiting| waiting.id == run.id.to_string()),
                StopAt::Running => current.phase == RunPhase::Running && current.turn.is_some(),
            } {
                break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
            }
            if task.is_finished() {
                return Err("The own native Restore merge ended before the Stop boundary".into());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let command = Uuid::now_v7();
    port.stop(command)?;
    let held = tokio::time::timeout(Duration::from_secs(35), &mut task).await???;
    observed??;
    assert_eq!(held.phase, RunPhase::Held);
    assert!(held.host.is_none());
    assert!(!crate::process::is_alive(identity));
    assert_eq!(memory.snapshot().hosts, 0);
    drop(occupied);
    assert_eq!(capacity.snapshot().active, 0);
    assert!(capacity.snapshot().waiting.is_empty());
    assert_eq!(f.journal.snapshot().await?["workers"], before);
    assert_eq!(fs::read(f.workspaces[0].path.join("parent.txt"))?, original);
    assert!(workspace.checkout().workdir.exists());
    assert!(
        f.journal
            .project_uses()
            .await?
            .iter()
            .all(|usage| usage.host.nonce != nonce)
    );
    assert!(
        crate::process::Tracker::restore_host(&directory, Some(identity))?
            .refresh()?
            .is_empty()
    );
    let job = f.journal.restore_path(request).await?;
    assert_eq!(job.phase, RestorePhase::NeedsMerge);
    assert_eq!(
        job.merge_input.as_ref().unwrap().revision,
        run.owner.revision()
    );
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
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
    masked(&db)?;
    assert_eq!(git(&f.source, &["rev-parse", "HEAD"])?, f.head);
    assert_eq!(fs::read(f.source.join(".git/index"))?, f.index);
    Ok(())
}
#[tokio::test]
#[ignore = "native Muse echo: Restore merge Stop while capacity is occupied; dummy auth only"]
async fn a_native_restore_merge_stops_before_capacity_is_available() -> Result<()> {
    stop(StopAt::Capacity).await
}
#[tokio::test]
#[ignore = "native Muse echo: Restore merge Stop after confirmed SDK start; dummy auth only"]
async fn a_native_restore_merge_stops_only_its_own_running_turn() -> Result<()> {
    stop(StopAt::Running).await
}
