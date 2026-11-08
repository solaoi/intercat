use intercat_daemon::{
    Result,
    instructions::Instructions,
    worktree::{self, GitTool},
};
use std::{fs, path::Path, process::Command};

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
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
fn source(path: &Path) {
    fs::create_dir(path).unwrap();
    git(path, &["init", "--quiet", "-b", "main"]);
    fs::write(path.join("file.txt"), "committed\n").unwrap();
    git(path, &["add", "."]);
    git(
        path,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
}

#[tokio::test]
async fn prepared_workspace_uses_the_selected_starting_commit_after_the_user_moves_head()
-> Result<()> {
    let temporary = tempfile::tempdir()?;
    let repo = temporary.path().join("source");
    source(&repo);
    let project = intercat_daemon::project::inspect(
        uuid::Uuid::now_v7(),
        "Dummy project".into(),
        vec![repo.clone()],
    )
    .await?
    .only_folder()?
    .clone();
    fs::write(repo.join("file.txt"), "new user commit\n")?;
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "later user work",
        ],
    );
    let current_head = git(&repo, &["rev-parse", "HEAD"]);
    let current_index = fs::read(repo.join(".git/index"))?;
    assert_ne!(current_head, project.base);
    let frozen = project.source().await?;
    assert_eq!(frozen.head.to_string(), project.base);
    let journal = intercat_daemon::journal::Journal::open(
        temporary.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let workspace = temporary.path().join("worker");
    worktree::prepare(
        &frozen,
        &workspace,
        "refs/intercat/frozen-start/tracks/A",
        uuid::Uuid::now_v7(),
        &journal,
    )
    .await?;
    assert_eq!(
        fs::read_to_string(workspace.join("file.txt"))?,
        "committed\n"
    );
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), current_head);
    assert_eq!(fs::read(repo.join(".git/index"))?, current_index);
    assert_eq!(git(&workspace, &["rev-parse", "HEAD"]), project.base);
    Ok(())
}

#[tokio::test]
async fn detached_checkout_does_not_touch_source_changes_or_execute_hooks() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let repo = temporary.path().join("source");
    source(&repo);
    let marker = temporary.path().join("hook-ran");
    fs::write(
        repo.join(".git/hooks/post-checkout"),
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        repo.join(".git/hooks/post-checkout"),
        fs::Permissions::from_mode(0o755),
    )?;
    fs::write(repo.join("file.txt"), "unfinished user change\n")?;
    fs::write(repo.join("manual.txt"), "untracked user file\n")?;
    let index_before = fs::read(repo.join(".git/index"))?;
    let head_before = git(&repo, &["rev-parse", "HEAD"]);
    let source = worktree::inspect(&repo).await?;
    let worktree = temporary.path().join("worker");
    GitTool::system()
        .await?
        .prepare_detached(&source, &worktree)
        .await?;
    assert_eq!(
        fs::read_to_string(worktree.join("file.txt"))?,
        "committed\n"
    );
    assert_eq!(
        fs::read_to_string(repo.join("file.txt"))?,
        "unfinished user change\n"
    );
    assert_eq!(
        fs::read_to_string(repo.join("manual.txt"))?,
        "untracked user file\n"
    );
    assert_eq!(fs::read(repo.join(".git/index"))?, index_before);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head_before);
    assert!(
        Command::new("git")
            .current_dir(&worktree)
            .args(["symbolic-ref", "-q", "HEAD"])
            .status()?
            .code()
            == Some(1)
    );
    assert!(!marker.exists());
    Ok(())
}

#[tokio::test]
async fn rejects_filters_and_submodules_before_checkout() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    for (name, file, contents) in [
        ("filter", ".gitattributes", "*.txt filter=external\n"),
        (
            "submodule",
            ".gitmodules",
            "[submodule \"example\"]\npath = sub\n",
        ),
    ] {
        let repo = temporary.path().join(name);
        source(&repo);
        fs::write(repo.join(file), contents)?;
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "unsupported",
            ],
        );
        assert!(worktree::inspect(&repo).await.is_err());
    }
    Ok(())
}

#[test]
fn scoped_instructions_preserve_precedence_and_change_digest() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    fs::write(root.join("AGENTS.md"), "Root rule\n")?;
    fs::write(root.join("CLAUDE.md"), "Must not duplicate root rule\n")?;
    fs::create_dir_all(root.join("nested/.agents"))?;
    fs::write(root.join("nested/.agents/AGENTS.md"), "Nested rule\n")?;
    let original = Instructions::capture(root)?;
    assert_eq!(original.rules.len(), 2);
    assert_eq!(original.rules[0].path, Path::new("AGENTS.md"));
    assert_eq!(original.rules[1].scope, Path::new("nested"));
    assert!(
        !original
            .with_request("change file")?
            .contains("Must not duplicate")
    );
    fs::write(
        root.join("nested/.agents/AGENTS.md"),
        "Updated nested rule\n",
    )?;
    assert_ne!(Instructions::capture(root)?.digest, original.digest);
    Ok(())
}

#[test]
fn refuses_instruction_symlink_outside_workspace() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().join("work");
    fs::create_dir(&root)?;
    fs::write(temporary.path().join("private.txt"), "private\n")?;
    std::os::unix::fs::symlink(temporary.path().join("private.txt"), root.join("AGENTS.md"))?;
    assert!(Instructions::capture(&root).is_err());
    Ok(())
}
