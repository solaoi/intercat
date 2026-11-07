//! Frozen assumptions belonging to the checkpoints reviewed by an integration job.
use crate::{Result, assumptions::Assumption, integration::Job};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Review {
    #[serde(flatten)]
    pub assumption: Assumption,
    pub eligible: bool,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS integration_assumptions(
        job TEXT NOT NULL, assumption TEXT NOT NULL, record TEXT NOT NULL,
        PRIMARY KEY(job,assumption));",
    )?;
    Ok(())
}

fn available(db: &Connection, job: &Job) -> Result<Vec<Review>> {
    let mut query = db.prepare("SELECT a.record,c.ordinal FROM assumptions a
        JOIN assumption_sources s ON s.id=a.source JOIN timeline_clips c ON c.id=a.clip
        WHERE a.state IN ('open','kept') AND instr(s.text,json_extract(a.record,'$.evidence'))>0
        AND NOT EXISTS(SELECT 1 FROM integration_assumptions old JOIN integration_jobs j ON j.id=old.job
            WHERE old.assumption=a.id AND json_extract(j.record,'$.status')='done'
            AND json_extract(old.record,'$.eligible')=1
            AND EXISTS(SELECT 1 FROM json_each(j.record,'$.candidate.commits') committed
                WHERE json_extract(committed.value,'$.track')=json_extract(a.record,'$.trackId')))
        ORDER BY a.rowid")?;
    let rows = query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut result = Vec::new();
    for row in rows {
        let (record, ordinal) = row?;
        let assumption: Assumption = serde_json::from_str(&record)?;
        if let Some(track) = job
            .tracks
            .iter()
            .find(|track| track.id == assumption.track_id)
        {
            result.push(Review {
                eligible: track.last_clip.is_some_and(|last| ordinal <= last),
                assumption,
            });
        }
    }
    Ok(result)
}

pub(crate) fn capture(db: &Connection, job: &mut Job) -> Result<()> {
    job.assumptions = available(db, job)?;
    db.execute(
        "DELETE FROM integration_assumptions WHERE job=?1",
        [job.id.to_string()],
    )?;
    for review in &job.assumptions {
        save(db, job.id, review)?;
    }
    Ok(())
}

pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Vec<Review>> {
    db.prepare("SELECT record FROM integration_assumptions WHERE job=?1 ORDER BY rowid")?
        .query_map([id.to_string()], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect()
}

fn save(db: &Connection, job: Uuid, review: &Review) -> Result<()> {
    db.execute(
        "INSERT INTO integration_assumptions VALUES(?1,?2,?3)
        ON CONFLICT(job,assumption) DO UPDATE SET record=excluded.record",
        params![
            job.to_string(),
            review.assumption.id,
            serde_json::to_string(review)?
        ],
    )?;
    Ok(())
}

pub(crate) fn included(job: &Job, review: &Review) -> bool {
    review.eligible
        && job.tracks.iter().any(|track| {
            track.id == review.assumption.track_id
                && track.included
                && job.candidate.as_ref().is_none_or(|candidate| {
                    candidate
                        .commits
                        .iter()
                        .chain(candidate.blocked_commit.iter())
                        .any(|commit| commit.track == track.id)
                })
        })
}

pub(crate) fn current(db: &Connection, job: &Job) -> Result<bool> {
    for review in job
        .assumptions
        .iter()
        .filter(|review| included(job, review))
    {
        let record: Option<String> = db.query_row("SELECT a.record FROM assumptions a JOIN assumption_sources s ON s.id=a.source
            WHERE a.id=?1 AND s.clip=a.clip AND instr(s.text,json_extract(a.record,'$.evidence'))>0",
            [&review.assumption.id], |row| row.get(0)).optional()?;
        if record
            .as_deref()
            .map(serde_json::from_str::<Assumption>)
            .transpose()?
            .is_none_or(|value| {
                value.revision != review.assumption.revision
                    || value.state != review.assumption.state
            })
        {
            return Ok(false);
        }
    }
    Ok(available(db, job)?
        .iter()
        .filter(|review| included(job, review))
        .all(|review| {
            job.assumptions
                .iter()
                .any(|saved| saved.assumption.id == review.assumption.id)
        }))
}

pub(crate) fn messages(job: &Job, original: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut messages = original.clone();
    for track in &job.tracks {
        let choices: Vec<_> = job
            .assumptions
            .iter()
            .filter(|review| {
                review.eligible
                    && review.assumption.track_id == track.id
                    && matches!(review.assumption.state.as_str(), "open" | "kept")
            })
            .map(|review| {
                format!(
                    "- {} — {}",
                    review.assumption.question, review.assumption.text
                )
            })
            .collect();
        if !choices.is_empty() {
            let body = messages
                .entry(track.id.clone())
                .or_insert_with(|| track.title.clone());
            body.push_str("\n\nAssumptions kept\n");
            body.push_str(&choices.join("\n"));
        }
    }
    messages
}

pub(crate) fn keep(db: &Connection, job: &mut Job, id: Option<&str>) -> Result<()> {
    if unfinished(db, job)? != (0, 0) {
        return Err("Read the included clips for assumptions before committing.".into());
    }
    if !current(db, job)? {
        return Err("Assumptions changed · Refresh to review them.".into());
    }
    let selected: Vec<_> = job
        .assumptions
        .iter()
        .enumerate()
        .filter(|(_, review)| {
            included(job, review)
                && review.assumption.state == "open"
                && id.is_none_or(|id| review.assumption.id == id)
        })
        .map(|(index, _)| index)
        .collect();
    if id.is_some() && selected.is_empty() {
        return Err("This assumption is no longer available in the candidate.".into());
    }
    for index in selected {
        let review = &mut job.assumptions[index];
        review.assumption.state = "kept".into();
        review.assumption.kept_at = Some(crate::journal::timestamp());
        review.assumption.revision += 1;
        save_answer(db, job.id, review)?;
    }
    Ok(())
}

fn save_answer(db: &Connection, job: Uuid, review: &Review) -> Result<()> {
    db.execute(
        "UPDATE assumptions SET state=?2,record=?3 WHERE id=?1",
        params![
            review.assumption.id,
            review.assumption.state,
            serde_json::to_string(&review.assumption)?
        ],
    )?;
    save(db, job, review)?;
    db.execute(
        "UPDATE timeline_clips SET record=record WHERE id=?1",
        [&review.assumption.clip_id],
    )?;
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'assumption.commit-answer',?2)",
        params![
            crate::journal::timestamp(),
            serde_json::json!({"job":job,"assumption":review.assumption}).to_string()
        ],
    )?;
    Ok(())
}

pub(crate) fn begin_change(db: &Connection, job: &mut Job, id: Uuid, revision: i64) -> Result<()> {
    if unfinished(db, job)? != (0, 0) {
        return Err("Wait for assumption reading before changing the candidate.".into());
    }
    if !current(db, job)? {
        return Err("Assumptions changed · Refresh to review them.".into());
    }
    let index = job
        .assumptions
        .iter()
        .position(|review| {
            included(job, review)
                && review.assumption.id == id.to_string()
                && review.assumption.revision == revision
                && review.assumption.state == "open"
        })
        .ok_or("The assumption changed or is not included in this candidate.")?;
    let review = &mut job.assumptions[index];
    crate::editor_log::assumption_changed(
        db,
        &format!("commit-assumption-change/{}/{id}/{revision}", job.id),
        &review.assumption.track_id,
        true,
    )?;
    review.assumption.state = "changing".into();
    review.assumption.revision += 1;
    save_answer(db, job.id, review)
}

pub(crate) fn finish_change(db: &Connection, job: Uuid, id: Uuid, completed: bool) -> Result<()> {
    let mut reviews = read(db, job)?;
    let review = reviews
        .iter_mut()
        .find(|review| review.assumption.id == id.to_string())
        .ok_or("Missing assumption for this repair.")?;
    if review.assumption.state != "changing" && (completed || review.assumption.state != "changed")
    {
        return Ok(());
    }
    review.assumption.state = if completed { "changed" } else { "open" }.into();
    review.assumption.changed_at = completed.then(crate::journal::timestamp);
    review.assumption.revision += 1;
    save_answer(db, job, review)
}

pub fn project(job: &Job) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(job)?;
    let assumptions = job
        .assumptions
        .iter()
        .map(|review| -> Result<_> {
            let mut value = serde_json::to_value(&review.assumption)?;
            value["included"] = included(job, review).into();
            Ok(value)
        })
        .collect::<Result<Vec<_>>>()?;
    value["assumptions"] = serde_json::to_value(assumptions)?;
    value["assumptionsChanged"] = job.assumptions_changed.into();
    value["assumptionsPending"] = job.assumptions_pending.into();
    value["assumptionsFailed"] = job.assumptions_failed.into();
    Ok(value)
}

pub(crate) fn discard_changes(db: &Connection, job: &Job) -> Result<()> {
    for review in &job.assumptions {
        if matches!(review.assumption.state.as_str(), "changing" | "changed") {
            finish_change(db, job.id, review.assumption.id.parse()?, false)?;
        }
    }
    Ok(())
}

pub(crate) fn unfinished(db: &Connection, job: &Job) -> Result<(usize, usize)> {
    let mut pending = 0;
    let mut failed = 0;
    for track in job.tracks.iter().filter(|track| {
        track.included
            && job.candidate.as_ref().is_none_or(|candidate| {
                candidate
                    .commits
                    .iter()
                    .chain(candidate.blocked_commit.iter())
                    .any(|commit| commit.track == track.id)
            })
    }) {
        let Some(last) = track.last_clip else {
            continue;
        };
        let counts: (i64, i64) = db.query_row(
            "SELECT
            coalesce(sum(s.state='running' OR s.attempts<2),0),
            coalesce(sum(s.state='pending' AND s.attempts>=2),0)
            FROM assumption_sources s JOIN timeline_clips c ON c.id=s.clip
            WHERE c.track=?1 AND c.ordinal<=?2 AND s.state<>'complete'
            AND json_extract(c.record,'$.repairRunId') IS NULL",
            params![track.id, last],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        pending += usize::try_from(counts.0)?;
        failed += usize::try_from(counts.1)?;
    }
    Ok((pending, failed))
}

pub(crate) fn retry(db: &Connection, job: &Job) -> Result<()> {
    for track in job.tracks.iter().filter(|track| track.included) {
        let Some(last) = track.last_clip else {
            continue;
        };
        db.execute(
            "UPDATE assumption_sources SET attempts=0 WHERE state='pending' AND attempts>=2
            AND clip IN (SELECT id FROM timeline_clips WHERE track=?1 AND ordinal<=?2)",
            params![track.id, last],
        )?;
    }
    Ok(())
}
