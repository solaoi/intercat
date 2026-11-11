//! Record the published commits in the same transaction as their lineage.
use crate::{Result, integration::Job, integration_record::Record};
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE integration_records(id TEXT PRIMARY KEY,job TEXT NOT NULL,session TEXT NOT NULL,folder TEXT NOT NULL,track TEXT NOT NULL,checkpoint INTEGER NOT NULL,ordinal INTEGER NOT NULL,position INTEGER NOT NULL,record TEXT NOT NULL,UNIQUE(job,track),UNIQUE(session,folder,track,ordinal),UNIQUE(session,folder,position));
        CREATE INDEX integration_records_track ON integration_records(session,folder,track,checkpoint);")?;
    Ok(())
}
pub(crate) fn published(db: &Connection, job: &Job, operation: Uuid) -> Result<()> {
    let candidate = job
        .candidate
        .as_ref()
        .ok_or("Missing published candidate.")?;
    let mut coverage = crate::lineage_store::coverage_before_publication(db, job)?;
    if let Some(undo) = &job.undo {
        crate::replacement_delta::rewind(&undo.records, &mut coverage);
    }
    let mut recorded = 0;
    for track in job.tracks.iter().filter(|track| track.included) {
        let Some(checkpoint) = &track.checkpoint else {
            continue;
        };
        let point = job.lineage.by_sha(&track.id, &checkpoint.sha)?;
        let before = coverage.clone();
        job.lineage.incorporate(&mut coverage, point)?;
        let Some(commit) = candidate
            .commits
            .iter()
            .find(|commit| commit.track == track.id)
        else {
            continue;
        };
        let ordinal = track
            .integration_count
            .checked_add(1)
            .ok_or("Integration record ordinal overflow.")?;
        let id = Uuid::new_v5(&job.id, commit.sha.as_bytes());
        let old: Option<String> = db
            .query_row(
                "SELECT record FROM integration_records WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        let old = old
            .map(|raw| serde_json::from_str::<Record>(&raw))
            .transpose()?;
        let position = if let Some(old) = &old {
            old.position
        } else {
            let previous:i64=db.query_row("SELECT coalesce(max(position),0) FROM integration_records WHERE session=?1 AND folder=?2",params![job.session_id,job.project.id],|row|row.get(0))?;
            u64::try_from(previous)?
                .checked_add(1)
                .ok_or("Published integration order overflow.")?
        };
        let record = Record {
            id,
            job_id: job.id,
            operation,
            session_id: job.session_id.clone(),
            folder_id: job.project.id.clone(),
            track_id: track.id.clone(),
            checkpoint: point.inherited(),
            ordinal,
            position,
            reference: format!(
                "refs/intercat/{}/integration/{}/{ordinal}",
                job.slug, track.label
            ),
            sha: commit.sha.clone(),
            parent: commit.parent.clone(),
            tree: commit.tree.clone(),
            coverage_before: before,
            coverage_after: coverage.clone(),
        };
        record.validate()?;
        if let Some(old) = old {
            if old != record {
                return Err("The published integration record changed.".into());
            }
        } else {
            db.execute(
                "INSERT INTO integration_records VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    record.id.to_string(),
                    job.id.to_string(),
                    job.session_id,
                    job.project.id,
                    track.id,
                    i64::try_from(record.checkpoint.number)?,
                    i64::try_from(ordinal)?,
                    i64::try_from(position)?,
                    serde_json::to_string(&record)?
                ],
            )?;
        }
        recorded += 1;
    }
    if recorded != candidate.commits.len() {
        return Err("A published commit has no integration record owner.".into());
    }
    Ok(())
}
