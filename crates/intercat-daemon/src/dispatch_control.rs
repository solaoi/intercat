//! Durable application-wide pause; resuming never releases held inputs.
use crate::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};

/// The pause bit and its epoch change atomically so an old Resume cannot
/// clear a newer Stop, and queued starts cannot revive after a resume.
#[derive(Debug)]
pub struct Gate {
    state: AtomicU64,
}
impl Gate {
    pub fn new(paused: bool) -> Self {
        Self {
            state: AtomicU64::new(u64::from(paused)),
        }
    }
    pub fn paused(&self) -> bool {
        self.state.load(Ordering::Acquire) & 1 == 1
    }
    pub(crate) fn epoch(&self) -> u64 {
        self.state.load(Ordering::Acquire) >> 1
    }
    pub(crate) fn begin(&self, pause: bool) -> u64 {
        let old = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                Some((((old >> 1) + 1) << 1) | u64::from(pause || old & 1 == 1))
            })
            .unwrap();
        (old >> 1) + 1
    }
    pub(crate) fn resume(&self, epoch: u64) -> bool {
        self.state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                (old >> 1 == epoch).then_some(old & !1)
            })
            .is_ok()
    }
}

pub(crate) fn paused(connection: &Connection) -> Result<bool> {
    let value: Option<String> = connection
        .query_row(
            "SELECT value FROM settings WHERE key='dispatchPaused'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| serde_json::from_str(&value).map_err(Into::into))
        .unwrap_or(Ok(false))
}
pub(crate) fn history(connection: &Connection) -> Result<Vec<(uuid::Uuid, bool)>> {
    let mut statement =
        connection.prepare("SELECT id,payload FROM operations WHERE kind='system.pause'")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (id, payload) = row?;
        let value: serde_json::Value = serde_json::from_str(&payload)?;
        Ok((
            id.parse()?,
            value["paused"].as_bool().ok_or("Invalid pause receipt.")?,
        ))
    })
    .collect()
}

pub(crate) fn set(
    connection: &mut Connection,
    id: &str,
    pause: bool,
    unlogged: bool,
) -> Result<()> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let tx = connection.transaction()?;
    let payload = json!({"paused":pause}).to_string();
    if let Some((kind, old)) = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if kind != "system.pause" || old != payload {
            return Err("Operation ID reused.".into());
        }
        return Ok(());
    }
    tx.execute("INSERT INTO settings VALUES('dispatchPaused',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[pause.to_string()])?;
    if pause {
        crate::plan_store::hold_all(&tx)?;
        crate::review_store::hold_all(&tx)?;
        tx.execute("UPDATE utterances SET state='held',revision=revision+1,reason='Turns stopped' WHERE state IN ('staged','reading','placed','held') AND coalesce(json_extract(decision,'$.kind'),'') NOT IN ('park','command')",[])?;
    }
    tx.execute(
        "INSERT INTO operations VALUES(?1,'system.pause',?2,'complete')",
        params![id, payload],
    )?;
    let event = if unlogged {
        mark_uncertain(&tx, None)?;
        json!({"paused":pause,"opId":id,"unlogged":true}).to_string()
    } else {
        payload
    };
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'dispatch.paused',?2)",
        params![crate::journal::timestamp(), event],
    )?;
    tx.commit()?;
    Ok(())
}

/// A new user request can release a local hold. Requests that existed before
/// the hold, including a classification that finishes late, cannot release it.
pub(crate) fn local_resumes(connection: &Connection) -> Result<serde_json::Value> {
    let mut statement = connection.prepare(
        "SELECT json_extract(h.payload,'$.trackId'),h.id FROM operations h
         WHERE h.kind='worker.hold'
         AND NOT EXISTS(SELECT 1 FROM operations later WHERE later.kind='worker.hold'
             AND later.rowid>h.rowid AND json_extract(later.payload,'$.trackId')=json_extract(h.payload,'$.trackId'))
         AND EXISTS(SELECT 1 FROM utterances u JOIN operations request ON request.id=u.id
             WHERE request.kind='utterance.create' AND request.rowid>h.rowid
             AND NOT EXISTS(SELECT 1 FROM review_fix_inputs fi WHERE fi.input=u.id)
             AND u.state='placed' AND json_extract(u.decision,'$.trackId')=json_extract(h.payload,'$.trackId'))",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut resumes = serde_json::Map::new();
    for row in rows {
        let (track, hold) = row?;
        resumes.insert(track, hold.into());
    }
    Ok(resumes.into())
}

pub(crate) fn mark_uncertain(connection: &Connection, track: Option<&str>) -> Result<()> {
    connection.execute("UPDATE utterances SET state='held',revision=revision+1,reason='Held after a log failure' WHERE state IN ('staged','reading')",[])?;
    connection.execute("UPDATE workers SET record=json_set(record,'$.phase','needs-recovery','$.detail','A stop was sent while the log was unavailable. Review the recovered work before resuming.') WHERE (?1 IS NULL OR track_id=?1) AND (json_extract(record,'$.activeTurn') IS NOT NULL OR json_extract(record,'$.checkpointPending')=1)",[track])?;
    connection.execute("UPDATE tracks SET status='waiting' WHERE (?1 IS NULL OR id=?1) AND id IN (SELECT track_id FROM workers WHERE json_extract(record,'$.phase')='needs-recovery')",[track])?;
    Ok(())
}
pub(crate) fn unlogged(connection: &Connection) -> Result<Vec<serde_json::Value>> {
    let mut query=connection.prepare("SELECT payload FROM events WHERE kind IN ('worker.hold','dispatch.paused') AND json_extract(payload,'$.unlogged')=1 ORDER BY seq DESC LIMIT 32")?;
    let rows = query.query_map([], |row| row.get::<_, String>(0))?;
    rows.map(|row| Ok(serde_json::from_str(&row?)?)).collect()
}
