//! Read the same owned-process evidence used by recovery before reporting a stop.
use crate::{
    Result,
    process::{Identity, Tracker},
};
use serde::Serialize;
use std::{collections::HashSet, path::Path};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Audit {
    pub hosts: usize,
    pub processes: usize,
    pub groups: usize,
    pub live: Vec<Identity>,
    pub issues: Vec<String>,
}

pub fn inspect(data: &Path) -> Result<Audit> {
    let mut live = HashSet::new();
    let mut hosts = HashSet::new();
    let mut groups = HashSet::new();
    let mut issues = Vec::new();
    for (id, _, path) in crate::process_ledger::host_paths(data)? {
        let inspected = (|| -> Result<()> {
            let record = crate::process_ledger::read(&path.join(crate::process_ledger::NAME))?;
            let mut tracker = Tracker::restore_host(&path, None)?;
            if let Some(root) = record.root
                && crate::process::is_alive(root)
            {
                hosts.insert(root);
            }
            let scan = tracker.refresh()?;
            live.extend(scan.processes.into_iter().map(|process| process.identity));
            groups.extend(scan.groups);
            Ok(())
        })();
        if let Err(error) = inspected {
            issues.push(format!("{id}: {error}"));
        }
    }
    let processes = live.difference(&hosts).count();
    live.extend(hosts.iter().copied());
    let mut live: Vec<_> = live.into_iter().collect();
    live.sort_by_key(|identity| {
        (
            identity.pid,
            identity.started_seconds,
            identity.started_micros,
        )
    });
    Ok(Audit {
        hosts: hosts.len(),
        processes,
        groups: groups.len(),
        live,
        issues,
    })
}

pub fn stop(data: &Path) -> Result<Audit> {
    let mut errors = Vec::new();
    for (id, _, path) in crate::process_ledger::host_paths(data)? {
        if let Err(error) =
            Tracker::restore_host(&path, None).and_then(|mut tracker| tracker.stop_confirmed())
        {
            errors.push(format!("{id}: {error}"));
        }
    }
    let mut audit = inspect(data)?;
    audit.issues.extend(errors);
    audit.issues.sort();
    audit.issues.dedup();
    Ok(audit)
}
