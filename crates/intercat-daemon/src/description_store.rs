use crate::{
    Result,
    descriptions::{Description, Finish, Ticket},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

const READING: &str =
    "coalesce((SELECT max(number) FROM input_readings WHERE input=u.id AND state='complete'),0)";
const ELIGIBLE: &str = "u.decision IS NOT NULL AND u.state IN ('placed','asked','held','redirecting','starting','sent','landed')";

pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS decision_descriptions(
            input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE,
            revision INTEGER NOT NULL, reading INTEGER NOT NULL, decision TEXT NOT NULL,
            request TEXT NOT NULL, attempts INTEGER NOT NULL, state TEXT NOT NULL, result TEXT, finished_at INTEGER);
        UPDATE decision_descriptions SET state='pending' WHERE state='running';",
    )?;
    Ok(())
}

pub fn current(db: &Connection, ticket: &Ticket) -> Result<bool> {
    Ok(db.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM utterances u
            JOIN decision_descriptions d ON d.input=u.id
            WHERE u.id=?1 AND u.decision=?2 AND {READING}=?3
                AND d.request=?4 AND d.state='running' AND {ELIGIBLE})"
        ),
        params![
            ticket.input.id,
            ticket.decision,
            ticket.reading,
            ticket.request.input
        ],
        |row| row.get(0),
    )?)
}

pub fn claim(db: &mut Connection) -> Result<Option<Ticket>> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let candidates = tx
        .prepare(&format!(
            "SELECT u.id,u.decision,{READING} FROM utterances u
        LEFT JOIN decision_descriptions d ON d.input=u.id
        WHERE {ELIGIBLE} AND (d.input IS NULL OR d.decision<>u.decision OR d.reading<>{READING}
            OR (d.state='pending' AND d.attempts<2))
        ORDER BY u.created_at DESC,u.id DESC"
        ))?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, decision, reading) in candidates {
        if crate::edit_hold::active(&tx, &id)? {
            continue;
        }
        let input = crate::journal::pending_input(&tx, &id)?;
        let request = crate::input_readings::Ticket {
            input: uuid::Uuid::now_v7().to_string(),
            revision: 0,
            index: 1,
        };
        tx.execute(
            "INSERT INTO decision_descriptions(input,revision,reading,decision,request,attempts,state)
            VALUES(?1,?2,?3,?4,?5,1,'running')
            ON CONFLICT(input) DO UPDATE SET revision=excluded.revision,reading=excluded.reading,
                decision=excluded.decision,request=excluded.request,state='running',result=NULL,finished_at=NULL,
                attempts=CASE WHEN reading=excluded.reading
                    AND decision=excluded.decision THEN attempts+1 ELSE 1 END",
            params![id, input.revision, reading, decision, request.input],
        )?;
        let track_label = tx
            .query_row(
                "SELECT label FROM tracks WHERE id=json_extract(?1,'$.trackId')",
                [&decision],
                |row| row.get(0),
            )
            .optional()?;
        tx.commit()?;
        return Ok(Some(Ticket {
            input,
            reading,
            decision,
            request,
            track_label,
        }));
    }
    tx.commit()?;
    Ok(None)
}

pub fn finish(db: &mut Connection, ticket: &Ticket, outcome: Finish) -> Result<bool> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let valid = current(&tx, ticket)?;
    let (state, result, interrupted) = match outcome {
        _ if !valid => ("stale", None, false),
        Finish::Complete(description) => {
            crate::editor_log::explained(&tx, ticket, &description)?;
            let title = description.title.trim();
            if !title.is_empty()
                && title.chars().count() <= 30
                && !title.chars().any(char::is_control)
            {
                tx.execute(
                    "UPDATE tracks SET title=?1 WHERE id=json_extract(?2,'$.trackId')
                    AND json_extract(?2,'$.kind')='fork'",
                    params![title, ticket.decision],
                )?;
                tx.execute(
                    "UPDATE settings SET value=json_set(value,'$.name',?1)
                    WHERE key='session' AND json_extract(value,'$.id')=?2",
                    params![title, ticket.input.id],
                )?;
            }
            (
                "complete",
                Some(serde_json::to_string(&description)?),
                false,
            )
        }
        Finish::Interrupted => ("pending", None, true),
        Finish::Failed => ("pending", None, false),
    };
    tx.execute(
        "UPDATE decision_descriptions SET state=?2,result=?3,attempts=attempts-?4,finished_at=?6
        WHERE input=?1 AND request=?5 AND state='running'",
        params![
            ticket.input.id,
            state,
            result,
            i64::from(interrupted),
            ticket.request.input,
            crate::journal::timestamp()
        ],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'description.finished',?2)",
        params![
            crate::journal::timestamp(),
            json!({
                "input": ticket.input.id, "revision": ticket.input.revision,
                "reading": ticket.reading, "request": ticket.request.input, "state": state,
            })
            .to_string()
        ],
    )?;
    tx.commit()?;
    Ok(true)
}

pub fn read(db: &Connection, input: &str) -> Result<Option<Value>> {
    let result: Option<(String, i64)> = db
        .query_row(
            &format!(
                "SELECT d.result,d.finished_at FROM decision_descriptions d JOIN utterances u ON u.id=d.input
            WHERE u.id=?1 AND d.state='complete' AND u.decision=d.decision AND {READING}=d.reading AND {ELIGIBLE}"
            ),
            [input],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    result
        .map(|(text, created_at)| -> Result<Value> {
            let mut value = serde_json::to_value(serde_json::from_str::<Description>(&text)?)?;
            value["createdAt"] = created_at.into();
            Ok(value)
        })
        .transpose()
}
