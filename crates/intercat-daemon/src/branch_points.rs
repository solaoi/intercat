//! Read-only, paged saved branch points. Confirmation reuses branch's source checks.
use crate::{Result, checkpoint::Checkpoint};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const PAGE_SIZE: usize = 50;

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Query {
    pub track_id: String,
    pub before: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Point {
    pub id: i64,
    pub turn_id: uuid::Uuid,
    pub checkpoint: String,
    pub ended_at: Option<i64>,
    pub timeline_at: Option<i64>,
    pub shell_was_running: bool,
    pub available: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub points: Vec<Point>,
    pub before: Option<i64>,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE INDEX IF NOT EXISTS branch_point_track ON operations(json_extract(payload,'$.trackId')) WHERE kind='checkpoint.save' AND state='complete';")?;
    Ok(())
}

pub(crate) fn read(db: &Connection, query: &Query) -> Result<Page> {
    if query.track_id.is_empty()
        || query.track_id.len() > 128
        || query.before.is_some_and(|n| n <= 0)
    {
        return Err("Invalid branch point query.".into());
    }
    if !db.query_row(
        "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)",
        [&query.track_id],
        |row| row.get::<_, bool>(0),
    )? {
        return Err("This track is no longer available.".into());
    }
    let mut statement = db.prepare("SELECT rowid,payload FROM operations WHERE kind='checkpoint.save' AND state='complete' AND json_extract(payload,'$.trackId')=?1 AND (?2 IS NULL OR rowid<?2) ORDER BY rowid DESC LIMIT ?3")?;
    let mut rows = statement
        .query_map(
            params![query.track_id, query.before, (PAGE_SIZE + 1) as i64],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_more = rows.len() > PAGE_SIZE;
    rows.truncate(PAGE_SIZE);
    let before = has_more.then(|| rows.last().unwrap().0);
    let mut points = Vec::with_capacity(rows.len());
    for (id, raw) in rows {
        let value: Value = serde_json::from_str(&raw)?;
        let checkpoint: Checkpoint = serde_json::from_value(value["checkpoint"].clone())?;
        let source = crate::branch::source(db, &query.track_id, checkpoint.turn_id);
        let session = source
            .as_ref()
            .ok()
            .map(|source| source.session_id.as_str());
        let (ended_at,timeline_at,unknown_time) = db.query_row("SELECT max(json_extract(record,'$.end.epochMs')),max(json_extract(record,'$.end.timelineMs')),coalesce(max(coalesce(json_extract(record,'$.recovered'),0) OR coalesce(json_extract(record,'$.start.bootId')<>json_extract(record,'$.end.bootId'),1)),1) FROM timeline_clips WHERE track=?1 AND json_extract(record,'$.turnId')=?2 AND json_extract(record,'$.repairRunId') IS NULL AND (?3 IS NULL OR json_extract(record,'$.sessionId')=?3)",params![query.track_id,checkpoint.turn_id.to_string(),session],|row|Ok((row.get(0)?,row.get(1)?,row.get::<_,bool>(2)?)))?;
        points.push(Point {
            id,
            turn_id: checkpoint.turn_id,
            checkpoint: checkpoint.sha,
            ended_at: if unknown_time { None } else { ended_at },
            timeline_at: if unknown_time { None } else { timeline_at },
            shell_was_running: checkpoint.live_processes,
            available: source.is_ok(),
        });
    }
    Ok(Page { points, before })
}
