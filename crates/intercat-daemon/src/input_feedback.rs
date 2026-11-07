//! Human corrections are separate from dispatch and timeline editing.
use crate::{Result, decision::Decision};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Misread {
    pub op_id: Uuid,
    pub utterance_id: Uuid,
    pub revision: i64,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE INDEX IF NOT EXISTS input_corrections ON events(json_extract(payload,'$.inputId'),seq DESC) WHERE kind='utterance.corrected';
        CREATE INDEX IF NOT EXISTS input_correction_replacements ON events(json_extract(payload,'$.replacementInputId'),seq DESC) WHERE kind='utterance.corrected';")?;
    Ok(())
}

pub(crate) fn summary(db: &Connection, input: &str) -> Result<Option<Value>> {
    let (explicit, inferred, misread): (i64, i64, bool) = db.query_row(
        "SELECT coalesce(sum(json_extract(payload,'$.kind')='explicit'),0),
         coalesce(sum(json_extract(payload,'$.kind')='inferred'),0),
         coalesce(max(json_extract(payload,'$.reason')='misread'),0)
         FROM events WHERE kind='utterance.corrected' AND json_extract(payload,'$.inputId')=?1",
        [input],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok((explicit + inferred > 0)
        .then(|| json!({"explicit":explicit,"inferred":inferred,"misread":misread})))
}

pub(crate) fn latest(db: &Connection, input: &str) -> Result<Option<Value>> {
    let raw: Option<String> = db.query_row(
        "SELECT payload,seq FROM events WHERE kind='utterance.corrected' AND json_extract(payload,'$.inputId')=?1
         UNION ALL SELECT payload,seq FROM events WHERE kind='utterance.corrected' AND json_extract(payload,'$.replacementInputId')=?1 ORDER BY seq DESC LIMIT 1",
        [input], |row| row.get(0),
    ).optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}

fn record(
    db: &Transaction<'_>,
    input: &str,
    revision: i64,
    reason: &str,
    after: Option<&Decision>,
) -> Result<()> {
    let (text, before): (String, Option<String>) = db.query_row(
        "SELECT text,decision FROM utterances WHERE id=?1",
        [input],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let before = before
        .map(|raw| serde_json::from_str::<Decision>(&raw))
        .transpose()?;
    let sequence: Option<i64> = db.query_row(
        "SELECT seq FROM events WHERE kind='utterance.classified' AND json_extract(payload,'$.id')=?1 ORDER BY seq DESC LIMIT 1",
        [input], |row| row.get(0),
    ).optional()?;
    let sources: Option<String> = db
        .query_row(
            "SELECT record FROM utterance_sources WHERE input=?1",
            [input],
            |row| row.get(0),
        )
        .optional()?;
    let payload = json!({"inputId":input,"revision":revision,"kind":"explicit","reason":reason,
        "text":text,"sources":sources.map(|raw|serde_json::from_str::<Value>(&raw)).transpose()?,
        "decisionSequence":sequence,"before":before,"after":after});
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.corrected',?2)",
        params![crate::clock::Stamp::now().epoch_ms, payload.to_string()],
    )?;
    crate::confirmation::observe(db, true, input)
}

pub(crate) fn read_as(
    db: &Transaction<'_>,
    input: &str,
    revision: i64,
    after: &Decision,
) -> Result<()> {
    let raw: Option<String> = db.query_row(
        "SELECT decision FROM utterances WHERE id=?1",
        [input],
        |row| row.get(0),
    )?;
    let Some(raw) = raw else {
        return Ok(());
    };
    let before: Decision = serde_json::from_str(&raw)?;
    if before.kind != after.kind
        || before.track_id != after.track_id
        || before.boundary != after.boundary
        || before.command != after.command
    {
        record(db, input, revision, "read-as", Some(after))?;
    }
    Ok(())
}

pub(crate) fn mark(db: &mut Connection, request: &Misread) -> Result<()> {
    if request.op_id.get_version_num() != 7 || request.revision < 1 {
        return Err("Invalid correction request.".into());
    }
    let input = request.utterance_id.to_string();
    crate::session_retirement::ensure_input(db, &input)?;
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let payload = serde_json::to_string(request)?;
    let previous: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, saved)) = previous {
        if kind != "utterance.misread" || saved != payload {
            return Err("Correction operation ID reused.".into());
        }
        return Ok(());
    }
    let (state, revision): (String, i64) = tx.query_row(
        "SELECT state,revision FROM utterances WHERE id=?1",
        [&input],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if state != "landed" || revision != request.revision {
        return Err(
            "This request changed. Reopen its details before marking it as misread.".into(),
        );
    }
    if !summary(&tx, &input)?.is_some_and(|value| value["misread"] == true) {
        record(&tx, &input, revision, "misread", None)?;
    }
    tx.execute(
        "INSERT INTO operations VALUES(?1,'utterance.misread',?2,'complete')",
        params![request.op_id.to_string(), payload],
    )?;
    tx.commit()?;
    Ok(())
}
