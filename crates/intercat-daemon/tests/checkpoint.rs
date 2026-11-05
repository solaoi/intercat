use intercat_daemon::{Result, checkpoint, journal::Journal, preparation::Workspace, worktree};
use std::{fs, path::Path, process::Command};
fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .current_dir(path)
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
async fn fixture(root: &Path) -> Result<Workspace> {
    let source = root.join("source");
    fs::create_dir(&source)?;
    git(&source, &["init", "-q", "-b", "main"]);
    fs::write(source.join("keep"), "before\n")?;
    fs::write(source.join("gone"), "deleted later\n")?;
    fs::write(source.join(".gitignore"), "ignored*\n.env\n")?;
    fs::write(source.join("ignored-base"), "tracked despite ignore\n")?;
    git(&source, &["add", "."]);
    git(&source, &["add", "-f", "ignored-base"]);
    git(
        &source,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "base",
        ],
    );
    let base = git(&source, &["rev-parse", "HEAD"]);
    let path = root.join("worker");
    let src = worktree::inspect(&source).await?;
    worktree::GitTool::system()
        .await?
        .prepare_detached(&src, &path)
        .await?;
    Ok(Workspace {
        attachment_roots: Vec::new(),
        track_id: "track".into(),
        path,
        reference: "refs/intercat/test/tracks/track".into(),
        base,
        state: "ready".into(),
        error: None,
    })
}
#[tokio::test]
async fn snapshots_real_index_without_changing_either_checkout_and_chains_turns() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let workspace = fixture(&root).await?;
    let source = root.join("source");
    let path = &workspace.path;
    fs::write(path.join("ignored-staged"), "staged force-add\n")?;
    git(path, &["add", "-f", "ignored-staged"]);
    git(path, &["update-index", "--assume-unchanged", "keep"]);
    git(path, &["update-index", "--skip-worktree", "gone"]);
    git(path, &["update-index", "--split-index"]);
    fs::write(path.join("keep"), "after\n")?;
    fs::remove_file(path.join("gone"))?;
    fs::write(path.join("untracked"), "new\n")?;
    fs::write(path.join("ignored-build"), "not saved\n")?;
    fs::write(path.join(".env"), "secret fixture only\n")?;
    git(path, &["add", "-f", ".env"]);
    fs::write(path.join(".env.example"), "EXAMPLE=\n")?;
    fs::write(path.join(".env.production"), "untracked dummy secret\n")?;
    std::os::unix::fs::symlink("keep", path.join("link"))?;
    let index_path = git(path, &["rev-parse", "--git-path", "index"]);
    let index = fs::read(&index_path)?;
    let source_index = fs::read(source.join(".git/index"))?;
    let journal = Journal::open(
        root.join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let first = checkpoint::save(
        workspace.clone(),
        uuid::Uuid::now_v7(),
        None,
        false,
        journal.clone(),
    )
    .await?;
    assert_eq!(
        git(path, &["show", &format!("{}:keep", first.sha)]),
        "after"
    );
    let files = git(path, &["ls-tree", "-r", "--name-only", &first.sha]);
    for present in [
        "ignored-base",
        "ignored-staged",
        "untracked",
        ".env.example",
        "link",
    ] {
        assert!(
            files.lines().any(|p| p == present),
            "missing {present}: {files}"
        );
    }
    for absent in ["gone", "ignored-build", ".env", ".env.production"] {
        assert!(!files.lines().any(|p| p == absent), "unexpected {absent}");
    }
    let secret_blob = git(path, &["hash-object", ".env.production"]);
    assert!(
        !Command::new("/usr/bin/git")
            .current_dir(path)
            .args(["cat-file", "-e", &secret_blob])
            .output()?
            .status
            .success()
    );
    assert_eq!(fs::read(&index_path)?, index);
    assert_eq!(fs::read(source.join(".git/index"))?, source_index);
    assert_eq!(git(path, &["rev-parse", "HEAD"]), workspace.base);
    assert_eq!(git(&source, &["rev-parse", "HEAD"]), workspace.base);
    assert_eq!(fs::read_to_string(source.join("keep"))?, "before\n");
    fs::write(path.join("keep"), "third\n")?;
    let second = checkpoint::save(
        workspace.clone(),
        uuid::Uuid::now_v7(),
        Some(first.clone()),
        true,
        journal.clone(),
    )
    .await?;
    assert_eq!(
        git(path, &["rev-parse", &format!("{}^", second.sha)]),
        first.sha
    );
    assert_eq!(
        git(
            path,
            &["rev-parse", &format!("{}-checkpoint", workspace.reference)]
        ),
        second.sha
    );
    assert!(second.live_processes);
    Ok(())
}
#[tokio::test]
async fn filter_can_transform_content_but_cannot_read_original_objects() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let workspace = fixture(&root).await?;
    let path = &workspace.path;
    fs::write(path.join(".gitattributes"), "*.txt filter=upper\n")?;
    git(
        path,
        &["config", "filter.upper.clean", "/usr/bin/tr a-z A-Z"],
    );
    git(path, &["config", "filter.upper.required", "true"]);
    fs::write(path.join("new.txt"), "lower\n")?;
    let journal = Journal::open(
        root.join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let saved = checkpoint::save(
        workspace.clone(),
        uuid::Uuid::now_v7(),
        None,
        false,
        journal.clone(),
    )
    .await?;
    assert_eq!(
        git(path, &["show", &format!("{}:new.txt", saved.sha)]),
        "LOWER"
    );
    git(
        path,
        &[
            "config",
            "filter.upper.clean",
            &format!(
                "/usr/bin/env GIT_OBJECT_DIRECTORY={} {} --git-dir={} cat-file -p {}",
                root.join("source/.git/objects").display(),
                worktree::GitTool::system().await?.binary().display(),
                root.join("source/.git").display(),
                git(path, &["rev-parse", "HEAD:keep"])
            ),
        ],
    );
    fs::write(path.join("new.txt"), "changed\n")?;
    let error = checkpoint::save(
        workspace.clone(),
        uuid::Uuid::now_v7(),
        Some(saved.clone()),
        false,
        journal,
    )
    .await
    .unwrap_err()
    .to_string()
    .to_ascii_lowercase();
    assert!(
        error.contains("operation not permitted")
            || error.contains("bad object")
            || error.contains("not a valid object"),
        "{error}"
    );
    assert_eq!(
        git(
            path,
            &["rev-parse", &format!("{}-checkpoint", workspace.reference)]
        ),
        saved.sha
    );
    Ok(())
}
#[tokio::test]
async fn nested_repository_is_not_published_as_a_checkpoint() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let workspace = fixture(&root).await?;
    let nested = workspace.path.join("nested");
    fs::create_dir(&nested)?;
    git(&nested, &["init", "-q"]);
    fs::write(nested.join("file"), "nested\n")?;
    git(&nested, &["add", "."]);
    git(
        &nested,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "nested",
        ],
    );
    let journal = Journal::open(
        root.join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    assert!(
        checkpoint::save(
            workspace.clone(),
            uuid::Uuid::now_v7(),
            None,
            false,
            journal
        )
        .await
        .is_err()
    );
    assert_eq!(
        git(&workspace.path, &["for-each-ref", "refs/intercat/"]),
        ""
    );
    Ok(())
}

#[tokio::test]
async fn unresolved_index_paths_are_recorded_with_the_worktree_contents() -> Result<()> {
    use std::io::Write;
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let workspace = fixture(&root).await?;
    let blob = git(&workspace.path, &["rev-parse", "HEAD:keep"]);
    let mut child = Command::new("/usr/bin/git")
        .current_dir(&workspace.path)
        .args(["update-index", "--index-info"])
        .stdin(std::process::Stdio::piped())
        .spawn()?;
    write!(
        child.stdin.take().unwrap(),
        "100644 {blob} 1\tconflict\n100644 {blob} 2\tconflict\n100644 {blob} 3\tconflict\n"
    )?;
    assert!(child.wait()?.success());
    fs::write(
        workspace.path.join("conflict"),
        "unresolved working contents\n",
    )?;
    let index_path = git(&workspace.path, &["rev-parse", "--git-path", "index"]);
    let index = fs::read(&index_path)?;
    let journal = Journal::open(
        root.join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let saved = checkpoint::save(
        workspace.clone(),
        uuid::Uuid::now_v7(),
        None,
        false,
        journal,
    )
    .await?;
    assert_eq!(saved.unmerged_paths, vec!["conflict"]);
    assert_eq!(
        git(
            &workspace.path,
            &["show", &format!("{}:conflict", saved.sha)]
        ),
        "unresolved working contents"
    );
    assert_eq!(fs::read(index_path)?, index);
    Ok(())
}

#[tokio::test]
async fn interrupted_checkpoint_reuses_its_saved_tree_despite_later_file_changes() -> Result<()> {
    use intercat_git::Repository;
    use serde_json::json;
    use uuid::Uuid;
    for written_refs in 0..=3 {
        let dir = tempfile::tempdir()?;
        let root = dir.path().canonicalize()?;
        let workspace = fixture(&root).await?;
        let journal = Journal::open(
            root.join("events.sqlite"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        let turn = Uuid::now_v7();
        fs::write(workspace.path.join("keep"), "frozen snapshot\n")?;
        let captured = checkpoint::capture(&workspace, &workspace.base, "frozen\n").await?;
        let saved = checkpoint::Checkpoint {
            turn_id: turn,
            sha: captured.sha,
            reference: format!("{}-checkpoints/{turn}", workspace.reference),
            unmerged_paths: captured.unmerged_paths,
            live_processes: true,
        };
        let latest = format!("{}-checkpoint", workspace.reference);
        let id = Uuid::new_v5(&turn, b"checkpoint");
        journal.intent(&id.to_string(),"checkpoint.save",json!({"trackId":workspace.track_id,"checkpoint":saved,"latestRef":latest,"expected":null})).await?;
        let repo = Repository::open(&workspace.path)?;
        if written_refs >= 1 {
            repo.compare_and_swap(&saved.reference, None, saved.sha.parse()?)?;
        }
        if written_refs >= 2 {
            repo.compare_and_swap(&latest, None, saved.sha.parse()?)?;
            journal.finish(&id.to_string(), "dispatched").await?;
        }
        if written_refs == 3 {
            journal.finish(&id.to_string(), "complete").await?;
        }
        fs::write(
            workspace.path.join("keep"),
            "later edits must stay outside the snapshot\n",
        )?;
        // A retry must not run filters or attempt to rebuild a different tree.
        fs::write(workspace.path.join(".gitattributes"), "keep filter=fail\n")?;
        git(
            &workspace.path,
            &["config", "filter.fail.clean", "/usr/bin/false"],
        );
        git(&workspace.path, &["config", "filter.fail.required", "true"]);
        let resumed =
            checkpoint::save(workspace.clone(), turn, None, false, journal.clone()).await?;
        assert_eq!(resumed.sha, saved.sha);
        assert!(resumed.live_processes);
        assert_eq!(
            git(&workspace.path, &["show", &format!("{}:keep", resumed.sha)]),
            "frozen snapshot"
        );
        assert_eq!(git(&workspace.path, &["rev-parse", &latest]), saved.sha);
        assert_eq!(
            fs::read_to_string(workspace.path.join("keep"))?,
            "later edits must stay outside the snapshot\n"
        );
        assert_eq!(journal.operation(id).await?.unwrap()["state"], "complete");
    }
    Ok(())
}

#[tokio::test]
async fn symlinked_worktree_path_uses_its_physical_path_for_the_sandbox() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut workspace = fixture(&root.path().canonicalize()?).await?;
    let alias = root.path().join("worker-alias");
    std::os::unix::fs::symlink(&workspace.path, &alias)?;
    workspace.path = alias;
    fs::write(workspace.path.join("keep"), "saved through alias\n")?;
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let saved = checkpoint::save(
        workspace.clone(),
        uuid::Uuid::now_v7(),
        None,
        false,
        journal,
    )
    .await?;
    assert_eq!(
        git(&workspace.path, &["show", &format!("{}:keep", saved.sha)]),
        "saved through alias"
    );
    Ok(())
}

#[tokio::test]
async fn attachment_copies_stay_out_of_checkpoints_even_when_force_added_and_unignored()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let mut workspace = fixture(&root).await?;
    workspace.attachment_roots =
        intercat_daemon::attachments::reserve(&workspace.path, &workspace.base)?;
    workspace
        .attachment_roots
        .extend(intercat_daemon::attachments::reserve(
            &workspace.path,
            &workspace.base,
        )?);
    let path = &workspace.path;
    fs::create_dir_all(path.join(".intercat/attachments"))?;
    fs::create_dir_all(path.join(".intercat-attachments-2"))?;
    fs::write(
        path.join(".intercat/attachments/brief.txt"),
        "dummy private reference",
    )?;
    fs::write(
        path.join(".intercat-attachments-2/second.txt"),
        "another reference",
    )?;
    fs::write(
        path.join(".gitignore"),
        ".intercat/*\n!.intercat/attachments/\n!.intercat/attachments/**\n",
    )?;
    git(
        path,
        &[
            "add",
            "-f",
            ".intercat/attachments/brief.txt",
            ".intercat-attachments-2/second.txt",
        ],
    );
    let raw = git(path, &["write-tree"]);
    assert!(git(path, &["ls-tree", "-r", "--name-only", &raw]).contains("brief.txt"));
    let index_path = git(path, &["rev-parse", "--git-path", "index"]);
    let index = fs::read(&index_path)?;
    let journal = Journal::open(
        root.join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let saved = checkpoint::save(
        workspace.clone(),
        uuid::Uuid::now_v7(),
        None,
        false,
        journal,
    )
    .await?;
    let names = git(path, &["ls-tree", "-r", "--name-only", &saved.sha]);
    assert!(!names.contains(".intercat/attachments"));
    assert!(!names.contains(".intercat-attachments-2"));
    assert_eq!(fs::read(&index_path)?, index);
    assert_eq!(
        fs::read_to_string(path.join(".intercat/attachments/brief.txt"))?,
        "dummy private reference"
    );
    assert_eq!(git(path, &["rev-parse", "HEAD"]), workspace.base);
    Ok(())
}

#[tokio::test]
async fn tracked_attachment_name_collisions_are_preserved_and_copies_use_another_root() -> Result<()>
{
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let mut workspace = fixture(&root).await?;
    let source = root.join("source");
    fs::create_dir_all(source.join(".intercat/attachments"))?;
    fs::write(
        source.join(".intercat/attachments/code.txt"),
        "project code",
    )?;
    git(&source, &["add", ".intercat/attachments/code.txt"]);
    git(
        &source,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "legitimate project path",
        ],
    );
    workspace.base = git(&source, &["rev-parse", "HEAD"]);
    git(&workspace.path, &["checkout", "--detach", &workspace.base]);
    let repo = intercat_git::Repository::open(&workspace.path)?;
    workspace.attachment_roots =
        intercat_daemon::attachments::reserve(&workspace.path, &workspace.base)?;
    let location = workspace.attachment_roots[0].clone();
    assert_eq!(location, ".intercat-attachments-2");
    fs::create_dir_all(workspace.path.join(&location))?;
    fs::write(
        workspace.path.join(&location).join("brief.txt"),
        "reference copy",
    )?;
    fs::write(
        workspace.path.join(".intercat/attachments/code.txt"),
        "changed project code",
    )?;
    fs::write(
        workspace.path.join(".intercat/attachments/new-code.txt"),
        "new project code",
    )?;
    git(
        &workspace.path,
        &["add", "-f", ".intercat-attachments-2/brief.txt"],
    );
    let journal = Journal::open(
        root.join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let saved = checkpoint::save(
        workspace.clone(),
        uuid::Uuid::now_v7(),
        None,
        false,
        journal,
    )
    .await?;
    let tree = repo.saved_files(saved.sha.parse()?)?;
    assert!(!tree.contains_key(".intercat-attachments-2/brief.txt"));
    assert_eq!(
        repo.saved_bytes(tree.get(".intercat/attachments/code.txt").unwrap())?,
        b"changed project code"
    );
    assert!(tree.contains_key(".intercat/attachments/new-code.txt"));
    assert_eq!(
        fs::read_to_string(source.join(".intercat/attachments/code.txt"))?,
        "project code"
    );
    Ok(())
}

#[tokio::test]
async fn unregistered_attachment_names_remain_user_files() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let root = dir.path().canonicalize()?;
    let mut workspace = fixture(&root).await?;
    fs::create_dir_all(workspace.path.join(".intercat/attachments"))?;
    fs::write(
        workspace.path.join(".intercat/attachments/user.txt"),
        "user file",
    )?;
    git(
        &workspace.path,
        &["add", "-f", ".intercat/attachments/user.txt"],
    );
    workspace.attachment_roots =
        intercat_daemon::attachments::reserve(&workspace.path, &workspace.base)?;
    assert_eq!(workspace.attachment_roots, vec![".intercat-attachments-2"]);
    fs::write(
        workspace.path.join(".intercat-attachments-2/copy.txt"),
        "reference",
    )?;
    git(
        &workspace.path,
        &["add", "-f", ".intercat-attachments-2/copy.txt"],
    );
    let journal = Journal::open(
        root.join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let saved = checkpoint::save(
        workspace.clone(),
        uuid::Uuid::now_v7(),
        None,
        false,
        journal,
    )
    .await?;
    let files = intercat_git::Repository::open(&workspace.path)?.saved_files(saved.sha.parse()?)?;
    assert!(files.contains_key(".intercat/attachments/user.txt"));
    assert!(!files.contains_key(".intercat-attachments-2/copy.txt"));
    Ok(())
}
