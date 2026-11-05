//! Read only the local evidence needed to reconcile one host session.
use crate::{
    Result,
    reconcile::{Local, Operation},
    session_view::SessionView,
};
use rusqlite::{Connection, params};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

enum Entry<'a> {
    Frame(&'a Value, i64),
    Snapshot(&'a crate::session_snapshot::Snapshot, i64),
}

fn visit_frames(
    db: &Connection,
    session: &str,
    mut visit: impl FnMut(Entry<'_>) -> Result<()>,
) -> Result<()> {
    let snapshot = crate::snapshot_store::latest(db, session)?;
    let mut seeded = false;
    let mut query=db.prepare("SELECT CASE WHEN e.kind='msp.replayed' THEN original.payload ELSE e.payload END,e.source_seq,e.kind,e.seq FROM events e LEFT JOIN events original ON e.kind='msp.replayed' AND original.seq=json_extract(e.payload,'$.appliedSequence') AND original.kind='msp.applied' AND original.session_id=e.session_id WHERE e.kind IN ('msp.applied','msp.replayed','history.compacted','history.snapshot') AND e.session_id=?1 ORDER BY e.seq")?;
    let rows = query.query_map([session], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (payload, source, kind, sequence) = row?;
        let frame: Value = serde_json::from_str(&payload)?;
        let cursor = if kind == "history.compacted" {
            frame["boundaryCursor"].as_str()
        } else if kind == "history.snapshot" {
            frame["cursor"].as_str()
        } else {
            frame["params"]["viewCursor"].as_str()
        };
        if kind == "msp.applied" || kind == "msp.replayed" {
            visit(Entry::Frame(&frame, source))?;
        }
        if let Some(stored) = &snapshot
            && !seeded
        {
            let matches = match stored.placement {
                crate::snapshot_store::Placement::History => {
                    cursor == Some(stored.snapshot.cursor.as_str())
                }
                crate::snapshot_store::Placement::Opening(at) => sequence == at,
            };
            if matches {
                visit(Entry::Snapshot(&stored.snapshot, stored.source))?;
                seeded = true;
            }
        }
    }
    if snapshot.is_some() && !seeded {
        return Err("Snapshot has lost its history anchor.".into());
    }
    Ok(())
}

pub fn local(db: &Connection, session: &str) -> Result<Local> {
    let mut view = SessionView::default();
    visit_frames(db, session, |entry| match entry {
        Entry::Frame(frame, _) => view.apply(session, frame),
        Entry::Snapshot(snapshot, _) => view.seed(session, snapshot),
    })?;
    let mut query=db.prepare("SELECT id,kind,state,coalesce(json_extract(payload,'$.approvalId'),json_extract(payload,'$.request.approvalId')),json_quote(coalesce(json_extract(payload,'$.requirementId'),json_extract(payload,'$.request.requirementId'))),json_extract(payload,'$.turnId'),json_extract(payload,'$.reclaimedStart') FROM operations WHERE coalesce(json_extract(payload,'$.sessionId'),json_extract(payload,'$.request.sessionId'))=?1")?;
    let rows = query
        .query_map([session], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut operations = BTreeMap::new();
    for (id, kind, state, approval, requirement, target, reclaimed_start) in rows {
        let id = id.parse()?;
        operations.insert(
            id,
            Operation {
                id,
                kind,
                state,
                approval,
                requirement: serde_json::from_str(&requirement)?,
                target_turn: target.map(|id| id.parse()).transpose()?,
                reclaimed_start: reclaimed_start.map(|id| id.parse()).transpose()?,
            },
        );
    }
    let compacted = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM events WHERE kind='history.compacted' AND session_id=?1)",
        [session],
        |r| r.get(0),
    )?;
    Ok(Local {
        view,
        operations,
        compacted,
    })
}

pub fn record(db: &mut Connection, session: &str, summary: &Value, retired: &[Uuid]) -> Result<()> {
    let tx = db.transaction()?;
    if let Some(command) = summary["command"].as_str() {
        tx.execute("UPDATE operations SET state='dispatched' WHERE id=?1 AND kind='turn.start' AND state='prepared' AND json_extract(payload,'$.sessionId')=?2",params![command,session])?;
    }
    for id in retired {
        let changed=tx.execute("UPDATE operations SET state='superseded' WHERE id=?1 AND kind='turn.start' AND state IN ('prepared','dispatched') AND json_extract(payload,'$.sessionId')=?2",params![id.to_string(),session])?;
        if changed != 1 {
            return Err("A recovered start changed before it could be held.".into());
        }
        tx.execute("UPDATE utterances SET state='held',reason='The previous start was rejected or reclaimed. Review before sending it again.' WHERE dispatch_command=?1 AND state IN ('starting','sent')",[id.to_string()])?;
    }
    tx.execute(
        "INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'worker.reconciled',?3)",
        params![crate::journal::timestamp(), session, summary.to_string()],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn replay(db: &Connection, session: &str, turn: Uuid) -> Result<Vec<(Value, i64)>> {
    let mut report = None;
    let mut terminal = None;
    let turn = turn.to_string();
    visit_frames(db, session, |entry| {
        let (frame, source) = match entry {
            Entry::Snapshot(snapshot, source) => {
                report = None;
                for item in snapshot.state["items"].as_array().unwrap() {
                    if item["turnId"] == turn
                        && item["kind"] == "agentMessage"
                        && item["messagePhase"] == "finalAnswer"
                        && item["status"] == "completed"
                    {
                        report = Some((
                            serde_json::json!({"jsonrpc":"2.0","method":"item/completed","params":{"sessionId":session,"item":item},"_intercatRecovered":true}),
                            source,
                        ));
                    }
                }
                return Ok(());
            }
            Entry::Frame(frame, source) => (frame, source),
        };
        if frame["method"] == "turn/completed" && frame["params"]["turnId"] == turn {
            terminal = Some((frame.clone(), source));
        } else if frame["method"] == "item/completed"
            && frame["params"]["item"]["turnId"] == turn
            && frame["params"]["item"]["messagePhase"] == "finalAnswer"
        {
            report = Some((frame.clone(), source));
        }
        Ok(())
    })?;
    Ok(report.into_iter().chain(terminal).collect())
}
