//! Durable reading identities and the bounded, low-priority park queue.
use crate::{Result, decision::Outcome};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    pub input: String,
    pub revision: i64,
    pub index: i64,
}

pub struct Pending {
    pub ticket: Ticket,
    pub input: crate::decision::PendingInput,
    pub context: Option<crate::decision::PendingInput>,
}

const PARK: &str = "state='placed' AND json_extract(decision,'$.kind')='park'";

pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS input_readings(
        input TEXT NOT NULL REFERENCES utterances(id) ON DELETE CASCADE, number INTEGER NOT NULL,
        revision INTEGER NOT NULL, kind TEXT NOT NULL, state TEXT NOT NULL,
        context_input TEXT, context_revision INTEGER, queued_at INTEGER NOT NULL,
        result TEXT, PRIMARY KEY(input,number));
        CREATE INDEX IF NOT EXISTS input_readings_queue ON input_readings(kind,state,queued_at);
        CREATE INDEX IF NOT EXISTS input_reading_sources ON events(seq DESC) WHERE kind='utterance.staged';",
    )?;
    Ok(())
}

fn event(db: &Connection, ticket: &Ticket, state: &str) -> Result<()> {
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.reading-state',?2)",
        params![
            crate::journal::timestamp(),
            json!({
                "id": ticket.input,
                "revision": ticket.revision,
                "readingIndex": ticket.index,
                "state": state,
            })
            .to_string()
        ],
    )?;
    Ok(())
}

fn create(
    db: &Connection,
    input: &str,
    revision: i64,
    kind: &str,
    state: &str,
    context: Option<(&str, i64)>,
) -> Result<Ticket> {
    let index = db.query_row(
        "SELECT coalesce(max(number),0)+1 FROM input_readings WHERE input=?1",
        [input],
        |row| row.get(0),
    )?;
    db.execute(
        "INSERT INTO input_readings(input,number,revision,kind,state,context_input,context_revision,queued_at)
        VALUES(?1,?2,?3,?4,?5,?6,?7,(SELECT coalesce(max(seq),0)+1 FROM events))",
        params![input, index, revision, kind, state, context.map(|c| c.0), context.map(|c| c.1)],
    )?;
    let ticket = Ticket {
        input: input.into(),
        revision,
        index,
    };
    event(db, &ticket, state)?;
    Ok(ticket)
}

pub fn begin_input(db: &Connection, input: &str, revision: i64) -> Result<Ticket> {
    db.execute(
        "UPDATE input_readings SET state='stale' WHERE input=?1 AND state IN ('queued','running','late')",
        [input],
    )?;
    create(db, input, revision, "input", "running", None)
}

pub fn finish_input(db: &Connection, input: &str, revision: i64) -> Result<()> {
    db.execute(
        "UPDATE input_readings SET state='complete',result=(SELECT decision FROM utterances WHERE id=?1)
        WHERE input=?1 AND revision=?2 AND kind='input' AND state='running'",
        params![input, revision],
    )?;
    Ok(())
}

/// The soft deadline changes only the question, never its destination or revision.
pub fn expire(db: &mut Connection, ticket: &Ticket) -> Result<()> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    if foreground_current(&tx, ticket, "running", "reading")?
        && !crate::edit_hold::active(&tx, &ticket.input)?
    {
        tx.execute(
            "UPDATE utterances SET state='asked',decision=NULL,reason=CASE
            WHEN json_extract(target,'$.mode')='lock' THEN 'The Editor took too long. Send as an instruction?'
            WHEN json_extract(target,'$.mode')='new' THEN 'The Editor took too long. Start a new track now?'
            ELSE 'Where should this request go?' END WHERE id=?1",
            [&ticket.input],
        )?;
        tx.execute(
            "UPDATE input_readings SET state='late' WHERE input=?1 AND number=?2",
            params![ticket.input, ticket.index],
        )?;
        event(&tx, ticket, "late")?;
    }
    tx.commit()?;
    Ok(())
}

fn foreground_current(
    db: &Connection,
    ticket: &Ticket,
    state: &str,
    input_state: &str,
) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM input_readings r JOIN utterances u ON u.id=r.input
        WHERE r.input=?1 AND r.number=?2 AND r.revision=?3 AND r.kind='input'
        AND r.state=?4 AND u.revision=r.revision AND u.state=?5
        AND r.number=(SELECT max(number) FROM input_readings WHERE input=r.input))",
        params![
            ticket.input,
            ticket.index,
            ticket.revision,
            state,
            input_state
        ],
        |row| row.get(0),
    )?)
}

/// Consume late or stale results without calling the dispatch path.
pub fn finish_late(db: &mut Connection, ticket: &Ticket, outcome: &Outcome) -> Result<bool> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    if foreground_current(&tx, ticket, "running", "reading")? {
        return Ok(false);
    }
    let valid = foreground_current(&tx, ticket, "late", "asked")?
        && !crate::edit_hold::active(&tx, &ticket.input)?;
    if valid && let Outcome::Rephrase { generate } = outcome {
        crate::rephrases::begin(&tx, &ticket.input, ticket.revision, *generate)?;
        tx.execute(
            "UPDATE utterances SET reason=?2 WHERE id=?1",
            params![
                ticket.input,
                if *generate {
                    "Too long for the Editor · give it a one-line request"
                } else {
                    "What should I do with this?"
                }
            ],
        )?;
    }
    let decision = match outcome {
        Outcome::Classified(decision) => serde_json::to_value(decision)?,
        _ => Value::Null,
    };
    let state = if valid { "complete" } else { "stale" };
    if tx.execute(
        "UPDATE input_readings SET state=?3,result=?4
        WHERE input=?1 AND number=?2 AND state IN ('running','late','stale') AND result IS NULL",
        params![
            ticket.input,
            ticket.index,
            state,
            json!({"late":true,"stale":!valid,"decision":decision}).to_string()
        ],
    )? > 0
    {
        event(&tx, ticket, state)?;
    }
    tx.commit()?;
    Ok(true)
}

pub fn deadline(db: &Connection, input: &str) -> Result<Option<Value>> {
    if crate::edit_hold::active(db, input)? {
        return Ok(None);
    }
    let row: Option<(String, Option<String>)> = db
        .query_row(
            "SELECT u.target,r.result FROM utterances u JOIN input_readings r ON r.input=u.id
        WHERE u.id=?1 AND u.state='asked' AND u.revision=r.revision AND r.kind='input'
        AND r.number=(SELECT max(number) FROM input_readings WHERE input=u.id)
        AND (r.state='late' OR (r.state='complete' AND json_extract(r.result,'$.late')=1))",
            [input],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((target, result)) = row else {
        return Ok(None);
    };
    let target: crate::decision::Target = serde_json::from_str(&target)?;
    let track = match target {
        crate::decision::Target::Lock(track) => Some(track),
        _ => None,
    };
    let result: Value = result
        .map(|text| serde_json::from_str(&text))
        .transpose()?
        .unwrap_or(Value::Null);
    let decision = &result["decision"];
    let recommendation = match decision["kind"].as_str() {
        Some("fork") => Some(json!({"trackId":null})),
        Some("extend" | "schedule" | "trim") => {
            if let Some(id) = decision["trackId"].as_str()
                && db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)",
                    [id],
                    |row| row.get::<_, bool>(0),
                )?
            {
                Some(json!({"trackId":id}))
            } else {
                None
            }
        }
        _ => None,
    };
    Ok(Some(
        json!({"trackId":track,"recommendation":recommendation}),
    ))
}

pub fn instruction(db: &Connection, input: &str) -> Result<Option<crate::decision::Decision>> {
    let Some(value) = deadline(db, input)? else {
        return Ok(None);
    };
    let track = value["trackId"].as_str();
    let new_track: bool = db.query_row(
        "SELECT json_extract(target,'$.mode')='new' FROM utterances WHERE id=?1",
        [input],
        |row| row.get(0),
    )?;
    if track.is_none() && !new_track {
        return Ok(None);
    }
    if let Some(track) = track
        && !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)",
            [track],
            |row| row.get::<_, bool>(0),
        )?
    {
        return Err("The chosen track is unavailable.".into());
    }
    Ok(Some(crate::decision::Decision {
        kind: if new_track {
            crate::decision::Kind::Fork
        } else {
            crate::decision::Kind::Extend
        },
        track_id: track.map(str::to_owned),
        boundary: Some(if new_track { "now" } else { "after-turn" }.into()),
        command: None,
        confidence: 1.0,
        inference_ms: 0.0,
        timed_out: false,
        scores: json!({"human":true,"confirmation":{"choice":{"kind":"instruction"},"by":"user"}}),
    }))
}

pub fn invalidate_parks(db: &Connection) -> Result<()> {
    db.execute(
        "UPDATE input_readings SET state='stale' WHERE kind='park' AND state IN ('queued','running')",
        [],
    )?;
    Ok(())
}

fn eligible(db: &Connection, input: &str) -> Result<bool> {
    Ok(db.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM utterances WHERE id=?1 AND {PARK})"),
        [input],
        |row| row.get(0),
    )? && !crate::edit_hold::active(db, input)?)
}

pub fn queue_latest(db: &Connection, context: &str, revision: i64) -> Result<()> {
    invalidate_parks(db)?;
    if crate::dispatch_control::paused(db)? {
        return Ok(());
    }
    let inputs: Vec<(String, i64)> = db
        .prepare(&format!(
            "SELECT u.id,u.revision FROM events e
            JOIN utterances u ON u.id=json_extract(e.payload,'$.id')
            WHERE e.kind='utterance.staged' AND u.id<>?1 AND {PARK}
            ORDER BY e.seq DESC LIMIT 3"
        ))?
        .query_map([context], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (input, version) in inputs.into_iter().rev() {
        if eligible(db, &input)? {
            create(
                db,
                &input,
                version,
                "park",
                "queued",
                Some((context, revision)),
            )?;
        }
    }
    Ok(())
}

pub fn touched(db: &Connection, input: &str) -> Result<()> {
    if !eligible(db, input)? || crate::dispatch_control::paused(db)? {
        return Ok(());
    }
    let revision = db.query_row(
        "SELECT revision FROM utterances WHERE id=?1",
        [input],
        |row| row.get(0),
    )?;
    let context: Option<(String, i64)> = db
        .query_row(
            "SELECT u.id,u.revision FROM events e
        JOIN utterances u ON u.id=json_extract(e.payload,'$.id')
        WHERE e.kind='utterance.staged' AND u.id<>?1 AND u.state NOT IN ('cut','orphaned')
        ORDER BY e.seq DESC LIMIT 1",
            [input],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    db.execute(
        "UPDATE input_readings SET state='stale' WHERE input=?1 AND kind='park' AND state IN ('queued','running')",
        [input],
    )?;
    create(
        db,
        input,
        revision,
        "park",
        "queued",
        context
            .as_ref()
            .map(|(id, revision)| (id.as_str(), *revision)),
    )?;
    db.execute(
        "UPDATE input_readings SET state='stale'
        WHERE kind='park' AND state IN ('queued','running') AND (input,number) NOT IN (
            SELECT input,number FROM input_readings
            WHERE kind='park' AND state IN ('queued','running') ORDER BY queued_at DESC LIMIT 3)",
        [],
    )?;
    Ok(())
}

pub fn current(db: &Connection, ticket: &Ticket) -> Result<bool> {
    let valid: bool = db.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM input_readings r JOIN utterances u ON u.id=r.input
            WHERE r.input=?1 AND r.number=?2 AND r.revision=?3
              AND r.state IN ('queued','running') AND u.revision=r.revision
              AND (r.context_input IS NULL OR EXISTS(
                  SELECT 1 FROM utterances c WHERE c.id=r.context_input
                    AND c.revision=r.context_revision AND c.state NOT IN ('cut','orphaned'))))",
        params![ticket.input, ticket.index, ticket.revision],
        |row| row.get(0),
    )?;
    Ok(valid && eligible(db, &ticket.input)? && !crate::dispatch_control::paused(db)?)
}

pub fn claim(db: &mut Connection) -> Result<Option<Pending>> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    if crate::dispatch_control::paused(&tx)?
        || tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM utterances WHERE state IN ('staged','reading'))",
            [],
            |row| row.get::<_, bool>(0),
        )?
    {
        return Ok(None);
    }
    let rows: Vec<(Ticket, Option<String>)> = tx
        .prepare(
            "SELECT input,revision,number,context_input FROM input_readings
            WHERE kind='park' AND state='queued' ORDER BY queued_at DESC",
        )?
        .query_map([], |row| {
            Ok((
                Ticket {
                    input: row.get(0)?,
                    revision: row.get(1)?,
                    index: row.get(2)?,
                },
                row.get(3)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (ticket, context) in rows {
        if current(&tx, &ticket)? {
            let mut input = crate::journal::pending_input(&tx, &ticket.input)?;
            input.reading_index = ticket.index;
            let context = context
                .map(|id| crate::journal::pending_input(&tx, &id))
                .transpose()?;
            tx.execute(
                "UPDATE input_readings SET state='running' WHERE input=?1 AND number=?2",
                params![ticket.input, ticket.index],
            )?;
            event(&tx, &ticket, "running")?;
            tx.commit()?;
            return Ok(Some(Pending {
                ticket,
                input,
                context,
            }));
        }
        tx.execute(
            "UPDATE input_readings SET state='stale' WHERE input=?1 AND number=?2",
            params![ticket.input, ticket.index],
        )?;
        event(&tx, &ticket, "stale")?;
    }
    tx.commit()?;
    Ok(None)
}

fn requeue(db: &Connection, ticket: &Ticket) -> Result<()> {
    let (context, revision): (Option<String>, Option<i64>) = db.query_row(
        "SELECT context_input,context_revision FROM input_readings WHERE input=?1 AND number=?2",
        params![ticket.input, ticket.index],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    db.execute(
        "UPDATE input_readings SET state='stale' WHERE input=?1 AND number=?2",
        params![ticket.input, ticket.index],
    )?;
    event(db, ticket, "stale")?;
    create(
        db,
        &ticket.input,
        ticket.revision,
        "park",
        "queued",
        context.as_deref().zip(revision),
    )?;
    Ok(())
}

pub fn release(db: &mut Connection, ticket: &Ticket) -> Result<()> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    if current(&tx, ticket)? {
        requeue(&tx, ticket)?;
    } else if tx.execute(
        "UPDATE input_readings SET state='stale' WHERE input=?1 AND number=?2 AND state IN ('queued','running')",
        params![ticket.input, ticket.index],
    )? > 0 {
        event(&tx, ticket, "stale")?;
    }
    tx.commit()?;
    Ok(())
}

pub fn apply(db: &mut Connection, ticket: &Ticket, outcome: Outcome) -> Result<bool> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let result = match &outcome {
        Outcome::Classified(decision) => serde_json::to_value(decision)?,
        Outcome::Asked(reason) => json!({"reason":reason}),
        Outcome::Rephrase { generate } => json!({"rephrase":true,"generate":generate}),
        Outcome::Defer => json!({"deferred":true}),
    };
    if !current(&tx, ticket)? {
        if tx.execute(
            "UPDATE input_readings SET state='stale',result=?3
            WHERE input=?1 AND number=?2 AND state<>'complete' AND result IS NULL",
            params![
                ticket.input,
                ticket.index,
                json!({"stale":true,"decision":result}).to_string()
            ],
        )? > 0
        {
            event(&tx, ticket, "stale")?;
        }
        tx.commit()?;
        return Ok(true);
    }
    if matches!(outcome, Outcome::Defer) {
        requeue(&tx, ticket)?;
        tx.commit()?;
        return Ok(true);
    }
    tx.execute_batch("SAVEPOINT park_outcome")?;
    crate::journal::apply_outcome(&tx, &ticket.input, ticket.revision, outcome)?;
    let state: String = tx.query_row(
        "SELECT state FROM utterances WHERE id=?1",
        [&ticket.input],
        |row| row.get(0),
    )?;
    let adopted = matches!(state.as_str(), "placed" | "redirecting");
    if !adopted {
        tx.execute_batch("ROLLBACK TO park_outcome")?;
    }
    tx.execute_batch("RELEASE park_outcome")?;
    tx.execute(
        "UPDATE input_readings SET state='complete',result=?3 WHERE input=?1 AND number=?2",
        params![
            ticket.input,
            ticket.index,
            json!({"adopted":adopted,"decision":result}).to_string()
        ],
    )?;
    event(&tx, ticket, "complete")?;
    tx.commit()?;
    Ok(true)
}

pub fn recover(db: &mut Connection) -> Result<()> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let rows: Vec<(String, i64, Option<String>, Option<i64>)> = tx
        .prepare(
            "SELECT input,revision,context_input,context_revision FROM input_readings
            WHERE kind='park' AND state='running'",
        )?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    // A stopped inference cannot resume after restart, but its manual question can.
    tx.execute("UPDATE input_readings SET state='complete',result='{\"late\":true,\"decision\":null}' WHERE state='late'", [])?;
    tx.execute(
        "UPDATE input_readings SET state='stale' WHERE state='running'",
        [],
    )?;
    for (input, revision, context, context_revision) in rows {
        create(
            &tx,
            &input,
            revision,
            "park",
            "queued",
            context.as_deref().zip(context_revision),
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub fn status(db: &Connection, input: &str) -> Result<Option<Value>> {
    Ok(db
        .query_row(
            "SELECT number,kind,state FROM input_readings
            WHERE input=?1 AND revision=(SELECT revision FROM utterances WHERE id=?1)
            ORDER BY number DESC LIMIT 1",
            [input],
            |row| {
                Ok(json!({
                    "index": row.get::<_, i64>(0)?,
                    "kind": row.get::<_, String>(1)?,
                    "state": row.get::<_, String>(2)?,
                }))
            },
        )
        .optional()?)
}
