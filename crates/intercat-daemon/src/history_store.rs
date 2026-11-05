//! Durable view position advances only with the corresponding projection.
use crate::{Result, clock::Stamp};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};

pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE msp_positions(session TEXT PRIMARY KEY,track TEXT NOT NULL,cursor TEXT);
    CREATE TABLE msp_applied(session TEXT NOT NULL,cursor TEXT NOT NULL,sequence INTEGER NOT NULL REFERENCES events(seq),PRIMARY KEY(session,cursor));
    CREATE TABLE msp_gaps(source_seq INTEGER PRIMARY KEY REFERENCES events(seq),session TEXT NOT NULL,track TEXT NOT NULL,after TEXT NOT NULL,next TEXT NOT NULL);
    CREATE INDEX msp_gaps_session ON msp_gaps(session,source_seq);
    CREATE INDEX events_kind_session ON events(kind,session_id,seq);")?;
    Ok(())
}

pub fn cursor(db: &Connection, track: &str, session: &str) -> Result<Option<String>> {
    crate::session_retirement::ensure_open(db, session)?;
    let saved: Option<(String, Option<String>)> = db
        .query_row(
            "SELECT track,cursor FROM msp_positions WHERE session=?1",
            [session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match saved {
        Some((owner, cursor)) if owner == track => Ok(cursor),
        Some(_) => Err("The recovered session belongs to a different track.".into()),
        None => Ok(None),
    }
}

pub fn seen(db: &Connection, session: &str, cursor: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM msp_applied WHERE session=?1 AND cursor=?2) OR EXISTS(SELECT 1 FROM events WHERE session_id=?1 AND kind='history.snapshot' AND json_extract(payload,'$.placement')='opening' AND json_extract(payload,'$.cursor')=?2)",
        params![session, cursor],
        |row| row.get(0),
    )?)
}

pub fn floor(db: &Connection, session: &str) -> Result<Option<String>> {
    Ok(db.query_row("SELECT json_extract(payload,'$.cursor') FROM events WHERE session_id=?1 AND kind='history.snapshot' AND json_extract(payload,'$.placement')='opening' ORDER BY seq DESC LIMIT 1",[session],|row|row.get(0)).optional()?)
}

pub fn gap(db: &Connection, track: &str, session: &str) -> Result<Option<crate::history::Gap>> {
    cursor(db, track, session)?;
    let mut query=db.prepare("SELECT source_seq,after,next FROM msp_gaps WHERE session=?1 AND track=?2 ORDER BY source_seq")?;
    let rows = query.query_map(params![session, track], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut pending: Option<crate::history::Gap> = None;
    for row in rows {
        let (sequence, after, next) = row?;
        if let Some(gap) = &mut pending {
            if !gap.targets.iter().any(|target| target == &next) {
                gap.targets.push(next.clone());
                gap.next = next;
            }
            gap.through = sequence;
        } else {
            pending = Some(crate::history::Gap {
                after,
                next: next.clone(),
                through: sequence,
                targets: vec![next],
            });
        }
    }
    Ok(pending)
}

pub fn filled(db: &mut Connection, track: &str, session: &str, through: i64) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    cursor(&tx, track, session)?;
    let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE seq=?1 AND track_id=?2 AND session_id=?3 AND kind='msp.received' AND json_extract(payload,'$.method')='view/gap' AND json_extract(payload,'$.params.sessionId')=?3)",params![through,track,session],|row|row.get(0))?;
    if !valid {
        return Err("The filled gap has no matching receipt.".into());
    }
    let removed = tx.execute(
        "DELETE FROM msp_gaps WHERE session=?1 AND track=?2 AND source_seq<=?3",
        params![session, track, through],
    )?;
    if removed > 0 {
        let at = Stamp::now();
        tx.execute("INSERT INTO events(ts,mono,boot,position,track_id,session_id,kind,payload,source_seq) VALUES(?1,?2,?3,?4,?5,?6,'history.gapFilled',?7,?8)",params![at.epoch_ms,at.mono_ms,at.boot_id,at.timeline_ms,track,session,json!({"through":through}).to_string(),through])?;
        if let Some(pending) = gap(&tx, track, session)? {
            rewind(&tx, track, session, &pending.after)?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn compacted(
    db: &mut Connection,
    track: &str,
    session: &str,
    boundary: &str,
    source: i64,
) -> Result<()> {
    if boundary.is_empty() {
        return Err("The compaction boundary is empty.".into());
    }
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    cursor(&tx, track, session)?;
    let valid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE seq=?1 AND kind='msp.received' AND track_id=?2 AND session_id=?3 AND json_type(payload,'$.method') IS NULL AND json_type(payload,'$.error') IS NULL AND json_type(payload,'$.id')='integer' AND json_extract(payload,'$.id')>0 AND json_type(payload,'$.result.events')='array' AND json_type(payload,'$.result.nextCursor') IN ('null','text') AND json_extract(payload,'$.result.resolvedAnchor.boundaryCursor')=?4)",params![source,track,session,boundary],|r|r.get(0))?;
    if !valid {
        return Err("The compaction boundary has no matching received page.".into());
    }
    // Knowing the boundary event does not mean that the last saved position
    // is still usable. Always re-anchor, but keep one summary per boundary.
    let existing = tx.query_row("SELECT seq FROM events WHERE kind='history.compacted' AND session_id=?1 AND json_extract(payload,'$.boundaryCursor')=?2 ORDER BY seq LIMIT 1", params![session,boundary], |row|row.get::<_,i64>(0)).optional()?;
    let sequence = if let Some(sequence) = existing {
        sequence
    } else {
        let at = Stamp::now();
        tx.execute("INSERT INTO events(ts,mono,boot,position,track_id,session_id,kind,payload,source_seq) VALUES(?1,?2,?3,?4,?5,?6,'history.compacted',?7,?8)",params![at.epoch_ms,at.mono_ms,at.boot_id,at.timeline_ms,track,session,serde_json::json!({"trackId":track,"sessionId":session,"boundaryCursor":boundary}).to_string(),source])?;
        tx.last_insert_rowid()
    };
    tx.execute(
        "INSERT INTO msp_applied VALUES(?1,?2,?3) ON CONFLICT(session,cursor) DO NOTHING",
        params![session, boundary, sequence],
    )?;
    tx.execute("INSERT INTO msp_positions VALUES(?1,?2,?3) ON CONFLICT(session) DO UPDATE SET cursor=excluded.cursor",params![session,track,boundary])?;
    tx.commit()?;
    Ok(())
}

fn append_raw(
    db: &Transaction<'_>,
    track: &str,
    session: &str,
    at: &Stamp,
    frame: &Value,
) -> Result<i64> {
    crate::session_retirement::ensure_open(db, session)?;
    db.execute("INSERT INTO events(ts,mono,boot,position,track_id,session_id,kind,payload) VALUES(?1,?2,?3,?4,?5,?6,'msp.received',?7)",params![at.epoch_ms,at.mono_ms,at.boot_id,at.timeline_ms,track,session,serde_json::to_string(frame)?])?;
    let sequence = db.last_insert_rowid();
    // A gap can follow events already beyond its hole. Persist the rewind
    // with the raw receipt, including a crash before projection catches up.
    if frame["method"] == "view/gap"
        && frame["params"]["sessionId"] == session
        && cursor(db, track, session).is_ok()
        && let (Some(after), Some(next)) = (
            frame["params"]["after"].as_str().filter(|s| !s.is_empty()),
            frame["params"]["next"].as_str().filter(|s| !s.is_empty()),
        )
    {
        db.execute(
            "INSERT INTO msp_gaps(source_seq,session,track,after,next) VALUES(?1,?2,?3,?4,?5)",
            params![sequence, session, track, after, next],
        )?;
        if let Some(pending) = gap(db, track, session)? {
            rewind(db, track, session, &pending.after)?;
        }
    }
    Ok(sequence)
}

fn rewind(db: &Connection, track: &str, session: &str, after: &str) -> Result<()> {
    let contiguous = if seen(db, session, after)? {
        Some(after.to_owned())
    } else {
        floor(db, session)?
    };
    db.execute("INSERT INTO msp_positions VALUES(?1,?2,?3) ON CONFLICT(session) DO UPDATE SET cursor=excluded.cursor",params![session,track,contiguous])?;
    Ok(())
}

pub fn raw_batch(
    db: &mut Connection,
    track: &str,
    session: &str,
    frames: &[crate::journal::RawFrame],
) -> Result<Vec<i64>> {
    // Acquire the writer reservation before checking session retirement. A
    // deferred read cannot upgrade while another connection owns the writer.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut sequences = Vec::with_capacity(frames.len());
    for frame in frames {
        sequences.push(append_raw(&tx, track, session, &frame.stamp, &frame.frame)?);
    }
    tx.commit()?;
    Ok(sequences)
}

pub fn raw(
    db: &mut Connection,
    track: &str,
    session: &str,
    at: &Stamp,
    frame: &Value,
) -> Result<i64> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let sequence = append_raw(&tx, track, session, at, frame)?;
    tx.commit()?;
    Ok(sequence)
}

pub fn apply(
    db: &mut Connection,
    track: &str,
    session: &str,
    at: &Stamp,
    frame: &Value,
    source: Option<i64>,
) -> Result<i64> {
    let raw = if let Some(source) = source {
        let valid: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE seq=?1 AND kind='msp.received' AND track_id=?2 AND session_id=?3)", params![source,track,session], |row| row.get(0))?;
        if !valid {
            return Err("The view event has no matching received frame.".into());
        }
        source
    } else {
        self::raw(db, track, session, at, frame)?
    };
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if frame["method"].as_str().is_none() || frame["params"]["sessionId"] != session {
        tx.commit()?;
        return Ok(raw);
    }
    let position = crate::recording::view_cursor(frame);
    if let Some(position) = position {
        cursor(&tx, track, session)?;
        if let Some(applied) = tx
            .query_row(
                "SELECT sequence FROM msp_applied WHERE session=?1 AND cursor=?2",
                params![session, position],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
        {
            if frame["_intercatRecovered"] == true {
                // A page can fill an older hole after newer state was already
                // observed. Reapply the canonical fact in page order without
                // duplicating its payload or replaying effects. The reference
                // and cursor advance commit together, including across a crash.
                tx.execute("INSERT INTO events(ts,mono,boot,position,track_id,session_id,kind,payload,source_seq) VALUES(?1,?2,?3,?4,?5,?6,'msp.replayed',?7,?8)",params![at.epoch_ms,at.mono_ms,at.boot_id,at.timeline_ms,track,session,json!({"appliedSequence":applied}).to_string(),raw])?;
                tx.execute(
                    "UPDATE msp_positions SET cursor=?2 WHERE session=?1",
                    params![session, position],
                )?;
            }
            tx.commit()?;
            return Ok(applied);
        }
        // A resume snapshot already contains the event at its own cursor.
        // It has no invented msp.applied row to use as a replay reference.
        if seen(&tx, session, position)? {
            tx.commit()?;
            return Ok(raw);
        }
    }
    tx.execute("INSERT INTO events(ts,mono,boot,position,track_id,session_id,kind,payload,source_seq) VALUES(?1,?2,?3,?4,?5,?6,'msp.applied',?7,?8)",params![at.epoch_ms,at.mono_ms,at.boot_id,at.timeline_ms,track,session,serde_json::to_string(frame)?,raw])?;
    let sequence = tx.last_insert_rowid();
    crate::attachment_history::observe(&tx, session, frame)?;
    crate::timeline::fold(&tx, track, session, at, frame)?;
    crate::effects::fold(&tx, session, at.epoch_ms, frame)?;
    if frame["method"] == "view/gap" {
        cursor(&tx, track, session)?;
        frame["params"]["after"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("Delivery gap has no start cursor.")?;
        // A protected gap can arrive after later live events. Rewind with
        // the gap record so a restart cannot skip the still-missing range.
        frame["params"]["next"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("Delivery gap has no next cursor.")?;
        if let Some(pending) = gap(&tx, track, session)? {
            rewind(&tx, track, session, &pending.after)?;
        }
    }
    if let Some(position) = position {
        tx.execute(
            "INSERT INTO msp_applied VALUES(?1,?2,?3)",
            params![session, position, sequence],
        )?;
        let pending: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM msp_gaps WHERE session=?1)",
            [session],
            |row| row.get(0),
        )?;
        if frame["_intercatRecovered"] == true || !pending {
            tx.execute("INSERT INTO msp_positions VALUES(?1,?2,?3) ON CONFLICT(session) DO UPDATE SET cursor=excluded.cursor",params![session,track,position])?;
        }
    }
    tx.commit()?;
    Ok(sequence)
}
