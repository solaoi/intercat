use intercat_daemon::{Result, instructions::Instructions, worktree};
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
    worktree::prepare_detached(&source, &worktree).await?;
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
async fn rejects_submodules_before_checkout() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let repo = temporary.path().join("submodule");
    source(&repo);
    fs::write(
        repo.join(".gitmodules"),
        "[submodule \"example\"]\npath = sub\n",
    )?;
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "unsupported",
        ],
    );
    assert!(worktree::inspect(&repo).await.is_err());
    Ok(())
}

#[tokio::test]
async fn filter_conversion_is_materialized_from_isolated_objects_without_changing_source()
-> Result<()> {
    let temporary = tempfile::tempdir()?;
    let repo = temporary.path().join("filtered-source");
    source(&repo);
    fs::write(repo.join(".gitattributes"), "*.txt filter=case\n")?;
    git(&repo, &["config", "filter.case.clean", "tr A-Z a-z"]);
    git(&repo, &["config", "filter.case.smudge", "tr a-z A-Z"]);
    git(&repo, &["config", "filter.case.required", "true"]);
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "filtered",
        ],
    );
    let index = fs::read(repo.join(".git/index"))?;
    let source = intercat_daemon::source_snapshot::clean_project(&repo).await?;
    let worker = temporary.path().join("worker");
    worktree::prepare_detached(&source, &worker).await?;
    assert_eq!(fs::read(worker.join("file.txt"))?, b"COMMITTED\n");
    assert_eq!(fs::read(repo.join("file.txt"))?, b"committed\n");
    assert_eq!(fs::read(repo.join(".git/index"))?, index);
    intercat_daemon::source_snapshot::clean_project(&worker).await?;
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

#[tokio::test]
async fn lfs_project_starts_from_its_local_payload_and_saves_a_new_worker_payload() -> Result<()> {
    use intercat_daemon::{checkpoint, journal::Journal, preparation::Workspace};
    use intercat_git::Repository;
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    let repo = root.join("source");
    source(&repo);
    let tool = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.tools/git-lfs-3.8.1/git-lfs")
        .canonicalize()?;
    for (field, command) in [
        ("clean", "clean -- %f"),
        ("smudge", "smudge -- %f"),
        ("process", "filter-process"),
    ] {
        git(
            &repo,
            &[
                "config",
                &format!("filter.lfs.{field}"),
                &format!("'{}' {command}", tool.display()),
            ],
        );
    }
    git(&repo, &["config", "filter.lfs.required", "true"]);
    git(&repo, &["config", "lfs.storage", "../../lfs-storage"]);
    fs::write(repo.join(".gitattributes"), "*.lfs filter=lfs -text\n")?;
    fs::write(
        repo.join(".lfsconfig"),
        "[lfs]\nurl = https://unused.example.invalid/dummy\n",
    )?;
    fs::write(repo.join("asset.lfs"), b"agent-created original LFS text\n")?;
    git(&repo, &["add", "."]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "dummy LFS base",
        ],
    );
    let index = fs::read(repo.join(".git/index"))?;
    let source = intercat_daemon::source_snapshot::clean_project(&repo).await?;
    let worker = root.join("worker");
    worktree::prepare_detached(&source, &worker).await?;
    assert_eq!(
        fs::read(worker.join("asset.lfs"))?,
        b"agent-created original LFS text\n"
    );
    let changed = b"agent-created changed LFS text\n";
    fs::write(worker.join("asset.lfs"), changed)?;
    let workspace = Workspace {
        denied_roots: Vec::new(),
        attachment_roots: Vec::new(),
        track_id: "dummy-track".into(),
        path: worker.clone(),
        reference: "refs/intercat/dummy/tracks/A".into(),
        base: source.head.to_string(),
        state: "ready".into(),
        error: None,
    };
    let journal = Journal::open(
        root.join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let saved =
        checkpoint::capture(&workspace, &workspace.base, "dummy checkpoint", &journal).await?;
    let repo_api = Repository::open(&repo)?;
    let entries = repo_api.lfs_entries(repo_api.tree_id(saved.sha.parse()?)?)?;
    assert_eq!(entries.len(), 1);
    let mut body = Vec::new();
    use std::io::Read;
    intercat_daemon::lfs_store::read(&saved.lfs_directory, &entries[0])?
        .unwrap()
        .read_to_end(&mut body)?;
    assert_eq!(body, changed);
    assert_eq!(fs::read(repo.join(".git/index"))?, index);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), source.head.to_string());
    assert_eq!(
        fs::read(repo.join("asset.lfs"))?,
        b"agent-created original LFS text\n"
    );
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

#[test]
fn removing_the_last_instruction_explicitly_replaces_the_inherited_snapshot() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path();
    fs::write(root.join("AGENTS.md"), "Use the old dummy rule.\n")?;
    let old = Instructions::capture(root)?;
    fs::remove_file(root.join("AGENTS.md"))?;
    let current = Instructions::capture(root)?;
    assert_ne!(current.digest, old.digest);
    let request = current.with_request("Keep the user's dummy requirement.")?;
    assert!(request.contains("It replaces previous project instructions"));
    assert!(request.contains("\"rules\": []"));
    assert!(!request.contains("Use the old dummy rule."));
    assert!(request.ends_with("User request:\nKeep the user's dummy requirement."));
    Ok(())
}

#[test]
fn refuses_instruction_files_through_symlinked_configuration_directories() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let outside = temporary.path().join("generated-outside");
    fs::create_dir(&outside)?;
    fs::write(outside.join("AGENTS.md"), "Dummy outside rule.\n")?;
    fs::write(outside.join("CLAUDE.md"), "Dummy outside rule.\n")?;
    for name in [".agents", ".claude"] {
        let root = temporary.path().join(name.trim_start_matches('.'));
        fs::create_dir(&root)?;
        std::os::unix::fs::symlink(&outside, root.join(name))?;
        assert!(Instructions::capture(&root).is_err());
    }
    Ok(())
}
