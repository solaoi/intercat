//! Isolated worktree snapshots and durable hidden refs.
use crate::{
    Result,
    journal::{Admission, Journal},
    preparation::Workspace,
};
use intercat_git::{Repository, SnapshotIndex};
use serde::{Deserialize, Serialize};
use std::{ffi::OsStr, path::Path, process::Stdio};
use tokio::process::Command;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub turn_id: Uuid,
    pub sha: String,
    pub reference: String,
    pub unmerged_paths: Vec<String>,
    pub live_processes: bool,
}
fn quote(path: &Path) -> Result<String> {
    Ok(format!(
        "\"{}\"",
        path.to_str()
            .ok_or_else(|| std::io::Error::other("Non-UTF8 sandbox path."))?
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    ))
}
fn profile(
    workspace: &Path,
    temporary: &Path,
    plan: &SnapshotIndex,
    tool: &crate::worktree::GitTool,
) -> Result<String> {
    let mut profile = format!(
        "(version 1)(deny default)(allow process-fork)(allow process-info*)(allow sysctl-read)(allow mach-lookup)(allow signal (target self))(allow process-exec)(allow file-read-metadata)(allow file-read* (literal \"/\") (subpath \"/private/preboot\") (subpath \"/private/var/db/dyld\") (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/Library/Developer\") (subpath \"/dev\") (subpath {}) (subpath {}) (subpath {}) (subpath {}))(allow file-write* (subpath {}) (literal \"/dev/null\"))",
        quote(workspace)?,
        quote(&plan.git_dir)?,
        quote(&plan.common_dir)?,
        quote(temporary)?,
        quote(temporary)?
    );
    profile.push_str(&format!(
        "(allow file-read* (subpath {}))",
        quote(
            tool.binary()
                .parent()
                .and_then(Path::parent)
                .ok_or_else(|| std::io::Error::other("Invalid Git binary path."))?
        )?
    ));
    for objects in &plan.object_dirs {
        profile.push_str(&format!("(deny file-read* (subpath {}))", quote(objects)?));
    }
    Ok(profile)
}
async fn git(
    workspace: &Path,
    temporary: &Path,
    plan: &SnapshotIndex,
    args: &[&OsStr],
) -> Result<String> {
    let tool = crate::worktree::GitTool::system().await?;
    let profile = profile(workspace, temporary, plan, &tool)?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", &profile])
            .arg(tool.binary())
            .args([
                "-c",
                "core.splitIndex=false",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.untrackedCache=false",
                "-c",
                "gc.auto=0",
            ])
            .args(args)
            .current_dir(workspace)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_INDEX_FILE", temporary.join("index"))
            .env("GIT_OBJECT_DIRECTORY", temporary.join("objects"))
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", "")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("TMPDIR", temporary)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "Checkpoint could not be saved ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().into())
}

#[derive(Clone, Debug)]
pub struct Capture {
    pub sha: String,
    pub unmerged_paths: Vec<String>,
}

/// Capture and sync the tree without publishing a ref or changing the real index.
pub async fn capture(workspace: &Workspace, parent: &str, message: &str) -> Result<Capture> {
    // Seatbelt resolves symlinks before checking file access. Use the same
    // physical path in the sandbox profile and in Git's working directory.
    let mut workspace = workspace.clone();
    workspace.path = workspace.path.canonicalize()?;
    let temporary = tempfile::Builder::new()
        .prefix("intercat-checkpoint-")
        .tempdir()?;
    let temp = temporary.path().canonicalize()?;
    let path = workspace.path.clone();
    let base = workspace.base.parse()?;
    let target = temp.clone();
    let attachment_roots = workspace.attachment_roots.clone();
    let plan = tokio::task::spawn_blocking(move || -> Result<_> {
        Repository::init_isolated(&target)?;
        let repo = Repository::open(&path)?;
        repo.snapshot_index(base, &target.join("index"), &attachment_roots)
    })
    .await??;
    git(
        &workspace.path,
        &temp,
        &plan,
        &[OsStr::new("add"), OsStr::new("--sparse"), OsStr::new("-A")],
    )
    .await?;
    let path = workspace.path.clone();
    let index = temp.join("index");
    let attachment_roots = workspace.attachment_roots.clone();
    tokio::task::spawn_blocking(move || {
        Repository::open(path)?.sanitize_snapshot_index(base, &index, &attachment_roots)
    })
    .await??;
    let tree = git(
        &workspace.path,
        &temp,
        &plan,
        &[OsStr::new("write-tree"), OsStr::new("--missing-ok")],
    )
    .await?
    .parse()?;
    let path = workspace.path.clone();
    let temp_path = temp.clone();
    let parent = parent.parse()?;
    let message = message.to_owned();
    let sha = tokio::task::spawn_blocking(move || -> Result<_> {
        let repo = Repository::open(path)?;
        repo.import_tree(&temp_path, tree)?;
        let sha = repo.checkpoint_commit(tree, parent, &message)?;
        repo.sync_objects(&[sha])?;
        Ok(sha)
    })
    .await??;
    Ok(Capture {
        sha: sha.to_string(),
        unmerged_paths: plan.unmerged_paths,
    })
}

pub async fn save(
    workspace: Workspace,
    turn: Uuid,
    previous: Option<Checkpoint>,
    live_processes: bool,
    journal: Journal,
) -> Result<Checkpoint> {
    let id = Uuid::new_v5(&turn, b"checkpoint");
    let latest = format!("{}-checkpoint", workspace.reference);
    let (checkpoint, expected, admission) = if let Some(operation) = journal.operation(id).await? {
        let payload = &operation["payload"];
        let checkpoint: Checkpoint = serde_json::from_value(payload["checkpoint"].clone())?;
        if operation["kind"] != "checkpoint.save"
            || payload["trackId"] != workspace.track_id
            || payload["latestRef"] != latest
            || checkpoint.turn_id != turn
            || checkpoint.reference != format!("{}-checkpoints/{turn}", workspace.reference)
        {
            return Err("The saved checkpoint belongs to a different turn or workspace.".into());
        }
        let expected: Option<String> = serde_json::from_value(payload["expected"].clone())?;
        let state = operation["state"]
            .as_str()
            .filter(|state| matches!(*state, "prepared" | "dispatched" | "complete"))
            .ok_or("The checkpoint operation cannot be resumed.")?;
        (checkpoint, expected, Admission::Replay(state.into()))
    } else {
        let parent = previous
            .as_ref()
            .map(|p| p.sha.as_str())
            .unwrap_or(&workspace.base);
        let captured =
            capture(&workspace, parent, &format!("Intercat checkpoint {turn}\n")).await?;
        let checkpoint = Checkpoint {
            turn_id: turn,
            sha: captured.sha,
            reference: format!("{}-checkpoints/{turn}", workspace.reference),
            unmerged_paths: captured.unmerged_paths,
            live_processes,
        };
        let expected = previous.as_ref().map(|p| p.sha.clone());
        let admission=journal.intent(&id.to_string(),"checkpoint.save",serde_json::json!({"trackId":workspace.track_id,"checkpoint":checkpoint,"latestRef":latest,"expected":expected})).await?;
        (checkpoint, expected, admission)
    };
    // The durable intent freezes the tree, parent and ref targets. A retry must
    // finish that exact publication, even if files or process state have changed.
    let sha: intercat_git::ObjectId = checkpoint.sha.parse()?;
    let expected: Option<intercat_git::ObjectId> = expected.map(|sha| sha.parse()).transpose()?;
    let id = id.to_string();
    let reference = checkpoint.reference.clone();
    let path = workspace.path;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(path)?;
        repo.compare_and_swap(&reference, None, sha)?;
        repo.sync_ref(&reference)?;
        repo.compare_and_swap(&latest, expected, sha)?;
        repo.sync_ref(&latest)?;
        Ok(())
    })
    .await??;
    match admission {
        Admission::New => {
            journal.finish(&id, "dispatched").await?;
            journal.finish(&id, "complete").await?;
        }
        Admission::Replay(state) if state == "prepared" => {
            journal.finish(&id, "dispatched").await?;
            journal.finish(&id, "complete").await?;
        }
        Admission::Replay(state) if state == "dispatched" => {
            journal.finish(&id, "complete").await?
        }
        Admission::Replay(state) if state == "complete" => {}
        _ => return Err(std::io::Error::other("Checkpoint was superseded.").into()),
    }
    Ok(checkpoint)
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    #[tokio::test]
    async fn checkpoint_profile_cannot_connect_to_the_private_rpc_socket() -> Result<()> {
        let probe = crate::sandbox_probe::Probe::new()?;
        let tool = crate::worktree::GitTool::system().await?;
        let plan = SnapshotIndex {
            git_dir: probe.path("source/.git"),
            common_dir: probe.path("source/.git"),
            object_dirs: vec![probe.path("source/.git/objects")],
            unmerged_paths: vec![],
        };
        probe.check(
            "checkpoint",
            &profile(
                &probe.path("job/repo"),
                &probe.path("job/tmp"),
                &plan,
                &tool,
            )?,
        )
    }
}
