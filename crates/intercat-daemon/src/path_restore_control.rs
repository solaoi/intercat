use crate::{
    Result,
    path_restore::{Control, ControlAction, Job, Phase},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Followup {
    pub request_id: uuid::Uuid,
    pub revision: u64,
    pub track: String,
    pub label: String,
    pub target: String,
    pub source: crate::realign::Source,
}
fn followup(db: &Connection, job: &Job) -> Result<Option<Followup>> {
    let current: Option<String> = db
        .query_row(
            "SELECT json_extract(value,'$.id') FROM settings WHERE key='session'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if current.as_deref() != Some(job.session_id.as_str()) {
        return Ok(None);
    }
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM track_lineage WHERE track=?1)",
        [&job.track_id],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(None);
    }
    let Some(restored) = job.restored.as_ref().filter(|r| r.suggest_realign) else {
        return Ok(None);
    };
    if job.phase != Phase::Done
        || crate::path_retirement::holds(db, &job.track_id)?
        || crate::lineage_store::basis(db, &job.track_id)?.base != restored.base
    {
        return Ok(None);
    }
    let Some((app, _)) = crate::path_restore_store::application(db, &job.request)? else {
        return Err("A completed Restore has no saved file intent.".into());
    };
    let target: String = db.query_row(
        "SELECT head FROM published_lineage WHERE reference=?1",
        [&app.source.target_ref],
        |row| row.get(0),
    )?;
    if target == restored.base {
        return Ok(None);
    }
    let dismissed:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind='path.restore-control' AND state='complete' AND json_extract(payload,'$.id')=?1 AND json_extract(payload,'$.action.kind')='dismissRealign' AND json_extract(payload,'$.action.head')=?2)",params![job.request.op_id.to_string(),target],|row|row.get(0))?;
    if dismissed {
        return Ok(None);
    }
    let id:Option<String>=db.query_row("SELECT id FROM integration_jobs WHERE session=?1 AND json_extract(record,'$.project.id')=?2 AND json_extract(record,'$.status')='done' AND json_extract(record,'$.candidate.head')=?3 ORDER BY rowid DESC LIMIT 1",params![job.session_id,job.folder_id,target],|row|row.get(0)).optional()?;
    let Some(id) = id else {
        return Ok(None);
    };
    let publication = crate::integration_store::read(db, id.parse()?)?;
    Ok(Some(Followup {
        request_id: job.request.op_id,
        revision: job.revision,
        track: job.track_id.clone(),
        label: app.source.entry.label,
        target,
        source: crate::realign::Source::Commit {
            job_id: publication.id,
            revision: publication.revision,
        },
    }))
}
pub(crate) fn snapshot(db: &Connection) -> Result<Vec<Followup>> {
    crate::path_restore_store::snapshot(db)?
        .iter()
        .map(|job| followup(db, job))
        .collect::<Result<Vec<_>>>()
        .map(|rows| rows.into_iter().flatten().collect())
}
pub(crate) fn apply(db: &Connection, control: Control) -> Result<Job> {
    if control.op_id.get_version_num() != 7 {
        return Err("Invalid Restore control ID.".into());
    }
    let old: Option<(String, String)> = db
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [control.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, raw)) = old {
        if kind != "path.restore-control" || serde_json::from_str::<Control>(&raw)? != control {
            return Err("Restore control operation ID reused.".into());
        }
        return crate::path_restore_store::read_id(db, control.id);
    }
    let mut job = crate::path_restore_store::read_id(db, control.id)?;
    if job.revision != control.revision {
        return Err("The Restore changed. Review its current state.".into());
    }
    match &control.action {
        ControlAction::KeepProcesses => {
            if job.phase != Phase::WaitingForProcesses {
                return Err("Review the current Restore before keeping its processes.".into());
            }
            job.keep_processes = true;
            crate::path_restore_store::save_job(db, &mut job)?;
        }
        ControlAction::DismissRealign { head } => {
            head.parse::<intercat_git::ObjectId>()?;
            if followup(db, &job)?.is_none_or(|item| item.target != *head) {
                return Err("The realignment suggestion changed.".into());
            }
        }
        ControlAction::Retry | ControlAction::Hold => {
            if matches!(job.phase, Phase::Done | Phase::Applying)
                || crate::repair_run_store::active(db, job.request.op_id)?
            {
                return Err(
                    "Finish or stop the active Restore merge before changing this request.".into(),
                );
            }
            job.phase = if control.action == ControlAction::Hold {
                Phase::Held
            } else {
                Phase::Queued
            };
            job.error = None;
            job.inspection = None;
            crate::path_restore_store::save_job(db, &mut job)?;
        }
    }
    db.execute(
        "INSERT INTO operations VALUES(?1,'path.restore-control',?2,'complete')",
        params![control.op_id.to_string(), serde_json::to_string(&control)?],
    )?;
    db.execute(
        "INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'path.restore-control',?3)",
        params![
            crate::journal::timestamp(),
            job.session_id,
            serde_json::to_string(&control)?
        ],
    )?;
    Ok(job)
}
