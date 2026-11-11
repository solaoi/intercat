//! A snapshot's live-file identity is distinct from its historical track ID.
//! Private Run checkouts may share that ID, but never a worker's live directory.
use crate::{Result, preparation::Workspace};
use rusqlite::{Connection, OptionalExtension};
use std::path::PathBuf;
use tokio::sync::OwnedRwLockReadGuard;

pub(crate) struct Context {
    pub exclusions: Vec<String>,
    isolated_from: Vec<PathBuf>,
}
pub(crate) struct Scope {
    pub context: Context,
    pub permit: OwnedRwLockReadGuard<()>,
}
impl Context {
    pub fn check_directory(&self, path: &std::path::Path) -> Result<()> {
        if self.isolated_from.is_empty() {
            return Ok(());
        }
        let path = path.canonicalize()?;
        for worker in &self.isolated_from {
            if !worker.exists() {
                continue;
            }
            let worker = worker.canonicalize()?;
            if path.starts_with(&worker) || worker.starts_with(&path) {
                return Err("A private snapshot cannot read a worker's live directory.".into());
            }
        }
        Ok(())
    }
}
pub(crate) fn context(db: &Connection, workspace: &Workspace) -> Result<Context> {
    let isolated = workspace
        .reference
        .strip_prefix("refs/intercat/repairs/")
        .is_some_and(|id| id.parse::<uuid::Uuid>().is_ok());
    let mut worker_paths = Vec::new();
    if isolated {
        let mut query = db.prepare("SELECT json_extract(record,'$.path') FROM workspaces")?;
        worker_paths = query
            .query_map([], |row| row.get::<_, String>(0).map(PathBuf::from))?
            .collect::<rusqlite::Result<Vec<PathBuf>>>()?;
    } else {
        // Reject closed metadata before opening or inspecting its live path.
        crate::continuation::ensure_writer(db, &workspace.track_id)?;
        let raw: Option<String> = db
            .query_row(
                "SELECT record FROM workspaces WHERE track_id=?1",
                [&workspace.track_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(raw) = raw {
            let stored: Workspace = serde_json::from_str(&raw)?;
            if stored.path != workspace.path
                || stored.reference != workspace.reference
                || stored.base != workspace.base
            {
                return Err("The snapshot no longer names this worker's live workspace.".into());
            }
        } else {
            // Standalone snapshots still cannot impersonate a registered writer.
            let mut query = db.prepare("SELECT json_extract(record,'$.path') FROM workspaces")?;
            worker_paths = query
                .query_map([], |row| row.get::<_, String>(0).map(PathBuf::from))?
                .collect::<rusqlite::Result<Vec<PathBuf>>>()?;
        }
    }
    Ok(Context {
        exclusions: crate::checkpoint_scope::exclusions(db, &workspace.track_id)?,
        isolated_from: worker_paths,
    })
}
