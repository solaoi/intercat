use intercat_daemon::{
    Result,
    checkout::{Checkout, Configuration},
    project::Folder,
    push_review::{Choice, Member, Request, Review},
    push_target::{Destination, Target},
    push_transfer::Snapshot,
};
use intercat_git::Repository;
use std::{
    collections::BTreeMap, fs, net::TcpListener, os::unix::fs::PermissionsExt, path::Path,
    process::Command, sync::atomic::AtomicBool,
};
use tokio::sync::watch;
use uuid::Uuid;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .current_dir(root)
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
fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}
fn no_cancel() -> watch::Receiver<bool> {
    watch::channel(false).1
}

#[tokio::test]
async fn pre_push_is_offline_shallow_and_uses_native_arguments_without_transport_access()
-> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    let source_root = root.join("source");
    fs::create_dir(&source_root)?;
    git(&source_root, &["init", "-q", "-b", "intercat/dummy"]);
    git(&source_root, &["config", "user.name", "Dummy"]);
    git(
        &source_root,
        &["config", "user.email", "dummy@example.invalid"],
    );
    fs::write(
        source_root.join("removed.txt"),
        "dummy historical content\n",
    )?;
    git(&source_root, &["add", "."]);
    git(&source_root, &["commit", "-qm", "old dummy"]);
    let old = git(&source_root, &["rev-parse", "HEAD"]);
    fs::remove_file(source_root.join("removed.txt"))?;
    fs::write(source_root.join("current.txt"), "dummy current content\n")?;
    git(&source_root, &["add", "-A"]);
    git(&source_root, &["commit", "-qm", "current dummy"]);
    let source = Repository::worktree_source(&source_root, None)?;
    let index = fs::read(source.common_dir.join("index"))?;
    let remote = root.join("dummy.git");
    git(&root, &["init", "--bare", "-q", remote.to_str().unwrap()]);
    let url = reqwest::Url::from_file_path(&remote).unwrap().to_string();
    let folder = Folder {
        id: Uuid::now_v7().to_string(),
        name: "Dummy".into(),
        path: source.root.clone(),
        root: source.root.clone(),
        common_dir: source.common_dir.clone(),
        base: source.head.to_string(),
        base_branch: Some("intercat/dummy".into()),
    };
    let reference = "refs/heads/intercat/dummy";
    let review = Review {
        request: Request {
            op_id: Uuid::now_v7(),
            choices: vec![Choice {
                folder_id: folder.id.clone(),
                remote: "origin".into(),
                target_ref: None,
                through: None,
            }],
        },
        project_id: Uuid::now_v7(),
        session_id: Uuid::now_v7().to_string(),
        session_name: "Dummy".into(),
        members: vec![Member {
            ssh: Vec::new(),
            lfs: vec![intercat_daemon::push_lfs::Binding {
                git: Destination::capture(&url)?,
                upload: Destination::capture(&url)?,
                api: Destination::capture(&url)?,
            }],
            folder,
            slug: "dummy".into(),
            target: Target {
                source_ref: reference.into(),
                sha: source.head.to_string(),
                remote: "origin".into(),
                target_ref: reference.into(),
                destinations: vec![Destination::capture(&url)?],
            },
        }],
    };
    let snapshot = Snapshot::prepare(
        &review,
        &review.members[0].folder.id,
        &root.join("transport"),
        &AtomicBool::new(false),
    )?;
    assert!(snapshot.commits.contains(&old));
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let hook = source.common_dir.join("hooks/pre-push");
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\nset -eu\ntest \"$GIT_LFS_SKIP_PUSH\" = 1\nprintf '%s\\n' \"$1\" \"$2\" > hook.args\ncat > hook.stdin\nif git cat-file -e {old}; then exit 41; fi\nif cat {original} >/dev/null 2>&1; then exit 42; fi\nif cat {private} >/dev/null 2>&1; then exit 43; fi\nif printf 'changed' > {original} 2>/dev/null; then exit 44; fi\nif /usr/bin/curl --max-time 1 -s http://{address}/ >/dev/null 2>&1; then exit 45; fi\nprintf '%s\\n' hook-finished\n",
            original = quote(&source.root.join("current.txt")),
            private = quote(&snapshot.git_directory.join("config")),
        ),
    )?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700))?;
    let config = Configuration {
        values: BTreeMap::new(),
        attributes: None,
        info_attributes: None,
        hooks_directory: source.common_dir.join("hooks"),
        lfs_directory: source.common_dir.join("lfs"),
        object_directories: vec![source.common_dir.join("objects")],
        denied_roots: vec![snapshot.root.clone()],
    };
    let checkout = Checkout::create(
        &root.join("hook-checkout"),
        source.clone(),
        source.head,
        config,
        "/usr/bin:/bin:/usr/sbin:/sbin".into(),
        vec![],
        no_cancel(),
    )
    .await?;
    assert!(
        intercat_daemon::push_hook::run(
            &checkout,
            &snapshot,
            "file:///wrong-dummy.git",
            None,
            no_cancel()
        )
        .await
        .is_err()
    );
    assert!(!checkout.workdir.join("hook.stdin").exists());
    let output = intercat_daemon::push_hook::run(&checkout, &snapshot, &url, None, no_cancel())
        .await?
        .unwrap();
    assert!(
        output.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout)?, "hook-finished\n");
    assert_eq!(
        fs::read_to_string(checkout.workdir.join("hook.args"))?,
        format!("origin\n{url}\n")
    );
    assert_eq!(
        fs::read_to_string(checkout.workdir.join("hook.stdin"))?,
        format!(
            "{sha} {sha} {reference} {}\n",
            "0".repeat(40),
            sha = source.head
        )
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    fs::write(&hook, "#!/bin/sh\nexit 23\n")?;
    let failed =
        intercat_daemon::push_hook::run(&checkout, &snapshot, &url, Some(source.head), no_cancel())
            .await?
            .unwrap();
    assert_eq!(failed.code, Some(23));
    assert!(!failed.success());
    assert!(git(&remote, &["for-each-ref"]).is_empty());
    let client = intercat_daemon::push_file::Client::new(
        &snapshot,
        intercat_daemon::git_tool::GitTool::system().await?,
        &url,
    )?;
    assert!(client.advertise(no_cancel()).await?.is_empty());
    let transport = Repository::open(&snapshot.git_directory)?;
    let range = transport.push_commits(source.head, &[], &AtomicBool::new(false))?;
    let scanner = intercat_daemon::secret_scan::Engine::capture()?.start(&[])?;
    for sha in &range {
        let scan = scanner
            .push_commit(
                &transport,
                *sha,
                &snapshot.git_directory.join("lfs"),
                no_cancel(),
            )
            .await?;
        assert!(scan.findings.is_empty());
    }
    fs::write(
        &hook,
        "#!/bin/sh\ntest \"$GIT_LFS_SKIP_PUSH\" = 1\ncat >/dev/null\n",
    )?;
    assert!(
        intercat_daemon::push_hook::run(&checkout, &snapshot, &url, None, no_cancel())
            .await?
            .unwrap()
            .success()
    );
    assert_eq!(fs::read(source.common_dir.join("index"))?, index);
    assert_eq!(
        git(&source.root, &["rev-parse", "HEAD"]),
        source.head.to_string()
    );
    // A newer local commit and annotated tag are outside the saved SHA.
    fs::write(source.root.join("later.txt"), "dummy later work\n")?;
    git(&source.root, &["add", "later.txt"]);
    git(&source.root, &["commit", "-qm", "dummy later commit"]);
    git(
        &source.root,
        &["tag", "-a", "dummy-later", "-m", "dummy unreviewed tag"],
    );
    git(&source.root, &["config", "push.followTags", "true"]);
    let advanced = git(&source.root, &["rev-parse", "HEAD"]);
    let advanced_index = fs::read(source.common_dir.join("index"))?;
    git(&snapshot.git_directory, &["config", "user.name", "Dummy"]);
    git(
        &snapshot.git_directory,
        &["config", "user.email", "dummy@example.invalid"],
    );
    git(
        &snapshot.git_directory,
        &["config", "push.followTags", "true"],
    );
    git(
        &snapshot.git_directory,
        &[
            "tag",
            "-a",
            "dummy-reachable",
            "-m",
            "dummy unsent tag",
            &source.head.to_string(),
        ],
    );
    let receipt = client.send(no_cancel()).await?;
    assert!(receipt.changed);
    assert_eq!(receipt.sha, source.head.to_string());
    assert_eq!(receipt.reference, reference);
    let refs = client.advertise(no_cancel()).await?;
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].sha, source.head);
    assert_eq!(refs[0].reference, reference);
    assert!(!client.send(no_cancel()).await?.changed);
    assert_eq!(fs::read(source.common_dir.join("index"))?, advanced_index);
    assert_eq!(git(&source.root, &["rev-parse", "HEAD"]), advanced);
    assert!(git(&remote, &["for-each-ref", "refs/tags"]).is_empty());
    assert_eq!(
        fs::read_to_string(source.root.join("current.txt"))?,
        "dummy current content\n"
    );
    Ok(())
}

#[tokio::test]
async fn scanned_lfs_objects_arrive_before_git_refs_and_a_new_clone_can_read_them() -> Result<()> {
    use intercat_daemon::{
        git_tool::GitTool,
        push_inspect::Inspector,
        push_lfs::{Binding, Tool},
    };
    use sha2::{Digest, Sha256};
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    let home = root.join("home");
    let source_root = root.join("source");
    fs::create_dir(&home)?;
    fs::create_dir(&source_root)?;
    git(&source_root, &["init", "-q", "-b", "intercat/dummy"]);
    git(&source_root, &["config", "user.name", "Dummy"]);
    git(
        &source_root,
        &["config", "user.email", "dummy@example.invalid"],
    );
    let old_oid = "ab".repeat(32);
    fs::write(
        source_root.join("old.dat"),
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{old_oid}\nsize 17\n"),
    )?;
    git(&source_root, &["add", "."]);
    git(&source_root, &["commit", "-qm", "dummy old LFS pointer"]);
    fs::remove_file(source_root.join("old.dat"))?;
    fs::write(source_root.join("safe.txt"), "dummy safe current base\n")?;
    git(&source_root, &["add", "-A"]);
    git(
        &source_root,
        &["commit", "-qm", "dummy removes historical LFS pointer"],
    );
    let base = git(&source_root, &["rev-parse", "HEAD"]);
    let git_remote = root.join("git-remote.git");
    let lfs_remote = root.join("lfs-remote.git");
    git(
        &root,
        &["init", "--bare", "-q", git_remote.to_str().unwrap()],
    );
    git(
        &root,
        &["init", "--bare", "-q", lfs_remote.to_str().unwrap()],
    );
    let git_url = reqwest::Url::from_file_path(&git_remote)
        .unwrap()
        .to_string();
    let lfs_url = reqwest::Url::from_file_path(&lfs_remote)
        .unwrap()
        .to_string();
    git(&source_root, &["remote", "add", "origin", &git_url]);
    git(
        &source_root,
        &[
            "push",
            "--no-verify",
            "origin",
            "HEAD:refs/heads/intercat/dummy",
        ],
    );
    let bytes = b"DUMMY GENERATED LFS PAYLOAD\0\x01\x02\n";
    let oid = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
        bytes.len()
    );
    fs::write(source_root.join("dummy.dat"), &pointer)?;
    fs::write(
        source_root.join(".gitattributes"),
        "*.dat filter=lfs -text\n",
    )?;
    fs::write(
        source_root.join(".lfsconfig"),
        format!("[lfs]\n url = {lfs_url}\n"),
    )?;
    let object_tail = Path::new("objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(&oid);
    let source_object = source_root.join(".git/lfs").join(&object_tail);
    fs::create_dir_all(source_object.parent().unwrap())?;
    fs::write(&source_object, bytes)?;
    git(&source_root, &["add", "."]);
    git(&source_root, &["commit", "-qm", "dummy new LFS payload"]);
    let source = Repository::worktree_source(&source_root, None)?;
    let index = fs::read(source.common_dir.join("index"))?;
    let folder = Folder {
        id: Uuid::now_v7().to_string(),
        name: "Dummy".into(),
        path: source.root.clone(),
        root: source.root.clone(),
        common_dir: source.common_dir.clone(),
        base: base.clone(),
        base_branch: Some("intercat/dummy".into()),
    };
    let inspector = Inspector::new(
        GitTool::system().await?,
        home.clone(),
        home.join(".config"),
        false,
    )?;
    let tool = Tool::open(
        GitTool::system().await?,
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tools/git-lfs-3.8.1/git-lfs"),
    )?;
    let target = inspector
        .freeze(
            &folder,
            "refs/heads/intercat/dummy",
            "origin",
            "refs/heads/intercat/dummy",
            Some(source.head),
            no_cancel(),
        )
        .await?;
    let endpoints = tool
        .resolve(
            &inspector,
            &folder,
            &target,
            std::slice::from_ref(&git_url),
            no_cancel(),
        )
        .await?;
    assert_eq!(endpoints[0].api_url, lfs_url);
    let review = Review {
        request: Request {
            op_id: Uuid::now_v7(),
            choices: vec![Choice {
                folder_id: folder.id.clone(),
                remote: "origin".into(),
                target_ref: None,
                through: None,
            }],
        },
        project_id: Uuid::now_v7(),
        session_id: Uuid::now_v7().to_string(),
        session_name: "Dummy".into(),
        members: vec![Member {
            ssh: Vec::new(),
            folder,
            slug: "dummy".into(),
            target,
            lfs: endpoints
                .iter()
                .map(Binding::capture)
                .collect::<Result<Vec<_>>>()?,
        }],
    };
    let snapshot = Snapshot::prepare(
        &review,
        &review.members[0].folder.id,
        &root.join("transport"),
        &AtomicBool::new(false),
    )?;
    let client =
        intercat_daemon::push_file::Client::new(&snapshot, GitTool::system().await?, &git_url)?;
    let tips = client
        .advertise(no_cancel())
        .await?
        .iter()
        .map(|tip| tip.sha)
        .collect::<Vec<_>>();
    let transport = Repository::open(&snapshot.git_directory)?;
    let range = transport.push_commits(source.head, &tips, &AtomicBool::new(false))?;
    assert_eq!(range, vec![source.head]);
    let payloads = snapshot.copy_lfs_range(
        &range,
        &source.common_dir.join("lfs"),
        &AtomicBool::new(false),
    )?;
    assert_eq!(payloads.entries().len(), 1);
    assert_eq!(payloads.entries()[0].pointer.oid(), oid);
    assert!(
        !snapshot
            .git_directory
            .join("lfs/objects")
            .join(&old_oid[..2])
            .join(&old_oid[2..4])
            .join(&old_oid)
            .exists()
    );
    let scan = intercat_daemon::secret_scan::Engine::capture()?
        .start(&[])?
        .push_commit(
            &transport,
            source.head,
            &snapshot.git_directory.join("lfs"),
            no_cancel(),
        )
        .await?;
    assert!(scan.findings.is_empty());
    assert_eq!(scan.unscanned_lfs, vec!["dummy.dat"]);
    let mut other_review = review.clone();
    other_review.request.op_id = Uuid::now_v7();
    let other_snapshot = Snapshot::prepare(
        &other_review,
        &other_review.members[0].folder.id,
        &root.join("another-transport"),
        &AtomicBool::new(false),
    )?;
    assert!(
        tool.push_file(&other_snapshot, &endpoints[0], &payloads, no_cancel())
            .await
            .is_err()
    );
    assert!(
        tool.push_file(&snapshot, &endpoints[0], &payloads, watch::channel(true).1)
            .await
            .is_err()
    );
    assert!(!lfs_remote.join("lfs").join(&object_tail).exists());
    tool.push_file(&snapshot, &endpoints[0], &payloads, no_cancel())
        .await?;
    assert_eq!(fs::read(lfs_remote.join("lfs").join(&object_tail))?, bytes);
    assert_eq!(
        git(&git_remote, &["rev-parse", "refs/heads/intercat/dummy"]),
        base
    );
    assert!(client.send(no_cancel()).await?.changed);
    tool.push_file(&snapshot, &endpoints[0], &payloads, no_cancel())
        .await?;
    let clone = root.join("fresh-clone");
    git(
        &root,
        &[
            "clone",
            "-q",
            "--branch",
            "intercat/dummy",
            &git_url,
            clone.to_str().unwrap(),
        ],
    );
    for arguments in [&["install", "--local"][..], &["pull"][..]] {
        let output = Command::new(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tools/git-lfs-3.8.1/git-lfs"),
        )
        .args(arguments)
        .current_dir(&clone)
        .env_clear()
        .env("HOME", &home)
        .env(
            "PATH",
            format!(
                "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../.tools/git-lfs-3.8.1")
                    .canonicalize()?
                    .display()
            ),
        )
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(fs::read(clone.join("dummy.dat"))?, bytes);
    assert_eq!(fs::read(source.common_dir.join("index"))?, index);
    assert_eq!(
        git(&source.root, &["rev-parse", "HEAD"]),
        source.head.to_string()
    );
    assert_eq!(fs::read_to_string(source.root.join("dummy.dat"))?, pointer);
    Ok(())
}
