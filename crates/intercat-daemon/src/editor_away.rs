//! Read the durable speech and observed activity for one absence interval.
use crate::{Result, editor_log::Entry};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Query {
    pub from: i64,
    pub to: i64,
    pub before: Option<i64>,
}

pub(crate) fn read(db: &Connection, query: Query) -> Result<Value> {
    if query.from < 0
        || query.to <= query.from
        || query.to > 9_007_199_254_740_991
        || query.before.is_some_and(|id| id <= 0)
    {
        return Err("Invalid Editor away interval".into());
    }
    let updates: i64 = db.query_row(
        "SELECT count(*) FROM editor_entries WHERE created_at>?1 AND created_at<=?2 AND scene<>'interpretation'",
        params![query.from, query.to], |row| row.get(0),
    )?;
    let effects: i64 = db.query_row(
        "SELECT count(*) FROM effects WHERE json_extract(record,'$.at')>?1 AND json_extract(record,'$.at')<=?2",
        params![query.from, query.to], |row| row.get(0),
    )?;
    let clips: i64 = db.query_row(
        "SELECT count(*) FROM timeline_clips WHERE json_extract(record,'$.end.epochMs')>?1 AND json_extract(record,'$.end.epochMs')<=?2",
        params![query.from, query.to], |row| row.get(0),
    )?;
    // This is the current answer gate, not a claim about task completion.
    let waiting = db.prepare(
        "SELECT t.label FROM tracks t LEFT JOIN workers w ON w.track_id=t.id
        WHERE json_extract(w.record,'$.phase') IN ('approval','question','waiting-secrets','needs-recovery','lost','after-connection-loss')
        OR EXISTS(SELECT 1 FROM report_questions q WHERE q.track_id=t.id AND q.state IN ('open','queued','held'))
        ORDER BY t.rowid",
    )?.query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut entries = db.prepare(
        "SELECT id,track,scene,input,title,line,detail,created_at,expires_at FROM editor_entries
        WHERE created_at>?1 AND created_at<=?2 AND scene<>'interpretation' AND (?3 IS NULL OR id<?3)
        ORDER BY id DESC LIMIT 51",
    )?.query_map(params![query.from, query.to, query.before], |row| Ok(Entry {
        id: row.get(0)?, track_id: row.get(1)?, scene: row.get(2)?, input_id: row.get(3)?,
        title: row.get(4)?, line: row.get(5)?, detail: row.get(6)?, created_at: row.get(7)?, expires_at: row.get(8)?,
    }))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let more = entries.len() > 50;
    entries.truncate(50);
    let before = more.then(|| entries.last().unwrap().id);
    Ok(
        json!({"updates":updates,"effects":effects,"clips":clips,"waiting":waiting,"entries":entries,"before":before}),
    )
}
