//! Process identity and host attribution. Never expose other environment values.

use libproc::{
    bsd_info::BSDInfo,
    proc_pid::pidinfo,
    processes::{ProcFilter, pids_by_type},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity {
    pub pid: u32,
    pub started_seconds: u64,
    pub started_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub identity: Identity,
    pub parent: u32,
    pub group: u32,
}

#[derive(Debug, Serialize)]
pub struct Scan {
    pub processes: Vec<Process>,
    pub groups: Vec<Identity>,
    pub unreadable: Vec<u32>,
}
impl Scan {
    pub fn is_empty(&self) -> bool {
        self.processes.is_empty() && self.groups.is_empty()
    }
}

fn information(pid: u32) -> io::Result<BSDInfo> {
    pidinfo::<BSDInfo>(pid as i32, 0).map_err(io::Error::other)
}

fn from_info(info: &BSDInfo) -> Identity {
    Identity {
        pid: info.pbi_pid,
        started_seconds: info.pbi_start_tvsec,
        started_micros: info.pbi_start_tvusec,
    }
}

pub fn identity(pid: u32) -> io::Result<Identity> {
    information(pid).map(|info| from_info(&info))
}

/// Whether the exact process is still alive, ignoring zombies awaiting a reap.
pub fn is_alive(expected: Identity) -> bool {
    match information(expected.pid) {
        Ok(info) => from_info(&info) == expected && info.pbi_status != 5,
        Err(_) => {
            // An inaccessible PID is not evidence of death.
            (unsafe { libc::kill(expected.pid as i32, 0) }) != -1
                || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
        }
    }
}
pub fn working_directory(pid: u32) -> io::Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    // proc_pidinfo fills a fixed-size C structure and reports the bytes written.
    let mut value: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&value);
    let count = unsafe {
        libc::proc_pidinfo(
            pid as i32,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut value as *mut libc::proc_vnodepathinfo).cast(),
            size as i32,
        )
    };
    if count != size as i32 {
        return Err(io::Error::last_os_error());
    }
    let path = value
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .map(|c| *c as u8)
        .take_while(|b| *b != 0)
        .collect::<Vec<_>>();
    if path.is_empty() {
        return Err(io::Error::other(
            "Process working directory is unavailable.",
        ));
    }
    Ok(std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&path)))
}

fn environment_contains(bytes: &[u8], marker: &[u8]) -> bool {
    if bytes.len() < 4 {
        return false;
    }
    let argc = i32::from_ne_bytes(bytes[..4].try_into().unwrap());
    if !(0..=65536).contains(&argc) {
        return false;
    }
    let mut remaining = &bytes[4..];
    let Some(end) = remaining.iter().position(|c| *c == 0) else {
        return false;
    };
    remaining = &remaining[end + 1..]; // executable path
    while remaining.first() == Some(&0) {
        remaining = &remaining[1..];
    }
    for _ in 0..argc {
        let Some(end) = remaining.iter().position(|c| *c == 0) else {
            return false;
        };
        remaining = &remaining[end + 1..];
    }
    remaining
        .split(|byte| *byte == 0)
        .any(|entry| entry == marker)
}

pub fn has_host_marker(pid: u32, nonce: &str) -> io::Result<bool> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
    let mut size = 0usize;
    // SAFETY: mib and size are live writable buffers; the first call only queries size.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if size > 2 * 1024 * 1024 {
        return Err(io::Error::other("process arguments exceed limit"));
    }
    let mut bytes = vec![0u8; size];
    // SAFETY: bytes owns size bytes and the kernel receives that same bound.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        bytes.fill(0);
        return Err(io::Error::last_os_error());
    }
    let found = environment_contains(&bytes[..size], format!("INTERCAT_HOST={nonce}").as_bytes());
    bytes.fill(0);
    Ok(found)
}

pub struct Tracker {
    nonces: HashSet<String>,
    known: HashMap<u32, Process>,
    owned_groups: HashMap<u32, Identity>,
    hosts: HashSet<Identity>,
    workspace: PathBuf,
    root: Option<Identity>,
    launching: bool,
    ledger: Option<crate::process_ledger::Writer>,
}

impl Tracker {
    pub fn new(nonce: String, root: Identity) -> io::Result<Self> {
        let info = information(root.pid)?;
        if from_info(&info) != root {
            return Err(io::Error::other("host identity changed"));
        }
        let process = Process {
            identity: root,
            parent: info.pbi_ppid,
            group: info.pbi_pgid,
        };
        Ok(Self {
            nonces: HashSet::from([nonce]),
            known: HashMap::from([(root.pid, process)]),
            owned_groups: HashMap::new(),
            hosts: HashSet::from([root]),
            workspace: PathBuf::new(),
            root: Some(root),
            launching: false,
            ledger: None,
        })
    }

    /// The caller created a new process group for this exact launcher. Retain
    /// its group while members remain, including platform binaries whose
    /// environment cannot be inspected after their short-lived parent exits.
    pub fn new_process_group(nonce: String, root: Identity) -> io::Result<Self> {
        let mut tracker = Self::new(nonce, root)?;
        if tracker.known[&root.pid].group != root.pid {
            return Err(io::Error::other(
                "The command does not own a process group.",
            ));
        }
        tracker.owned_groups.insert(root.pid, root);
        Ok(tracker)
    }

    /// Preserve ownership from earlier connections without assuming their
    /// processes exited when the host did. Every signal still rechecks identity.
    pub fn inherit(&mut self, previous: &Self) -> io::Result<()> {
        self.nonces.extend(previous.nonces.iter().cloned());
        self.hosts.extend(previous.hosts.iter().copied());
        for (&pid, process) in &previous.known {
            let entry = self.known.entry(pid).or_insert_with(|| process.clone());
            if (
                entry.identity.started_seconds,
                entry.identity.started_micros,
            ) < (
                process.identity.started_seconds,
                process.identity.started_micros,
            ) {
                *entry = process.clone();
            }
        }
        for (&pid, identity) in &previous.owned_groups {
            let entry = self.owned_groups.entry(pid).or_insert(*identity);
            if (entry.started_seconds, entry.started_micros)
                < (identity.started_seconds, identity.started_micros)
            {
                *entry = *identity;
            }
        }
        self.save()
    }

    /// Save the launch intent before spawning. An interrupted, unidentified
    /// launch is uncertainty, never evidence that the workspace is idle.
    pub fn prepare(directory: &Path, workspace: &Path, nonce: String) -> io::Result<Self> {
        let mut tracker = Self {
            nonces: HashSet::from([nonce]),
            known: HashMap::new(),
            owned_groups: HashMap::new(),
            hosts: HashSet::new(),
            workspace: workspace.canonicalize()?,
            root: None,
            launching: true,
            ledger: Some(crate::process_ledger::Writer::new(directory)?),
        };
        tracker.save()?;
        Ok(tracker)
    }

    pub fn attach_process_group(&mut self, root: Identity) -> io::Result<()> {
        let nonce = self
            .nonces
            .iter()
            .next()
            .ok_or_else(|| io::Error::other("Missing host marker."))?
            .clone();
        let initial = Self::new_process_group(nonce, root)?;
        self.root = Some(root);
        self.launching = false;
        self.inherit(&initial)
    }

    /// Only call when spawn itself failed: no process was created.
    pub fn cancel_launch(&mut self) -> io::Result<()> {
        if self.root.is_some() {
            return Err(io::Error::other(
                "A started host cannot cancel its launch record.",
            ));
        }
        self.launching = false;
        self.save()
    }

    fn save(&mut self) -> io::Result<()> {
        if let Some(ledger) = &mut self.ledger {
            let mut nonces: Vec<_> = self.nonces.iter().cloned().collect();
            let mut processes: Vec<_> = self.known.values().cloned().collect();
            let mut groups: Vec<_> = self.owned_groups.values().copied().collect();
            nonces.sort();
            processes.sort_by_key(|process| process.identity.pid);
            groups.sort_by_key(|group| group.pid);
            ledger.save(crate::process_ledger::Record {
                workspace: self.workspace.clone(),
                root: self.root,
                launching: self.launching,
                nonces,
                processes,
                groups,
            })?;
        }
        Ok(())
    }

    /// Read only this track's private host directories. No cwd-based attribution.
    /// The original root must be present when a worker record expects one.
    pub fn restore(
        directory: &Path,
        workspace: &Path,
        expected: Option<Identity>,
    ) -> io::Result<Option<Self>> {
        if !directory.try_exists()? {
            return if expected.is_some() {
                Err(io::Error::other(
                    "The previous host's process records are missing.",
                ))
            } else {
                Ok(None)
            };
        }
        let workspace = workspace.canonicalize()?;
        let mut restored: Option<Self> = None;
        let mut found_expected = expected.is_none();
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                return Err(io::Error::other(
                    "Unexpected entry in the worker's host directory.",
                ));
            }
            let record =
                crate::process_ledger::read(&entry.path().join(crate::process_ledger::NAME))?;
            if record.workspace != workspace {
                return Err(io::Error::other("The saved host workspace changed."));
            }
            if record.launching {
                return Err(io::Error::other(
                    "A host launch was interrupted before its identity was saved. Its processes need review.",
                ));
            }
            found_expected |= record.root.is_some() && record.root == expected;
            let tracker = Self {
                nonces: record.nonces.into_iter().collect(),
                known: record
                    .processes
                    .into_iter()
                    .map(|process| (process.identity.pid, process))
                    .collect(),
                owned_groups: record
                    .groups
                    .into_iter()
                    .map(|group| (group.pid, group))
                    .collect(),
                hosts: record.root.into_iter().collect(),
                workspace: workspace.clone(),
                root: record.root,
                launching: false,
                ledger: None,
            };
            if let Some(restored) = &mut restored {
                restored.inherit(&tracker)?;
            } else {
                restored = Some(tracker);
            }
        }
        if !found_expected {
            return Err(io::Error::other(
                "The previous host's process identity is missing from its records.",
            ));
        }
        if let Some(tracker) = &mut restored {
            tracker.refresh()?;
        }
        Ok(restored)
    }

    pub fn has_live_host(&self) -> bool {
        self.hosts.iter().copied().any(is_alive)
    }

    /// A recovery review scans only the daemon's private host directories,
    /// including records whose creating operation is absent from an old DB.
    pub fn restore_host(directory: &Path, expected: Option<Identity>) -> io::Result<Self> {
        let record = crate::process_ledger::read(&directory.join(crate::process_ledger::NAME))?;
        if expected.is_some() && record.root != expected {
            return Err(io::Error::other("The recorded process owner changed."));
        }
        if record.launching {
            return Err(io::Error::other(
                "A host launch has no confirmed process identity.",
            ));
        }
        if record.workspace.canonicalize()? != record.workspace {
            return Err(io::Error::other("The recorded host workspace changed."));
        }
        let mut tracker = Self {
            nonces: record.nonces.into_iter().collect(),
            known: record
                .processes
                .into_iter()
                .map(|process| (process.identity.pid, process))
                .collect(),
            owned_groups: record
                .groups
                .into_iter()
                .map(|group| (group.pid, group))
                .collect(),
            hosts: record.root.into_iter().collect(),
            workspace: record.workspace,
            root: record.root,
            launching: false,
            ledger: None,
        };
        tracker.refresh()?;
        Ok(tracker)
    }

    /// Used when no live host transport remains. Wait for both recorded PIDs
    /// and groups to disappear before authorizing worktree recovery.
    pub fn stop_confirmed(&mut self) -> io::Result<()> {
        for (force, grace) in [(false, 5), (true, 1)] {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(grace);
            loop {
                let scan = self.refresh()?;
                if scan.is_empty() {
                    return Ok(());
                }
                for process in scan.processes {
                    self.terminate(process.identity, force)?;
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
        if self.refresh()?.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(
                "Worker processes or groups could not be confirmed stopped.",
            ))
        }
    }

    /// Reconcile live records, discover marked orphans, and follow observed ancestry.
    /// Unreadable unowned processes remain visible as uncertainty, not as owned PIDs.
    pub fn refresh(&mut self) -> io::Result<Scan> {
        // SAFETY: geteuid has no pointer arguments or preconditions.
        let uid = unsafe { libc::geteuid() };
        let mut infos = HashMap::new();
        let mut unreadable = Vec::new();
        for pid in pids_by_type(ProcFilter::ByUID { uid })? {
            if let Ok(info) = information(pid)
                && info.pbi_status != 5
            {
                infos.insert(pid, info);
            }
        }
        self.known.retain(|pid, process| match infos.get(pid) {
            Some(info) if from_info(info) == process.identity => {
                process.parent = info.pbi_ppid;
                process.group = info.pbi_pgid;
                true
            }
            Some(_) => false,
            None => {
                unreadable.push(*pid);
                is_alive(process.identity)
            }
        });
        for root in self.owned_groups.values().copied().collect::<Vec<_>>() {
            let reused = infos
                .get(&root.pid)
                .is_some_and(|info| from_info(info) != root);
            let members: Vec<_> = infos
                .values()
                .filter(|info| info.pbi_pgid == root.pid)
                .collect();
            // A fork can race the PID enumeration. An empty sample alone is
            // not proof that the process group ceased to exist.
            let gone = members.is_empty()
                && unsafe { libc::kill(-(root.pid as i32), 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            if reused || gone {
                self.owned_groups.remove(&root.pid);
            } else {
                for info in members {
                    let id = from_info(info);
                    if (id.started_seconds, id.started_micros)
                        >= (root.started_seconds, root.started_micros)
                    {
                        self.known.insert(
                            id.pid,
                            Process {
                                identity: id,
                                parent: info.pbi_ppid,
                                group: info.pbi_pgid,
                            },
                        );
                    }
                }
            }
        }
        for (&pid, info) in &infos {
            if self.known.contains_key(&pid) {
                continue;
            }
            let mut marked = false;
            let mut inaccessible = false;
            for nonce in &self.nonces {
                match has_host_marker(pid, nonce) {
                    Ok(true) => {
                        marked = true;
                        break;
                    }
                    Err(_) => inaccessible = true,
                    Ok(false) => {}
                }
            }
            if marked {
                self.known.insert(
                    pid,
                    Process {
                        identity: from_info(info),
                        parent: info.pbi_ppid,
                        group: info.pbi_pgid,
                    },
                );
            } else if inaccessible {
                unreadable.push(pid);
            }
        }
        loop {
            let before = self.known.len();
            for (&pid, info) in &infos {
                if self.known.contains_key(&pid) {
                    continue;
                }
                let owned = |pid| {
                    self.known
                        .get(&pid)
                        .zip(infos.get(&pid))
                        .is_some_and(|(process, info)| process.identity == from_info(info))
                };
                if owned(info.pbi_ppid) || owned(info.pbi_pgid) {
                    self.known.insert(
                        pid,
                        Process {
                            identity: from_info(info),
                            parent: info.pbi_ppid,
                            group: info.pbi_pgid,
                        },
                    );
                }
            }
            if before == self.known.len() {
                break;
            }
        }
        // A child may own a separate group. Keep the group's founding identity
        // after its leader exits, so unmarked members remain attributable.
        for process in self.known.values() {
            if process.group == process.identity.pid {
                self.owned_groups
                    .entry(process.group)
                    .or_insert(process.identity);
            }
        }
        unreadable.retain(|pid| !self.known.contains_key(pid));
        self.save()?;
        Ok(Scan {
            processes: self.known.values().cloned().collect(),
            groups: self.owned_groups.values().copied().collect(),
            unreadable,
        })
    }

    pub fn terminate(&self, expected: Identity, force: bool) -> io::Result<SignalResult> {
        if !self
            .known
            .get(&expected.pid)
            .is_some_and(|known| known.identity == expected)
        {
            return Ok(SignalResult::NotOwned);
        }
        let info = match information(expected.pid) {
            Ok(info) => info,
            Err(error) => {
                // SAFETY: signal 0 only checks the existence of this positive PID.
                if unsafe { libc::kill(expected.pid as i32, 0) } == -1
                    && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    return Ok(SignalResult::Gone);
                }
                return Err(error);
            }
        };
        if from_info(&info) != expected {
            return Ok(SignalResult::IdentityChanged);
        }
        if info.pbi_status == 5 {
            return Ok(SignalResult::Gone);
        }
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        // SAFETY: this is a recorded, revalidated positive PID, never a whole group.
        if unsafe { libc::kill(expected.pid as i32, signal) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(SignalResult::Gone);
            }
            return Err(error);
        }
        Ok(SignalResult::Sent)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum SignalResult {
    Gone,
    IdentityChanged,
    NotOwned,
    Sent,
}

#[cfg(test)]
mod tests {
    use super::environment_contains;
    #[test]
    fn attribution_reads_environment_not_command_arguments() {
        let mut bytes = 2i32.to_ne_bytes().to_vec();
        bytes.extend_from_slice(
            b"/bin/tool\0\0tool\0INTERCAT_HOST=wrong\0SECRET=not-returned\0INTERCAT_HOST=right\0\0",
        );
        assert!(!environment_contains(&bytes, b"INTERCAT_HOST=wrong"));
        assert!(environment_contains(&bytes, b"INTERCAT_HOST=right"));
        assert!(!environment_contains(&bytes, b"HOST=right"));
    }
}
