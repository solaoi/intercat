//! Cancellation becomes durable only in the successful publication transaction.
use crate::{Result, integration::Job, integration_record::Record};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Published {
    id: Uuid,
    job_id: Uuid,
    operation: Uuid,
    session_id: String,
    folder_id: String,
    reference: String,
    commit: crate::replacement_undo::Commit,
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE integration_undos(id TEXT PRIMARY KEY,job TEXT NOT NULL UNIQUE,session TEXT NOT NULL,folder TEXT NOT NULL,record TEXT NOT NULL);
        CREATE TABLE integration_cancelled(record TEXT PRIMARY KEY REFERENCES integration_records(id),undo TEXT NOT NULL REFERENCES integration_undos(id));")?;
    Ok(())
}
pub(crate) fn available(db: &Connection, job: &Job) -> Result<()> {
    let Some(plan) = &job.undo else {
        return Ok(());
    };
    plan.validate(job)?;
    for record in &plan.records {
        let saved: String = db.query_row(
            "SELECT record FROM integration_records WHERE id=?1",
            [record.id.to_string()],
            |row| row.get(0),
        )?;
        if serde_json::from_str::<Record>(&saved)? != *record {
            return Err("The replacement's saved integration record changed.".into());
        }
        let cancelled: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM integration_cancelled WHERE record=?1)",
            [record.id.to_string()],
            |row| row.get(0),
        )?;
        if cancelled {
            return Err(
                "This integration record was already undone. Refresh the replacement review."
                    .into(),
            );
        }
    }
    Ok(())
}
pub(crate) fn published(db: &Connection, job: &Job, operation: Uuid) -> Result<()> {
    let Some(plan) = &job.undo else {
        if job
            .candidate
            .as_ref()
            .is_some_and(|candidate| candidate.undo.is_some())
        {
            return Err("An undo commit has no frozen replacement plan.".into());
        }
        return Ok(());
    };
    let commit = job
        .candidate
        .as_ref()
        .and_then(|candidate| candidate.undo.as_ref())
        .ok_or("Missing published undo commit.")?;
    if commit.records != plan.ids()
        || commit.parent != plan.parent
        || !commit.scan.accepted(&job.kept_secrets)
    {
        return Err("The published undo no longer matches its reviewed records.".into());
    }
    let record = Published {
        id: Uuid::new_v5(&job.id, format!("undo/{}", commit.sha).as_bytes()),
        job_id: job.id,
        operation,
        session_id: job.session_id.clone(),
        folder_id: job.project.id.clone(),
        reference: format!("refs/intercat/{}/undo/{}", job.slug, job.id),
        commit: commit.clone(),
    };
    let encoded = serde_json::to_string(&record)?;
    let old: Option<String> = db
        .query_row(
            "SELECT record FROM integration_undos WHERE id=?1",
            [record.id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(old) = old {
        if old != encoded {
            return Err("The published undo receipt changed.".into());
        }
        for id in &commit.records {
            let owner: String = db.query_row(
                "SELECT undo FROM integration_cancelled WHERE record=?1",
                [id.to_string()],
                |row| row.get(0),
            )?;
            if owner != record.id.to_string() {
                return Err("The undo cancellation owner changed.".into());
            }
        }
        return Ok(());
    }
    available(db, job)?;
    db.execute(
        "INSERT INTO integration_undos VALUES(?1,?2,?3,?4,?5)",
        params![
            record.id.to_string(),
            job.id.to_string(),
            job.session_id,
            job.project.id,
            encoded
        ],
    )?;
    for id in &commit.records {
        db.execute(
            "INSERT INTO integration_cancelled VALUES(?1,?2)",
            params![id.to_string(), record.id.to_string()],
        )?;
    }
    Ok(())
}

/// Restore replays this publication's cancelled deltas for the retired writer
/// and its exact saved prefix. Later source checkpoints and earlier retirements
/// are excluded; a continued conversation keeps its closed source's changes.
pub(crate) fn restore_records(
    db: &Connection,
    entry: &crate::path_retirement::Entry,
    job: &Job,
    prefix: &crate::lineage::Ancestors,
) -> Result<Vec<Record>> {
    if job.id != entry.job
        || job.session_id != entry.session
        || job.project.id != entry.folder
        || job.status != crate::integration::Status::Done
    {
        return Err("The Restore source is not this path's published replacement.".into());
    }
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM integration_undos WHERE job=?1",
            [job.id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(plan) = &job.undo else {
        if raw.is_some()
            || job
                .candidate
                .as_ref()
                .is_some_and(|candidate| candidate.undo.is_some())
        {
            return Err("The retired path's undo has no saved replacement plan.".into());
        }
        return Ok(Vec::new());
    };
    let published: Published =
        serde_json::from_str(&raw.ok_or("Missing published replacement cancellation.")?)?;
    let commit = job
        .candidate
        .as_ref()
        .and_then(|candidate| candidate.undo.as_ref())
        .ok_or("Missing replacement undo commit.")?;
    if published.job_id != job.id
        || published.operation != entry.publication
        || published.session_id != entry.session
        || published.folder_id != entry.folder
        || published.id != Uuid::new_v5(&job.id, format!("undo/{}", commit.sha).as_bytes())
        || published.reference != format!("refs/intercat/{}/undo/{}", job.slug, job.id)
        || serde_json::to_value(&published.commit)? != serde_json::to_value(commit)?
        || commit.records != plan.ids()
    {
        return Err("The retired path's cancellation receipt changed.".into());
    }
    let mut selected = Vec::new();
    for record in &plan.records {
        record.validate()?;
        let (saved, owner): (String, String) = db.query_row(
            "SELECT r.record,c.undo FROM integration_records r JOIN integration_cancelled c ON c.record=r.id WHERE r.id=?1",
            [record.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if serde_json::from_str::<Record>(&saved)? != *record
            || owner != published.id.to_string()
            || record.session_id != entry.session
            || record.folder_id != entry.folder
        {
            return Err("The restored delta does not belong to this replacement.".into());
        }
        if record.track_id == entry.track
            || prefix
                .get(&record.track_id)
                .is_some_and(|through| record.checkpoint.number <= through.number)
        {
            selected.push(record.clone());
        }
    }
    Ok(selected)
}
