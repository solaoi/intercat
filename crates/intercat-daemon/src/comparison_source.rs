//! Only journal-owned, completed checkpoints can become comparison inputs.
use crate::{
    Result,
    checkpoint::Checkpoint,
    comparison::{Open, Source},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Capture {
    pub request: Open,
    pub session: String,
    pub folder: crate::project::Folder,
    pub slug: String,
    pub base: crate::branch_start::Point,
    pub sources: [Source; 2],
    pub environment: crate::check_environment::Selection,
    pub checks: crate::check_settings::Setting,
    pub denied_roots: Vec<std::path::PathBuf>,
}
fn source(
    db: &Connection,
    point: &crate::comparison::Point,
    slug: &str,
) -> Result<(Source, std::path::PathBuf)> {
    let label: String = db.query_row(
        "SELECT label FROM tracks WHERE id=?1",
        [&point.track_id],
        |row| row.get(0),
    )?;
    let mut query=db.prepare("SELECT payload FROM operations WHERE kind='checkpoint.save' AND state='complete' AND json_extract(payload,'$.trackId')=?1 AND json_extract(payload,'$.checkpoint.sha')=?2 LIMIT 2")?;
    let rows = query
        .query_map(params![point.track_id, point.sha], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let [raw] = rows.as_slice() else {
        return Err("Choose one completed checkpoint from this path.".into());
    };
    let payload: Value = serde_json::from_str(raw)?;
    let checkpoint: Checkpoint = serde_json::from_value(payload["checkpoint"].clone())?;
    let saved = crate::branch::source(db, &point.track_id, checkpoint.turn_id)?;
    if saved.workspace.reference != format!("refs/intercat/{slug}/tracks/{label}")
        || serde_json::to_value(&saved.checkpoint)? != serde_json::to_value(&checkpoint)?
    {
        return Err("This saved checkpoint belongs to another session.".into());
    }
    Ok((
        Source {
            point: point.clone(),
            label,
            checkpoint,
        },
        saved.workspace.path,
    ))
}
pub(crate) fn capture(db: &Connection, request: Open) -> Result<Capture> {
    request.validate()?;
    let raw: String = db.query_row(
        "SELECT value FROM settings WHERE key='session'",
        [],
        |row| row.get(0),
    )?;
    let session: Value = serde_json::from_str(&raw)?;
    if session["bound"] != true {
        return Err("Wait for the session to be ready.".into());
    }
    let session = session["id"]
        .as_str()
        .ok_or("Missing comparison session.")?
        .to_owned();
    let folder = crate::folders::track(db, &request.parent.track_id)?;
    if crate::folders::track(db, &request.child.track_id)?.id != folder.id {
        return Err("Compare paths in the same folder.".into());
    }
    let slug: String = db.query_row(
        "SELECT slug FROM session_folders WHERE session=?1 AND folder=?2",
        params![session, folder.id],
        |row| row.get(0),
    )?;
    let branches = crate::branch::snapshot(db)?;
    let child_writer = crate::continuation::current(db, &request.child.track_id)?;
    let mut selected = None;
    for branch in &branches {
        if branch.phase == crate::branch::Phase::Ready
            && crate::continuation::current(db, &branch.track_id)? == child_writer
        {
            if selected.is_some() {
                return Err("This path has more than one branch source.".into());
            }
            selected = Some(branch);
        }
    }
    let branch = selected.ok_or("This path has no ready branch source.")?;
    if crate::continuation::current(db, &branch.request.parent_track_id)?
        != crate::continuation::current(db, &request.parent.track_id)?
    {
        return Err("Compare a branch with its recorded parent.".into());
    }
    if crate::folders::track(db, &branch.request.parent_track_id)?.id != folder.id
        || crate::folders::track(db, &branch.track_id)?.id != folder.id
    {
        return Err("The saved branch belongs to another folder.".into());
    }
    let (_, original, _) = crate::branch_start::resolve(db, &branch.request)?;
    if original != branch.point {
        return Err("The saved branch source changed.".into());
    }
    let base =
        crate::lineage_store::point(db, &branch.request.parent_track_id, branch.point.sha())?;
    let parent = crate::lineage_store::point(db, &request.parent.track_id, &request.parent.sha)?;
    let coverage = if parent.track == base.track {
        Some(parent.inherited())
    } else {
        parent.inherits.get(&base.track).cloned()
    };
    if coverage.is_none_or(|mark| {
        mark.number < base.number || mark.number == base.number && mark.sha != base.sha
    }) {
        return Err("Choose the parent at or after the saved branch point.".into());
    }
    let (left, left_path) = source(db, &request.parent, &slug)?;
    let (right, right_path) = source(db, &request.child, &slug)?;
    let sources = [left, right];
    let denied_roots = vec![left_path, right_path];
    Ok(Capture {
        request,
        session,
        folder,
        slug,
        base: branch.point.clone(),
        sources,
        environment: crate::check_environment::selection(db)?,
        checks: crate::check_settings::read(db)?,
        denied_roots,
    })
}
