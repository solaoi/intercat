use intercat_daemon::{Result, deletion, journal::Journal, preparation::Workspace, worktree};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};
fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("/usr/bin/git")
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
async fn fixture(root: &Path) -> Result<(Workspace, Journal)> {
    let source = root.join("source");
    fs::create_dir(&source)?;
    git(&source, &["init", "-q", "-b", "main"]);
    fs::write(source.join("keep"), "base\n")?;
    fs::write(source.join(".gitignore"), "ignored*\n.env\n")?;
    git(&source, &["add", "."]);
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
    worktree::prepare_detached(&worktree::inspect(&source).await?, &path).await?;
    Ok((
        Workspace {
            denied_roots: Vec::new(),
            attachment_roots: Vec::new(),
            track_id: "track".into(),
            path,
            reference: "refs/intercat/test/tracks/A".into(),
            base,
            state: "ready".into(),
            error: None,
        },
        Journal::open(root.join("db"), intercat_daemon::secrets::Key::new([7; 32])).await?,
    ))
}
fn request(argv: &[&str]) -> Value {
    json!({"sessionId":"session","approvalId":"approval","itemId":"item","currentRequirementId":{"sourceIndex":0},"subject":{"kind":"shell","command":argv.join(" "),"stages":[{"argv":argv,"argvComplete":true}]}})
}
async fn snapshot(w: &Workspace, j: &Journal, args: &[&str]) -> Result<deletion::Snapshot> {
    deletion::prepare(w.clone(), None, request(args), w.base.clone(), j.clone()).await
}

#[tokio::test]
async fn before_delete_covers_same_turn_files_and_restore_is_undoable_without_touching_index()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let (w, j) = fixture(&root).await?;
    fs::write(w.path.join("keep"), "same-turn edit\n")?;
    fs::write(w.path.join("new"), "same-turn file\n")?;
    fs::create_dir(w.path.join("folder"))?;
    fs::write(w.path.join("folder/run"), "executable\n")?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(w.path.join("folder/run"), fs::Permissions::from_mode(0o755))?;
    std::os::unix::fs::symlink("keep", w.path.join("link"))?;
    fs::write(w.path.join("ignored-cache"), "not deleted\n")?;
    let index_path = git(&w.path, &["rev-parse", "--git-path", "index"]);
    let index = fs::read(&index_path)?;
    let saved = snapshot(&w, &j, &["rm", "-rf", "keep", "new", "folder", "link"]).await?;
    assert!(saved.complete, "{:?}", saved.uncovered);
    assert_eq!(saved.paths.len(), 4);
    assert_eq!(git(&w.path, &["rev-parse", &saved.reference]), saved.sha);
    assert_eq!(j.deletion(saved.id).await?.sha, saved.sha);
    assert!(
        intercat_git::Repository::open(&w.path)?
            .reference_target(&format!("{}-checkpoint", w.reference))?
            .is_none()
    );
    for name in ["keep", "new", "link"] {
        fs::remove_file(w.path.join(name))?;
    }
    fs::remove_dir_all(w.path.join("folder"))?;
    let op = uuid::Uuid::now_v7();
    let restored = deletion::restore(w.clone(), None, saved.clone(), op, j.clone()).await?;
    assert_eq!(fs::read_to_string(w.path.join("keep"))?, "same-turn edit\n");
    assert_eq!(fs::read_to_string(w.path.join("new"))?, "same-turn file\n");
    assert_ne!(
        fs::metadata(w.path.join("folder/run"))?
            .permissions()
            .mode()
            & 0o111,
        0
    );
    assert_eq!(fs::read_link(w.path.join("link"))?, Path::new("keep"));
    assert_eq!(fs::read_to_string(root.join("source/keep"))?, "base\n");
    assert_eq!(fs::read(&index_path)?, index);
    fs::write(w.path.join("new"), "later user edit\n")?;
    deletion::restore(w.clone(), None, saved.clone(), op, j.clone()).await?;
    assert_eq!(
        fs::read_to_string(w.path.join("new"))?,
        "later user edit\n",
        "replayed operation must not write again"
    );
    let undo = j.deletion(restored.backup_id).await?;
    assert_eq!(undo.absent.len(), 4);
    deletion::restore(w.clone(), None, undo, uuid::Uuid::now_v7(), j.clone()).await?;
    assert!(!w.path.join("keep").exists());
    assert!(!w.path.join("new").exists());
    assert!(fs::symlink_metadata(w.path.join("link")).is_err());
    assert_eq!(
        fs::read_to_string(w.path.join("ignored-cache"))?,
        "not deleted\n"
    );
    assert_eq!(fs::read(&index_path)?, index);
    Ok(())
}

#[tokio::test]
async fn ignored_external_and_symlink_parent_targets_are_not_automatically_recoverable()
-> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let (w, j) = fixture(&root).await?;
    fs::write(w.path.join("ignored-secret"), "dummy ignored\n")?;
    fs::write(root.join("outside"), "outside unchanged\n")?;
    std::os::unix::fs::symlink(&root, w.path.join("escape"))?;
    for args in [
        vec!["rm", "ignored-secret"],
        vec!["rm", "../outside"],
        vec!["rm", "escape/outside"],
        vec!["rm", "escape/"],
        vec!["rm", "escape/../outside"],
        vec!["rm", "*.txt"],
        vec!["git", "stash", "drop"],
    ] {
        let saved = snapshot(&w, &j, &args).await?;
        assert!(!saved.complete, "{args:?}");
        assert!(!saved.uncovered.is_empty());
    }
    let saved = snapshot(&w, &j, &["rm", "escape"]).await?;
    assert!(saved.complete, "removing the symlink itself is recoverable");
    assert_eq!(
        fs::read_to_string(root.join("outside"))?,
        "outside unchanged\n"
    );
    Ok(())
}

#[tokio::test]
async fn git_discard_and_clean_account_for_tracked_and_ignored_files() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let (w, j) = fixture(&root).await?;
    fs::write(w.path.join("keep"), "changed\n")?;
    fs::write(w.path.join("untracked"), "new\n")?;
    fs::write(w.path.join("ignored-cache"), "ignored\n")?;
    fs::write(w.path.join("ignored-forced"), "forced\n")?;
    git(&w.path, &["add", "-f", "ignored-forced"]);
    for args in [
        vec!["git", "restore", "keep"],
        vec!["git", "checkout", "--", "keep"],
        vec!["git", "reset", "--hard"],
        vec!["git", "clean", "-fd"],
    ] {
        let saved = snapshot(&w, &j, &args).await?;
        assert!(saved.complete, "{args:?}: {:?}", saved.uncovered);
    }
    for args in [vec!["git", "clean", "-fdx"], vec!["git", "clean", "-fdX"]] {
        let saved = snapshot(&w, &j, &args).await?;
        assert!(!saved.complete);
        assert!(saved.uncovered.iter().any(|s| s.contains("ignored-cache")));
    }
    let saved = snapshot(&w, &j, &["git", "restore", "keep"]).await?;
    git(&w.path, &["restore", "keep"]);
    assert_eq!(fs::read_to_string(w.path.join("keep"))?, "base\n");
    deletion::restore(w.clone(), None, saved, uuid::Uuid::now_v7(), j.clone()).await?;
    assert_eq!(fs::read_to_string(w.path.join("keep"))?, "changed\n");
    Ok(())
}

#[tokio::test]
async fn restore_refuses_symlink_parents_and_directory_conflicts_before_writing() -> Result<()> {
    let root = tempfile::tempdir()?;
    let root = root.path().canonicalize()?;
    let (w, j) = fixture(&root).await?;
    fs::create_dir(w.path.join("folder"))?;
    fs::write(w.path.join("folder/file"), "saved\n")?;
    let saved = snapshot(&w, &j, &["rm", "-rf", "folder"]).await?;
    fs::remove_dir_all(w.path.join("folder"))?;
    fs::create_dir(root.join("outside"))?;
    fs::write(root.join("outside/file"), "outside\n")?;
    std::os::unix::fs::symlink(root.join("outside"), w.path.join("folder"))?;
    assert!(
        deletion::restore(
            w.clone(),
            None,
            saved.clone(),
            uuid::Uuid::now_v7(),
            j.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(fs::read_to_string(root.join("outside/file"))?, "outside\n");
    fs::remove_file(w.path.join("folder"))?;
    fs::create_dir_all(w.path.join("folder/file"))?;
    assert!(
        deletion::restore(w.clone(), None, saved, uuid::Uuid::now_v7(), j)
            .await
            .is_err()
    );
    Ok(())
}
