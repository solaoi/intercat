use intercat_daemon::{
    Result, candidate, check_environment, check_run, check_settings, checkout::Checkout, project,
};
use intercat_git::{ObjectId, Repository};
use std::{
    fs,
    path::Path,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::watch;
use uuid::Uuid;

fn git(root: &Path, args: &[&str]) -> Result<String> {
    let result = Command::new("/usr/bin/git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "init.templateDir=/dev/null",
        ])
        .args(args)
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()?;
    if !result.status.success() {
        return Err(String::from_utf8_lossy(&result.stderr).into_owned().into());
    }
    Ok(String::from_utf8(result.stdout)?.trim().into())
}
async fn check(
    folder: &project::Folder,
    root: &Path,
    head: ObjectId,
    settings: &candidate::Settings,
) -> Result<check_run::Report> {
    let hash = settings.hash()?;
    let tree = Repository::open(&folder.root)?.tree_id(head)?;
    let checkout = Checkout::create(
        root,
        folder.source().await?,
        head,
        settings.checkout.clone(),
        settings.path.clone().into(),
        settings.toolchain.clone(),
        watch::channel(false).1,
    )
    .await?;
    check_run::execute(
        checkout,
        check_run::Snapshot {
            head,
            tree,
            settings_hash: &hash,
        },
        &root.parent().unwrap().join("downloads"),
        check_run::Settings {
            command: &settings.command,
            history: settings.history,
            npm: settings.npm.clone(),
            registry_origins: &settings.registry_origins,
            timeout_ms: settings.timeout_ms,
        },
        &settings.environment_values()?,
        watch::channel(false).1,
        check_run::Deployment::RestoreSnapshot,
    )
    .await
}

#[tokio::test]
async fn two_checks_read_their_saved_trees_while_the_workers_files_keep_changing() -> Result<()> {
    let own = tempfile::Builder::new()
        .prefix("ic-own-check-pair-")
        .tempdir_in("/private/tmp")?;
    let source = own.path().join("source");
    fs::create_dir(&source)?;
    git(&source, &["init", "-b", "main"])?;
    git(&source, &["config", "user.name", "Own Check fixture"])?;
    git(
        &source,
        &["config", "user.email", "own-check@example.invalid"],
    )?;
    fs::write(source.join("result.txt"), "first\n")?;
    git(&source, &["add", "result.txt"])?;
    git(&source, &["commit", "-m", "Own first saved tree"])?;
    let first = git(&source, &["rev-parse", "HEAD"])?;
    let folder = project::inspect(
        Uuid::now_v7(),
        "Own frozen Check".into(),
        vec![source.clone()],
    )
    .await?
    .only_folder()?
    .clone();
    let worker = own.path().join("worker");
    git(
        &source,
        &[
            "worktree",
            "add",
            "--detach",
            worker.to_str().unwrap(),
            &first,
        ],
    )?;
    fs::write(worker.join("result.txt"), "second\n")?;
    git(&worker, &["commit", "-am", "Own second saved tree"])?;
    let second = git(&worker, &["rev-parse", "HEAD"])?;
    git(
        &source,
        &[
            "update-ref",
            "refs/intercat/own-frozen-check/second",
            &second,
        ],
    )?;
    // The live worktree is already different from both comparison inputs.
    fs::write(worker.join("result.txt"), "new unsaved work\n")?;
    let source_index = fs::read(source.join(".git/index"))?;
    let worker_index = fs::read(git(
        &worker,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?)?;
    let command = check_run::CheckCommand::Run(format!(
        "test ! -r {}/result.txt && test \"$(cat result.txt)\" = first",
        worker.display()
    ));
    let choices = check_settings::Setting {
        revision: 1,
        folders: std::collections::BTreeMap::from([(
            folder.id.clone(),
            check_settings::Folder {
                revision: 1,
                command,
                history: false,
            },
        )]),
    };
    let settings =
        candidate::Settings::capture(&folder, check_environment::Selection::default(), choices)?;
    let (writer_done, mut writer_control) = watch::channel(false);
    let writes = Arc::new(AtomicUsize::new(0));
    let writer_count = writes.clone();
    let writing = worker.join("result.txt");
    let writer = tokio::spawn(async move {
        loop {
            tokio::select! {
                changed=writer_control.changed()=>if changed.is_err()||*writer_control.borrow(){break;},
                _=tokio::time::sleep(Duration::from_millis(5))=>{
                    let number=writer_count.fetch_add(1,Ordering::SeqCst);
                    fs::write(&writing,format!("Own unsaved work {number}\n"))?;
                }
            }
        }
        Ok::<_, std::io::Error>(())
    });
    let one = check(
        &folder,
        &own.path().join("check-one"),
        first.parse()?,
        &settings,
    )
    .await?;
    let two = check(
        &folder,
        &own.path().join("check-two"),
        second.parse()?,
        &settings,
    )
    .await?;
    writer_done.send(true)?;
    writer.await??;
    assert!(writes.load(Ordering::SeqCst) > 1);
    assert_eq!(one.state, check_run::State::Passed, "{:?}", one.reason);
    assert_eq!(two.state, check_run::State::Failed, "{:?}", two.reason);
    assert_ne!(one.workspace, two.workspace);
    for (report, sha, exit) in [(&one, &first, 0), (&two, &second, 1)] {
        let record = report.check.as_ref().unwrap();
        assert_eq!(
            record.tree,
            Repository::open(&source)?
                .tree_id(sha.parse()?)?
                .to_string()
        );
        assert_eq!(record.settings_hash, settings.hash()?);
        assert_eq!(record.code, Some(exit));
        assert!(!report.workspace.starts_with(&worker));
        assert!(!report.workspace.starts_with(&source));
    }
    assert!(fs::read_to_string(worker.join("result.txt"))?.starts_with("Own unsaved work "));
    assert_eq!(git(&worker, &["rev-parse", "HEAD"])?, second);
    assert_eq!(git(&source, &["rev-parse", "HEAD"])?, first);
    assert_eq!(fs::read(source.join(".git/index"))?, source_index);
    assert_eq!(
        fs::read(git(
            &worker,
            &["rev-parse", "--path-format=absolute", "--git-path", "index"]
        )?)?,
        worker_index
    );
    // No command is a recorded absence, not an exit-zero test result.
    let mut disabled = settings.clone();
    disabled.command = check_run::CheckCommand::Disabled;
    let none = check(
        &folder,
        &own.path().join("check-none"),
        first.parse()?,
        &disabled,
    )
    .await?;
    let record = none.check.as_ref().unwrap();
    assert_eq!(none.state, check_run::State::Passed);
    assert!(record.command.is_none());
    assert!(record.code.is_none());
    assert_ne!(record.settings_hash, one.check.unwrap().settings_hash);
    Ok(())
}
