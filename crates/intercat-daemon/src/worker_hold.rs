//! Persistent admission after a local hold. Generated plan inputs keep the
//! original request's age, so materialization cannot masquerade as a new word.
use crate::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE worker_holds(track TEXT PRIMARY KEY REFERENCES tracks(id),operation TEXT NOT NULL REFERENCES operations(id));")?;
    Ok(())
}
pub(crate) fn record(db: &Connection, id: &str, track: &str) -> Result<()> {
    let payload = json!({"trackId":track}).to_string();
    let existing: Option<(String, String)> = db
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, old)) = existing {
        if kind != "worker.hold" || old != payload {
            return Err("Operation ID reused.".into());
        }
        // A replayed old Hold never replaces a later Hold or a later resume.
        return Ok(());
    }
    db.execute(
        "INSERT INTO operations VALUES(?1,'worker.hold',?2,'complete')",
        params![id, payload],
    )?;
    db.execute("INSERT INTO worker_holds VALUES(?1,?2) ON CONFLICT(track) DO UPDATE SET operation=excluded.operation",params![track,id])?;
    Ok(())
}
pub(crate) fn current(db: &Connection, track: &str) -> Result<Option<Uuid>> {
    let id: Option<String> = db
        .query_row(
            "SELECT h.operation FROM worker_holds h JOIN operations stop ON stop.id=h.operation WHERE h.track=?1 AND NOT EXISTS(SELECT 1 FROM operations start WHERE start.kind='worker.start' AND start.state='complete' AND start.rowid>stop.rowid AND json_extract(start.payload,'$.trackId')=h.track AND json_extract(start.payload,'$.scope')='worker')",
            [track],
            |row| row.get(0),
        )
        .optional()?;
    id.map(|id| Ok(id.parse()?)).transpose()
}
/// A continuation keeps the causal cutoff of its path. Copy the original hold
/// rather than recording a new one, so newer explicit work remains authorized.
pub(crate) fn inherit(db: &Connection, source: &str, successor: &str) -> Result<()> {
    if let Some(hold) = current(db, source)? {
        db.execute(
            "INSERT INTO worker_holds VALUES(?1,?2)",
            params![successor, hold.to_string()],
        )?;
    }
    Ok(())
}
/// Resolve only a turn actually received by this SDK session. Imported history
/// is not a new user authorization after a local hold.
pub(crate) fn turn_input(db: &Connection, session: &str, turn: &str) -> Result<Option<String>> {
    Ok(db.query_row("SELECT u.id FROM attachment_turns t JOIN utterances u ON u.dispatch_command=t.command WHERE t.session=?1 AND t.turn=?2 AND t.inherited=0",params![session,turn],|row|row.get(0)).optional()?)
}
pub(crate) fn turn_allowed(
    db: &Connection,
    track: &str,
    session: &str,
    turn: &str,
) -> Result<bool> {
    if current(db, track)?.is_none() {
        return Ok(true);
    }
    match turn_input(db, session, turn)? {
        Some(input) => allowed(db, track, &input),
        None => Ok(false),
    }
}
pub(crate) fn allowed(db: &Connection, track: &str, input: &str) -> Result<bool> {
    let Some(hold) = current(db, track)? else {
        return Ok(true);
    };
    let after: i64 = db.query_row(
        "SELECT rowid FROM operations WHERE id=?1",
        [hold.to_string()],
        |row| row.get(0),
    )?;
    if let Some(owner) = crate::continuation::start(db, track)?
        && owner.first_input == input
        && db.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1 AND rowid>?2)",
            params![owner.operation.to_string(), after],
            |row| row.get::<_, bool>(0),
        )?
    {
        // Start fresh authorizes its own summary after an earlier cancellation.
        // A subsequent Hold still requires a new explicit start.
        return Ok(true);
    }
    let mut input = input.to_owned();
    let mut seen = std::collections::BTreeSet::new();
    loop {
        if !seen.insert(input.clone()) {
            return Err("The work's input origin contains a cycle.".into());
        }
        let manual:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE rowid>?1 AND ((kind='utterance.start' AND json_extract(payload,'$.utteranceId')=?2) OR (kind='report.respond' AND id=?2 AND json_extract(payload,'$.answer.action')='reply')))",params![after,input],|row|row.get(0))?;
        if manual {
            return Ok(true);
        }
        let paired:Option<String>=db.query_row("SELECT json_extract(payload,'$.request.inputId') FROM operations WHERE kind='utterance.two-ways' AND json_extract(payload,'$.phase') IN ('awaiting-receipt','queued') AND json_extract(payload,'$.childInput')=?1",[&input],|row|row.get(0)).optional()?;
        if let Some(source) = paired {
            input = source;
            continue;
        }
        let fix: Option<String> = db
            .query_row(
                "SELECT fix FROM review_fix_inputs WHERE input=?1",
                [&input],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(fix) = fix {
            let explicit:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE rowid>?1 AND kind='review.fix-command' AND json_extract(payload,'$.fixId')=?2 AND json_extract(payload,'$.action')='fixOnceMore')",params![after,fix],|row|row.get(0))?;
            if explicit {
                return Ok(true);
            }
            let source:Option<(String,String)>=db.query_row("SELECT json_extract(j.record,'$.target.sessionId'),json_extract(j.record,'$.target.turnId') FROM review_fixes f JOIN review_notes n ON n.id=f.note JOIN review_jobs j ON j.id=n.job WHERE f.id=?1",[fix],|row|Ok((row.get(0)?,row.get(1)?))).optional()?;
            let Some((session, turn)) = source else {
                return Ok(false);
            };
            let Some(parent) = turn_input(db, &session, &turn)? else {
                return Ok(false);
            };
            input = parent;
            continue;
        }
        let plan:Option<(String,String,String)>=db.query_row("SELECT p.source,p.id,i.item FROM plan_inputs i JOIN plans p ON p.id=i.plan WHERE i.input=?1 AND i.role='initial'",[&input],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
        if let Some((source, plan, item)) = plan {
            let started:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE rowid>?1 AND ((kind='plan.command' AND json_extract(payload,'$.planId')=?2 AND (json_extract(payload,'$.action.kind')='start' OR (json_extract(payload,'$.action.kind')='start-without' AND json_extract(payload,'$.action.itemId')=?3))) OR (kind='plan.go' AND json_extract(payload,'$.planId')=?2)))",params![after,plan,item],|row|row.get(0))?;
            if started {
                return Ok(true);
            }
            input = source;
            continue;
        }
        return Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind='utterance.create' AND id=?1 AND rowid>?2)",params![input,after],|row|row.get(0))?);
    }
}
pub(crate) fn hold_blocked(db: &Connection, track: &str, input: &str) -> Result<()> {
    if !allowed(db, track, input)? {
        db.execute("UPDATE utterances SET state='held',revision=revision+1,reason='Held after this turn' WHERE id=?1 AND state='placed'",[input])?;
    }
    Ok(())
}
pub(crate) fn snapshot(db: &Connection) -> Result<Value> {
    let mut query = db.prepare("SELECT track,operation FROM worker_holds ORDER BY rowid")?;
    let mut result = serde_json::Map::new();
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (track, id) = row?;
        if current(db, &track)?.is_some() {
            result.insert(track, id.into());
        }
    }
    Ok(result.into())
}
pub(crate) fn resumes(db: &Connection) -> Result<Value> {
    let mut query=db.prepare("SELECT h.track,h.operation,u.id FROM worker_holds h JOIN utterances u ON json_extract(u.decision,'$.trackId')=h.track WHERE u.state='placed' AND NOT EXISTS(SELECT 1 FROM path_retirements p WHERE p.track=h.track AND p.active=1) ORDER BY u.rowid")?;
    let mut result = serde_json::Map::new();
    for row in query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (track, id, input) = row?;
        if current(db, &track)?.is_some() && allowed(db, &track, &input)? {
            result.insert(track, id.into());
        }
    }
    Ok(result.into())
}
