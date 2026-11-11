use intercat_daemon::{Result, source_snapshot::clean_project};
use intercat_git::Repository;
use std::{fs, path::Path, process::Command};
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
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    git(root.path(), &["init", "-q", "-b", "main"]);
    git(root.path(), &["config", "user.name", "Dummy"]);
    git(
        root.path(),
        &["config", "user.email", "dummy@example.invalid"],
    );
    fs::write(root.path().join("file.txt"), "dummy\n").unwrap();
    git(root.path(), &["add", "."]);
    git(root.path(), &["commit", "-qm", "dummy base"]);
    root
}

#[tokio::test]
async fn inspection_normalizes_filters_in_isolation_and_respects_manual_starting_point_flags()
-> Result<()> {
    let dir = fixture();
    let path = dir.path();
    let index = fs::read(path.join(".git/index"))?;
    fs::write(path.join(".git/info/attributes"), "file.txt filter=lower\n")?;
    git(path, &["config", "filter.lower.clean", "tr A-Z a-z"]);
    git(path, &["config", "filter.lower.smudge", "tr a-z A-Z"]);
    git(path, &["config", "filter.lower.required", "true"]);
    fs::write(path.join("file.txt"), b"DUMMY\n")?;
    clean_project(path).await?;
    assert_eq!(fs::read(path.join(".git/index"))?, index);
    fs::write(path.join("file.txt"), b"DIFFERENT\n")?;
    assert!(clean_project(path).await.is_err());
    for flag in ["--assume-unchanged", "--skip-worktree"] {
        git(path, &["update-index", flag, "file.txt"]);
        let marked = fs::read(path.join(".git/index"))?;
        clean_project(path).await?;
        assert_eq!(fs::read(path.join(".git/index"))?, marked);
        git(
            path,
            &[
                "update-index",
                "--no-assume-unchanged",
                "--no-skip-worktree",
                "file.txt",
            ],
        );
    }
    Ok(())
}
#[tokio::test]
async fn project_inspection_rejects_working_staged_and_hidden_untracked_changes_without_writing_index()
-> Result<()> {
    let dir = fixture();
    let before = fs::read(dir.path().join(".git/index"))?;
    let head = git(dir.path(), &["rev-parse", "HEAD"]);
    assert_eq!(clean_project(dir.path()).await?.head.to_string(), head);
    assert_eq!(fs::read(dir.path().join(".git/index"))?, before);
    fs::write(dir.path().join("file.txt"), "unfinished\n")?;
    assert!(clean_project(dir.path()).await.is_err());
    assert_eq!(fs::read(dir.path().join(".git/index"))?, before);
    git(dir.path(), &["add", "file.txt"]);
    let staged = fs::read(dir.path().join(".git/index"))?;
    assert!(clean_project(dir.path()).await.is_err());
    assert_eq!(fs::read(dir.path().join(".git/index"))?, staged);
    git(dir.path(), &["reset", "--hard", "HEAD"]);
    git(dir.path(), &["config", "status.showUntrackedFiles", "no"]);
    fs::write(dir.path().join("untracked.txt"), "not committed\n")?;
    assert!(clean_project(dir.path()).await.is_err());
    fs::write(dir.path().join(".git/info/exclude"), "untracked.txt\n")?;
    assert_eq!(clean_project(dir.path()).await?.head.to_string(), head);
    assert_eq!(git(dir.path(), &["rev-parse", "HEAD"]), head);
    Ok(())
}

#[tokio::test]
async fn empty_unborn_project_has_a_stable_parentless_base_without_moving_head_or_index()
-> Result<()> {
    let dir = tempfile::tempdir()?;
    git(dir.path(), &["init", "-q", "-b", "main"]);
    let head = fs::read(dir.path().join(".git/HEAD"))?;
    let source = clean_project(dir.path()).await?;
    let repo = Repository::open(dir.path())?;
    let details = repo.commit_details(source.head)?;
    assert!(details.parents.is_empty());
    assert!(repo.tree_files(details.tree)?.is_empty());
    assert!(repo.tracked_paths()?.is_empty());
    assert_eq!(clean_project(dir.path()).await?.head, source.head);
    assert_eq!(repo.base_branch()?, None);
    assert_eq!(repo.reference_target("refs/heads/main")?, None);
    assert_eq!(fs::read(dir.path().join(".git/HEAD"))?, head);
    assert!(!dir.path().join(".git/index").exists());
    fs::write(dir.path().join("unsaved.txt"), "keep this work")?;
    assert!(clean_project(dir.path()).await.is_err());
    assert_eq!(
        fs::read_to_string(dir.path().join("unsaved.txt"))?,
        "keep this work"
    );
    assert!(!dir.path().join(".git/index").exists());
    Ok(())
}
