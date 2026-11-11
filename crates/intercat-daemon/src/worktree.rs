//! Worktree orchestration; Git CLI runs only within a per-job macOS sandbox.
use crate::Result;
use crate::journal::{Admission, Journal};
use intercat_git::{Repository, WorktreeSource};
use serde_json::json;
use std::{ffi::OsStr, path::Path, process::Stdio};
use tokio::process::Command;

fn quote(value: &OsStr) -> Result<String> {
    let value = value
        .to_str()
        .ok_or_else(|| std::io::Error::other("sandbox path is not UTF-8"))?;
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn profile(
    tool: &crate::git_tool::GitTool,
    source: &WorktreeSource,
    destination: &Path,
    temporary: &Path,
) -> Result<String> {
    Ok(format!(
        "(version 1)(deny default)(allow process-fork)(allow process-info*)(allow sysctl-read)(allow mach-lookup)(allow signal (target self))(allow file-read*)(allow process-exec (literal {}) (subpath {}))(allow file-write* (subpath {}) (subpath {}) (subpath {}) (literal \"/dev/null\"))",
        quote(tool.binary().as_os_str())?,
        quote(tool.exec_dir().as_os_str())?,
        quote(source.common_dir.as_os_str())?,
        quote(destination.as_os_str())?,
        quote(temporary.as_os_str())?
    ))
}
async fn run(
    tool: &crate::git_tool::GitTool,
    source: &WorktreeSource,
    destination: &Path,
    arguments: &[&OsStr],
) -> Result<()> {
    let temporary = tempfile::Builder::new().prefix("intercat-git-").tempdir()?;
    let profile = profile(tool, source, destination, temporary.path())?;
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command
        .args(["-p", &profile])
        .arg(tool.binary())
        .args([
            "-c",
            "core.fsync=objects,reference",
            "-c",
            "core.fsyncMethod=fsync",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(arguments)
        .current_dir(&source.root)
        .env_clear()
        .env(
            "HOME",
            std::env::var_os("HOME").ok_or_else(|| std::io::Error::other("HOME missing"))?,
        )
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("TMPDIR", temporary.path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output =
        tokio::time::timeout(std::time::Duration::from_secs(30), command.output()).await??;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "sandboxed Git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }
    Ok(())
}

pub async fn prepare_detached(source: &WorktreeSource, destination: &Path) -> Result<()> {
    let tool = crate::git_tool::GitTool::system().await?;
    let parent = destination
        .parent()
        .ok_or_else(|| std::io::Error::other("worktree needs a parent directory"))?;
    std::fs::create_dir_all(parent)?;
    let destination = parent.canonicalize()?.join(
        destination
            .file_name()
            .ok_or_else(|| std::io::Error::other("worktree needs a name"))?,
    );
    let head = source.head.to_string();
    if destination.exists() {
        Repository::verify_prepared_worktree(&destination, source)?;
    } else {
        run(
            &tool,
            source,
            &destination,
            &[
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--detach"),
                OsStr::new("--no-checkout"),
                OsStr::new("--"),
                destination.as_os_str(),
                OsStr::new(&head),
            ],
        )
        .await?;
    }
    let temporary = tempfile::Builder::new()
        .prefix("intercat-worker-checkout-")
        .tempdir()?;
    let path = std::env::var("PATH")?;
    let checkout = crate::checkout::Checkout::create(
        &temporary.path().join("job"),
        source.clone(),
        source.head,
        crate::checkout::Configuration::capture(source)?,
        path.clone().into(),
        crate::checkout::toolchain_paths(&path, &source.root)?,
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    checkout
        .materialize_into(&destination, tokio::sync::watch::channel(false).1)
        .await?;
    Ok(())
}

pub async fn inspect(source: &Path) -> Result<WorktreeSource> {
    let path = source.to_path_buf();
    tokio::task::spawn_blocking(move || Repository::worktree_source(&path, None)).await?
}

pub async fn prepare(
    source: &WorktreeSource,
    destination: &Path,
    track_ref: &str,
    operation: uuid::Uuid,
    journal: &Journal,
) -> Result<()> {
    let root = source.root.clone();
    let head = source.head;
    tokio::task::spawn_blocking(move || Repository::open(root)?.sync_objects(&[head])).await??;
    let id = operation.to_string();
    let intent = json!({"source":source.root,"path":destination,"head":source.head.to_string(),"ref":track_ref});
    let state = journal.intent(&id, "worktree.prepare", intent).await?;
    if state == Admission::Replay("superseded".into()) {
        return Err(std::io::Error::other("Worktree preparation was superseded.").into());
    }
    prepare_detached(source, destination).await?;
    let root = source.root.clone();
    let reference = track_ref.to_owned();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(root)?;
        repo.compare_and_swap(&reference, None, head)?;
        repo.sync_ref(&reference)?;
        Ok(())
    })
    .await??;
    match state {
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
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    #[test]
    fn worktree_profile_cannot_connect_to_the_private_rpc_socket() -> Result<()> {
        let probe = crate::sandbox_probe::Probe::new()?;
        let tool = crate::git_tool::GitTool {
            binary: probe.program.clone(),
            exec_dir: probe.exec_dir.clone(),
        };
        let source = probe.source();
        probe.check(
            "worktree",
            &profile(
                &tool,
                &source,
                &probe.path("job/repo"),
                &probe.path("job/tmp"),
            )?,
        )
    }
}
