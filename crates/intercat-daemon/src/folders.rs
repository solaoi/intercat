//! Repository ownership for tracks and session branches.
use crate::{
    Result,
    project::{Folder, Project},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE track_folders(track TEXT PRIMARY KEY REFERENCES tracks(id) ON DELETE CASCADE,folder TEXT NOT NULL);
        CREATE TABLE session_folders(session TEXT NOT NULL,folder TEXT NOT NULL,slug TEXT NOT NULL,PRIMARY KEY(session,folder));")?;
    Ok(())
}
pub(crate) fn project(db: &Connection) -> Result<Option<Project>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key='project'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}
pub(crate) fn bind(db: &Connection, track: &str, folder: &str) -> Result<()> {
    project(db)?
        .ok_or("Choose a project folder.")?
        .folder(folder)?;
    let old: Option<String> = db
        .query_row(
            "SELECT folder FROM track_folders WHERE track=?1",
            [track],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(old) = old {
        if old != folder {
            return Err("A track cannot change its repository.".into());
        }
    } else {
        db.execute(
            "INSERT INTO track_folders VALUES(?1,?2)",
            params![track, folder],
        )?;
    }
    Ok(())
}
pub(crate) fn bind_default(db: &Connection, track: &str) -> Result<()> {
    if let Some(project) = project(db)?
        && let [folder] = project.folders.as_slice()
    {
        bind(db, track, &folder.id())?;
    }
    Ok(())
}

#[derive(Clone, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Assignment {
    pub op_id: uuid::Uuid,
    pub track_id: String,
    pub folder_id: String,
}

pub(crate) fn assign(db: &mut Connection, request: Assignment) -> Result<()> {
    if request.op_id.get_version_num() != 7 {
        return Err("Invalid folder operation.".into());
    }
    let tx = db.transaction()?;
    let payload = serde_json::to_string(&request)?;
    let old: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, old)) = old {
        if kind != "folder.assign" || old != payload {
            return Err("Folder operation ID reused.".into());
        }
        return Ok(());
    }
    let prepared: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE track_id=?1)",
        [&request.track_id],
        |row| row.get(0),
    )?;
    if prepared {
        return Err("This track already has a workspace.".into());
    }
    bind(&tx, &request.track_id, &request.folder_id)?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'folder.assign',?2,'complete')",
        params![request.op_id.to_string(), payload],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'folder.assigned',?2)",
        params![crate::journal::timestamp(), payload],
    )?;
    tx.commit()?;
    Ok(())
}
pub(crate) fn track(db: &Connection, track: &str) -> Result<Folder> {
    let id: String = db.query_row(
        "SELECT folder FROM track_folders WHERE track=?1",
        [track],
        |row| row.get(0),
    )?;
    Ok(project(db)?
        .ok_or("Choose a project folder.")?
        .folder(&id)?
        .clone())
}
pub(crate) fn track_ids(db: &Connection) -> Result<std::collections::BTreeMap<String, String>> {
    let mut query = db.prepare("SELECT track,folder FROM track_folders")?;
    Ok(query
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}
pub(crate) fn sessions(db: &Connection) -> Result<Vec<Value>> {
    let mut query = db.prepare("SELECT session,folder,slug FROM session_folders ORDER BY rowid")?;
    Ok(query.query_map([], |row| Ok(json!({"sessionId":row.get::<_,String>(0)?,"folderId":row.get::<_,String>(1)?,"slug":row.get::<_,String>(2)?})))?.collect::<rusqlite::Result<_>>()?)
}
pub(crate) fn from_snapshot(snapshot: &Value, track: &str) -> Result<Folder> {
    let project: Project = serde_json::from_value(snapshot["project"].clone())?;
    let folder = snapshot["trackFolders"][track]
        .as_str()
        .ok_or("The track has no project folder.")?;
    Ok(project.folder(folder)?.clone())
}
pub(crate) fn session_slug<'a>(snapshot: &'a Value, folder: &str) -> Result<&'a str> {
    snapshot["sessionFolders"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|entry| {
            entry["sessionId"] == snapshot["session"]["id"] && entry["folderId"] == folder
        })
        .and_then(|entry| entry["slug"].as_str())
        .ok_or_else(|| "This folder's session branch is not ready.".into())
}

#[cfg(test)]
#[path = "folders_tests.rs"]
mod tests;
