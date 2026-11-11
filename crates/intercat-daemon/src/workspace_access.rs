//! Metadata-only evidence of other apps using a worker's files or directory.
//! No file contents, arguments, environments, or signal operations are used.
use crate::{Result, process::Identity};
use libproc::{
    bsd_info::BSDInfo,
    file_info::ListFDs,
    proc_pid::{listpidinfo, pidinfo},
    processes::{ProcFilter, pids_by_type},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashSet},
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

// Darwin sys/proc_info.h. libproc supplies the descriptor list and libc the
// vnode path, but neither binding exposes this containing structure.
#[repr(C)]
struct FileInfo {
    flags: u32,
    status: u32,
    offset: i64,
    kind: i32,
    guard: u32,
}
#[repr(C)]
struct VnodeFdInfo {
    file: FileInfo,
    path: libc::vnode_info_path,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Access {
    pub identity: Identity,
    pub app: String,
    pub files: usize,
    pub working_directory: bool,
    pub fingerprint: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Inspection {
    pub processes: Vec<Access>,
    pub unreadable: usize,
    pub fingerprint: String,
}
impl Inspection {
    pub fn clear(&self) -> bool {
        self.processes.is_empty() && self.unreadable == 0
    }
}
#[derive(Debug)]
pub struct ConfirmationRequired {
    pub inspection: Inspection,
}
impl std::fmt::Display for ConfirmationRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Another app may be using this worker’s files · Go ahead / Wait"
        )
    }
}
impl std::error::Error for ConfirmationRequired {}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Consent {
    pub op_id: uuid::Uuid,
    pub request: crate::realign::Request,
    pub fingerprint: String,
}
fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
fn fd_path(pid: u32, fd: i32) -> io::Result<PathBuf> {
    // SAFETY: the C structure contains integer fields only. The kernel gets
    // its exact initialized buffer size, and partial writes are rejected.
    let mut value: VnodeFdInfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&value);
    let count = unsafe {
        libc::proc_pidfdinfo(
            pid as i32,
            fd,
            2,
            (&mut value as *mut VnodeFdInfo).cast(),
            size as i32,
        )
    };
    if count != size as i32 {
        return Err(io::Error::last_os_error());
    }
    let bytes = value
        .path
        .vip_path
        .iter()
        .flatten()
        .map(|c| *c as u8)
        .take_while(|c| *c != 0)
        .collect::<Vec<_>>();
    if bytes.is_empty() {
        return Err(io::Error::other("The open file path is unavailable."));
    }
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes)))
}
fn paths(root: &Path, path: &Path, evidence: &mut BTreeSet<[u8; 32]>) -> bool {
    if let Ok(relative) = path.strip_prefix(root) {
        evidence.insert(Sha256::digest(relative.as_os_str().as_bytes()).into());
        true
    } else {
        false
    }
}
fn inspect_pids(
    root: &Path,
    excluded: &[Identity],
    pids: impl IntoIterator<Item = u32>,
) -> Result<Inspection> {
    let root = root.canonicalize()?;
    let excluded = excluded.iter().copied().collect::<HashSet<_>>();
    let mut processes = Vec::new();
    let mut unreadable = BTreeSet::new();
    for pid in pids {
        let info = match pidinfo::<BSDInfo>(pid as i32, 1) {
            Ok(info) => info,
            Err(_) => {
                if io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                    unreadable.insert(pid);
                }
                continue;
            }
        };
        if info.pbi_uid != unsafe { libc::geteuid() } || info.pbi_status == libc::SZOMB {
            continue;
        }
        let identity = Identity {
            pid: info.pbi_pid,
            started_seconds: info.pbi_start_tvsec,
            started_micros: info.pbi_start_tvusec,
        };
        if excluded.contains(&identity) {
            continue;
        }
        let mut evidence = BTreeSet::new();
        let working_directory = match crate::process::working_directory(pid) {
            Ok(path) => paths(&root, &path, &mut evidence),
            Err(_) => {
                unreadable.insert(pid);
                false
            }
        };
        let mut capacity = (info.pbi_nfiles as usize + 32).clamp(32, 65536);
        let mut files = 0;
        loop {
            match listpidinfo::<ListFDs>(pid as i32, capacity) {
                Ok(fds) => {
                    if fds.len() == capacity && capacity < 65536 {
                        capacity = (capacity * 2).min(65536);
                        continue;
                    }
                    if fds.len() == capacity {
                        unreadable.insert(pid);
                    }
                    for fd in fds.into_iter().filter(|fd| fd.proc_fdtype == 1) {
                        match fd_path(pid, fd.proc_fd) {
                            Ok(path) => {
                                if paths(&root, &path, &mut evidence) {
                                    files += 1;
                                }
                            }
                            Err(error)
                                if matches!(
                                    error.raw_os_error(),
                                    Some(libc::EBADF | libc::ENOENT | libc::ESRCH)
                                ) => {}
                            Err(_) => {
                                unreadable.insert(pid);
                            }
                        }
                    }
                }
                Err(_) => {
                    unreadable.insert(pid);
                }
            }
            break;
        }
        if crate::process::identity(pid).ok() != Some(identity) {
            unreadable.remove(&pid);
            continue;
        }
        if !evidence.is_empty() {
            let fingerprint = hex(Sha256::digest(serde_json::to_vec(&(
                identity,
                working_directory,
                evidence,
            ))?));
            processes.push(Access {
                identity,
                files,
                working_directory,
                fingerprint,
                app: libproc::proc_pid::name(pid as i32)
                    .unwrap_or_else(|_| "Another app".into())
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(80)
                    .collect(),
            });
        }
    }
    processes.sort_by_key(|p| {
        (
            p.identity.pid,
            p.identity.started_seconds,
            p.identity.started_micros,
        )
    });
    let fingerprint = hex(Sha256::digest(serde_json::to_vec(&(
        root.as_os_str().as_bytes(),
        processes
            .iter()
            .map(|p| (&p.identity, &p.fingerprint))
            .collect::<Vec<_>>(),
        &unreadable,
    ))?));
    Ok(Inspection {
        processes,
        unreadable: unreadable.len(),
        fingerprint,
    })
}

pub fn inspect(root: &Path, excluded: &[Identity]) -> Result<Inspection> {
    inspect_pids(
        root,
        excluded,
        pids_by_type(ProcFilter::ByUID {
            uid: unsafe { libc::geteuid() },
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    #[tokio::test]
    async fn an_external_file_reader_is_reported_without_reading_contents_or_stopping_it()
    -> Result<()> {
        let root = tempfile::tempdir_in("/private/tmp")?;
        let workspace = root.path().join("worker");
        std::fs::create_dir(&workspace)?;
        let first = workspace.join("dummy-one.txt");
        let second = workspace.join("dummy-two.txt");
        std::fs::write(&first, b"agent-created dummy body, never inspected")?;
        std::fs::write(&second, b"another agent-created dummy body")?;
        let mut child=tokio::process::Command::new("/usr/bin/python3").args(["-I","-u","-c","import sys\nf=open(sys.argv[1],'rb')\nprint('ready')\nsys.stdin.readline()\ng=open(sys.argv[2],'rb')\nprint('more')\nsys.stdin.read()"])
            .arg(&first).arg(&second).current_dir(root.path()).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn()?;
        let pid = child.id().ok_or("Missing dummy reader PID")?;
        let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).await?;
        assert_eq!(line, "ready\n");
        let original = inspect_pids(&workspace, &[], [pid])?;
        assert_eq!(original.processes.len(), 1);
        assert_eq!(original.processes[0].files, 1);
        assert!(!original.processes[0].working_directory);
        assert_eq!(original.unreadable, 0);
        assert!(!original.clear());
        let serialized = serde_json::to_string(&original)?;
        assert!(!serialized.contains("dummy-one") && !serialized.contains("never inspected"));
        child.stdin.as_mut().unwrap().write_all(b"more\n").await?;
        line.clear();
        output.read_line(&mut line).await?;
        assert_eq!(line, "more\n");
        let changed = inspect_pids(&workspace, &[], [pid])?;
        assert_eq!(changed.processes[0].files, 2);
        assert_ne!(original.fingerprint, changed.fingerprint);
        assert!(crate::process::is_alive(original.processes[0].identity));
        assert!(inspect_pids(&workspace, &[original.processes[0].identity], [pid])?.clear());
        let adjacent = root.path().join("worker-other");
        std::fs::create_dir(&adjacent)?;
        assert!(inspect_pids(&adjacent, &[], [pid])?.clear());
        drop(child.stdin.take());
        assert!(child.wait().await?.success());
        Ok(())
    }
    #[tokio::test]
    async fn a_process_in_the_working_directory_is_reported_without_an_open_file() -> Result<()> {
        let root = tempfile::tempdir_in("/private/tmp")?;
        let mut child = tokio::process::Command::new("/bin/sleep")
            .arg("30")
            .current_dir(root.path())
            .kill_on_drop(true)
            .spawn()?;
        let pid = child.id().ok_or("Missing dummy directory PID")?;
        let inspected = inspect_pids(root.path(), &[], [pid])?;
        assert_eq!(inspected.processes.len(), 1);
        assert!(inspected.processes[0].working_directory);
        assert!(crate::process::is_alive(inspected.processes[0].identity));
        child.kill().await?;
        let _ = child.wait().await;
        Ok(())
    }
}
