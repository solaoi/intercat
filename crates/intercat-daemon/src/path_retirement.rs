//! Admission holds precede Stop and publication. A failed pre-publication
//! operation releases only this hold; ordinary Stop and held words remain.
use crate::{Result, checkpoint::Checkpoint, integration::Job, preparation::Workspace};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    Aside,
    LeaveOut,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    Stopping,
    Saving,
    Ready,
    Aside,
    Released,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Saved {
    pub session_id: Option<String>,
    pub checkpoint: Option<Checkpoint>,
    pub reference: String,
    pub sha: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Entry {
    pub id: Uuid,
    pub job: Uuid,
    pub publication: Uuid,
    pub session: String,
    pub folder: String,
    pub track: String,
    pub label: String,
    pub kind: Kind,
    pub phase: Phase,
    pub workspace: Workspace,
    pub project_root: std::path::PathBuf,
    pub slug: String,
    pub saved: Option<Saved>,
    pub choice: Option<crate::branch_choices::Choice>,
    pub error: Option<String>,
}
pub(crate) enum Change {
    Freeze {
        id: Uuid,
        session: Option<String>,
        checkpoint: Option<Checkpoint>,
    },
    Ready(Uuid),
    Failed {
        id: Uuid,
        error: String,
    },
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE path_retirements(id TEXT PRIMARY KEY,job TEXT NOT NULL,track TEXT NOT NULL,active INTEGER NOT NULL,record TEXT NOT NULL,UNIQUE(job,track));
        CREATE UNIQUE INDEX one_active_path_retirement ON path_retirements(track) WHERE active=1;")?;
    Ok(())
}
pub(crate) fn holds(db: &Connection, track: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM path_retirements WHERE track=?1 AND active=1)",
        [track],
        |row| row.get(0),
    )?)
}
pub(crate) fn already_aside(
    db: &Connection,
    track: &str,
    choice: &crate::branch_choices::Choice,
) -> Result<bool> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM path_retirements WHERE track=?1 AND active=1",
            [track],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else { return Ok(false) };
    let entry: Entry = serde_json::from_str(&raw)?;
    Ok(entry.phase == Phase::Aside
        && entry.kind == Kind::Aside
        && entry.choice.as_ref().is_some_and(|old| {
            old.root == choice.root
                && old.kept == choice.kept
                && old.retired.get(track) == choice.retired.get(track)
        }))
}
pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Entry> {
    let raw: String = db.query_row(
        "SELECT record FROM path_retirements WHERE id=?1",
        [id.to_string()],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&raw)?)
}
pub(crate) fn save(db: &Connection, entry: &Entry) -> Result<()> {
    db.execute("INSERT INTO path_retirements VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET active=excluded.active,record=excluded.record",params![entry.id.to_string(),entry.job.to_string(),entry.track,entry.phase!=Phase::Released,serde_json::to_string(entry)?])?;
    db.execute("INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'path.retirement',?3)",params![crate::journal::timestamp(),entry.session,serde_json::json!({"id":entry.id,"job":entry.job,"track":entry.track,"phase":entry.phase}).to_string()])?;
    Ok(())
}
pub(crate) fn hold_words(db: &Connection) -> Result<()> {
    db.execute("UPDATE utterances SET state='held',revision=revision+1,reason='Held while this path is set aside' WHERE state IN ('staged','reading','placed') AND coalesce(json_extract(decision,'$.kind'),'') NOT IN ('park','command') AND EXISTS(SELECT 1 FROM path_retirements p WHERE p.active=1 AND (p.track=json_extract(utterances.decision,'$.trackId') OR (utterances.decision IS NULL AND p.track=utterances.provisional)))",[])?;
    Ok(())
}
pub(crate) fn begin(db: &Connection, job: &Job, publication: Uuid) -> Result<()> {
    let state = job
        .path_state
        .as_ref()
        .ok_or("Missing saved path review.")?;
    if !state.ready() {
        return Err("Choose a path and review its affected dependencies before committing.".into());
    }
    let targets = state
        .retired
        .keys()
        .map(|id| (id.as_str(), Kind::Aside))
        .chain(
            state
                .descendants
                .iter()
                .filter(|child| !child.included)
                .map(|child| (child.track.as_str(), Kind::LeaveOut)),
        );
    for (track, kind) in targets {
        // Historical continuation segments share the current writer’s files.
        // Their mathematical cutoff remains in Choice, never a physical Stop/Restore.
        if crate::continuation::closed(db, track)? {
            continue;
        }
        let choice = job
            .path_review
            .choices
            .values()
            .find(|choice| choice.retired.contains_key(track))
            .cloned();
        if kind == Kind::Aside
            && let Some(choice) = &choice
            && already_aside(db, track, choice)?
        {
            continue;
        }
        let id = Uuid::new_v5(&publication, format!("retire/{track}").as_bytes());
        let old: Option<String> = db
            .query_row(
                "SELECT record FROM path_retirements WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(old) = old {
            let old: Entry = serde_json::from_str(&old)?;
            if old.job != job.id
                || old.track != track
                || old.publication != publication
                || old.kind != kind
            {
                return Err("The saved path retirement changed.".into());
            }
            continue;
        }
        if holds(db, track)? {
            return Err("Restore this path before choosing it in another replacement.".into());
        }
        let raw: String = db.query_row(
            "SELECT record FROM workspaces WHERE track_id=?1",
            [track],
            |row| row.get(0),
        )?;
        let workspace: Workspace = serde_json::from_str(&raw)?;
        let label: String =
            db.query_row("SELECT label FROM tracks WHERE id=?1", [track], |row| {
                row.get(0)
            })?;
        if workspace.reference != format!("refs/intercat/{}/tracks/{label}", job.slug)
            || crate::folders::track(db, track)?.id() != job.project.id()
        {
            return Err("The retired path belongs to another folder or session.".into());
        }
        save(
            db,
            &Entry {
                id,
                job: job.id,
                publication,
                session: job.session_id.clone(),
                folder: job.project.id.clone(),
                track: track.into(),
                label,
                kind,
                phase: Phase::Stopping,
                workspace,
                project_root: job.project.root.clone(),
                slug: job.slug.clone(),
                saved: None,
                choice,
                error: None,
            },
        )?;
    }
    hold_words(db)
}
pub(crate) fn pending(db: &Connection, job: Uuid) -> Result<bool> {
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM path_retirements WHERE job=?1 AND active=1 AND json_extract(record,'$.phase') NOT IN ('ready','aside'))",[job.to_string()],|row|row.get(0))?)
}
pub(crate) fn finish(db: &Connection, job: Uuid, published: bool) -> Result<()> {
    let ids = db
        .prepare("SELECT id FROM path_retirements WHERE job=?1 AND active=1")?
        .query_map([job.to_string()], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let mut entry = read(db, Uuid::parse_str(&id)?)?;
        if published && !matches!(entry.phase, Phase::Ready | Phase::Aside) {
            return Err("Confirm every stopped path before publishing its replacement.".into());
        }
        entry.phase = if published && entry.kind == Kind::Aside {
            Phase::Aside
        } else {
            Phase::Released
        };
        save(db, &entry)?;
    }
    Ok(())
}
pub(crate) fn snapshot(db: &Connection) -> Result<Vec<Entry>> {
    db.prepare("SELECT record FROM path_retirements WHERE active=1 ORDER BY rowid")?
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect()
}
pub(crate) fn change(db: &mut Connection, change: Change) -> Result<()> {
    let tx = db.transaction()?;
    match change {
        Change::Freeze {
            id,
            session,
            checkpoint,
        } => {
            let mut entry = read(&tx, id)?;
            if entry.phase == Phase::Released {
                return Err("This path retirement is no longer active.".into());
            }
            let worker: Option<String> = tx
                .query_row(
                    "SELECT record FROM workers WHERE track_id=?1",
                    [&entry.track],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(raw) = &worker {
                let worker: crate::runtime::WorkerState = serde_json::from_str(raw)?;
                if worker.session_id != session
                    || worker.active_turn.is_some()
                    || worker.checkpoint_pending
                    || !worker.approvals.is_empty()
                    || !worker.prompts.is_empty()
                    || serde_json::to_value(worker.checkpoint)?
                        != serde_json::to_value(&checkpoint)?
                {
                    return Err(
                        "The stopped worker boundary changed before saving its path.".into(),
                    );
                }
            } else if session.is_some() || checkpoint.is_some() {
                return Err("The stopped worker is unavailable.".into());
            }
            let checkpoint = if worker.is_none() {
                let raw:Option<String>=tx.query_row("SELECT json_extract(payload,'$.checkpoint') FROM operations WHERE kind='checkpoint.save' AND state='complete' AND json_extract(payload,'$.trackId')=?1 ORDER BY rowid DESC LIMIT 1",[&entry.track],|row|row.get(0)).optional()?;
                raw.map(|raw| serde_json::from_str::<Checkpoint>(&raw))
                    .transpose()?
            } else {
                checkpoint
            };
            let saved = Saved {
                session_id: session,
                sha: checkpoint
                    .as_ref()
                    .map(|cp| cp.sha.clone())
                    .unwrap_or_else(|| entry.workspace.base.clone()),
                checkpoint,
                reference: format!(
                    "refs/intercat/{}/aside/{}/{}",
                    entry.slug, entry.label, entry.id
                ),
            };
            if let Some(old) = &entry.saved
                && serde_json::to_value(old)? != serde_json::to_value(&saved)?
            {
                return Err("The frozen path retirement changed.".into());
            }
            if entry.phase == Phase::Stopping {
                entry.saved = Some(saved);
                entry.phase = Phase::Saving;
                entry.error = None;
                save(&tx, &entry)?;
            }
        }
        Change::Ready(id) => {
            let mut entry = read(&tx, id)?;
            if entry.phase == Phase::Saving {
                entry.phase = Phase::Ready;
                entry.error = None;
                save(&tx, &entry)?;
            } else if !matches!(entry.phase, Phase::Ready | Phase::Aside) {
                return Err(
                    "Save the protected path reference before confirming its retirement.".into(),
                );
            }
        }
        Change::Failed { id, error } => {
            let mut entry = read(&tx, id)?;
            if matches!(entry.phase, Phase::Stopping | Phase::Saving)
                && entry.error.as_ref() != Some(&error)
            {
                entry.error = Some(error);
                save(&tx, &entry)?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}
