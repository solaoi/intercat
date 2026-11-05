//! Install an observed-history splice or a full resume snapshot at its opening cut.
use crate::{Result, clock::Stamp, session_snapshot::Snapshot};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde_json::{Value, json};

#[derive(Clone, Copy)]
pub enum Placement {
    History,
    Opening(i64),
}

pub struct Stored {
    pub snapshot: Snapshot,
    pub placement: Placement,
    pub source: i64,
}

pub fn latest(db: &Connection, session: &str) -> Result<Option<Stored>> {
    let row: Option<(i64,i64,String,String)> = db.query_row("SELECT s.seq,s.source_seq,s.payload,r.payload FROM events s LEFT JOIN events r ON r.seq=s.source_seq AND r.kind='msp.received' AND r.session_id=s.session_id WHERE s.kind='history.snapshot' AND s.session_id=?1 ORDER BY s.seq DESC LIMIT 1", [session], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).optional()?;
    let Some((sequence, source, metadata, raw)) = row else {
        return Ok(None);
    };
    let metadata: Value = serde_json::from_str(&metadata)?;
    let placement = match metadata["placement"].as_str() {
        Some("history") => Placement::History,
        Some("opening") => Placement::Opening(sequence),
        _ => return Err("Stored snapshot has no valid placement.".into()),
    };
    let snapshot = Snapshot::from_read(&serde_json::from_str::<Value>(&raw)?["result"])?
        .ok_or("Stored snapshot is no longer readable.")?;
    if metadata["cursor"] != snapshot.cursor {
        return Err("Stored snapshot cursor changed.".into());
    }
    Ok(Some(Stored {
        snapshot,
        placement,
        source,
    }))
}

fn receipt(db: &Connection, track: &str, session: &str, source: i64) -> Result<Value> {
    let raw:String=db.query_row("SELECT payload FROM events WHERE seq=?1 AND kind='msp.received' AND track_id=?2 AND session_id=?3",params![source,track,session],|row|row.get(0))?;
    let frame: Value = serde_json::from_str(&raw)?;
    if frame["id"].as_u64().is_none_or(|id| id == 0)
        || frame.get("method").is_some()
        || frame.get("error").is_some()
        || frame["result"]["session"]["sessionId"] != session
    {
        return Err("Snapshot has no matching read receipt.".into());
    }
    Ok(frame["result"].clone())
}

pub fn record(db: &mut Connection, track: &str, session: &str, source: i64) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    crate::history_store::cursor(&tx, track, session)?;
    let read = receipt(&tx, track, session, source)?;
    let Some(snapshot) = Snapshot::from_read(&read)? else {
        return Ok(());
    };
    if !crate::history_store::seen(&tx, session, &snapshot.cursor)? {
        return Err(
            "The snapshot cursor is not in recovered history. Retry before dispatch.".into(),
        );
    }
    if latest(&tx, session)?.is_some_and(|previous| {
        previous.snapshot.cursor == snapshot.cursor && previous.snapshot.state == snapshot.state
    }) {
        return Ok(());
    }
    let at = Stamp::now();
    tx.execute("INSERT INTO events(ts,mono,boot,position,track_id,session_id,kind,payload,source_seq) VALUES(?1,?2,?3,?4,?5,?6,'history.snapshot',?7,?8)", params![at.epoch_ms,at.mono_ms,at.boot_id,at.timeline_ms,track,session,json!({"cursor":snapshot.cursor,"placement":"history"}).to_string(),source])?;
    // Validate the complete splice before committing its source reference.
    let local = crate::reconcile_store::local(&tx, session)?;
    crate::timeline::project_snapshot(&tx, track, session, &local.view, &at)?;
    tx.commit()?;
    Ok(())
}

/// A resume snapshot is captured before the connection's replayed suffix.
pub fn opening(
    db: &mut Connection,
    track: &str,
    session: &str,
    source: i64,
    covered_gap: Option<i64>,
) -> Result<bool> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    crate::history_store::cursor(&tx, track, session)?;
    if let Some(through) = covered_gap {
        let valid: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM msp_gaps WHERE source_seq=?1 AND session=?2 AND track=?3)",
            params![through, session, track],
            |r| r.get(0),
        )?;
        if !valid || through >= source {
            return Err("Opening snapshot has no matching earlier gap receipt.".into());
        }
    }
    let read = receipt(&tx, track, session, source)?;
    if read["history"]["mode"] != "snapshot" {
        return Ok(false);
    }
    let Some(snapshot) = Snapshot::from_read(&read)? else {
        return Ok(false);
    };
    if read["viewCursor"] != snapshot.cursor {
        return Err("Opening snapshot does not cover its advertised head.".into());
    }
    let later_gap: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM msp_gaps WHERE session=?1 AND source_seq>?2)",
        params![session, covered_gap.unwrap_or(0)],
        |r| r.get(0),
    )?;
    if later_gap {
        return Err("A new delivery gap interrupted the opening snapshot.".into());
    }
    let at = Stamp::now();
    tx.execute("INSERT INTO events(ts,mono,boot,position,track_id,session_id,kind,payload,source_seq) VALUES(?1,?2,?3,?4,?5,?6,'history.snapshot',?7,?8)",params![at.epoch_ms,at.mono_ms,at.boot_id,at.timeline_ms,track,session,json!({"cursor":snapshot.cursor,"placement":"opening","coveredGapThrough":covered_gap}).to_string(),source])?;
    tx.execute("INSERT INTO msp_positions VALUES(?1,?2,?3) ON CONFLICT(session) DO UPDATE SET cursor=excluded.cursor",params![session,track,snapshot.cursor])?;
    if let Some(through) = covered_gap {
        tx.execute(
            "DELETE FROM msp_gaps WHERE session=?1 AND source_seq<=?2",
            params![session, through],
        )?;
    }
    let local = crate::reconcile_store::local(&tx, session)?;
    crate::timeline::project_snapshot(&tx, track, session, &local.view, &at)?;
    tx.commit()?;
    Ok(true)
}
