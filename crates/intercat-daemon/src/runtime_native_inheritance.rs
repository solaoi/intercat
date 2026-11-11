//! Explicit native acceptance of interrupted file-only inheritance recovery.
use super::*;
use crate::{inheritance_apply_store::Application, lineage::Basis, preparation::Workspace};
use intercat_git::Repository;
use std::{fs, time::Duration};

#[tokio::test]
#[ignore = "native offline echo host loss and file recovery; fake authentication, no model turn"]
async fn host_loss_keeps_taken_back_work_until_explicit_file_recovery() -> Result<()> {
    let (root, mut runner, _port, service, _scheduled, stop) =
        reconnect_fixture("Dummy request that must remain taken back").await?;
    let output = PathBuf::from(std::env::var("INTERCAT_INHERITANCE_RECOVERY_OUTPUT")?)
        .join(Uuid::now_v7().to_string());
    fs::create_dir_all(&output)?;
    let path = root.path().join("worker");
    fs::create_dir(&path)?;
    let path = path.canonicalize()?;
    gix::init(&path)?;
    let repo = Repository::open(&path)?;
    let base = repo.empty_starting_point()?;
    let keep = repo.write_blob(b"keep the child's work\n")?;
    let old = repo.write_blob(b"old parent content\n")?;
    let updated = repo.write_blob(b"updated parent content\n")?;
    let added = repo.write_blob(b"new parent file\n")?;
    let before = repo.checkpoint_commit(
        repo.write_tree(&[("keep.txt", keep), ("parent.txt", old)])?,
        base,
        "Dummy child before inheritance",
    )?;
    let after = repo.checkpoint_commit(
        repo.write_tree(&[
            ("keep.txt", keep),
            ("parent.txt", updated),
            ("second.txt", added),
        ])?,
        before,
        "Dummy inherited update",
    )?;
    let parent = repo.checkpoint_commit(
        repo.write_tree(&[("parent.txt", updated), ("second.txt", added)])?,
        base,
        "Dummy parent checkpoint",
    )?;
    repo.sync_objects(&[before, after, parent])?;
    native_git(&path, &["read-tree", &before.to_string()])?;
    fs::write(path.join("keep.txt"), b"keep the child's work\n")?;
    fs::write(path.join("parent.txt"), b"old parent content\n")?;
    let original_index = fs::read(path.join(".git/index"))?;
    let original_head = fs::read(path.join(".git/HEAD"))?;
    let track = runner.state.track_id.clone();
    let journal = runner.journal.clone();
    let workspace = Workspace {
        denied_roots: Vec::new(),
        track_id: track.clone(),
        path: path.clone(),
        reference: "refs/intercat/native-recovery/tracks/A".into(),
        base: base.to_string(),
        state: "ready".into(),
        error: None,
        attachment_roots: vec![],
    };
    repo.compare_and_swap(&workspace.reference, None, before)?;
    repo.sync_ref(&workspace.reference)?;
    journal.workspace_result(workspace.clone()).await?;
    let binary = crate::host::prepare_binary(
        &PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        &root.path().join("muse"),
    )?;
    let credentials = root.path().join("dummy-auth.json");
    fs::write(&credentials, r#"{"schema_version":1}"#)?;
    runner.config = Config {
        source: binary.clone(),
        binary,
        credentials,
        path: std::env::var("PATH")?,
    };
    runner.path = path.clone();
    runner.state.session_id = None;
    runner.state.active_turn = None;
    runner.state.checkpoint_pending = false;
    fs::create_dir(&runner.rpc)?;
    let task = tokio::spawn(crate::host::OFFLINE_ECHO.scope((), async move {
        let result = runner.manage().await;
        let cleanup = runner.stop_owned().await;
        (result, cleanup)
    }));
    let result = async {
        let first = wait_for(&journal, None).await?;
        let session = first["sessionId"]
            .as_str()
            .ok_or("No native session")?
            .to_owned();
        let source = journal.snapshot().await?["utterances"][0].clone();
        let input = source["id"].as_str().ok_or("No dummy input")?.to_owned();
        let db = rusqlite::Connection::open(root.path().join("db"))?;
        let opened: String = db.query_row("SELECT payload FROM operations WHERE kind='session/start' ORDER BY rowid LIMIT 1", [], |row| row.get(0))?;
        assert_eq!(serde_json::from_str::<Value>(&opened)?["providerId"], "echo", "Never submit this recovery fixture to a paid provider");
        db.execute(
            "INSERT INTO tracks VALUES('dummy-parent','B','Dummy parent','done')",
            [],
        )?;
        crate::lineage_store::initialize(&db, "dummy-parent", &Basis::root(base.to_string()))?;
        let checkpoint = Uuid::now_v7();
        journal
            .intent(
                &checkpoint.to_string(),
                "checkpoint.save",
                json!({"trackId":"dummy-parent","checkpoint":{"sha":parent.to_string()}}),
            )
            .await?;
        journal
            .finish(&checkpoint.to_string(), "dispatched")
            .await?;
        journal.finish(&checkpoint.to_string(), "complete").await?;
        let point = crate::lineage_store::point(&db, "dummy-parent", &parent.to_string())?;
        let dependencies = crate::inheritance::Dependencies {
            input: input.clone(),
            track: track.clone(),
            points: vec![point.clone()],
        };
        crate::inheritance::queue(&db, &dependencies)?;
        let context = journal
            .inheritance_application(input.clone())
            .await?
            .ok_or("No inherited input")?;
        let mut basis = context.basis.clone();
        basis
            .inherits
            .insert("dummy-parent".into(), point.inherited());
        let operation = Uuid::new_v5(&input.parse()?, b"apply-inherited-files");
        let application = Application {
            operation,
            dependencies,
            revision: source["revision"].as_i64().ok_or("No input revision")?,
            request_digest: context.request_digest,
            session: session.clone(),
            workspace: workspace.clone(),
            previous: None,
            before: before.to_string(),
            after: after.to_string(),
            before_basis: context.basis,
            basis,
            checkpoint_turn: Uuid::new_v5(&input.parse()?, b"inherited-checkpoint"),
            expected_base: None,
        };
        // Persist the real operation shape, then simulate loss after one of its
        // two writes. No model is asked to produce this deterministic boundary.
        journal
            .intent(
                &operation.to_string(),
                "workspace.apply-inheritance",
                serde_json::to_value(application)?,
            )
            .await?;
        journal.finish(&operation.to_string(), "dispatched").await?;
        fs::write(path.join("parent.txt"), b"updated parent content\n")?;
        journal.cut_input(Uuid::now_v7(), input.clone()).await?;
        journal.pause_dispatch(Uuid::now_v7(), true).await?;
        let pause = Uuid::now_v7();
        service.interrupt_all(pause)?;
        // Kill after the actor has consumed the boundary hold, rather than
        // racing that command with connection loss.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let worker = journal.snapshot().await?["workers"][0].clone();
                let saved = worker["phase"] == "held" && worker["detail"] == "Finishing this turn · next requests held";
                if saved { return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(()); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await??;
        let identity: crate::process::Identity = serde_json::from_value(first["identity"].clone())?;
        if crate::process::identity(identity.pid)? != identity {
            return Err("Native host identity changed".into());
        }
        // SAFETY: the test owns the exact PID and start time just revalidated.
        if unsafe { libc::kill(identity.pid as i32, libc::SIGKILL) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let recovered = wait_for(&journal, Some(&first["identity"])).await?;
        assert_eq!(recovered["sessionId"], session);
        assert!(
            !path.join("second.txt").exists(),
            "Reconnect applied files without an explicit request"
        );
        let waiting = journal.snapshot().await?;
        assert_eq!(
            waiting["pendingInheritance"][0]["operationId"],
            operation.to_string()
        );
        assert_eq!(journal.read_utterance(input.clone()).await?["state"], "cut");
        fs::write(
            output.join("before-finish.json"),
            serde_json::to_vec_pretty(&waiting)?,
        )?;
        let request = Uuid::now_v7();
        service
            .finish_inheritance(request, &track, operation)
            .await?;
        service
            .finish_inheritance(request, &track, operation)
            .await?;
        assert_eq!(
            fs::read(path.join("parent.txt"))?,
            b"updated parent content\n"
        );
        assert_eq!(fs::read(path.join("second.txt"))?, b"new parent file\n");
        assert_eq!(fs::read(path.join("keep.txt"))?, b"keep the child's work\n");
        assert_eq!(fs::read(path.join(".git/index"))?, original_index);
        assert_eq!(fs::read(path.join(".git/HEAD"))?, original_head);
        let finished = journal.snapshot().await?;
        assert!(
            finished["pendingInheritance"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(finished["dispatchPaused"], true);
        assert_eq!(journal.read_utterance(input).await?["state"], "cut");
        let turns: i64 = db.query_row(
            "SELECT COUNT(*) FROM operations WHERE kind='turn.start'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(turns, 0);
        let saved = crate::lineage_store::basis(&db, &track)?;
        assert_eq!(saved.inherits["dummy-parent"], point.inherited());
        fs::write(
            output.join("result.json"),
            serde_json::to_vec_pretty(&json!({
                "sameSession":true,"newHost":true,"modelTurns":turns,"paidTurns":0,"provider":"echo","schema":crate::journal::SCHEMA_VERSION,"requestStayedTakenBack":true,
                "indexUnchanged":true,"headUnchanged":true,"inheritedFilesFinished":true,
                "applicationId":operation,"first":first,"recovered":recovered,"finished":finished
            }))?,
        )?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let _ = stop.send(true);
    let (managed, cleanup) = task.await?;
    if let Err(error) = &result {
        fs::write(output.join("failure.txt"), error.to_string())?;
    }
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    managed?;
    cleanup?;
    result
}

async fn wait_for(journal: &Journal, previous: Option<&Value>) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let state = journal.snapshot().await?["workers"][0].clone();
            if matches!(state["phase"].as_str(), Some("ready" | "held"))
                && !state["identity"].is_null()
                && previous.is_none_or(|previous| previous != &state["identity"])
            {
                return Ok(state);
            }
            if matches!(state["phase"].as_str(), Some("lost" | "failed")) {
                return Err(format!("Native file recovery stopped: {}", state["detail"]).into());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?
}

#[tokio::test]
#[ignore = "one paid dummy Muse turn reads a frozen dependency volume; requires saved authentication"]
async fn worker_reads_the_frozen_dependency_and_shutdown_unmounts_it() -> Result<()> {
    let (root,mut runner,_port,service,_scheduled,stop)=reconnect_fixture(
        "Read reference.txt from the read-only dependency snapshot listed below. Copy its full contents into result.txt in your primary workspace. Do not edit the reference, make commits, run commands or use the network. Report briefly when done."
    ).await?;
    let output =
        PathBuf::from(std::env::var("INTERCAT_REFERENCE_OUTPUT")?).join(Uuid::now_v7().to_string());
    fs::create_dir_all(&output)?;
    let path = root.path().join("worker");
    fs::create_dir(&path)?;
    gix::init(&path)?;
    let path = path.canonicalize()?;
    let repo = Repository::open(&path)?;
    let base = repo.empty_starting_point()?;
    native_git(&path, &["read-tree", &base.to_string()])?;
    let index = fs::read(path.join(".git/index"))?;
    let head = fs::read(path.join(".git/HEAD"))?;
    let source = root.path().join("other-project");
    fs::create_dir(&source)?;
    gix::init(&source)?;
    let source_repo = Repository::open(&source)?;
    let marker = format!("READ_FROM_SAVED_TREE_{}\n", Uuid::now_v7());
    let blob = source_repo.write_blob(marker.as_bytes())?;
    let secret = source_repo.write_blob(b"DUMMY_PRIVATE_EXCLUDED\n")?;
    let sha = source_repo.checkpoint_commit(
        source_repo.write_tree(&[("reference.txt", blob), (".env", secret)])?,
        source_repo.empty_starting_point()?,
        "Dummy frozen reference",
    )?;
    fs::write(source.join("reference.txt"), b"UNSAVED_DIFFERENT_CONTENT\n")?;
    let journal = runner.journal.clone();
    let track = runner.state.track_id.clone();
    let input = journal.snapshot().await?["utterances"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    journal
        .workspace_result(Workspace {
            denied_roots: Vec::new(),
            track_id: track.clone(),
            path: path.clone(),
            reference: "refs/intercat/reference/tracks/A".into(),
            base: base.to_string(),
            state: "ready".into(),
            error: None,
            attachment_roots: vec![],
        })
        .await?;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let reference = crate::reference_volume::Source {
        track: Uuid::now_v7().to_string(),
        checkpoint: sha.to_string(),
        number: 1,
        repository: source.clone(),
    };
    crate::dependency_references::queue(&db, input.parse()?, &track, vec![reference.clone()])?;
    let editor = native_editor(root.path()).await?;
    runner.execution.editor = editor.clone();
    runner.config = Config {
        source: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        binary: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        credentials: PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?),
        path: std::env::var("PATH")?,
    };
    runner.path = path.clone();
    runner.state.session_id = None;
    runner.state.active_turn = None;
    runner.state.checkpoint_pending = false;
    fs::create_dir(&runner.rpc)?;
    let task = tokio::spawn(async move {
        let result = runner.manage().await;
        let cleanup = runner.stop_owned().await;
        (result, cleanup)
    });
    let result=async {
        wait_for(&journal,None).await?;
        let word=journal.read_utterance(input.clone()).await?;
        fs::write(output.join("attempt.json"),serde_json::to_vec_pretty(&json!({"maximumPaidTurns":1,"input":input,"track":track}))?)?;
        service.start_held_input(Uuid::now_v7(),input.clone(),word["revision"].as_i64().unwrap()).await?;
        tokio::time::timeout(Duration::from_secs(150),async {
            loop {
                let snapshot=journal.snapshot().await?;
                fs::write(output.join("latest.json"),serde_json::to_vec_pretty(&snapshot)?)?;
                let worker=&snapshot["workers"][0];
                if worker["phase"]=="held" && worker["detail"].as_str().is_some() {
                    return Err(format!("Dummy reference worker held: {}",worker["detail"]).into());
                }
                if !worker["checkpoint"].is_null() && worker["activeTurn"].is_null() && worker["checkpointPending"]==false {
                    return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(snapshot);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }).await??;
        assert_eq!(fs::read_to_string(path.join("result.txt"))?,marker);
        assert_eq!(fs::read(source.join("reference.txt"))?,b"UNSAVED_DIFFERENT_CONTENT\n");
        assert_eq!(fs::read(path.join(".git/index"))?,index);
        assert_eq!(fs::read(path.join(".git/HEAD"))?,head);
        let mount=root.path().join("dependency-references").join(&track).join(&input).join("mounted");
        assert_eq!(fs::read_to_string(mount.join(reference.name()?).join("reference.txt"))?,marker);
        assert!(!mount.join(reference.name()?).join(".env").exists());
        let turns:i64=db.query_row("SELECT COUNT(*) FROM operations WHERE kind='turn.start'",[],|row|row.get(0))?;
        assert_eq!(turns,1);
        fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"paidTurns":1,"readSavedTree":true,"sourceUnchanged":true,"indexUnchanged":true,"headUnchanged":true,"secretExcluded":true,"snapshot":journal.snapshot().await?}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(mount)
    }.await;
    let _ = stop.send(true);
    let (managed, cleanup) = task.await?;
    editor.stop().await;
    if let Err(error) = &result {
        fs::write(output.join("failure.txt"), error.to_string())?;
    }
    managed?;
    cleanup?;
    let mount = result?;
    assert!(
        fs::read_dir(mount)?.next().is_none(),
        "Shutdown left a reference mounted"
    );
    let mut evidence: Value = serde_json::from_slice(&fs::read(output.join("result.json"))?)?;
    evidence["unmountedAfterShutdown"] = true.into();
    fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&evidence)?,
    )?;
    Ok(())
}

#[tokio::test]
#[ignore = "native offline echo: explicit realignment, Commit lineage and worker boundary; fake authentication only"]
async fn explicit_realign_keeps_unsaved_work_and_publishes_checkpoint_before_base() -> Result<()> {
    use crate::integration::{Action, Command as IntegrationCommand, Job, Status};
    let (root, mut runner, port, service, _scheduled, stop) =
        reconnect_fixture("Dummy A request that must remain waiting").await?;
    let output = PathBuf::from(std::env::var("INTERCAT_REALIGN_OUTPUT")?);
    fs::create_dir_all(&output)?;
    let source = root.path().join("source");
    fs::create_dir(&source)?;
    native_git(&source, &["init", "-q", "-b", "main"])?;
    fs::write(source.join("a.txt"), b"base A\n")?;
    fs::write(source.join("b.txt"), b"base B\n")?;
    native_git(&source, &["add", "."])?;
    native_git(&source, &["commit", "-qm", "Dummy base"])?;
    let source_head = fs::read(source.join(".git/HEAD"))?;
    let source_commit = Repository::worktree_source(&source, None)?.head;
    let source_index = fs::read(source.join(".git/index"))?;
    let journal = runner.journal.clone();
    journal
        .configure_project(
            Uuid::now_v7(),
            crate::project::inspect(
                Uuid::now_v7(),
                "Dummy realignment".into(),
                vec![source.clone()],
            )
            .await?,
        )
        .await?;
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let project_id = journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    let key = crate::secrets::Key::new([7; 32]);
    let catalog = journal
        .prepare_project_secrets(crate::project_secrets::Prepare {
            op_id: Uuid::now_v7(),
            project_id: project_id.clone(),
            revision: 0,
            fingerprints: vec![],
            key_check: key.check().iter().map(|b| format!("{b:02x}")).collect(),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    service
        .activate_project_secrets(crate::project_secrets::Load {
            project_id,
            revision: 0,
            entries: vec![],
        })
        .await?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Dummy B request that must remain waiting".into(),
            crate::decision::Target::New,
            None,
            vec![],
        )
        .await?;
    let input = journal
        .claim_input()
        .await?
        .ok_or("Missing dummy B input")?;
    journal
        .apply_input(
            input.id,
            input.revision,
            crate::decision::Outcome::Classified(crate::decision::Decision {
                kind: crate::decision::Kind::Fork,
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
    let preparation =
        crate::preparation::Preparation::start(&root.path().join("workspaces"), journal.clone())?;
    let workspaces = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let snapshot = journal.snapshot().await?;
            let values: Vec<Workspace> = serde_json::from_value(snapshot["workspaces"].clone())?;
            if let Some(failed) = values.iter().find(|w| w.state == "failed") {
                return Err(format!("Dummy preparation failed: {:?}", failed.error).into());
            }
            if values.len() == 2 && values.iter().all(|w| w.state == "ready") {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(values);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    preparation.shutdown().await;
    let a = workspaces
        .iter()
        .find(|w| w.track_id == runner.state.track_id)
        .unwrap()
        .clone();
    let b = workspaces
        .iter()
        .find(|w| w.track_id != a.track_id)
        .unwrap()
        .clone();
    fs::write(a.path.join("a.txt"), b"A own change\n")?;
    fs::write(b.path.join("b.txt"), b"B own change\n")?;
    let checkpoint_a =
        crate::checkpoint::save(a.clone(), Uuid::now_v7(), None, false, journal.clone()).await?;
    let checkpoint_b =
        crate::checkpoint::save(b.clone(), Uuid::now_v7(), None, false, journal.clone()).await?;
    let integration =
        crate::integration::Service::new(journal.clone(), root.path().join("integration"));
    let mut job = integration
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Open { tracks: None },
        })
        .await?;
    if job.status == Status::Ordering {
        job = integration
            .command(IntegrationCommand {
                op_id: Uuid::now_v7(),
                action: Action::Order {
                    id: job.id,
                    revision: job.revision,
                    tracks: job.order_review.as_ref().unwrap().order.clone(),
                },
            })
            .await?;
    }
    integration
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Prepare {
                id: job.id,
                revision: job.revision,
            },
        })
        .await?;
    async fn settled(journal: &Journal) -> Result<Job> {
        tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                let job: Job =
                    serde_json::from_value(journal.snapshot().await?["integrations"][0].clone())?;
                if !matches!(
                    job.status,
                    Status::Preparing | Status::Building | Status::Staging | Status::Committing
                ) {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?
    }
    let checked = settled(&journal).await?;
    assert_eq!(checked.status, Status::Checked, "{:?}", checked.error);
    let candidate = checked.candidate.as_ref().unwrap();
    integration
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Commit {
                id: checked.id,
                revision: checked.revision,
                head: candidate.head.clone(),
                tree: candidate.tree.clone(),
                settings_hash: candidate.settings_hash.clone(),
            },
        })
        .await?;
    let committed = settled(&journal).await?;
    assert_eq!(committed.status, Status::Done, "{:?}", committed.error);
    let target = committed.candidate.as_ref().unwrap().head.clone();
    let review = integration
        .command(IntegrationCommand {
            op_id: Uuid::now_v7(),
            action: Action::Open { tracks: None },
        })
        .await?;
    fs::write(a.path.join("late.txt"), b"A unpublished edit\n")?;
    let worker_git = gix::open(&a.path)?.git_dir().to_path_buf();
    let binary = crate::host::prepare_binary(
        &PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        &root.path().join("muse"),
    )?;
    let credentials = root.path().join("dummy-auth.json");
    fs::write(&credentials, r#"{"schema_version":1}"#)?;
    runner.config = Config {
        source: binary.clone(),
        binary,
        credentials,
        path: std::env::var("PATH")?,
    };
    let merge_config = runner.config.clone();
    runner.path = a.path.clone();
    runner.state.session_id = None;
    runner.state.active_turn = None;
    runner.state.checkpoint_pending = false;
    runner.state.checkpoint = Some(checkpoint_a.clone());
    fs::create_dir(&runner.rpc)?;
    let task = tokio::spawn(crate::host::OFFLINE_ECHO.scope((), async move {
        let managed = runner.manage().await;
        let cleanup = runner.stop_owned().await;
        (managed, cleanup)
    }));
    let mut external = tokio::process::Command::new("/bin/sleep")
        .arg("120")
        .current_dir(&a.path)
        .kill_on_drop(true)
        .spawn()?;
    let external_identity =
        crate::process::identity(external.id().ok_or("Missing dummy external app")?)?;
    async fn align_dummy(service: &Service, request: &crate::realign::Request) -> Result<()> {
        for _ in 0..4 {
            match service.realign(request.clone()).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    let Some(required) =
                        error.downcast_ref::<crate::workspace_access::ConfirmationRequired>()
                    else {
                        return Err(error);
                    };
                    let consent = crate::workspace_access::Consent {
                        op_id: Uuid::now_v7(),
                        request: request.clone(),
                        fingerprint: required.inspection.fingerprint.clone(),
                    };
                    service.approve_realign_access(consent.clone()).await?;
                    service.approve_realign_access(consent).await?;
                }
            }
        }
        Err("The dummy access evidence did not settle".into())
    }
    let mut driver = Some(crate::realign_driver::Driver::start(
        service.clone(),
        journal.clone(),
        service.stopping.clone(),
    ));
    let mut owned: Option<tokio::process::Child> = None;
    async fn queued(
        journal: &Journal,
        id: Uuid,
        phases: &[crate::realign::Phase],
    ) -> Result<crate::realign::Job> {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let rows: Vec<crate::realign::Job> =
                    serde_json::from_value(journal.snapshot().await?["realignments"].clone())?;
                if let Some(job) = rows.into_iter().find(|job| job.request.op_id == id) {
                    if phases.contains(&job.phase) {
                        return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job);
                    }
                    if matches!(
                        job.phase,
                        crate::realign::Phase::Failed | crate::realign::Phase::Held
                    ) {
                        return Err(
                            format!("Dummy queued realignment failed: {:?}", job.error).into()
                        );
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await?
    }
    let result=Box::pin(async {
        wait_for(&journal,None).await?;
        let request=crate::realign::Request{op_id:Uuid::now_v7(),track_id:a.track_id.clone(),source:crate::realign::Source::Commit{job_id:committed.id,revision:committed.revision}};
        let blocked=service.realign(request.clone()).await.expect_err("The unowned dummy directory must require confirmation");
        let required=blocked.downcast_ref::<crate::workspace_access::ConfirmationRequired>().ok_or("Missing external access confirmation")?;
        assert!(required.inspection.processes.iter().any(|p|p.identity==external_identity));
        assert!(journal.operation(crate::realign::file_operation(&request)).await?.is_none());
        assert!(port.start_guard().is_some(),"Waiting before file application must permit ordinary work");
        assert_eq!(fs::read(a.path.join("b.txt"))?,b"base B\n");
        let host:crate::process::Identity=serde_json::from_value(journal.snapshot().await?["workers"][0]["identity"].clone())?;
        let record=crate::process_ledger::host_paths(root.path())?.into_iter().map(|(_,_,path)|crate::process_ledger::read(&path.join(crate::process_ledger::NAME))).collect::<std::io::Result<Vec<_>>>()?.into_iter().find(|record|record.root==Some(host)).ok_or("Missing dummy host ledger")?;
        owned=Some(tokio::process::Command::new(std::env::current_exe()?).args(["--exact","repair_process_service::tests::owned_scope_sleeper","--ignored","--nocapture"]).env_clear().env("INTERCAT_HOST",&record.nonces[0]).process_group(0).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).current_dir(&a.path).kill_on_drop(true).spawn()?);
        let owned_identity=crate::process::identity(owned.as_ref().unwrap().id().ok_or("Missing dummy owned child")?)?;
        tokio::time::timeout(Duration::from_secs(5),async {
            loop {
                if crate::process::has_host_marker(owned_identity.pid,&record.nonces[0])? {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(());}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await??;
        let saved_request=service.request_realign(request.clone()).await?;
        assert_eq!(service.request_realign(request.clone()).await?.revision,saved_request.revision);
        let needs=queued(&journal,request.op_id,&[crate::realign::Phase::NeedsProcesses]).await?;
        service.control_realign(crate::realign::Control{op_id:Uuid::now_v7(),id:request.op_id,revision:needs.revision,action:crate::realign::ControlAction::KeepProcesses {}}).await?;
        queued(&journal,request.op_id,&[crate::realign::Phase::WaitingProcesses]).await?;
        assert!(port.start_guard().is_some());assert!(crate::process::is_alive(owned_identity));
        assert_eq!(fs::read(a.path.join("b.txt"))?,b"base B\n");
        let waiting=queued(&journal,request.op_id,&[crate::realign::Phase::WaitingProcesses]).await?;
        service.control_realign(crate::realign::Control{op_id:Uuid::now_v7(),id:request.op_id,revision:waiting.revision,action:crate::realign::ControlAction::StopProcesses {}}).await?;
        let needs=queued(&journal,request.op_id,&[crate::realign::Phase::NeedsAccess]).await?;
        assert!(!crate::process::is_alive(owned_identity));assert!(crate::process::is_alive(external_identity));
        let wait=crate::realign::Control{op_id:Uuid::now_v7(),id:request.op_id,revision:needs.revision,action:crate::realign::ControlAction::Wait {}};
        service.control_realign(wait.clone()).await?;service.control_realign(wait).await?;
        queued(&journal,request.op_id,&[crate::realign::Phase::WaitingAccess]).await?;
        driver.take().unwrap().shutdown().await;
        driver=Some(crate::realign_driver::Driver::start(service.clone(),journal.clone(),service.stopping.clone()));
        assert_eq!(fs::read(a.path.join("b.txt"))?,b"base B\n");
        for _ in 0..4 {
            let needs=queued(&journal,request.op_id,&[crate::realign::Phase::WaitingAccess,crate::realign::Phase::NeedsAccess,crate::realign::Phase::Done]).await?;
            if needs.phase==crate::realign::Phase::Done {break;}
            let go=crate::realign::Control{op_id:Uuid::now_v7(),id:request.op_id,revision:needs.revision,action:crate::realign::ControlAction::GoAhead{fingerprint:needs.inspection.as_ref().unwrap().fingerprint.clone()}};
            service.control_realign(go.clone()).await?;service.control_realign(go).await?;
        }
        queued(&journal,request.op_id,&[crate::realign::Phase::Done]).await?;
        service.realign(request.clone()).await?;
        assert!(crate::process::is_alive(external_identity),"An unowned app must never be stopped by realignment");
        assert_eq!(fs::read(a.path.join("a.txt"))?,b"A own change\n");
        assert_eq!(fs::read(a.path.join("b.txt"))?,b"B own change\n");
        assert_eq!(fs::read(a.path.join("late.txt"))?,b"A unpublished edit\n");
        let worker_repo=Repository::open(&a.path)?;
        assert_eq!(worker_repo.worker_base()?,target.parse::<intercat_git::ObjectId>()?);
        assert_eq!(worker_repo.tree_from_index(&worker_git.join("index"))?,worker_repo.tree_id(target.parse()?)?);
        let db=rusqlite::Connection::open(root.path().join("db"))?;
        let snapshot=journal.snapshot().await?;
        assert!(snapshot["pendingRealignments"].as_array().unwrap().is_empty());
        let worker=&snapshot["workers"][0];
        let saved:crate::checkpoint::Checkpoint=serde_json::from_value(worker["checkpoint"].clone())?;
        let point=crate::lineage_store::point(&db,&a.track_id,&saved.sha)?;
        assert_eq!(point.number,2);
        assert_eq!(point.base_at,target);
        assert_eq!(point.base_inherits_at[&a.track_id].sha,checkpoint_a.sha);
        assert_eq!(point.base_inherits_at[&b.track_id].sha,checkpoint_b.sha);
        let repo=Repository::open(&a.path)?;
        assert_eq!(repo.commit_details(saved.sha.parse()?)?.parents,vec![target.parse::<intercat_git::ObjectId>()?]);
        let prefix=a.reference.rsplit_once("/tracks/").unwrap().0;
        assert_eq!(repo.reference_target(&format!("{prefix}/base/A"))?,Some(target.parse()?));
        assert_eq!(repo.reference_target(&format!("{prefix}/integrated/A"))?,Some(checkpoint_a.sha.parse()?));
        let stale=crate::integration_store::read(&db,review.id)?;
        assert!(stale.revision>review.revision);
        assert_eq!(stale.error.as_deref(),Some("A was realigned · Refresh"));
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'",[],|row|row.get::<_,i64>(0))?,0);
        // Publish B again while A retains its uncommitted work. Interrupt a
        // second realignment after its first file write and reconnect the
        // actual host. Only an explicit replay may finish the frozen update.
        integration.command(IntegrationCommand{op_id:Uuid::now_v7(),action:Action::Discard{id:stale.id,revision:stale.revision}}).await?;
        fs::write(b.path.join("b.txt"),b"B second change\n")?;
        fs::write(b.path.join("new-b.txt"),b"B added file\n")?;
        let checkpoint_b_second=crate::checkpoint::save(b.clone(),Uuid::now_v7(),Some(checkpoint_b.clone()),false,journal.clone()).await?;
        let mut second=integration.command(IntegrationCommand{op_id:Uuid::now_v7(),action:Action::Open{tracks:Some(vec![b.track_id.clone()])}}).await?;
        if second.status==Status::Ordering {
            second=integration.command(IntegrationCommand{op_id:Uuid::now_v7(),action:Action::Order{id:second.id,revision:second.revision,tracks:second.order_review.as_ref().unwrap().order.clone()}}).await?;
        }
        integration.command(IntegrationCommand{op_id:Uuid::now_v7(),action:Action::Prepare{id:second.id,revision:second.revision}}).await?;
        let second=settled(&journal).await?;
        assert_eq!(second.status,Status::Checked,"{:?}",second.error);
        let checked=second.candidate.as_ref().unwrap();
        integration.command(IntegrationCommand{op_id:Uuid::now_v7(),action:Action::Commit{id:second.id,revision:second.revision,head:checked.head.clone(),tree:checked.tree.clone(),settings_hash:checked.settings_hash.clone()}}).await?;
        let second=settled(&journal).await?;
        assert_eq!(second.status,Status::Done,"{:?}",second.error);
        let recovery=crate::realign::Request{op_id:Uuid::now_v7(),track_id:a.track_id.clone(),source:crate::realign::Source::Commit{job_id:second.id,revision:second.revision}};
        let context=journal.realign_context(recovery.clone()).await?;
        let before=crate::checkpoint::capture(&a,&saved.sha,"Before dummy interrupted realignment",&journal).await?;
        let plan=crate::realign::Plan::prepare(&repo, &context.graph, crate::realign::Input{track:&a.track_id,basis:&context.basis,before:before.sha.parse()?,target:context.target.parse()?,coverage:context.origin.coverage().ok_or("Missing committed realignment coverage")?,resolutions:&[]})?;
        let crate::realign::Resolution::Ready{tree}=&plan.resolution else {return Err("Dummy second realignment unexpectedly conflicted".into());};
        let after=repo.checkpoint_commit(tree.parse()?,context.target.parse()?,"Dummy saved realignment")?;
        // Persist the application before its protecting refs, as if the
        // daemon stopped immediately after recording the immutable intent.
        repo.sync_objects(&[before.sha.parse()?,after,context.target.parse()?])?;
        let application=crate::realign_store::Application{operation:crate::realign::file_operation(&recovery),request:recovery.clone(),workspace:a.clone(),session:context.session.clone(),previous:context.previous,
            target_ref:context.target_ref,plan,after:after.to_string(),expected_base:Some(target.clone()),expected_head:repo.worker_base()?.to_string(),checkpoint_turn:Uuid::new_v5(&crate::realign::file_operation(&recovery),b"realigned-checkpoint")};
        journal.change_realign(crate::realign_store::Change::Begin(Box::new(application))).await?;
        journal.finish(&crate::realign::file_operation(&recovery).to_string(),"dispatched").await?;
        fs::write(a.path.join("b.txt"),b"B second change\n")?;
        journal.pause_dispatch(Uuid::now_v7(),true).await?;
        service.interrupt_all(Uuid::now_v7())?;
        tokio::time::timeout(Duration::from_secs(5),async {
            loop {
                let worker=journal.snapshot().await?["workers"][0].clone();
                if worker["phase"]=="held" && worker["detail"]=="Finishing this turn · next requests held" {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(());}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await??;
        let first=journal.snapshot().await?["workers"][0].clone();
        let identity:crate::process::Identity=serde_json::from_value(first["identity"].clone())?;
        assert_eq!(crate::process::identity(identity.pid)?,identity);
        // SAFETY: this dummy SDK host's PID/start time was just revalidated.
        if unsafe {libc::kill(identity.pid as i32,libc::SIGKILL)}!=0 {return Err(std::io::Error::last_os_error().into());}
        let reconnected=wait_for(&journal,Some(&first["identity"])).await?;
        assert_eq!(reconnected["sessionId"],context.session);
        assert!(!a.path.join("new-b.txt").exists(),"Reconnect applied realignment without explicit replay");
        assert!(!journal.snapshot().await?["pendingRealignments"].as_array().unwrap().is_empty());
        align_dummy(&service,&recovery).await?;
        service.realign(recovery).await?;
        assert!(crate::process::is_alive(external_identity));
        assert_eq!(fs::read(a.path.join("new-b.txt"))?,b"B added file\n");
        assert_eq!(fs::read(a.path.join("b.txt"))?,b"B second change\n");
        assert_eq!(fs::read(a.path.join("late.txt"))?,b"A unpublished edit\n");
        let final_target=second.candidate.as_ref().unwrap().head.parse()?;
        assert_eq!(repo.worker_base()?,final_target);
        assert_eq!(repo.tree_from_index(&worker_git.join("index"))?,repo.tree_id(final_target)?);
        let final_state=journal.snapshot().await?;
        assert!(final_state["pendingRealignments"].as_array().unwrap().is_empty());
        assert_eq!(final_state["dispatchPaused"],true);
        let final_point=crate::lineage_store::point(&db,&a.track_id,final_state["workers"][0]["checkpoint"]["sha"].as_str().unwrap())?;
        assert_eq!(final_point.number,3);
        assert_eq!(final_point.base_at,second.candidate.as_ref().unwrap().head);
        assert_eq!(repo.reference_target(&format!("{prefix}/integrated/A"))?,Some(checkpoint_a.sha.parse()?));
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'",[],|row|row.get::<_,i64>(0))?,0);
        // Freeze a genuine overlapping realignment without moving any worker
        // files. Restarting its queue must not launch a fresh model turn.
        fs::write(b.path.join("b.txt"),b"B third committed change\n")?;
        crate::checkpoint::save(b.clone(),Uuid::now_v7(),Some(checkpoint_b_second),false,journal.clone()).await?;
        let mut third=integration.command(IntegrationCommand{op_id:Uuid::now_v7(),action:Action::Open{tracks:Some(vec![b.track_id.clone()])}}).await?;
        if third.status==Status::Ordering {
            third=integration.command(IntegrationCommand{op_id:Uuid::now_v7(),action:Action::Order{id:third.id,revision:third.revision,tracks:third.order_review.as_ref().unwrap().order.clone()}}).await?;
        }
        integration.command(IntegrationCommand{op_id:Uuid::now_v7(),action:Action::Prepare{id:third.id,revision:third.revision}}).await?;
        let third=settled(&journal).await?;
        assert_eq!(third.status,Status::Checked,"{:?}",third.error);
        let checked=third.candidate.as_ref().unwrap();
        integration.command(IntegrationCommand{op_id:Uuid::now_v7(),action:Action::Commit{id:third.id,revision:third.revision,head:checked.head.clone(),tree:checked.tree.clone(),settings_hash:checked.settings_hash.clone()}}).await?;
        let third=settled(&journal).await?;
        assert_eq!(third.status,Status::Done,"{:?}",third.error);
        fs::write(a.path.join("b.txt"),b"A unsaved overlapping change\n")?;
        let conflict_head=fs::read(worker_git.join("HEAD"))?;
        let conflict_index=fs::read(worker_git.join("index"))?;
        let conflict=crate::realign::Request{op_id:Uuid::now_v7(),track_id:a.track_id.clone(),source:crate::realign::Source::Commit{job_id:third.id,revision:third.revision}};
        service.request_realign(conflict.clone()).await?;
        // The dummy external process still owns its cwd; approve only that
        // exact inspection before the conflict-planning boundary.
        let waiting=queued(&journal,conflict.op_id,&[crate::realign::Phase::NeedsAccess]).await?;
        service.control_realign(crate::realign::Control{op_id:Uuid::now_v7(),id:conflict.op_id,revision:waiting.revision,action:crate::realign::ControlAction::GoAhead{fingerprint:waiting.inspection.as_ref().unwrap().fingerprint.clone()}}).await?;
        let merging=queued(&journal,conflict.op_id,&[crate::realign::Phase::NeedsMerge]).await?;
        let input_id=merging.merge_input.ok_or("Missing frozen realignment conflict")?;
        let input=journal.operation(input_id).await?.ok_or("Missing merge input intent")?;
        assert_eq!(input["kind"],"realign.merge.inputs");assert_eq!(input["state"],"complete");
        let frozen:crate::realign_merge::Frozen=serde_json::from_value(input["payload"].clone())?;
        frozen.pin()?;
        assert_eq!(frozen.id,input_id);
        assert_eq!(frozen.request,conflict);
        assert!(matches!(frozen.plan.resolution,crate::realign::Resolution::Conflict{..}));
        assert_eq!(fs::read(a.path.join("b.txt"))?,b"A unsaved overlapping change\n");
        assert_eq!(fs::read(worker_git.join("HEAD"))?,conflict_head);
        assert_eq!(fs::read(worker_git.join("index"))?,conflict_index);
        assert!(crate::realign_queue::holds_work(&db,&a.track_id)?);
        assert!(journal.operation(crate::realign::file_operation(&conflict)).await?.is_none());
        if let Some(driver)=driver.take(){driver.shutdown().await;}
        driver=Some(crate::realign_driver::Driver::start(service.clone(),journal.clone(),service.stopping.clone()));
        assert_eq!(queued(&journal,conflict.op_id,&[crate::realign::Phase::NeedsMerge]).await?.revision,merging.revision);
        // Exercise the same realignment owner through a separate SDK host,
        // stopping before any model turn. It must not borrow A's work session.
        let auto=crate::repair_run::Request{op_id:frozen.automatic_operation(),owner:crate::repair_run::Owner::Realign{id:conflict.op_id,revision:merging.revision},track:a.track_id.clone(),purpose:crate::repair_run::Purpose::Merge};
        let mut automatic=journal.repair_start(auto).await?;
        automatic=journal.repair_transition(Uuid::now_v7(),automatic.id,automatic.revision,crate::repair_run::Event::Prepare).await?;
        let (_,source)=journal.repair_source(automatic.id).await?;
        let workspace=crate::run_workspace::Workspace::open_source(source,&root.path().join("integration").join(conflict.op_id.to_string()).join(format!("repair-{}",automatic.id)),&automatic,watch::channel(false).1).await?;
        let host_directory=root.path().join("repair-hosts").join(automatic.id.to_string());
        let closed=crate::host::OFFLINE_ECHO.scope((),async {
            let hosted=crate::repair_host::open(&journal,&mut automatic,&workspace,crate::repair_host::Launch{memory:service.host_memory.clone(),worker:&merge_config,directory:&host_directory,rpc:&service.rpc,project_secrets:service.project_secrets.clone(),one_time:None}).await?;
            assert_ne!(automatic.session.as_deref(),Some(frozen.session.as_str()));
            hosted.close(&journal,crate::repair_run::Outcome::Stopped).await
        }).await?;
        assert_eq!(closed.phase,crate::repair_run::Phase::Held);
        assert!(closed.host.is_none());
        journal.repair_control(crate::repair_run::Command{op_id:Uuid::now_v7(),id:closed.id,revision:closed.revision,action:crate::repair_run::Control::Discard}).await?;
        assert_eq!(journal.snapshot().await?["workers"][0]["sessionId"],frozen.session);
        let repair_runtime=crate::repair_service::Runtime::start(service.host_memory.clone(),journal.clone(),root.path().join("integration"),root.path().into(),service.rpc.clone(),service.capacity.clone(),integration.clone(),service.project_secrets.clone(),Arc::new(crate::dispatch_control::Gate::new(false)));
        let request=crate::repair_run::Request{op_id:Uuid::now_v7(),owner:crate::repair_run::Owner::Realign{id:conflict.op_id,revision:merging.revision},track:a.track_id.clone(),purpose:crate::repair_run::Purpose::Merge};
        let mut stale=request.clone();stale.op_id=Uuid::now_v7();stale.owner=crate::repair_run::Owner::Realign{id:conflict.op_id,revision:merging.revision+1};
        assert!(repair_runtime.service.start_manual(stale.clone()).await.is_err());
        assert!(journal.operation(stale.op_id).await?.is_none());
        let merge=repair_runtime.service.start_manual(request.clone()).await?;
        assert_eq!(merge.owner,request.owner);
        assert_eq!(repair_runtime.service.start_manual(request).await?.revision,merge.revision);
        let view=repair_runtime.service.read_manual(crate::merge_manual::Read{id:merge.id,revision:merge.revision,path:None}).await?;
        assert_eq!(view.paths,vec!["b.txt"]);
        let apply=crate::merge_manual::Apply{op_id:Uuid::now_v7(),id:merge.id,revision:merge.revision,files:vec![crate::merge_manual::Edit::Side{path:"b.txt".into(),version:view.file.version,side:crate::merge_manual::Side::Incoming}]};
        db.execute_batch("CREATE TRIGGER dummy_merge_result_write_failure BEFORE UPDATE ON operations WHEN NEW.kind='realign.merge.result' AND NEW.state='complete' BEGIN SELECT RAISE(ABORT,'Dummy result storage unavailable'); END;")?;
        repair_runtime.service.apply_manual(apply.clone()).await?;
        tokio::time::timeout(Duration::from_secs(8),async {loop {
            let run=journal.repair_run(merge.id).await?;
            if run.phase==crate::repair_run::Phase::Applying {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(());}
            tokio::time::sleep(Duration::from_millis(20)).await;
        }}).await??;
        assert_eq!(queued(&journal,conflict.op_id,&[crate::realign::Phase::NeedsMerge]).await?.resolutions.len(),0);
        assert_eq!(fs::read(worker_git.join("HEAD"))?,conflict_head);
        assert_eq!(fs::read(worker_git.join("index"))?,conflict_index);
        repair_runtime.shutdown().await;
        fs::write(a.path.join("b.txt"),b"A newer edit during manual resolution\n")?;
        db.execute_batch("DROP TRIGGER dummy_merge_result_write_failure;")?;
        let repair_runtime=crate::repair_service::Runtime::start(service.host_memory.clone(),journal.clone(),root.path().join("integration"),root.path().into(),service.rpc.clone(),service.capacity.clone(),integration.clone(),service.project_secrets.clone(),Arc::new(crate::dispatch_control::Gate::new(false)));
        let changed=queued(&journal,conflict.op_id,&[crate::realign::Phase::Failed]).await?;
        assert!(changed.error.as_ref().is_some_and(|error|error.contains("Working files changed")));
        assert_eq!(fs::read(a.path.join("b.txt"))?,b"A newer edit during manual resolution\n");
        assert_eq!(fs::read(worker_git.join("HEAD"))?,conflict_head);
        assert_eq!(fs::read(worker_git.join("index"))?,conflict_index);
        assert!(journal.operation(crate::realign::file_operation(&conflict)).await?.is_none());
        let retry=crate::realign::Control{op_id:Uuid::now_v7(),id:conflict.op_id,revision:changed.revision,action:crate::realign::ControlAction::Retry{}};
        service.control_realign(retry.clone()).await?;service.control_realign(retry).await?;
        let again=queued(&journal,conflict.op_id,&[crate::realign::Phase::NeedsMerge]).await?;
        assert!(again.resolutions.is_empty());
        let final_input=again.merge_input.ok_or("Missing fresh realignment inputs")?;
        assert_ne!(final_input,input_id);
        let merge2=repair_runtime.service.start_manual(crate::repair_run::Request{op_id:Uuid::now_v7(),owner:crate::repair_run::Owner::Realign{id:conflict.op_id,revision:again.revision},track:a.track_id.clone(),purpose:crate::repair_run::Purpose::Merge}).await?;
        let view=repair_runtime.service.read_manual(crate::merge_manual::Read{id:merge2.id,revision:merge2.revision,path:None}).await?;
        let apply2=crate::merge_manual::Apply{op_id:Uuid::now_v7(),id:merge2.id,revision:merge2.revision,files:vec![crate::merge_manual::Edit::Side{path:"b.txt".into(),version:view.file.version,side:crate::merge_manual::Side::Incoming}]};
        repair_runtime.service.apply_manual(apply2.clone()).await?;
        let aligned=queued(&journal,conflict.op_id,&[crate::realign::Phase::Done]).await?;
        let finished=journal.repair_run(merge2.id).await?;
        assert_eq!(finished.phase,crate::repair_run::Phase::Done);
        assert_eq!(aligned.resolutions.len(),1);
        assert_eq!(fs::read(a.path.join("b.txt"))?,b"A newer edit during manual resolution\n");
        assert_eq!(fs::read(a.path.join("late.txt"))?,b"A unpublished edit\n");
        assert_eq!(repo.worker_base()?,third.candidate.as_ref().unwrap().head.parse::<intercat_git::ObjectId>()?);
        assert_eq!(repo.reference_target(&format!("{prefix}/integrated/A"))?,Some(checkpoint_a.sha.parse()?));
        repair_runtime.service.apply_manual(apply).await?;
        repair_runtime.service.apply_manual(apply2).await?;
        repair_runtime.shutdown().await;
        fs::write(output.join("frozen-conflict.json"),serde_json::to_vec_pretty(&json!({"id":final_input,"initialInput":input_id,"request":conflict,"phase":aligned.phase,"workerFilesKept":true,"headAndIndexKeptBeforeResolution":true,"normalWorkHeld":true,"queueRestartKeptInputs":true,"manualRun":merge.id,"manualOwnerBound":true,"separateMergeSdkHost":true,"stoppedBeforeModelTurn":true,"mainSdkSessionKept":true,"staleOwnerRejected":true,"manualStartReplay":true,"manualApplyReplay":true,"resolutionReplayed":true,"resultWriteFailureKeptBoundary":true,"resultRecoveredAfterCoordinatorRestart":true,"newerWorkerEditDetected":true,"explicitRetryTakesFreshCopy":true,"lateEditKept":true,"paidTurns":0,"modelTurns":0}))?)?;
        fs::write(output.join("verification.json"),serde_json::to_vec_pretty(&json!({"verified":false,"provider":"echo","paidTurns":0,"schema":crate::journal::SCHEMA_VERSION,"checkpointNumber":point.number,"checkpointParent":target,"baseMovedAfterCheckpoint":true,"integratedUnchanged":true,"lateEditKept":true,"otherTrackIncluded":true,"reviewInvalidated":true,"sourceIndexUnchanged":true,"sourceHeadUnchanged":true,"workerHeadAndIndexAdvanced":true,"idempotent":true,"partialRecoveryExplicit":true,"applicationPinsRecovered":true,"sameSessionAfterHostLoss":true,"recoveredCheckpointNumber":final_point.number,"modelTurns":0,"externalAccessConfirmed":true,"externalProcessKept":true,"unreservedWaitAllowsWork":true,"queueSurvivedDriverRestart":true,"ownedKeepAllowsWork":true,"ownedStopScoped":true}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }).await;
    if let Some(driver) = driver.take() {
        driver.shutdown().await;
    }
    if let Some(mut owned) = owned.take() {
        if owned.try_wait()?.is_none() {
            owned.kill().await?;
        }
        let _ = owned.wait().await;
    }
    external.kill().await?;
    let _ = external.wait().await;
    let _ = stop.send(true);
    let (managed, cleanup) = task.await?;
    integration.shutdown().await;
    if let Err(error) = &result {
        fs::write(output.join("failure.txt"), error.to_string())?;
        let state = journal.snapshot().await?;
        fs::write(
            output.join("failure-state.json"),
            serde_json::to_vec_pretty(
                &json!({"realignments":state["realignments"],"pendingRealignments":state["pendingRealignments"],"workers":state["workers"],"sequence":state["sequence"]}),
            )?,
        )?;
    }
    managed?;
    cleanup?;
    result?;
    assert_eq!(fs::read(source.join(".git/HEAD"))?, source_head);
    assert_eq!(fs::read(source.join(".git/index"))?, source_index);
    assert_eq!(
        Repository::worktree_source(&source, None)?.head,
        source_commit
    );
    let durable: crate::realign::Job =
        serde_json::from_value(journal.snapshot().await?["realignments"][0].clone())?;
    assert_eq!(durable.phase, crate::realign::Phase::Done);
    drop(integration);
    drop(service);
    drop(journal);
    let reopened = Journal::open(root.path().join("db"), crate::secrets::Key::new([7; 32])).await?;
    let saved: crate::realign::Job =
        serde_json::from_value(reopened.snapshot().await?["realignments"][0].clone())?;
    assert_eq!(saved.request, durable.request);
    assert_eq!(saved.phase, crate::realign::Phase::Done);
    assert_eq!(
        reopened
            .operation(crate::realign::file_operation(&saved.request))
            .await?
            .unwrap()["state"],
        "complete"
    );
    let conflict_proof: Value =
        serde_json::from_slice(&fs::read(output.join("frozen-conflict.json"))?)?;
    let conflict_id = conflict_proof["request"]["opId"].as_str().unwrap();
    let state = reopened.snapshot().await?;
    let conflict = state["realignments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|job| job["request"]["opId"] == conflict_id)
        .unwrap();
    assert_eq!(conflict["phase"], "done");
    assert_eq!(conflict["mergeInput"], serde_json::Value::Null);
    assert_eq!(conflict["mergeBoundary"], conflict_proof["id"]);
    let proof_path = output.join("verification.json");
    let mut proof: Value = serde_json::from_slice(&fs::read(&proof_path)?)?;
    proof["verified"] = true.into();
    proof["queueSurvivedJournalReopen"] = true.into();
    fs::write(proof_path, serde_json::to_vec_pretty(&proof)?)?;
    Ok(())
}
