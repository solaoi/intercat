use intercat_daemon::{
    Result,
    branch::{Branch, InstructionMode, Phase, Request},
    branch_instructions,
    checkpoint::Checkpoint,
    instructions::{Files, Instructions},
    preparation::Workspace,
};
use intercat_git::Repository;
use std::os::unix::fs::PermissionsExt;
use std::{fs, path::Path, process::Command};
use uuid::Uuid;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Dummy",
            "-c",
            "user.email=dummy@example.invalid",
        ])
        .args(args)
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
fn fixture(mode: InstructionMode) -> Result<(tempfile::TempDir, Branch)> {
    let root = tempfile::Builder::new()
        .prefix("ic-branch-instructions-")
        .tempdir_in("/private/tmp")?;
    let source = root.path().join("source");
    fs::create_dir(&source)?;
    git(&source, &["init", "-q", "-b", "main"]);
    fs::create_dir_all(source.join("nested/.agents"))?;
    fs::write(source.join("AGENTS.md"), "Old root rule.\n")?;
    fs::write(source.join("CLAUDE.md"), "Old lower-priority rule.\n")?;
    fs::write(
        source.join("nested/.agents/AGENTS.md"),
        "Old nested rule.\n",
    )?;
    fs::write(source.join("ordinary.txt"), "Keep dummy content.\n")?;
    git(&source, &["add", "-A"]);
    git(&source, &["commit", "-qm", "Generated branch fixture"]);
    let sha = git(&source, &["rev-parse", "HEAD"]);
    let child = root.path().join("child");
    git(
        &source,
        &["worktree", "add", "--detach", child.to_str().unwrap(), &sha],
    );
    let turn = Uuid::now_v7();
    let branch = Branch {
        origin: intercat_daemon::branch_points::Origin {
            ended_at: None,
            timeline_at: None,
            turn_number: 1,
        },
        request: Request {
            op_id: Uuid::now_v7(),
            parent_track_id: "parent".into(),
            point: intercat_daemon::branch_start::Selection::Turn { turn_id: turn },
            instructions: mode,
        },
        track_id: "child".into(),
        parent_session_id: "dummy-parent".into(),
        session_id: None,
        phase: Phase::Preparing,
        instructions: None,
        point: intercat_daemon::branch_start::Point::Turn {
            checkpoint: Checkpoint {
                turn_id: turn,
                sha: sha.clone(),
                reference: "refs/intercat/generated/tracks/A-checkpoint".into(),
                unmerged_paths: vec![],
                live_processes: false,
            },
        },
        workspace: Workspace {
            denied_roots: Vec::new(),
            track_id: "child".into(),
            path: child,
            reference: "refs/intercat/generated/tracks/A2".into(),
            base: sha,
            state: "branching".into(),
            error: None,
            attachment_roots: vec![],
        },
    };
    Ok((root, branch))
}

#[tokio::test]
async fn current_freezes_all_names_and_replays_partial_application_without_changing_head_or_index()
-> Result<()> {
    let (root, mut branch) = fixture(InstructionMode::Current)?;
    let source = root.path().join("source");
    fs::remove_file(source.join("AGENTS.md"))?;
    fs::write(source.join("CLAUDE.md"), "New root rule.\n")?;
    fs::set_permissions(source.join("CLAUDE.md"), fs::Permissions::from_mode(0o755))?;
    fs::remove_file(source.join("nested/.agents/AGENTS.md"))?;
    fs::create_dir_all(source.join("nested/.claude"))?;
    fs::write(
        source.join("nested/.claude/CLAUDE.md"),
        "New nested rule.\n",
    )?;
    fs::write(source.join("nested/CLAUDE.md"), "Selected nested rule.\n")?;
    let frozen_files = Files::capture(&source)?;
    let index = git(
        &branch.workspace.path,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    );
    let before_index = fs::read(&index)?;
    let head = git(&source, &["rev-parse", "HEAD"]);
    let original_index = fs::read(source.join(".git/index"))?;
    let snapshot = branch_instructions::freeze(&branch, &source).await?;
    assert_eq!(snapshot.changed.len(), 5);
    branch.instructions = Some(snapshot.clone());
    fs::write(
        source.join("CLAUDE.md"),
        "Changed after the frozen choice.\n",
    )?;
    assert_eq!(
        branch_instructions::freeze(&branch, &source).await?,
        snapshot
    );
    branch_instructions::apply(&branch).await?;
    assert_eq!(Files::capture(&branch.workspace.path)?, frozen_files);
    assert_ne!(
        fs::metadata(branch.workspace.path.join("CLAUDE.md"))?
            .permissions()
            .mode()
            & 0o111,
        0
    );
    let instructions = Instructions::capture(&branch.workspace.path)?;
    assert_eq!(instructions.rules.len(), 2);
    assert_eq!(instructions.rules[0].text, "New root rule.\n");
    assert_eq!(instructions.rules[1].text, "Selected nested rule.\n");
    // Recreate a crash between two file replacements and before atomic rename.
    fs::write(
        branch.workspace.path.join("CLAUDE.md"),
        "Old lower-priority rule.\n",
    )?;
    fs::set_permissions(
        branch.workspace.path.join("CLAUDE.md"),
        fs::Permissions::from_mode(0o644),
    )?;
    let stage = format!(
        ".intercat-instructions-{}",
        Uuid::new_v5(&branch.request.op_id, b"CLAUDE.md")
    );
    fs::write(
        branch.workspace.path.join(&stage),
        "Incomplete temporary dummy content.",
    )?;
    branch_instructions::apply(&branch).await?;
    assert!(!branch.workspace.path.join(stage).exists());
    assert_eq!(Files::capture(&branch.workspace.path)?, frozen_files);
    assert_eq!(fs::read(&index)?, before_index);
    assert_eq!(
        git(&branch.workspace.path, &["rev-parse", "HEAD"]),
        branch.point.sha()
    );
    assert_eq!(git(&source, &["rev-parse", "HEAD"]), head);
    assert_eq!(fs::read(source.join(".git/index"))?, original_index);
    fs::write(
        branch.workspace.path.join("ordinary.txt"),
        "Unexpected dummy edit.\n",
    )?;
    assert!(branch_instructions::apply(&branch).await.is_err());
    assert_eq!(
        fs::read_to_string(branch.workspace.path.join("ordinary.txt"))?,
        "Unexpected dummy edit.\n"
    );
    Ok(())
}

#[tokio::test]
async fn checkpoint_keeps_the_original_rules_even_after_current_files_are_removed() -> Result<()> {
    let (root, mut branch) = fixture(InstructionMode::Checkpoint)?;
    let source = root.path().join("source");
    let original = Files::capture(&source)?;
    for path in original.0.keys() {
        fs::remove_file(source.join(path))?;
    }
    branch.instructions = Some(branch_instructions::freeze(&branch, &source).await?);
    assert!(branch.instructions.as_ref().unwrap().changed.is_empty());
    branch_instructions::apply(&branch).await?;
    assert_eq!(Files::capture(&branch.workspace.path)?, original);
    assert!(git(&branch.workspace.path, &["status", "--porcelain"]).is_empty());
    Ok(())
}

#[tokio::test]
async fn an_empty_current_set_removes_old_rules_and_remains_the_initial_instruction_snapshot()
-> Result<()> {
    let (root, mut branch) = fixture(InstructionMode::Current)?;
    let source = root.path().join("source");
    for path in Files::capture(&source)?.0.keys() {
        fs::remove_file(source.join(path))?;
    }
    branch.instructions = Some(branch_instructions::freeze(&branch, &source).await?);
    branch_instructions::apply(&branch).await?;
    assert!(Files::capture(&branch.workspace.path)?.0.is_empty());
    // Initial dispatch uses the saved choice, even if an external edit happens
    // after activation. A later ordinary turn captures the worker's files.
    fs::write(
        branch.workspace.path.join("AGENTS.md"),
        "A later dummy rule.\n",
    )?;
    let initial = branch_instructions::initial(
        &branch.workspace.path,
        branch.instructions.as_ref().unwrap(),
    )?;
    assert!(initial.rules.is_empty());
    assert!(
        initial
            .with_request("Dummy work")?
            .contains("\"rules\": []")
    );
    assert_eq!(
        Instructions::capture(&branch.workspace.path)?.rules.len(),
        1
    );
    assert!(branch_instructions::verify(&branch).await.is_err());
    Ok(())
}

#[tokio::test]
async fn instruction_directory_symlinks_cannot_redirect_application() -> Result<()> {
    let (root, mut branch) = fixture(InstructionMode::Current)?;
    let source = root.path().join("source");
    fs::create_dir_all(source.join(".claude"))?;
    fs::write(source.join(".claude/CLAUDE.md"), "New dummy rule.\n")?;
    branch.instructions = Some(branch_instructions::freeze(&branch, &source).await?);
    let outside = root.path().join("generated-outside");
    fs::create_dir(&outside)?;
    fs::write(outside.join("CLAUDE.md"), "Keep outside dummy.\n")?;
    std::os::unix::fs::symlink(&outside, branch.workspace.path.join(".claude"))?;
    assert!(branch_instructions::apply(&branch).await.is_err());
    assert_eq!(
        fs::read_to_string(outside.join("CLAUDE.md"))?,
        "Keep outside dummy.\n"
    );
    assert!(
        Repository::open(&source)?
            .reference_target(&branch.instructions.as_ref().unwrap().reference)?
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn checkpoint_instructions_preserve_the_checkout_line_endings() -> Result<()> {
    let (root, mut branch) = fixture(InstructionMode::Checkpoint)?;
    git(&branch.workspace.path, &["config", "core.autocrlf", "true"]);
    fs::remove_file(branch.workspace.path.join("AGENTS.md"))?;
    git(&branch.workspace.path, &["checkout", "--", "AGENTS.md"]);
    assert_eq!(
        fs::read(branch.workspace.path.join("AGENTS.md"))?,
        b"Old root rule.\r\n"
    );
    branch.instructions =
        Some(branch_instructions::freeze(&branch, &root.path().join("source")).await?);
    branch_instructions::apply(&branch).await?;
    assert!(branch.instructions.as_ref().unwrap().changed.is_empty());
    assert!(git(&branch.workspace.path, &["status", "--porcelain"]).is_empty());
    assert_eq!(
        branch_instructions::initial(
            &branch.workspace.path,
            branch.instructions.as_ref().unwrap()
        )?
        .rules[0]
            .text,
        "Old root rule.\r\n"
    );
    Ok(())
}

#[tokio::test]
async fn staging_the_instruction_update_before_activation_is_rejected() -> Result<()> {
    let (root, mut branch) = fixture(InstructionMode::Current)?;
    let source = root.path().join("source");
    fs::write(source.join("AGENTS.md"), "New root rule.\n")?;
    branch.instructions = Some(branch_instructions::freeze(&branch, &source).await?);
    branch_instructions::apply(&branch).await?;
    git(&branch.workspace.path, &["add", "AGENTS.md"]);
    assert!(branch_instructions::verify(&branch).await.is_err());
    Ok(())
}
