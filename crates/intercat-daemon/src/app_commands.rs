//! Durable application actions and the requests captured by a go-ahead.
use crate::{
    Result,
    decision::{Decision, Kind},
    input_start::{self, Permission},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WaitingRequest {
    pub input_id: Uuid,
    pub revision: i64,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS input_command_candidates(
        input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS waiting_for_go ON utterances(created_at,id)
        WHERE state='placed' AND json_extract(decision,'$.boundary')='when-told';",
    )?;
    Ok(())
}

pub(crate) fn capture(db: &Connection, input: &str) -> Result<()> {
    let mut query = db.prepare(
        "SELECT id,revision FROM utterances WHERE state='placed'
        AND json_extract(decision,'$.boundary')='when-told'
        AND json_extract(decision,'$.kind') IN ('fork','extend','schedule')
        ORDER BY created_at,rowid",
    )?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut candidates = Vec::new();
    for (id, revision) in rows {
        if input_start::unavailable(db, &id, revision, Permission::GoAhead)?.is_none() {
            candidates.push(WaitingRequest {
                input_id: id.parse()?,
                revision,
            });
        }
    }
    db.execute(
        "INSERT INTO input_command_candidates VALUES(?1,?2)
        ON CONFLICT(input) DO UPDATE SET record=excluded.record",
        params![input, serde_json::to_string(&candidates)?],
    )?;
    Ok(())
}

fn captured(db: &Connection, input: &str) -> Result<Vec<WaitingRequest>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM input_command_candidates WHERE input=?1",
            [input],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?
        .unwrap_or_default())
}

fn is_go_ahead(decision: &Decision) -> bool {
    decision.kind == Kind::Command && decision.command.as_deref() == Some("start-waiting")
}

fn scoped(db: &Connection, input: &str, decision: &Decision) -> Result<Vec<WaitingRequest>> {
    let mut result = Vec::new();
    for candidate in captured(db, input)? {
        let track: Option<String> = db
            .query_row(
                "SELECT json_extract(decision,'$.trackId') FROM utterances WHERE id=?1",
                [candidate.input_id.to_string()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        if decision.track_id.is_none() || decision.track_id == track {
            result.push(candidate);
        }
    }
    Ok(result)
}

pub(crate) fn projection(
    db: &Connection,
    input: &str,
    decision: &Decision,
) -> Result<Option<Value>> {
    if !is_go_ahead(decision) {
        return Ok(None);
    }
    let mut candidates = Vec::new();
    for candidate in scoped(db, input, decision)? {
        let row: Option<(String, Option<String>)> = db
            .query_row(
                "SELECT coalesce(nullif(u.text,''),json_extract(s.record,'$.preview'),''),t.label
            FROM utterances u LEFT JOIN utterance_sources s ON s.input=u.id
            LEFT JOIN tracks t ON t.id=json_extract(u.decision,'$.trackId') WHERE u.id=?1",
                [candidate.input_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (text, label) = row.unwrap_or_else(|| ("Request unavailable".into(), None));
        let mut characters = text.chars();
        let mut preview: String = characters.by_ref().take(200).collect();
        if characters.next().is_some() {
            preview.push('…');
        }
        let available = input_start::unavailable(
            db,
            &candidate.input_id.to_string(),
            candidate.revision,
            Permission::GoAhead,
        )?
        .is_none();
        candidates.push(json!({"inputId":candidate.input_id,"revision":candidate.revision,"text":preview,"label":label,"available":available}));
    }
    Ok(Some(
        json!({"candidates":candidates,"started":decision.scores.get("commandResult")}),
    ))
}

pub(crate) fn begin(
    db: &Connection,
    input: &str,
    decision: &mut Decision,
) -> Result<Option<String>> {
    let candidates = scoped(db, input, decision)?;
    if candidates.len() != 1 {
        return Ok(Some(
            if candidates.is_empty() {
                "Nothing is waiting for your go."
            } else {
                "Which waiting request should start?"
            }
            .into(),
        ));
    }
    let choice = &candidates[0];
    if let Some(reason) = input_start::unavailable(
        db,
        &choice.input_id.to_string(),
        choice.revision,
        Permission::GoAhead,
    )? {
        return Ok(Some(reason.into()));
    }
    select(db, input, decision, choice)?;
    Ok(None)
}

pub(crate) fn select(
    db: &Connection,
    input: &str,
    decision: &mut Decision,
    choice: &WaitingRequest,
) -> Result<()> {
    if !is_go_ahead(decision) || !scoped(db, input, decision)?.contains(choice) {
        return Err("Choose a waiting request shown with this command.".into());
    }
    let source: Uuid = input.parse()?;
    let operation = Uuid::new_v5(&source, b"start-waiting");
    input_start::start(
        db,
        &operation.to_string(),
        &choice.input_id.to_string(),
        choice.revision,
        Permission::GoAhead,
    )?;
    decision.scores["commandResult"] =
        json!({"inputId":choice.input_id,"revision":choice.revision+1});
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Presentation {
    pub command: String,
    pub operation_id: Uuid,
    pub receipt_id: Uuid,
    pub track_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_tracks: Option<Vec<String>>,
}

pub(crate) fn prepare_presentation(decision: &mut Decision) -> Result<bool> {
    if decision.kind != Kind::Command
        || !matches!(
            decision.command.as_deref(),
            Some("commit" | "settings" | "stage-mode" | "branch-from")
        )
    {
        return Ok(false);
    }
    let presentation = Presentation {
        command: decision.command.clone().unwrap(),
        operation_id: Uuid::now_v7(),
        receipt_id: Uuid::now_v7(),
        track_id: decision.track_id.clone(),
        commit_tracks: None,
    };
    decision.scores["presentation"] = serde_json::to_value(presentation)?;
    Ok(true)
}

pub(crate) fn presentation(decision: &Decision) -> Result<Option<Presentation>> {
    if decision.kind != Kind::Command
        || !matches!(
            decision.command.as_deref(),
            Some("commit" | "settings" | "stage-mode" | "branch-from")
        )
    {
        return Ok(None);
    }
    let Some(record) = decision.scores.get("presentation") else {
        return Ok(None);
    };
    let presentation: Presentation = serde_json::from_value(record.clone())?;
    if decision.command.as_deref() != Some(&presentation.command)
        || decision.track_id != presentation.track_id
    {
        return Ok(None);
    }
    if let Some(tracks) = &presentation.commit_tracks
        && (presentation.command != "commit"
            || tracks.is_empty()
            || tracks.len() > 256
            || tracks
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != tracks.len()
            || tracks.iter().any(|id| Uuid::parse_str(id).is_err()))
    {
        return Err("Invalid Commit review selection.".into());
    }
    Ok(Some(presentation))
}

pub(crate) fn presented(db: &Connection, input: &str, receipt: &str) -> Result<()> {
    let raw: String = db.query_row(
        "SELECT decision FROM utterances WHERE id=?1",
        [input],
        |row| row.get(0),
    )?;
    let decision: Decision = serde_json::from_str(&raw)?;
    let presentation = presentation(&decision)?.ok_or("This application action changed.")?;
    if presentation.receipt_id.to_string() != receipt {
        return Err("This application action has a different receipt.".into());
    }
    db.execute(
        "UPDATE utterances SET state='landed',revision=revision+1,reason=NULL WHERE id=?1",
        [input],
    )?;
    Ok(())
}

pub(crate) fn resume_presentation(
    db: &Connection,
    operation: &str,
    input: &str,
    revision: i64,
) -> Result<bool> {
    let (state, current, raw): (String, i64, Option<String>) = db.query_row(
        "SELECT state,revision,decision FROM utterances WHERE id=?1",
        [input],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let Some(raw) = raw else {
        return Ok(false);
    };
    let decision: Decision = serde_json::from_str(&raw)?;
    if decision.kind != Kind::Command {
        return Ok(false);
    }
    if state != "held"
        || current != revision
        || presentation(&decision)?.is_none()
        || crate::edit_hold::active(db, input)?
        || crate::confirmation::pending(db, input)?.is_some()
        || crate::decision_review::read(&decision)?.is_some()
    {
        return Err("Confirm or reread this application action before opening it.".into());
    }
    db.execute(
        "UPDATE utterances SET state='placed',revision=revision+1,reason=NULL WHERE id=?1",
        [input],
    )?;
    let payload = json!({"utteranceId":input,"revision":revision}).to_string();
    db.execute(
        "INSERT INTO operations VALUES(?1,'utterance.start',?2,'complete')",
        params![operation, payload],
    )?;
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.started',?2)",
        params![crate::journal::timestamp(), payload],
    )?;
    Ok(true)
}
