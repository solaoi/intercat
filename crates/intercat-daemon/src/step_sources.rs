//! Immutable file names captured around one owned tool batch.
use crate::{Result, clock::Stamp, preparation::Workspace};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::{Component, Path};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Source {
    pub id: Uuid,
    pub track: String,
    pub session: String,
    pub turn: String,
    pub calls: Vec<String>,
    pub paths: Option<Vec<String>>,
    pub workspace: Workspace,
    pub at: Stamp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Active {
    pub track: String,
    pub session: String,
    pub turn: String,
    pub before: String,
    pub calls: Vec<String>,
    pub workspace: Workspace,
}

pub fn identity(session: &str, turn: &str, calls: &[String]) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_OID,
        &serde_json::to_vec(&(session, turn, calls)).unwrap(),
    )
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS step_file_sources(
        id TEXT PRIMARY KEY, track TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
        session TEXT NOT NULL, turn TEXT NOT NULL, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS step_file_source_turn ON step_file_sources(session,turn);
        CREATE INDEX IF NOT EXISTS timeline_item_call ON timeline_items(session,json_extract(record,'$.callId'));
        CREATE TABLE IF NOT EXISTS step_capture_hooks(track TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            file TEXT NOT NULL,PRIMARY KEY(track,file));
        CREATE TABLE IF NOT EXISTS step_capture_turns(session TEXT NOT NULL,turn TEXT NOT NULL,track TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            PRIMARY KEY(session,turn));
        CREATE TABLE IF NOT EXISTS step_capture_active(session TEXT NOT NULL,turn TEXT NOT NULL,track TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            record TEXT NOT NULL,PRIMARY KEY(session,turn));
        CREATE TABLE IF NOT EXISTS step_source_paths(source TEXT NOT NULL REFERENCES step_file_sources(id) ON DELETE CASCADE,
            path TEXT NOT NULL,PRIMARY KEY(source,path));
        CREATE INDEX IF NOT EXISTS step_source_path ON step_source_paths(path,source);
        CREATE TABLE IF NOT EXISTS step_source_clips(source TEXT PRIMARY KEY REFERENCES step_file_sources(id) ON DELETE CASCADE,
            clip TEXT UNIQUE NOT NULL REFERENCES timeline_clips(id) ON DELETE CASCADE);",
    )?;
    Ok(())
}

pub(crate) fn begin(db: &mut Connection, active: &Active) -> Result<bool> {
    Uuid::parse_str(&active.session)?;
    Uuid::parse_str(&active.turn)?;
    let _: intercat_git::ObjectId = active.before.parse()?;
    if active.calls.is_empty()
        || active.calls.len() > 256
        || active.calls.windows(2).any(|p| p[0] >= p[1])
    {
        return Err("Invalid active tool batch.".into());
    }
    let tx = db.transaction()?;
    let Some(workspace) = owner(&tx, &active.track, &active.session)? else {
        return Ok(false);
    };
    if serde_json::to_value(workspace)? != serde_json::to_value(&active.workspace)? {
        return Ok(false);
    }
    let previous: Option<String> = tx
        .query_row(
            "SELECT record FROM step_capture_active WHERE session=?1 AND turn=?2",
            params![active.session, active.turn],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(previous) = previous {
        let previous: Active = serde_json::from_str(&previous)?;
        if previous.before != active.before || previous.track != active.track {
            return Err("An active capture has a different before-state.".into());
        }
    }
    tx.execute(
        "DELETE FROM step_capture_active WHERE track=?1 AND (session<>?2 OR turn<>?3)",
        params![active.track, active.session, active.turn],
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO step_capture_turns VALUES(?1,?2,?3)",
        params![active.session, active.turn, active.track],
    )?;
    tx.execute("INSERT INTO step_capture_active VALUES(?1,?2,?3,?4) ON CONFLICT(session,turn) DO UPDATE SET record=excluded.record",params![active.session,active.turn,active.track,serde_json::to_string(active)?])?;
    tx.commit()?;
    Ok(true)
}

pub(crate) fn capturing(db: &Connection, session: &str, turn: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM step_capture_turns WHERE session=?1 AND turn=?2)",
        params![session, turn],
        |r| r.get(0),
    )?)
}

pub(crate) fn active(
    db: &Connection,
    track: &str,
    session: &str,
    turn: &str,
) -> Result<Option<Active>> {
    let Some(workspace) = owner(db, track, session)? else {
        return Ok(None);
    };
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM step_capture_active WHERE track=?1 AND session=?2 AND turn=?3",
            params![track, session, turn],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else { return Ok(None) };
    let active: Active = serde_json::from_str(&raw)?;
    Ok(
        (serde_json::to_value(&active.workspace)? == serde_json::to_value(workspace)?)
            .then_some(active),
    )
}

pub(crate) fn bind_ready(db: &Connection, session: &str, turn: &str) -> Result<()> {
    let records=db.prepare("SELECT s.record FROM step_file_sources s WHERE session=?1 AND turn=?2 AND NOT EXISTS(SELECT 1 FROM step_source_clips b WHERE b.source=s.id) ORDER BY rowid")?
        .query_map(params![session,turn],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for raw in records {
        let source: Source = serde_json::from_str(&raw)?;
        let mut clips = std::collections::BTreeSet::new();
        let mut found = 0;
        for call in &source.calls {
            let rows=db.prepare("SELECT i.clip,i.record FROM timeline_items i JOIN timeline_clips c ON c.id=i.clip
                WHERE i.session=?1 AND json_extract(i.record,'$.callId')=?2 AND json_extract(c.record,'$.turnId')=?3")?
                .query_map(params![session,call,turn],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            if rows.len() > 1 {
                return Err("A tool identity names more than one timeline item.".into());
            }
            if let Some((clip, record)) = rows.first() {
                let item: serde_json::Value = serde_json::from_str(record)?;
                if item["status"] == "inProgress"
                    && item["background"] != true
                    && !item["backgroundStatus"].is_object()
                {
                    continue;
                }
                clips.insert(clip.clone());
                found += 1;
            }
        }
        if found != source.calls.len() {
            continue;
        }
        if clips.len() != 1 {
            return Err("One captured batch spans multiple clips.".into());
        }
        let clip = clips.into_iter().next().unwrap();
        db.execute(
            "INSERT INTO step_source_clips VALUES(?1,?2)",
            params![source.id.to_string(), clip],
        )?;
        crate::timeline::close_captured_batch(db, &source, &clip)?;
        crate::clip_files::source_ready(db, &source, &clip)?;
        let state: Option<String> = db
            .query_row(
                "SELECT record FROM workers WHERE track_id=?1",
                [&source.track],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(state) = state {
            crate::review_store::observe(db, &serde_json::from_str(&state)?)?;
        }
    }
    Ok(())
}

pub(crate) fn register_hook(db: &Connection, track: &str, file: &Path) -> Result<()> {
    let file = file.to_str().ok_or("Invalid hook path.")?;
    db.execute(
        "INSERT OR IGNORE INTO step_capture_hooks VALUES(?1,?2)",
        params![track, file],
    )?;
    Ok(())
}

pub(crate) fn internal_hook(
    db: &Connection,
    track: &str,
    item: &serde_json::Value,
) -> Result<bool> {
    if item["kind"] != "hookRun" {
        return Ok(false);
    }
    let Some(key) = item["hookKey"].as_str() else {
        return Ok(false);
    };
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM step_capture_hooks WHERE track=?1 AND substr(?2,1,length(file)+1)=file||':')",params![track,key],|row|row.get(0))?)
}

pub(crate) fn owner(db: &Connection, track: &str, session: &str) -> Result<Option<Workspace>> {
    if crate::continuation::closed(db, track)? {
        return Ok(None);
    }
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT w.record,p.record FROM workers w JOIN workspaces p ON p.track_id=w.track_id
         WHERE w.track_id=?1 AND json_extract(w.record,'$.sessionId')=?2",
            params![track, session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((_, workspace)) = row else {
        return Ok(None);
    };
    let workspace: Workspace = serde_json::from_str(&workspace)?;
    Ok((workspace.state == "ready").then_some(workspace))
}

pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Option<Source>> {
    db.query_row(
        "SELECT record FROM step_file_sources WHERE id=?1",
        [id.to_string()],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|raw| Ok(serde_json::from_str(&raw)?))
    .transpose()
}

pub(crate) fn save(db: &mut Connection, source: &Source) -> Result<bool> {
    Uuid::parse_str(&source.session)?;
    Uuid::parse_str(&source.turn)?;
    if source.calls.is_empty()
        || source.calls.len() > 256
        || source
            .calls
            .iter()
            .any(|call| call.is_empty() || call.len() > 256 || call.chars().any(char::is_control))
        || source.calls.windows(2).any(|pair| pair[0] >= pair[1])
        || source.id != identity(&source.session, &source.turn, &source.calls)
    {
        return Err("Invalid step source identity.".into());
    }
    let paths = source.paths.as_deref().unwrap_or_default();
    if paths.len() > 65536
        || paths.iter().any(|name| {
            name.is_empty()
                || name.chars().any(char::is_control)
                || Path::new(name)
                    .components()
                    .any(|part| !matches!(part,Component::Normal(value) if value!=".git" && !intercat_git::secret_path(&value.to_string_lossy())))
                || intercat_git::secret_path(name)
        })
        || paths.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err("Invalid captured file names.".into());
    }
    let tx = db.transaction()?;
    let Some(workspace) = owner(&tx, &source.track, &source.session)? else {
        return Ok(false);
    };
    if serde_json::to_value(workspace)? != serde_json::to_value(&source.workspace)? {
        return Ok(false);
    }
    if let Some(old) = read(&tx, source.id)? {
        if old.track != source.track
            || old.session != source.session
            || old.turn != source.turn
            || old.calls != source.calls
        {
            return Err("A captured step identity was reused.".into());
        }
        return Ok(true);
    }
    let record = serde_json::to_string(source)?;
    tx.execute(
        "INSERT OR IGNORE INTO step_capture_turns VALUES(?1,?2,?3)",
        params![source.session, source.turn, source.track],
    )?;
    tx.execute(
        "INSERT INTO step_file_sources VALUES(?1,?2,?3,?4,?5)",
        params![
            source.id.to_string(),
            source.track,
            source.session,
            source.turn,
            record
        ],
    )?;
    for path in paths {
        tx.execute(
            "INSERT INTO step_source_paths VALUES(?1,?2)",
            params![source.id.to_string(), path],
        )?;
    }
    tx.execute(
        "DELETE FROM step_capture_active WHERE session=?1 AND turn=?2",
        params![source.session, source.turn],
    )?;
    bind_ready(&tx, &source.session, &source.turn)?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'step.files.captured',?2)",
        params![source.at.epoch_ms, record],
    )?;
    tx.commit()?;
    Ok(true)
}
