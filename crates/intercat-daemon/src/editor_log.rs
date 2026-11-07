//! Editor explanations are durable facts, separate from the current request.
use crate::{Result, descriptions::Description, runtime::WorkerState};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS editor_entries(
        id INTEGER PRIMARY KEY, source TEXT NOT NULL UNIQUE, track TEXT,
        scene TEXT NOT NULL, input TEXT, title TEXT NOT NULL, line TEXT NOT NULL,
        detail TEXT NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER, description_request TEXT);
        CREATE INDEX IF NOT EXISTS editor_entries_track ON editor_entries(track,id);
        CREATE INDEX IF NOT EXISTS editor_entries_expiry ON editor_entries(expires_at,id);")?;
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub id: i64,
    pub track_id: Option<String>,
    pub scene: String,
    pub input_id: Option<String>,
    pub title: String,
    pub line: String,
    pub detail: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Query {
    pub track_id: String,
    pub before: Option<i64>,
}

pub(crate) struct Note<'a> {
    pub source: &'a str,
    pub track: Option<&'a str>,
    pub scene: &'a str,
    pub input: Option<&'a str>,
    pub title: &'a str,
    pub line: &'a str,
    pub detail: &'a str,
    pub persistent: bool,
}

pub(crate) fn record(db: &Connection, note: Note<'_>) -> Result<()> {
    let now = crate::journal::timestamp();
    // Keep records beyond the longest on-screen lifetime. The renderer counts
    // graphemes and chooses the precise duration, including its fallback line.
    let expires = if note.persistent {
        None
    } else if note.scene == "interpretation" {
        Some(0)
    } else {
        Some(now + 12_000)
    };
    db.execute("INSERT INTO editor_entries(source,track,scene,input,title,line,detail,created_at,expires_at)
        VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(source) DO NOTHING",
        params![note.source,note.track,note.scene,note.input,note.title,note.line,note.detail,now,expires])?;
    Ok(())
}

pub(crate) fn input_masked(
    db: &mut Connection,
    ticket: &crate::input_readings::Ticket,
) -> Result<()> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let track: Option<Option<String>> = tx.query_row(
        "SELECT CASE WHEN json_extract(u.target,'$.mode')='lock' THEN json_extract(u.target,'$.trackId') END
        FROM utterances u JOIN input_readings r ON r.input=u.id
        WHERE u.id=?1 AND u.revision=?2 AND r.revision=u.revision AND r.number=?3
          AND r.number=(SELECT max(number) FROM input_readings WHERE input=u.id)
          AND r.state IN ('running','late') AND u.state IN ('reading','asked','placed')",
        params![ticket.input,ticket.revision,ticket.index], |row| row.get(0),
    ).optional()?;
    if let Some(track) = track {
        record(
            &tx,
            Note {
                source: &format!("masked-input/{}/{}", ticket.input, ticket.revision),
                track: track.as_deref(),
                scene: "masked",
                input: Some(&ticket.input),
                title: "Private details masked",
                line: "I masked private details in this request.",
                detail: "Private details in the request text were replaced before it was passed to the Editor. The original text stays on this Mac.",
                persistent: false,
            },
        )?;
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn pasted_masked(db: &Connection, source: &str) -> Result<()> {
    record(
        db,
        Note {
            source,
            track: None,
            scene: "masked",
            input: None,
            title: "Pasted text masked",
            line: "I masked private details in the pasted text.",
            detail: "The pasted original stays on this Mac. Private details were replaced in the reference copy before it can be given to a worker.",
            persistent: false,
        },
    )
}

pub(crate) fn assumed(db: &Connection, assumption: &crate::assumptions::Assumption) -> Result<()> {
    let label: Option<String> = db
        .query_row(
            "SELECT label FROM tracks WHERE id=?1",
            [&assumption.track_id],
            |row| row.get(0),
        )
        .optional()?;
    let subject = label
        .map(|label| format!("Worker {label}"))
        .unwrap_or_else(|| "The worker".into());
    record(
        db,
        Note {
            source: &format!("assumed/{}/{}", assumption.id, assumption.revision),
            track: Some(&assumption.track_id),
            scene: "assumed",
            input: None,
            title: "Assumption recorded",
            line: &format!(
                "{subject} went with {}. Change it at commit.",
                assumption.text
            ),
            detail: &format!(
                "{subject} recorded this choice: {}\nQuestion: {}\nYou can keep or change it on the assumption card or at commit. This does not confirm that the work is complete.",
                assumption.text, assumption.question
            ),
            persistent: false,
        },
    )
}

pub(crate) fn assumption_changed(
    db: &Connection,
    source: &str,
    track: &str,
    at_commit: bool,
) -> Result<()> {
    let label: String = db.query_row("SELECT label FROM tracks WHERE id=?1", [track], |row| {
        row.get(0)
    })?;
    let line = if at_commit {
        format!("A change here goes back to {label} first. Commit waits.")
    } else {
        format!("A correction is waiting for {label}.")
    };
    record(
        db,
        Note {
            source,
            track: Some(track),
            scene: "assumption-change",
            input: None,
            title: "Assumption correction requested",
            line: &line,
            detail: if at_commit {
                "A correction was requested for a reviewed assumption. Commit waits for the worker's change to be checked. The correction has not completed yet."
            } else {
                "Your correction was saved as a new request for the worker. It still follows the normal placement, confirmation and start conditions."
            },
            persistent: false,
        },
    )
}

pub(crate) fn reply_opened(db: &Connection, source: &str, question: &Value) -> Result<()> {
    let (Some(id), Some(revision), Some(track)) = (
        question["id"].as_str(),
        question["revision"].as_i64(),
        question["trackId"].as_str(),
    ) else {
        return Ok(());
    };
    let label: Option<String> = db.query_row("SELECT t.label FROM report_questions q JOIN tracks t ON t.id=q.track_id
        WHERE q.id=?1 AND q.track_id=?2 AND q.state IN ('open','queued','held') AND json_extract(q.record,'$.revision')=?3",
        params![id,track,revision], |row| row.get(0)).optional()?;
    let Some(label) = label else {
        return Ok(());
    };
    record(
        db,
        Note {
            source,
            track: Some(track),
            scene: "reply",
            input: None,
            title: "Reply draft opened",
            line: &format!("{label} asked. Answer in your words; I’ll pass it on."),
            detail: &format!(
                "Opened a reply draft for Worker {label}’s question. Nothing has been sent yet; send the draft when your answer is ready."
            ),
            persistent: false,
        },
    )
}

pub(crate) fn fork(db: &Connection, input: &str, track: &str, label: &str) -> Result<()> {
    record(
        db,
        Note {
            source: &format!("fork/{track}"),
            track: Some(track),
            scene: "fork",
            input: Some(input),
            title: "New track",
            line: &format!("New track for {label}."),
            detail: &format!(
                "Created Worker {label} for this request. Creating the track does not mean its work has started."
            ),
            persistent: false,
        },
    )
}

pub(crate) fn explained(
    db: &Connection,
    ticket: &crate::descriptions::Ticket,
    description: &Description,
) -> Result<()> {
    let decision: Value = serde_json::from_str(&ticket.decision)?;
    let track = decision["trackId"].as_str();
    if decision["kind"] == "fork"
        && let Some(track) = track
    {
        let changed = db.execute(
            "UPDATE editor_entries SET title=?3,line=?4,detail=?5,description_request=?6
            WHERE source=?1 AND input=?2 AND description_request IS NULL",
            params![
                format!("fork/{track}"),
                ticket.input.id,
                description.title,
                description.line,
                description.detail,
                ticket.request.input
            ],
        )?;
        if changed == 1 {
            return Ok(());
        }
    }
    record(
        db,
        Note {
            source: &format!("description/{}", ticket.request.input),
            track,
            scene: "interpretation",
            input: Some(&ticket.input.id),
            title: &description.title,
            line: &description.line,
            detail: &description.detail,
            persistent: false,
        },
    )
}

pub(crate) fn trim(db: &Connection, redirect: &crate::redirect::Redirect) -> Result<()> {
    let label: String = db.query_row(
        "SELECT label FROM tracks WHERE id=?1",
        [&redirect.track_id],
        |row| row.get(0),
    )?;
    record(
        db,
        Note {
            source: &format!("trim/{}/{}", redirect.session_id, redirect.turn_id),
            track: Some(&redirect.track_id),
            scene: "trim",
            input: Some(&redirect.input_id),
            title: "Turn stopped",
            line: &format!("Stopped {label}. The checkpoint is saved."),
            detail: "The original turn stopped and its checkpoint was saved. The correction can now be sent when its remaining conditions allow it.",
            persistent: false,
        },
    )
}

pub(crate) fn confirmation(
    db: &Connection,
    revision: u64,
    active: bool,
    automatic_entry: bool,
) -> Result<()> {
    let (scene, line, detail) = if active {
        (
            "confirmation",
            if automatic_entry {
                "I’ve been wrong a few times. I’ll show each one before placing it."
            } else {
                "I’ll show each interpretation before placing it."
            },
            "Confirmation mode is on. New interpretations wait for your OK before placement.",
        )
    } else {
        (
            "automatic",
            "Back to placing on my own.",
            "Confirmation mode is off. Requests already waiting for OK still need your confirmation.",
        )
    };
    record(
        db,
        Note {
            source: &format!("confirmation/{revision}"),
            track: None,
            scene,
            input: None,
            title: "Confirmation mode",
            line,
            detail,
            persistent: false,
        },
    )
}

pub(crate) fn worker_changed(
    db: &Connection,
    state: &WorkerState,
    terminal: Option<&str>,
) -> Result<()> {
    let old: Option<String> = db
        .query_row(
            "SELECT record FROM workers WHERE track_id=?1",
            [&state.track_id],
            |row| row.get(0),
        )
        .optional()?;
    let old: Option<WorkerState> = old.map(|raw| serde_json::from_str(&raw)).transpose()?;
    let old_phase = old.as_ref().map(|old| old.phase.as_str());
    if old_phase != Some(state.phase.as_str()) {
        db.execute(
            "UPDATE editor_entries SET expires_at=?2 WHERE track=?1 AND expires_at IS NULL
            AND scene IN ('reconnecting','lost','stopped')",
            params![state.track_id, crate::journal::timestamp()],
        )?;
        let label: Option<String> = db
            .query_row(
                "SELECT label FROM tracks WHERE id=?1",
                [&state.track_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(label) = label {
            let note = match state.phase.as_str() {
                "reconnecting" => Some((
                    "reconnecting",
                    format!("{label} dropped. Reconnecting."),
                    "The worker connection was lost. Its queued requests remain in Intercat while reconnection is attempted.",
                )),
                "lost" => Some((
                    "lost",
                    format!("{label} lost its session. See the card."),
                    "The worker session is unavailable. Review its recovery card before sending more work.",
                )),
                "approval" => Some((
                    "stopped",
                    format!("{label} needs your approval. See the card."),
                    "The worker has requested approval. The card contains the exact action and your available choices.",
                )),
                _ => None,
            };
            if let Some((scene, line, detail)) = note {
                record(
                    db,
                    Note {
                        source: &format!("worker/{}", uuid::Uuid::now_v7()),
                        track: Some(&state.track_id),
                        scene,
                        input: None,
                        title: "Worker status",
                        line: &line,
                        detail,
                        persistent: true,
                    },
                )?;
            }
        }
    }
    if terminal == Some("completed")
        && let Some(checkpoint) = &state.checkpoint
    {
        let label: String = db.query_row(
            "SELECT label FROM tracks WHERE id=?1",
            [&state.track_id],
            |row| row.get(0),
        )?;
        record(
            db,
            Note {
                source: &format!(
                    "completed/{}/{}",
                    state.session_id.as_deref().unwrap_or(""),
                    checkpoint.turn_id
                ),
                track: Some(&state.track_id),
                scene: "completed",
                input: None,
                title: "Turn completed",
                line: &format!("{label} finished its turn."),
                detail: "The worker reported that this turn completed and its checkpoint was saved. Check the report and diff for the result.",
                persistent: false,
            },
        )?;
    }
    Ok(())
}

fn entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: row.get(0)?,
        track_id: row.get(1)?,
        scene: row.get(2)?,
        input_id: row.get(3)?,
        title: row.get(4)?,
        line: row.get(5)?,
        detail: row.get(6)?,
        created_at: row.get(7)?,
        expires_at: row.get(8)?,
    })
}
const COLUMNS: &str = "id,track,scene,input,title,line,detail,created_at,expires_at";

pub(crate) fn current(db: &Connection) -> Result<Vec<Entry>> {
    let mut statement = db.prepare(&format!(
        "SELECT {COLUMNS} FROM editor_entries WHERE expires_at IS NULL OR id IN (SELECT id FROM editor_entries WHERE expires_at>?1 ORDER BY id DESC LIMIT 32) ORDER BY id"
    ))?;
    Ok(statement
        .query_map([crate::journal::timestamp()], entry)?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn read(db: &Connection, query: Query) -> Result<Value> {
    if query.track_id.is_empty()
        || query.track_id.len() > 128
        || query.before.is_some_and(|before| before <= 0)
    {
        return Err("Invalid Editor log query.".into());
    }
    let mut statement = db.prepare(&format!("SELECT {COLUMNS} FROM editor_entries WHERE (track=?1 OR track IS NULL) AND id<?2 ORDER BY id DESC LIMIT 51"))?;
    let mut entries = statement
        .query_map(
            params![query.track_id, query.before.unwrap_or(i64::MAX)],
            entry,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = entries.len() > 50;
    entries.truncate(50);
    let before = if more {
        entries.last().map(|entry| entry.id)
    } else {
        None
    };
    Ok(json!({"entries":entries,"before":before}))
}
