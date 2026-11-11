//! Per-folder Check choices. Open reviews retain their captured revision.
use crate::{Result, check_run::CheckCommand};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Folder {
    pub revision: u64,
    pub command: CheckCommand,
    pub history: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Setting {
    pub revision: u64,
    pub folders: BTreeMap<String, Folder>,
}
impl Setting {
    pub fn folder(&self, id: &str) -> Folder {
        self.folders.get(id).cloned().unwrap_or_default()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Change {
    pub op_id: Uuid,
    pub revision: u64,
    pub folder: String,
    pub command: CheckCommand,
    pub history: bool,
}
pub(crate) fn read(db: &Connection) -> Result<Setting> {
    let value: Option<String> = db
        .query_row("SELECT value FROM settings WHERE key='checks'", [], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(value
        .map(|value| serde_json::from_str(&value))
        .transpose()?
        .unwrap_or_default())
}
pub(crate) fn change(db: &mut Connection, request: &Change) -> Result<Setting> {
    if request.op_id.get_version_num() != 7 {
        return Err("Invalid Check settings operation.".into());
    }
    if let CheckCommand::Run(command) = &request.command
        && (command.trim().is_empty()
            || command.len() > 16 * 1024
            || command.contains(['\0', '\n', '\r']))
    {
        return Err("Enter a Check command on one line, up to 16 KiB.".into());
    }
    let tx = db.transaction()?;
    let payload = serde_json::to_string(request)?;
    if let Some((kind, saved)) = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if kind != "checks.change" || saved != payload {
            return Err("Operation ID was used for a different Check settings change.".into());
        }
        return read(&tx);
    }
    let project =
        crate::folders::project(&tx)?.ok_or("Choose a project before configuring Checks.")?;
    if !project
        .folders
        .iter()
        .any(|folder| folder.id == request.folder)
    {
        return Err("The Check folder is no longer in this project.".into());
    }
    let mut next = read(&tx)?;
    if next.revision != request.revision {
        return Err("Check settings changed in another window. Reload before saving.".into());
    }
    next.revision = next
        .revision
        .checked_add(1)
        .ok_or("Check settings revision overflow.")?;
    next.folders.insert(
        request.folder.clone(),
        Folder {
            revision: next.revision,
            command: request.command.clone(),
            history: request.history,
        },
    );
    let value = serde_json::to_string(&next)?;
    tx.execute("INSERT INTO settings VALUES('checks',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&value])?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'checks.change',?2,'complete')",
        params![request.op_id.to_string(), payload],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'checks.changed',?2)",
        params![crate::journal::timestamp(), value],
    )?;
    tx.commit()?;
    Ok(next)
}
