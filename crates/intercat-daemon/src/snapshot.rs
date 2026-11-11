//! One isolated Git clean-filter path for checkpoints and checkout inspection.
use crate::Result;
use intercat_git::{ObjectId, Repository, SnapshotIndex};
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::process::Command;
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
    tool: &crate::git_tool::GitTool,
    configuration: &crate::checkout::Configuration,
    toolchain: &[std::path::PathBuf],
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
    for objects in plan
        .object_dirs
        .iter()
        .chain(&configuration.object_directories)
    {
        profile.push_str(&format!("(deny file-read* (subpath {}))", quote(objects)?));
    }
    for path in toolchain {
        profile.push_str(&format!("(allow file-read* (subpath {}))", quote(path)?));
    }
    profile.push_str(&format!(
        "(deny file-read* (subpath {}))",
        quote(&configuration.lfs_directory)?
    ));
    for path in &configuration.denied_roots {
        profile.push_str(&format!("(deny file-read* (subpath {}))", quote(path)?));
    }
    Ok(profile)
}
async fn git(
    workspace: &Path,
    temporary: &Path,
    plan: &SnapshotIndex,
    configuration: &crate::checkout::Configuration,
    path: &str,
    args: &[&OsStr],
) -> Result<String> {
    let tool = crate::git_tool::GitTool::system().await?;
    let toolchain = crate::checkout::toolchain_paths(path, workspace)?;
    let profile = profile(workspace, temporary, plan, &tool, configuration, &toolchain)?;
    let settings = configuration.isolated_values(temporary, &temporary.join("lfs"))?;
    let mut arguments = Vec::new();
    for (key, value) in settings {
        arguments.extend([
            std::ffi::OsString::from("-c"),
            format!("{key}={value}").into(),
        ]);
    }
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
            .args(&arguments)
            .args(args)
            .current_dir(workspace)
            .env_clear()
            .env("PATH", path)
            .env("HOME", temporary.join("home"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_INDEX_FILE", temporary.join("index"))
            .env("GIT_OBJECT_DIRECTORY", temporary.join("objects"))
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", "")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_ATTR_NOSYSTEM", "1")
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

#[derive(Clone)]
pub(crate) enum Purpose {
    Workspace {
        denied_roots: Vec<PathBuf>,
        attachments: Vec<String>,
        exclusions: Vec<String>,
    },
    Inspection,
}
pub(crate) struct Snapshot {
    pub root: PathBuf,
    pub tree: ObjectId,
    pub plan: SnapshotIndex,
    pub configuration: crate::checkout::Configuration,
    _directory: tempfile::TempDir,
}
pub(crate) async fn capture(
    workspace: &Path,
    base: ObjectId,
    purpose: Purpose,
) -> Result<Snapshot> {
    // Seatbelt resolves symlinks before checking file access. Use the same
    // physical path in the sandbox profile and in Git's working directory.
    let workspace = workspace.canonicalize()?;
    let temporary = tempfile::Builder::new()
        .prefix("intercat-checkpoint-")
        .tempdir()?;
    let temp = temporary.path().canonicalize()?;
    let path = workspace.clone();
    let target = temp.clone();
    let purpose = purpose.clone();
    let build_purpose = purpose.clone();
    let plan = tokio::task::spawn_blocking(move || -> Result<_> {
        Repository::init_isolated(&target)?;
        let repo = Repository::open(&path)?;
        match build_purpose {
            Purpose::Workspace {
                attachments,
                exclusions,
                ..
            } => repo.snapshot_index(base, &target.join("index"), &attachments, &exclusions),
            Purpose::Inspection => repo.inspection_index(base, &target.join("index")),
        }
    })
    .await??;
    std::fs::create_dir(temp.join("home"))?;
    crate::lfs_store::Store::create(&temp.join("lfs"))?;
    let mut configuration =
        crate::checkout::Configuration::capture(&intercat_git::WorktreeSource {
            root: workspace.clone(),
            common_dir: plan.common_dir.clone(),
            head: base,
        })?;
    if let Purpose::Workspace { denied_roots, .. } = &purpose {
        configuration.denied_roots = denied_roots.clone();
    }
    let command_path = std::env::var("PATH")?;
    let mut add = vec![
        "add".into(),
        "--sparse".into(),
        "-A".into(),
        "--".into(),
        ".".into(),
    ];
    for path in &plan.add_exclusions {
        add.push(std::ffi::OsString::from(format!(
            ":(top,literal,exclude){path}"
        )));
    }
    let add: Vec<&OsStr> = add.iter().map(std::ffi::OsString::as_os_str).collect();
    git(
        &workspace,
        &temp,
        &plan,
        &configuration,
        &command_path,
        &add,
    )
    .await?;
    if let Purpose::Workspace {
        attachments,
        exclusions,
        ..
    } = purpose
    {
        let path = workspace.clone();
        let index = temp.join("index");
        tokio::task::spawn_blocking(move || {
            Repository::open(path)?.sanitize_snapshot_index(base, &index, &attachments, &exclusions)
        })
        .await??;
    }
    let tree = git(
        &workspace,
        &temp,
        &plan,
        &configuration,
        &command_path,
        &[OsStr::new("write-tree"), OsStr::new("--missing-ok")],
    )
    .await?
    .parse()?;
    Ok(Snapshot {
        root: temp,
        tree,
        plan,
        configuration,
        _directory: temporary,
    })
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn checkpoint_profile_cannot_connect_to_the_private_rpc_socket() -> Result<()> {
        let probe = crate::sandbox_probe::Probe::new()?;
        let tool = crate::git_tool::GitTool::system().await?;
        let plan = SnapshotIndex {
            git_dir: probe.path("source/.git"),
            common_dir: probe.path("source/.git"),
            object_dirs: vec![probe.path("source/.git/objects")],
            unmerged_paths: vec![],
            add_exclusions: vec![],
        };
        probe.check(
            "checkpoint",
            &profile(
                &probe.path("job/repo"),
                &probe.path("job/tmp"),
                &plan,
                &tool,
                &crate::checkout::Configuration {
                    values: BTreeMap::new(),
                    attributes: None,
                    info_attributes: None,
                    hooks_directory: probe.path("source/.git/hooks"),
                    lfs_directory: probe.path("source/.git/lfs"),
                    object_directories: vec![probe.path("source/.git/objects")],
                    denied_roots: vec![],
                },
                &[],
            )?,
        )
    }
}
