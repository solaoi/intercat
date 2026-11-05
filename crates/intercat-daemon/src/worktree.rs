//! Worktree orchestration; Git CLI runs only within a per-job macOS sandbox.
use crate::Result;
use crate::journal::{Admission, Journal};
use intercat_git::{Repository, WorktreeSource};
use serde_json::json;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::Command;

pub struct GitTool {
    binary: PathBuf,
    exec_dir: PathBuf,
}

fn quote(value: &OsStr) -> Result<String> {
    let value = value
        .to_str()
        .ok_or_else(|| std::io::Error::other("sandbox path is not UTF-8"))?;
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

impl GitTool {
    pub fn binary(&self) -> &Path {
        &self.binary
    }
    pub fn exec_dir(&self) -> &Path {
        &self.exec_dir
    }
    pub async fn system() -> Result<Self> {
        let output = Command::new("/usr/bin/xcrun")
            .args(["--find", "git"])
            .output()
            .await?;
        if !output.status.success() {
            return Err(std::io::Error::other("Xcode Git is unavailable").into());
        }
        Self::new(PathBuf::from(String::from_utf8(output.stdout)?.trim())).await
    }
    pub async fn new(binary: PathBuf) -> Result<Self> {
        let binary = binary.canonicalize()?;
        let output = Command::new(&binary)
            .arg("--exec-path")
            .env_remove("GIT_EXEC_PATH")
            .output()
            .await?;
        if !output.status.success() {
            return Err(std::io::Error::other("cannot find Git helper directory").into());
        }
        let exec_dir = PathBuf::from(String::from_utf8(output.stdout)?.trim()).canonicalize()?;
        Ok(Self { binary, exec_dir })
    }
    fn profile(
        &self,
        source: &WorktreeSource,
        destination: &Path,
        temporary: &Path,
    ) -> Result<String> {
        Ok(format!(
            "(version 1)(deny default)(allow process-fork)(allow process-info*)(allow sysctl-read)(allow mach-lookup)(allow signal (target self))(allow file-read*)(allow process-exec (literal {}) (subpath {}))(allow file-write* (subpath {}) (subpath {}) (subpath {}) (literal \"/dev/null\"))",
            quote(self.binary.as_os_str())?,
            quote(self.exec_dir.as_os_str())?,
            quote(source.common_dir.as_os_str())?,
            quote(destination.as_os_str())?,
            quote(temporary.as_os_str())?
        ))
    }
    async fn run(
        &self,
        source: &WorktreeSource,
        destination: &Path,
        arguments: &[&OsStr],
    ) -> Result<()> {
        let temporary = tempfile::Builder::new().prefix("intercat-git-").tempdir()?;
        let profile = self.profile(source, destination, temporary.path())?;
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .args(["-p", &profile])
            .arg(&self.binary)
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

    pub async fn prepare_detached(
        &self,
        source: &WorktreeSource,
        destination: &Path,
    ) -> Result<()> {
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
            self.run(
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
        self.run(
            source,
            &destination,
            &[
                OsStr::new("-C"),
                destination.as_os_str(),
                OsStr::new("read-tree"),
                OsStr::new("--reset"),
                OsStr::new("-u"),
                OsStr::new(&head),
            ],
        )
        .await?;
        Ok(())
    }
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
    GitTool::system()
        .await?
        .prepare_detached(source, destination)
        .await?;
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
        let tool = GitTool {
            binary: probe.program.clone(),
            exec_dir: probe.exec_dir.clone(),
        };
        let source = probe.source();
        probe.check(
            "worktree",
            &tool.profile(&source, &probe.path("job/repo"), &probe.path("job/tmp"))?,
        )
    }
}
