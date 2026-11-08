//! Private crash-recovery evidence. This file never contains argv or environment values.
use crate::process::{Identity, Process};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

pub const NAME: &str = "processes.json";
const MAX_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub workspace: PathBuf,
    pub root: Option<Identity>,
    pub launching: bool,
    pub nonces: Vec<String>,
    pub processes: Vec<Process>,
    pub groups: Vec<Identity>,
}

fn private_directory(path: &Path) -> io::Result<()> {
    let info = fs::symlink_metadata(path)?;
    if !info.is_dir() || info.uid() != unsafe { libc::geteuid() } || info.mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "Host process records need a private, owned directory.",
        ));
    }
    Ok(())
}

pub fn read(path: &Path) -> io::Result<Record> {
    private_directory(
        path.parent()
            .ok_or_else(|| io::Error::other("Missing host directory."))?,
    )?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let info = file.metadata()?;
    if !info.is_file()
        || info.uid() != unsafe { libc::geteuid() }
        || info.mode() & 0o077 != 0
        || info.nlink() != 1
        || info.len() > MAX_BYTES
    {
        return Err(io::Error::other(
            "Host process records must be a bounded private file.",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::other("Host process records exceed the limit."));
    }
    let record: Record = serde_json::from_slice(&bytes)?;
    let valid_id =
        |id: &Identity| id.pid > 1 && id.pid <= i32::MAX as u32 && id.started_micros < 1_000_000;
    if !record.workspace.is_absolute()
        || record.nonces.is_empty()
        || record
            .nonces
            .iter()
            .any(|nonce| uuid::Uuid::parse_str(nonce).map_or(true, |id| id.get_version_num() != 7))
        || record.root.as_ref().is_some_and(|id| !valid_id(id))
        || record
            .processes
            .iter()
            .any(|process| !valid_id(&process.identity) || process.group > i32::MAX as u32)
        || record.groups.iter().any(|id| !valid_id(id))
        || (record.launching && record.root.is_some())
        || (record.root.is_none() && (!record.processes.is_empty() || !record.groups.is_empty()))
    {
        return Err(io::Error::other("Invalid host process records."));
    }
    Ok(record)
}

pub struct Writer {
    path: PathBuf,
    saved: Option<Record>,
}
impl Writer {
    pub fn new(directory: &Path) -> io::Result<Self> {
        private_directory(directory)?;
        let path = directory.join(NAME);
        if path.try_exists()? {
            return Err(io::Error::other("Host process records already exist."));
        }
        Ok(Self { path, saved: None })
    }
    pub fn save(&mut self, record: Record) -> io::Result<()> {
        if self.saved.as_ref() == Some(&record) {
            return Ok(());
        }
        let parent = self.path.parent().unwrap();
        private_directory(parent)?;
        let bytes = serde_json::to_vec(&record)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(io::Error::other("Host process records exceed the limit."));
        }
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        File::open(parent)?.sync_all()?;
        self.saved = Some(record);
        Ok(())
    }
}

/// Persist discoveries while the transport or journal is busy, including idle
/// development servers. Stop a host if ownership can no longer be recorded.
pub struct Monitor {
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<io::Result<()>>,
}
impl Monitor {
    pub fn start(
        tracker: std::sync::Arc<std::sync::Mutex<crate::process::Tracker>>,
        root: Identity,
    ) -> Self {
        let (stop, mut stopping) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = stopping.changed() => return Ok(()),
                    _ = tick.tick() => {}
                }
                let source = tracker.clone();
                let result =
                    tokio::task::spawn_blocking(move || source.lock().unwrap().refresh()).await?;
                if let Err(error) = result {
                    let _ = tracker.lock().unwrap().terminate(root, true);
                    return Err(error);
                }
            }
        });
        Self { stop, task }
    }
    pub async fn stop(self) -> io::Result<()> {
        let _ = self.stop.send(true);
        self.task.await?
    }
}

pub(crate) fn host_paths(data: &Path) -> crate::Result<Vec<(String, String, PathBuf)>> {
    let mut result = Vec::new();
    for (folder, kind, depth) in [
        ("hosts", "work", 2),
        ("repair-hosts", "repair", 1),
        ("summary-hosts", "summary", 2),
        ("planner-hosts", "planner", 1),
        ("review-hosts", "review", 1),
    ] {
        let directory = data.join(folder);
        if !directory.try_exists()? {
            continue;
        }
        if !fs::symlink_metadata(&directory)?.is_dir() {
            return Err("The host record directory changed.".into());
        }
        for owner in fs::read_dir(directory)? {
            let owner = owner?;
            let name = owner
                .file_name()
                .into_string()
                .map_err(|_| "Invalid host directory name.")?;
            if !owner.file_type()?.is_dir() {
                return Err("Unexpected entry in the host record directory.".into());
            }
            if depth == 1 {
                result.push((format!("{kind}/{name}"), name, owner.path()));
            } else {
                for instance in fs::read_dir(owner.path())? {
                    let instance = instance?;
                    if !instance.file_type()?.is_dir() {
                        return Err("Unexpected entry in the host record directory.".into());
                    }
                    let child = instance
                        .file_name()
                        .into_string()
                        .map_err(|_| "Invalid host directory name.")?;
                    result.push((
                        format!("{kind}/{name}/{child}"),
                        name.clone(),
                        instance.path(),
                    ));
                    if result.len() > 4096 {
                        return Err("Too many host records to review at once.".into());
                    }
                }
            }
            if result.len() > 4096 {
                return Err("Too many host records to review at once.".into());
            }
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(result)
}
