//! Explicit native acceptance of interrupted file-only inheritance recovery.
use super::*;
use crate::{inheritance_apply_store::Application, lineage::Basis, preparation::Workspace};
use intercat_git::Repository;
use std::{fs, time::Duration};

#[tokio::test]
#[ignore = "native Muse host loss and file recovery; no model turn; requires saved authentication"]
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
    let result = async {
        let first = wait_for(&journal, None).await?;
        let session = first["sessionId"]
            .as_str()
            .ok_or("No native session")?
            .to_owned();
        let source = journal.snapshot().await?["utterances"][0].clone();
        let input = source["id"].as_str().ok_or("No dummy input")?.to_owned();
        let db = rusqlite::Connection::open(root.path().join("db"))?;
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
        service.interrupt_all(Uuid::now_v7())?;
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
                "sameSession":true,"newHost":true,"modelTurns":turns,"requestStayedTakenBack":true,
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
