//! Metadata-only accounting for hosts and children that may retain an old value.
use crate::{Result, process::Identity};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostUse {
    pub project_id: String,
    pub nonce: String,
    pub track_id: String,
    pub repair_run_id: Option<uuid::Uuid>,
    pub revision: u64,
    pub fingerprints: Vec<crate::project_secrets::Descriptor>,
    pub workspace: PathBuf,
    pub identity: Identity,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Record {
    pub host: HostUse,
    pub kept: Vec<Identity>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessUse {
    pub identity: Identity,
    pub host: bool,
    pub kept: bool,
    pub can_stop: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub nonce: String,
    pub track_id: String,
    pub repair_run_id: Option<uuid::Uuid>,
    pub revision: u64,
    pub names: Vec<String>,
    pub processes: Vec<ProcessUse>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Decision {
    pub nonce: String,
    pub identity: Identity,
    pub keep: bool,
}

pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE project_secret_hosts(nonce TEXT PRIMARY KEY,record TEXT NOT NULL,kept TEXT NOT NULL,closed INTEGER NOT NULL);")?;
    Ok(())
}
pub fn record(db: &Connection, host: &HostUse) -> Result<()> {
    db.execute(
        "INSERT INTO project_secret_hosts VALUES(?1,?2,'[]',0)",
        params![host.nonce, serde_json::to_string(host)?],
    )?;
    Ok(())
}
pub fn records(db: &Connection) -> Result<Vec<Record>> {
    db.prepare("SELECT record,kept FROM project_secret_hosts WHERE closed=0 ORDER BY rowid")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .map(|v| {
            let (host, kept) = v?;
            Ok(Record {
                host: serde_json::from_str(&host)?,
                kept: serde_json::from_str(&kept)?,
            })
        })
        .collect()
}
pub fn close(db: &Connection, nonce: &str) -> Result<()> {
    db.execute(
        "UPDATE project_secret_hosts SET closed=1 WHERE nonce=?1",
        [nonce],
    )?;
    Ok(())
}
pub fn keep(db: &Connection, decision: &Decision) -> Result<()> {
    let text: String = db.query_row(
        "SELECT kept FROM project_secret_hosts WHERE nonce=?1 AND closed=0",
        [&decision.nonce],
        |r| r.get(0),
    )?;
    let mut kept: Vec<Identity> = serde_json::from_str(&text)?;
    if !kept.contains(&decision.identity) {
        kept.push(decision.identity);
    }
    db.execute(
        "UPDATE project_secret_hosts SET kept=?2 WHERE nonce=?1",
        params![decision.nonce, serde_json::to_string(&kept)?],
    )?;
    Ok(())
}
pub fn scan(records: &[Record]) -> Result<Vec<Usage>> {
    use libproc::processes::{ProcFilter, pids_by_type};
    let pids = pids_by_type(ProcFilter::ByUID {
        uid: unsafe { libc::geteuid() },
    })?;
    let mut uses = Vec::new();
    for record in records {
        let mut processes = Vec::new();
        for &pid in &pids {
            let Ok(identity) = crate::process::identity(pid) else {
                continue;
            };
            let root = identity == record.host.identity;
            if !root && !crate::process::has_host_marker(pid, &record.host.nonce).unwrap_or(false) {
                continue;
            }
            let in_workspace = crate::process::working_directory(pid)
                .is_ok_and(|cwd| cwd.starts_with(&record.host.workspace));
            processes.push(ProcessUse {
                identity,
                host: root,
                kept: record.kept.contains(&identity),
                can_stop: !root && in_workspace,
            });
        }
        uses.push(Usage {
            nonce: record.host.nonce.clone(),
            track_id: record.host.track_id.clone(),
            repair_run_id: record.host.repair_run_id,
            revision: record.host.revision,
            names: record
                .host
                .fingerprints
                .iter()
                .map(|f| f.name.clone())
                .collect(),
            processes,
        });
    }
    Ok(uses)
}
pub async fn stop(record: &Record, identity: Identity) -> Result<()> {
    if identity == record.host.identity {
        return Err("Use Apply now to restart the host.".into());
    }
    for force in [false, true] {
        if !crate::process::is_alive(identity) {
            return Ok(());
        }
        if !crate::process::has_host_marker(identity.pid, &record.host.nonce)?
            || !crate::process::working_directory(identity.pid)?.starts_with(&record.host.workspace)
        {
            return Err("The process is no longer attributable to this workspace.".into());
        }
        if crate::process::identity(identity.pid)? != identity {
            return Err("The process identity changed.".into());
        }
        // The PID, start time, marker and workspace were all checked above.
        if unsafe {
            libc::kill(
                identity.pid as i32,
                if force { libc::SIGKILL } else { libc::SIGTERM },
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        for _ in 0..20 {
            if !crate::process::is_alive(identity) {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    Err("The process has not stopped yet.".into())
}
