use intercat_daemon::{
    Result,
    checkout::{Checkout, Configuration},
};
use intercat_git::{ObjectId, Repository, WorktreeSource};
use std::{ffi::OsString, fs, net::TcpListener, path::Path, process::Command, time::Duration};
use tokio::sync::watch;

fn lfs_tool() -> Result<std::path::PathBuf> {
    // A fixture uses only our explicitly installed test tool, never a user's
    // system LFS installation or configuration.
    Ok(Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.tools/git-lfs-3.8.1/git-lfs")
        .canonicalize()?)
}

fn lfs_pointer(bytes: &[u8]) -> (String, String) {
    use sha2::{Digest, Sha256};
    let oid = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
        bytes.len()
    );
    (oid, pointer)
}

fn lfs_object(root: &Path, oid: &str, bytes: &[u8]) -> Result<std::path::PathBuf> {
    let path = root
        .join("objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, bytes)?;
    Ok(path)
}

fn configure_lfs(f: &Fixture) -> Result<std::path::PathBuf> {
    let tool = lfs_tool()?;
    fs::write(
        f.source.common_dir.join("info/attributes"),
        "*.lfs filter=lfs -text\n",
    )?;
    for (key, command) in [
        ("filter.lfs.clean", "clean -- %f"),
        ("filter.lfs.smudge", "smudge -- %f"),
        ("filter.lfs.process", "filter-process"),
    ] {
        let command = format!("'{}' {command}", tool.display());
        git(&f.source.root, &["config", key, &command]);
    }
    git(&f.source.root, &["config", "filter.lfs.required", "true"]);
    git(
        &f.source.root,
        &["config", "lfs.storage", "../../lfs-cache"],
    );
    Ok(f.root.path().canonicalize()?.join("lfs-cache"))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn no_cancel() -> watch::Receiver<bool> {
    watch::channel(false).1
}
struct Fixture {
    root: tempfile::TempDir,
    source: WorktreeSource,
    head: ObjectId,
    index: Vec<u8>,
}
impl Fixture {
    fn new() -> Result<Self> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("source repo #1");
        fs::create_dir(&path)?;
        git(&path, &["init", "-q", "-b", "main"]);
        git(&path, &["config", "user.name", "Fixture"]);
        git(&path, &["config", "user.email", "fixture@example.invalid"]);
        fs::write(path.join("old-secret.txt"), "only in old history\n")?;
        git(&path, &["add", "."]);
        git(&path, &["commit", "-qm", "old"]);
        fs::remove_file(path.join("old-secret.txt"))?;
        fs::write(path.join("file.txt"), "current\n")?;
        fs::write(path.join("lines.txt"), "one\ntwo\n")?;
        std::os::unix::fs::symlink("file.txt", path.join("link"))?;
        git(&path, &["add", "-A"]);
        git(&path, &["commit", "-qm", "base"]);
        let source = Repository::worktree_source(&path, None)?;
        let head = source.head;
        let index = fs::read(path.join(".git/index"))?;
        Ok(Self {
            root,
            source,
            head,
            index,
        })
    }
    async fn checkout(&self, configuration: Configuration) -> Result<Checkout> {
        Checkout::create(
            &self
                .root
                .path()
                .join(format!("job-{}", uuid::Uuid::now_v7())),
            self.source.clone(),
            self.head,
            configuration,
            OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"),
            vec![],
            no_cancel(),
        )
        .await
    }
    fn unchanged(&self) -> Result<()> {
        assert_eq!(fs::read(self.source.root.join(".git/index"))?, self.index);
        assert_eq!(
            git(&self.source.root, &["rev-parse", "HEAD"]),
            self.head.to_string()
        );
        assert_eq!(
            fs::read_to_string(self.source.root.join("file.txt"))?,
            "current\n"
        );
        Ok(())
    }
}

#[tokio::test]
async fn attachment_ownership_survives_resume_and_rejects_incoming_project_collisions() -> Result<()>
{
    let f = Fixture::new()?;
    let config = Configuration::capture(&f.source)?;
    let checkout = f.checkout(config.clone()).await?;
    assert_eq!(checkout.attachment_roots, vec![".intercat/attachments"]);
    let root = checkout.root.clone();
    let record = root.join("control/attachment-roots.json");
    let original = fs::read(&record)?;
    let mut invalid: serde_json::Value = serde_json::from_slice(&original)?;
    invalid["roots"] = serde_json::json!([]);
    fs::write(&record, serde_json::to_vec(&invalid)?)?;
    assert!(
        intercat_daemon::attachments::resume_checkout(&root.join("control"), &checkout.workdir)
            .is_err()
    );
    fs::write(&record, original)?;
    let resumed = Checkout::resume(
        &root,
        f.source.clone(),
        config,
        OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"),
        vec![],
    )
    .await?;
    assert_eq!(resumed.attachment_roots, checkout.attachment_roots);
    fs::write(
        resumed.workdir.join(".intercat/attachments/copy.txt"),
        "reference",
    )?;
    resumed
        .deploy(
            Repository::open(&f.source.root)?.tree_id(f.head)?,
            no_cancel(),
        )
        .await?;
    assert_eq!(
        fs::read_to_string(resumed.workdir.join(".intercat/attachments/copy.txt"))?,
        "reference"
    );
    assert!(git(&resumed.workdir, &["status", "--porcelain"]).is_empty());
    let repo = Repository::open(&f.source.root)?;
    let collision = repo.write_tree(&[(
        ".intercat/attachments/project.txt",
        repo.write_blob(b"project file")?,
    )])?;
    assert!(resumed.deploy(collision, no_cancel()).await.is_err());
    assert!(
        !resumed
            .workdir
            .join(".intercat/attachments/project.txt")
            .exists()
    );
    fs::remove_dir_all(resumed.workdir.join(".intercat/attachments"))?;
    let outside = f.root.path().join("outside");
    fs::create_dir(&outside)?;
    std::os::unix::fs::symlink(&outside, resumed.workdir.join(".intercat/attachments"))?;
    assert!(resumed.ensure_attachments().is_err());
    assert!(!outside.join(".gitignore").exists());
    let moved = root.join("moved-repo");
    fs::rename(&resumed.workdir, &moved)?;
    fs::create_dir(&resumed.workdir)?;
    assert!(
        intercat_daemon::attachments::resume_checkout(&root.join("control"), &resumed.workdir)
            .is_err()
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn clones_only_the_frozen_revision_and_never_shares_objects_or_original_files() -> Result<()>
{
    let mut f = Fixture::new()?;
    let old_blob = git(&f.source.root, &["rev-parse", "HEAD~:old-secret.txt"]);
    let frozen = f.head;
    fs::write(f.source.root.join("later.txt"), "later\n")?;
    git(&f.source.root, &["add", "."]);
    git(&f.source.root, &["commit", "-qm", "later"]);
    f.index = fs::read(f.source.root.join(".git/index"))?;
    f.head = git(&f.source.root, &["rev-parse", "HEAD"]).parse()?;
    git(&f.source.root, &["gc", "--aggressive", "--prune=now"]);
    let config = Configuration::capture(&f.source)?;
    let checkout = Checkout::create(
        &f.root.path().join("job frozen"),
        f.source.clone(),
        frozen,
        config,
        OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"),
        vec![],
        no_cancel(),
    )
    .await?;
    assert!(
        !checkout.workdir.join("file.txt").exists(),
        "checkout must wait until conversion settings are copied"
    );
    checkout
        .deploy(
            Repository::open(&f.source.root)?.tree_id(frozen)?,
            no_cancel(),
        )
        .await?;
    assert_eq!(
        fs::read_to_string(checkout.workdir.join("file.txt"))?,
        "current\n"
    );
    assert!(!checkout.workdir.join("later.txt").exists());
    assert_eq!(
        git(&checkout.workdir, &["rev-list", "--count", "HEAD"]),
        "1"
    );
    assert!(git(&checkout.workdir, &["remote"]).is_empty());
    assert!(
        !checkout
            .workdir
            .join(".git/objects/info/alternates")
            .exists()
    );
    let unreadable = checkout
        .git(
            &["cat-file".into(), "-e".into(), old_blob.into()],
            false,
            no_cancel(),
        )
        .await?;
    assert!(!unreadable.success());
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn lfs_materializes_only_current_payload_and_returns_check_changes_before_import()
-> Result<()> {
    let f = Fixture::new()?;
    let storage = configure_lfs(&f)?;
    let bytes = b"agent-created current LFS text\n";
    let history = b"agent-created historical LFS text\n";
    let (oid, pointer) = lfs_pointer(bytes);
    let (old_oid, _) = lfs_pointer(history);
    let original = lfs_object(&storage, &oid, bytes)?;
    let historical = lfs_object(&storage, &old_oid, history)?;
    let repo = Repository::open(&f.source.root)?;
    let tree = repo.write_tree(&[("asset.lfs", repo.write_blob(pointer.as_bytes())?)])?;
    let config = Configuration::capture(&f.source)?;
    assert_eq!(config.lfs_directory, storage);
    let checkout = Checkout::create(
        &f.root.path().join("lfs-job"),
        f.source.clone(),
        f.head,
        config,
        OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"),
        vec![lfs_tool()?.parent().unwrap().to_path_buf()],
        no_cancel(),
    )
    .await?;
    checkout.deploy(tree, no_cancel()).await?;
    assert_eq!(fs::read(checkout.workdir.join("asset.lfs"))?, bytes);
    let own = checkout.workdir.join(".git/lfs");
    assert!(
        !own.join("objects")
            .join(&old_oid[..2])
            .join(&old_oid[2..4])
            .join(&old_oid)
            .exists()
    );
    let old_blob = git(&f.source.root, &["rev-parse", "HEAD~:old-secret.txt"]);
    let probe = format!(
        "! cat '{}' >/dev/null 2>&1 && ! git cat-file -e '{old_blob}' 2>/dev/null && ! cat '{}' >/dev/null 2>&1",
        historical.display(),
        original.display(),
    );
    let output = checkout
        .run_check(
            &probe,
            &Default::default(),
            no_cancel(),
            Duration::from_secs(20),
        )
        .await?;
    assert!(output.success(), "{output:?}");
    let changed = b"agent-created modified LFS text\n";
    fs::write(checkout.workdir.join("asset.lfs"), changed)?;
    checkout.stage(f.head, no_cancel()).await?;
    let output = checkout
        .git(
            &[
                "-c".into(),
                "user.name=Fixture".into(),
                "-c".into(),
                "user.email=fixture@example.invalid".into(),
                "commit".into(),
                "-qm".into(),
                "dummy LFS change".into(),
            ],
            true,
            no_cancel(),
        )
        .await?;
    assert!(output.success(), "{output:?}");
    let commit = Repository::open(&checkout.workdir)?.head_commit()?;
    checkout.import_commit(commit)?;
    let (new_oid, _) = lfs_pointer(changed);
    let transferred = storage
        .join("objects")
        .join(&new_oid[..2])
        .join(&new_oid[2..4])
        .join(new_oid);
    assert_eq!(fs::read(transferred)?, changed);
    assert_eq!(fs::read(original)?, bytes);
    assert_eq!(
        repo.commit_details(commit)?.tree,
        Repository::open(&checkout.workdir)?
            .commit_details(commit)?
            .tree
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn absent_lfs_payload_stops_deploy_before_filter_execution() -> Result<()> {
    let f = Fixture::new()?;
    let storage = configure_lfs(&f)?;
    let (_, pointer) = lfs_pointer(b"agent-created absent data\n");
    let repo = Repository::open(&f.source.root)?;
    let tree = repo.write_tree(&[("missing.lfs", repo.write_blob(pointer.as_bytes())?)])?;
    let checkout = Checkout::create(
        &f.root.path().join("missing-lfs-job"),
        f.source.clone(),
        f.head,
        Configuration::capture(&f.source)?,
        OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"),
        vec![lfs_tool()?.parent().unwrap().to_path_buf()],
        no_cancel(),
    )
    .await?;
    let error = checkout.deploy(tree, no_cancel()).await.unwrap_err();
    assert!(
        matches!(error.downcast_ref(), Some(intercat_daemon::lfs_store::Problem::Missing { path }) if path == "missing.lfs")
    );
    assert!(!checkout.workdir.join("missing.lfs").exists());
    assert!(
        !storage.exists(),
        "missing source storage must never be created"
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn copied_check_generation_rebinds_metadata_and_old_code_cannot_write_into_it() -> Result<()>
{
    let f = Fixture::new()?;
    let mut configuration = Configuration::capture(&f.source)?;
    configuration.attributes = Some(b"*.txt text\n".to_vec());
    let old = f.checkout(configuration).await?;
    old.deploy(
        Repository::open(&f.source.root)?.tree_id(f.head)?,
        no_cancel(),
    )
    .await?;
    fs::write(
        old.workdir.join("generated.txt"),
        b"dummy previous generation\n",
    )?;
    let next = old.next_check(f.head, no_cancel()).await?;
    assert_ne!(old.root, next.root);
    assert_eq!(
        fs::read(next.workdir.join("generated.txt"))?,
        b"dummy previous generation\n"
    );
    next.ensure_attachments()?;
    let attributes = git(&next.workdir, &["config", "--get", "core.attributesFile"]);
    assert_eq!(Path::new(&attributes), next.root.join("control/attributes"));
    assert_eq!(fs::read(attributes)?, b"*.txt text\n");
    assert_eq!(
        Path::new(&git(&next.workdir, &["config", "--get", "lfs.storage"])),
        next.workdir.join(".git/lfs")
    );
    for directory in [&next.workdir, &next.temporary] {
        let intrusion = directory.join("old-process-write");
        let script = format!("printf dummy > '{}'", intrusion.display());
        let result = old
            .run_check(
                &script,
                &Default::default(),
                no_cancel(),
                Duration::from_secs(20),
            )
            .await?;
        assert!(!result.success());
        assert!(!intrusion.exists());
    }
    fs::write(
        old.workdir.join("generated.txt"),
        b"dummy changed old generation\n",
    )?;
    assert_eq!(
        fs::read(next.workdir.join("generated.txt"))?,
        b"dummy previous generation\n"
    );
    fs::write(
        next.workdir.join("generated.txt"),
        b"dummy changed new generation\n",
    )?;
    assert_eq!(
        fs::read(old.workdir.join("generated.txt"))?,
        b"dummy changed old generation\n"
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn original_alternate_objects_remain_denied_inside_an_allowed_toolchain_directory()
-> Result<()> {
    let f = Fixture::new()?;
    let tools = f.root.path().join("dummy-shared-tools");
    fs::create_dir(&tools)?;
    let alternate = tools.join("object-pool.git");
    fs::create_dir(&alternate)?;
    Repository::init_isolated(&alternate)?;
    let alternate = alternate.canonicalize()?;
    let oid =
        Repository::open(&alternate)?.write_blob(b"agent-created history-only alternate object")?;
    fs::create_dir_all(f.source.common_dir.join("objects/info"))?;
    fs::write(
        f.source.common_dir.join("objects/info/alternates"),
        format!("{}\n", alternate.join("objects").display()),
    )?;
    let configuration = Configuration::capture(&f.source)?;
    assert!(
        configuration
            .object_directories
            .contains(&alternate.join("objects"))
    );
    let checkout = Checkout::create(
        &f.root.path().join("alternate-job"),
        f.source.clone(),
        f.head,
        configuration,
        OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"),
        vec![tools],
        no_cancel(),
    )
    .await?;
    let object = oid.to_string();
    let loose = alternate
        .join("objects")
        .join(&object[..2])
        .join(&object[2..]);
    let script = format!(
        "! cat '{}' >/dev/null 2>&1 && ! git --git-dir='{}' cat-file blob '{oid}' >/dev/null 2>&1",
        loose.display(),
        alternate.display()
    );
    let result = checkout
        .run_check(
            &script,
            &Default::default(),
            no_cancel(),
            Duration::from_secs(20),
        )
        .await?;
    assert!(result.success(), "{result:?}");
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn copied_attributes_filters_crlf_and_symlink_mode_match_the_original_configuration()
-> Result<()> {
    let f = Fixture::new()?;
    let external = f.root.path().join("global-attributes");
    fs::write(&external, "file.txt filter=upper\nlines.txt text\n")?;
    git(
        &f.source.root,
        &["config", "core.attributesFile", external.to_str().unwrap()],
    );
    git(&f.source.root, &["config", "core.autocrlf", "true"]);
    git(&f.source.root, &["config", "core.symlinks", "false"]);
    git(
        &f.source.root,
        &["config", "filter.upper.clean", "tr a-z A-Z"],
    );
    git(
        &f.source.root,
        &["config", "filter.upper.smudge", "tr A-Z a-z"],
    );
    git(&f.source.root, &["config", "filter.upper.required", "true"]);
    let repo = Repository::open(&f.source.root)?;
    // Use uppercase Git content, without changing the fixture's real index.
    let tree = repo.write_tree(&[
        ("file.txt", repo.write_blob(b"STORED\n")?),
        ("lines.txt", repo.write_blob(b"one\ntwo\n")?),
    ])?;
    let checkout = f.checkout(Configuration::capture(&f.source)?).await?;
    checkout.deploy(repo.tree_id(f.head)?, no_cancel()).await?;
    assert!(
        !fs::symlink_metadata(checkout.workdir.join("link"))?
            .file_type()
            .is_symlink()
    );
    assert_eq!(fs::read(checkout.workdir.join("link"))?, b"file.txt");
    checkout.deploy(tree, no_cancel()).await?;
    assert_eq!(fs::read(checkout.workdir.join("file.txt"))?, b"stored\r\n");
    assert_eq!(
        fs::read(checkout.workdir.join("lines.txt"))?,
        b"one\r\ntwo\r\n"
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn process_filters_are_copied_and_required_failures_stop_checkout() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(
        f.source.common_dir.join("info/attributes"),
        "file.txt filter=required\n",
    )?;
    git(&f.source.root, &["config", "filter.required.clean", "cat"]);
    git(&f.source.root, &["config", "filter.required.smudge", "cat"]);
    git(
        &f.source.root,
        &[
            "config",
            "filter.required.process",
            "missing-required-filter",
        ],
    );
    git(
        &f.source.root,
        &["config", "filter.required.required", "false"],
    );
    let config = Configuration::capture(&f.source)?;
    assert_eq!(
        config.values["filter.required.process"],
        "missing-required-filter"
    );
    let checkout = f.checkout(config).await?;
    assert!(
        checkout
            .deploy(
                Repository::open(&f.source.root)?.tree_id(f.head)?,
                no_cancel()
            )
            .await
            .is_err()
    );
    f.unchanged()?;
    Ok(())
}

fn shell_path(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

#[tokio::test]
async fn project_code_cannot_read_history_source_or_neighbor_files_write_outside_or_connect()
-> Result<()> {
    let f = Fixture::new()?;
    let neighbor = f.root.path().join("another-job-secret");
    fs::write(&neighbor, "private\n")?;
    let server = TcpListener::bind(("127.0.0.1", 0))?;
    server.set_nonblocking(true)?;
    let checkout = f.checkout(Configuration::capture(&f.source)?).await?;
    checkout
        .deploy(
            Repository::open(&f.source.root)?.tree_id(f.head)?,
            no_cancel(),
        )
        .await?;
    let script = format!(
        "set -eu\nprintf ok > allowed\nprintf ok > \"$TMPDIR/allowed\"\nif cat {} ; then exit 11; fi\nif cat {} ; then exit 12; fi\nif printf bad > {} ; then exit 13; fi\nif curl --silent --max-time 1 http://127.0.0.1:{} ; then exit 14; fi\n",
        shell_path(&f.source.root.join("file.txt")),
        shell_path(&neighbor),
        shell_path(&f.source.root.join("outside")),
        server.local_addr()?.port()
    );
    let result = checkout
        .run_check(
            &script,
            &std::collections::BTreeMap::new(),
            no_cancel(),
            Duration::from_secs(10),
        )
        .await?;
    assert!(
        result.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(fs::read(checkout.workdir.join("allowed"))?, b"ok");
    assert_eq!(fs::read(checkout.temporary.join("allowed"))?, b"ok");
    assert!(server.accept().is_err());
    assert!(!f.source.root.join("outside").exists());
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn command_cancellation_timeout_and_background_children_are_reaped_with_bounded_output()
-> Result<()> {
    let f = Fixture::new()?;
    let checkout = f.checkout(Configuration::capture(&f.source)?).await?;
    let background = checkout
        .run_check(
            "sleep 30 & echo $!",
            &std::collections::BTreeMap::new(),
            no_cancel(),
            Duration::from_secs(5),
        )
        .await?;
    assert!(background.success());
    let pid: u32 = String::from_utf8(background.stdout)?.trim().parse()?;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(match intercat_daemon::process::identity(pid) {
        Ok(identity) => !intercat_daemon::process::is_alive(identity),
        Err(_) => true,
    });
    let expired = checkout
        .run_check(
            "sleep 30",
            &std::collections::BTreeMap::new(),
            no_cancel(),
            Duration::from_millis(100),
        )
        .await?;
    assert!(expired.timed_out && !expired.success());
    let (cancel, receive) = watch::channel(false);
    let environment = std::collections::BTreeMap::new();
    let stopped = checkout.run_check("sleep 30", &environment, receive, Duration::from_secs(5));
    let change = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel.send(true).unwrap();
    };
    let (stopped, _) = tokio::join!(stopped, change);
    assert!(stopped?.cancelled);
    let noisy = checkout
        .run_check(
            "yes x | head -c 2097152",
            &std::collections::BTreeMap::new(),
            no_cancel(),
            Duration::from_secs(5),
        )
        .await?;
    assert!(noisy.success() && noisy.truncated);
    assert_eq!(noisy.stdout.len(), 1024 * 1024);
    Ok(())
}

#[tokio::test]
async fn tampered_git_metadata_is_rejected_before_daemon_object_import() -> Result<()> {
    let f = Fixture::new()?;
    let checkout = f.checkout(Configuration::capture(&f.source)?).await?;
    fs::remove_dir_all(checkout.workdir.join(".git/objects"))?;
    std::os::unix::fs::symlink(
        f.source.common_dir.join("objects"),
        checkout.workdir.join(".git/objects"),
    )?;
    assert!(
        checkout
            .deploy(
                Repository::open(&f.source.root)?.tree_id(f.head)?,
                no_cancel()
            )
            .await
            .is_err()
    );
    f.unchanged()?;
    Ok(())
}

#[test]
fn configuration_reads_global_includes_and_local_overrides_without_environment_overrides()
-> Result<()> {
    const KEY: &str = "INTERCAT_CHECKOUT_CONFIG_FIXTURE";
    if let Some(path) = std::env::var_os(KEY) {
        let source = Repository::worktree_source(Path::new(&path), None)?;
        let config = Configuration::capture(&source)?;
        assert_eq!(config.values["core.autocrlf"], "false");
        assert_eq!(config.values["core.symlinks"], "true");
        assert_eq!(
            config.values["filter.from-global.process"],
            "global-process-filter"
        );
        assert_eq!(config.attributes, Some(b"*.txt text\n".to_vec()));
        return Ok(());
    }
    let f = Fixture::new()?;
    let home = f.root.path().join("home");
    fs::create_dir(&home)?;
    fs::write(
        home.join(".gitconfig"),
        "[include]\npath=filters.inc\n[core]\nautocrlf=true\nattributesFile=~/attributes\n",
    )?;
    fs::write(
        home.join("filters.inc"),
        "[filter \"from-global\"]\nprocess=global-process-filter\nrequired=true\n",
    )?;
    fs::write(home.join("attributes"), "*.txt text\n")?;
    git(&f.source.root, &["config", "core.autocrlf", "false"]);
    git(&f.source.root, &["config", "core.symlinks", "true"]);
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            "configuration_reads_global_includes_and_local_overrides_without_environment_overrides",
            "--nocapture",
        ])
        .env(KEY, &f.source.root)
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("GIT_CONFIG_GLOBAL")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "core.symlinks")
        .env("GIT_CONFIG_VALUE_0", "false");
    let result = command.output()?;
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    fs::write(
        home.join(".gitconfig"),
        "[include]\npath=filters.inc\n[core]\nautocrlf=true\n",
    )?;
    fs::create_dir_all(home.join(".config/git"))?;
    fs::write(home.join(".config/git/attributes"), "*.txt text\n")?;
    let result = command.output()?;
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn transfer_does_not_run_a_source_pack_objects_hook() -> Result<()> {
    let f = Fixture::new()?;
    let root = f.root.path().join("hook-job");
    let marker = root.join("tmp/pack-hook-ran");
    let hook = format!(
        "/bin/sh -c 'printf ran > {}; exec /usr/bin/git pack-objects \"$@\"' hook",
        marker.display()
    );
    git(
        &f.source.root,
        &["config", "uploadpack.packObjectsHook", &hook],
    );
    let checkout = Checkout::create(
        &root,
        f.source.clone(),
        f.head,
        Configuration::capture(&f.source)?,
        "/usr/bin:/bin:/usr/sbin:/sbin".into(),
        vec![],
        no_cancel(),
    )
    .await?;
    assert!(!marker.exists());
    checkout
        .deploy(
            Repository::open(&f.source.root)?.tree_id(f.head)?,
            no_cancel(),
        )
        .await?;
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn dropping_a_running_check_stops_its_observed_processes() -> Result<()> {
    let f = Fixture::new()?;
    let checkout = std::sync::Arc::new(f.checkout(Configuration::capture(&f.source)?).await?);
    let task = {
        let checkout = checkout.clone();
        tokio::spawn(async move {
            checkout
                .run_check(
                    "sleep 30 & echo $! > child.pid; wait",
                    &std::collections::BTreeMap::new(),
                    no_cancel(),
                    Duration::from_secs(40),
                )
                .await
        })
    };
    let path = checkout.workdir.join("child.pid");
    let pid: u32 = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(text) = fs::read_to_string(&path)
                && let Ok(pid) = text.trim().parse()
            {
                break pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let identity = intercat_daemon::process::identity(pid)?;
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while intercat_daemon::process::is_alive(identity) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn runs_npm_test_with_the_selected_node_toolchain_inside_the_isolated_checkout() -> Result<()>
{
    let mut f = Fixture::new()?;
    fs::write(
        f.source.root.join("package.json"),
        r#"{"name":"intercat-check-fixture","private":true,"scripts":{"test":"node verify.cjs"}}"#,
    )?;
    fs::write(
        f.source.root.join("verify.cjs"),
        "const assert = require('node:assert/strict'); const fs = require('node:fs'); assert.equal(fs.readFileSync('file.txt', 'utf8'), 'current\\n'); console.log('isolated npm check passed');\n",
    )?;
    fs::write(
        f.source.root.join(".npmrc"),
        "update-notifier=false\naudit=false\nfund=false\n",
    )?;
    git(&f.source.root, &["add", "."]);
    git(&f.source.root, &["commit", "-qm", "npm fixture"]);
    f.head = git(&f.source.root, &["rev-parse", "HEAD"]).parse()?;
    f.source.head = f.head;
    f.index = fs::read(f.source.root.join(".git/index"))?;
    let path = std::env::var_os("PATH").ok_or("PATH missing")?;
    let node = std::env::split_paths(&path)
        .map(|dir| dir.join("node"))
        .find(|file| file.is_file())
        .ok_or("Node is unavailable")?
        .canonicalize()?;
    let runtime = node
        .parent()
        .and_then(Path::parent)
        .ok_or("Invalid Node install")?
        .to_path_buf();
    let checkout = Checkout::create(
        &f.root.path().join("npm-job"),
        f.source.clone(),
        f.head,
        Configuration::capture(&f.source)?,
        path,
        vec![runtime],
        no_cancel(),
    )
    .await?;
    checkout
        .deploy(
            Repository::open(&f.source.root)?.tree_id(f.head)?,
            no_cancel(),
        )
        .await?;
    let result = checkout
        .run_check(
            "npm test",
            &std::collections::BTreeMap::new(),
            no_cancel(),
            Duration::from_secs(15),
        )
        .await?;
    assert!(
        result.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8(result.stdout)?.contains("isolated npm check passed"));
    f.unchanged()?;
    Ok(())
}

#[tokio::test]
async fn check_history_is_explicit_and_turning_it_off_removes_historical_objects() -> Result<()> {
    let f = Fixture::new()?;
    fs::write(
        f.source.root.join("removed.env"),
        "DUMMY_REMOVED_VALUE=own-fixture-only\n",
    )?;
    git(&f.source.root, &["add", "."]);
    git(&f.source.root, &["commit", "-m", "dummy historical file"]);
    let old = git(&f.source.root, &["rev-parse", "HEAD:removed.env"]);
    git(&f.source.root, &["tag", "old-blob", &old]);
    fs::remove_file(f.source.root.join("removed.env"))?;
    git(&f.source.root, &["add", "-A"]);
    git(&f.source.root, &["commit", "-m", "remove dummy file"]);
    let head: ObjectId = git(&f.source.root, &["rev-parse", "HEAD"]).parse()?;
    git(&f.source.root, &["tag", "current"]);
    let source = Repository::worktree_source(&f.source.root, None)?;
    let checkout = Checkout::create(
        &f.root.path().join("history"),
        source.clone(),
        head,
        Configuration::capture(&source)?,
        OsString::from("/usr/bin:/bin:/usr/sbin:/sbin"),
        vec![],
        no_cancel(),
    )
    .await?;
    checkout
        .deploy(
            Repository::open(&checkout.workdir)?.tree_id(head)?,
            no_cancel(),
        )
        .await?;
    assert!(
        !checkout
            .run_check(
                &format!("git cat-file -e {old}"),
                &Default::default(),
                no_cancel(),
                Duration::from_secs(5)
            )
            .await?
            .success()
    );
    checkout.set_history(true, head, no_cancel()).await?;
    let output = checkout
        .run_check(
            &format!("git describe --tags && git cat-file blob {old}"),
            &Default::default(),
            no_cancel(),
            Duration::from_secs(5),
        )
        .await?;
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("own-fixture-only"));
    let next = checkout.next_check(head, no_cancel()).await?;
    next.set_history(false, head, no_cancel()).await?;
    let output = next
        .run_check(
            &format!("git cat-file -e {old}"),
            &Default::default(),
            no_cancel(),
            Duration::from_secs(5),
        )
        .await?;
    assert!(!output.success());
    assert_eq!(Repository::open(&next.workdir)?.head_commit()?, head);
    assert!(next.workdir.join(".git/shallow").is_file());
    Ok(())
}
