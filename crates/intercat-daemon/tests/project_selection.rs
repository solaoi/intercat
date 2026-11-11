use intercat_daemon::{
    Result,
    project_selection::{self, Issue},
};
use std::{fs, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Dummy")
        .env("GIT_COMMITTER_NAME", "Dummy")
        .env("GIT_AUTHOR_EMAIL", "dummy@example.invalid")
        .env("GIT_COMMITTER_EMAIL", "dummy@example.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn repository(root: &Path) -> Result<()> {
    fs::create_dir(root)?;
    git(root, &["init", "-q", "-b", "main"]);
    fs::create_dir(root.join("src"))?;
    fs::write(root.join("src/dummy.txt"), "agent-created dummy\n")?;
    fs::write(
        root.join(".gitignore"),
        "ignored/\nblocked/*\n!blocked/keep/\n",
    )?;
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "Dummy selection baseline"]);
    Ok(())
}

#[tokio::test]
async fn duplicate_subfolders_and_linked_worktrees_resolve_without_executing_filters() -> Result<()>
{
    let tmp = tempfile::tempdir()?;
    let main = tmp.path().join("main");
    let other = tmp.path().join("other");
    repository(&main)?;
    repository(&other)?;
    let linked = tmp.path().join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "dummy-linked",
            linked.to_str().unwrap(),
        ],
    );
    let main = main.canonicalize()?;
    let other = other.canonicalize()?;
    let linked = linked.canonicalize()?;
    // Selection must not run a configured command or alter the index/HEAD.
    git(
        &main,
        &[
            "config",
            "filter.dummy.clean",
            "touch selected-filter-must-not-run",
        ],
    );
    fs::write(main.join(".gitattributes"), "*.txt filter=dummy\n")?;
    let index = fs::read(main.join(".git/index"))?;
    let head = fs::read(main.join(".git/HEAD"))?;
    for duplicate in [main.join("src"), linked] {
        let paths = vec![main.clone(), duplicate, other.clone()];
        let Some(Issue::Duplicate { roots }) = project_selection::inspect(paths.clone()).await?
        else {
            panic!("Missing duplicate-root choice")
        };
        assert_eq!(roots, vec![main.clone(), other.clone()]);
        assert!(
            intercat_daemon::project::inspect(uuid::Uuid::now_v7(), "Dummy".into(), paths)
                .await
                .unwrap_err()
                .is::<Issue>()
        );
    }
    let Some(Issue::RepositoryRoot { root, roots }) =
        project_selection::inspect(vec![main.join("src")]).await?
    else {
        panic!("Missing explicit root choice")
    };
    assert_eq!(root, main);
    assert_eq!(roots, vec![main.clone()]);
    assert!(
        project_selection::inspect(vec![main.clone(), other])
            .await?
            .is_none()
    );
    assert!(!main.join("selected-filter-must-not-run").exists());
    assert_eq!(fs::read(main.join(".git/index"))?, index);
    assert_eq!(fs::read(main.join(".git/HEAD"))?, head);
    Ok(())
}

#[tokio::test]
async fn ignored_subfolders_are_rejected_before_initialization_and_respect_repository_rules()
-> Result<()> {
    let tmp = tempfile::tempdir()?;
    let main = tmp.path().join("main");
    repository(&main)?;
    let main = main.canonicalize()?;
    fs::create_dir_all(main.join("ignored/child"))?;
    fs::create_dir_all(main.join("blocked/keep"))?;
    fs::create_dir_all(main.join("info-ignored/child"))?;
    fs::write(main.join(".git/info/exclude"), "info-ignored/\n")?;
    let index = fs::read(main.join(".git/index"))?;
    let head = git(&main, &["rev-parse", "HEAD"]);
    for path in ["ignored", "ignored/child", "info-ignored/child"] {
        let path = main.join(path);
        let Some(Issue::Ignored { path: actual, root }) =
            project_selection::inspect(vec![path.clone()]).await?
        else {
            panic!("Ignored folder was accepted: {path:?}")
        };
        assert_eq!(actual, path);
        assert_eq!(root, main);
        assert!(
            intercat_daemon::project::inspect(
                uuid::Uuid::now_v7(),
                "Dummy".into(),
                vec![path.clone()]
            )
            .await
            .unwrap_err()
            .is::<Issue>()
        );
        assert!(!path.join(".git").exists());
    }
    assert!(matches!(
        project_selection::inspect(vec![main.join("blocked/keep")]).await?,
        Some(Issue::RepositoryRoot { .. })
    ));
    assert_eq!(fs::read(main.join(".git/index"))?, index);
    assert_eq!(git(&main, &["rev-parse", "HEAD"]), head);
    Ok(())
}

#[tokio::test]
async fn submodule_files_and_gitlinks_produce_a_cancel_only_issue() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("main");
    repository(&root)?;
    let root = root.canonicalize()?;
    fs::write(
        root.join(".gitmodules"),
        "agent-created dummy, never parsed\n",
    )?;
    assert!(matches!(
        project_selection::inspect(vec![root.clone()]).await?,
        Some(Issue::Submodules { .. })
    ));
    fs::remove_file(root.join(".gitmodules"))?;
    let head = git(&root, &["rev-parse", "HEAD"]);
    git(
        &root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},dummy-submodule"),
        ],
    );
    assert!(matches!(
        project_selection::inspect(vec![root.clone()]).await?,
        Some(Issue::Submodules { .. })
    ));
    git(&root, &["commit", "-qm", "Dummy gitlink"]);
    git(
        &root,
        &["update-index", "--force-remove", "dummy-submodule"],
    );
    assert!(matches!(
        project_selection::inspect(vec![root]).await?,
        Some(Issue::Submodules { .. })
    ));
    Ok(())
}
