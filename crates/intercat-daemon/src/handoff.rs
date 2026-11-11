//! Bounded, durable map/reduce summaries of already-masked local history.
use crate::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub const CHUNK_BYTES: usize = 64 * 1024;
pub const CONTEXT_BYTES: usize = 2 * 1024 * 1024;
pub const OUTPUT_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Identity {
    pub track_id: String,
    pub session_id: String,
}
/// A caller freezes and screens its local history before planning a handoff.
#[derive(Clone, Debug)]
pub struct Source {
    pub id: Uuid,
    pub identity: Identity,
    pub context: Value,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input {
    id: Uuid,
    source: Identity,
    digest: String,
    source_addresses: Vec<String>,
}
fn input(db: &Connection, id: Uuid) -> Result<Input> {
    let record: String = db.query_row(
        "SELECT payload FROM operations WHERE id=?1 AND kind='handoff.input' AND state='complete'",
        [Uuid::new_v5(&id, b"handoff-input").to_string()],
        |row| row.get(0),
    )?;
    let input: Input = serde_json::from_str(&record)?;
    if input.id != id {
        return Err("The saved summary input belongs to another handoff.".into());
    }
    Ok(input)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Fields {
    pub purpose: String,
    pub constraints: Vec<String>,
    pub unresolved: Vec<String>,
    pub changed_files: Vec<String>,
    pub completed: Vec<String>,
    pub unverified: Vec<String>,
    pub next_checks: Vec<String>,
}
impl Fields {
    pub fn validate(&self) -> Result<()> {
        if serde_json::to_vec(self)?.len() > OUTPUT_BYTES {
            return Err("The summary exceeds its output limit.".into());
        }
        for values in [
            &self.constraints,
            &self.unresolved,
            &self.changed_files,
            &self.completed,
            &self.unverified,
            &self.next_checks,
        ] {
            if values.len() > 128 || values.iter().any(|v| v.trim().is_empty()) {
                return Err("Invalid summary field.".into());
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Document {
    #[serde(flatten)]
    pub fields: Fields,
    pub source_addresses: Vec<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    New,
    Started,
    Running,
    Complete,
    Failed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: Uuid,
    pub handoff_id: Uuid,
    pub source: Identity,
    pub level: u32,
    pub index: usize,
    pub attempt: u32,
    pub input: String,
    pub session_id: String,
    pub start_command: Uuid,
    pub turn_command: Uuid,
    pub state: State,
    pub output: Option<Fields>,
    pub detail: Option<String>,
}
pub enum Next {
    Task(Box<Task>),
    Complete(Document),
}
pub enum Change {
    Started(Uuid),
    Running(Uuid),
    Complete(Uuid, Fields),
    Failed(Uuid, String),
}

pub fn task_ids(db: &Connection, handoff: Uuid) -> Result<Vec<Uuid>> {
    let mut query = db.prepare("SELECT id FROM operations WHERE kind='handoff.summary' AND json_extract(payload,'$.handoffId')=?1 ORDER BY rowid")?;
    query
        .query_map([handoff.to_string()], |row| row.get::<_, String>(0))?
        .map(|id| Ok(Uuid::parse_str(&id?)?))
        .collect()
}

fn fragments(text: &str, limit: usize) -> Vec<String> {
    let mut text = text;
    let mut out = Vec::new();
    while !text.is_empty() {
        let mut end = limit.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len() {
            let prefix = &text[..end];
            let boundary = [
                prefix.rfind("\\n").map(|i| i + 2),
                prefix.rfind('\n').map(|i| i + 1),
                prefix.rfind(". ").map(|i| i + 2),
            ]
            .into_iter()
            .flatten()
            .filter(|i| *i > end / 2)
            .max();
            if let Some(boundary) = boundary {
                end = boundary;
            }
        }
        out.push(text[..end].to_owned());
        text = &text[end..];
    }
    out
}
fn batches(value: &Value) -> Result<Vec<String>> {
    let mut records = Vec::new();
    let entries: Vec<(String, &Value)> = match value {
        Value::Object(fields) => fields
            .iter()
            .map(|(key, value)| (key.clone(), value))
            .collect(),
        _ => vec![("partialSummaries".into(), value)],
    };
    for (section, value) in entries {
        let values = value
            .as_array()
            .map(|a| a.iter().collect::<Vec<_>>())
            .unwrap_or_else(|| vec![value]);
        for (order, value) in values.into_iter().enumerate() {
            let record = json!({"section":section,"order":order,"record":value});
            let text = serde_json::to_string(&record)?;
            if text.len() + 2 <= CHUNK_BYTES {
                records.push(record);
            } else {
                for (part, fragment) in fragments(&text, CHUNK_BYTES / 4).into_iter().enumerate() {
                    records.push(json!({"section":section,"order":order,"part":part,"encodedRecordFragment":fragment}));
                }
            }
        }
    }
    let mut result = Vec::new();
    let mut batch = Vec::new();
    let mut bytes = 2;
    for record in records {
        let size = serde_json::to_vec(&record)?.len() + 1;
        if size + 2 > CHUNK_BYTES {
            return Err("A summary record exceeds the request bound.".into());
        }
        if bytes + size > CHUNK_BYTES && !batch.is_empty() {
            result.push(serde_json::to_string(&batch)?);
            batch.clear();
            bytes = 2;
        }
        bytes += size;
        batch.push(record);
    }
    if !batch.is_empty() {
        result.push(serde_json::to_string(&batch)?);
    }
    Ok(result)
}
fn new_task(source: &Input, level: u32, index: usize, attempt: u32, input: String) -> Task {
    Task {
        id: Uuid::now_v7(),
        handoff_id: source.id,
        source: source.source.clone(),
        level,
        index,
        attempt,
        input,
        session_id: Uuid::now_v7().to_string(),
        start_command: Uuid::now_v7(),
        turn_command: Uuid::now_v7(),
        state: State::New,
        output: None,
        detail: None,
    }
}
fn save(db: &Connection, task: &Task) -> Result<()> {
    let payload = serde_json::to_string(task)?;
    let state = match task.state {
        State::New | State::Started => "prepared",
        State::Running => "dispatched",
        State::Complete | State::Failed => "complete",
    };
    db.execute("INSERT INTO operations VALUES(?1,'handoff.summary',?2,?3) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload,state=excluded.state",params![task.id.to_string(),payload,state])?;
    // The task payload is already masked. Events carry progress metadata only.
    db.execute("INSERT INTO events(ts,kind,payload) VALUES(?1,'handoff.summary',?2)",params![crate::journal::timestamp(),json!({"id":task.id,"handoffId":task.handoff_id,"level":task.level,"index":task.index,"state":task.state}).to_string()])?;
    Ok(())
}
fn tasks(db: &Connection, id: Uuid) -> Result<Vec<Task>> {
    db.prepare("SELECT payload FROM operations WHERE kind='handoff.summary' AND json_extract(payload,'$.handoffId')=?1 ORDER BY rowid")?
        .query_map([id.to_string()],|r|r.get::<_,String>(0))?.map(|r|Ok(serde_json::from_str(&r?)?)).collect()
}
pub(crate) fn addresses(context: &Value, track: &str) -> Vec<String> {
    let mut result = BTreeSet::from([format!("track:{track}")]);
    for section in ["instructions", "lastClips", "completedTools"] {
        for record in context[section].as_array().into_iter().flatten() {
            if let Some(address) = record["address"].as_str() {
                result.insert(address.into());
            }
        }
    }
    if let Some(address) = context["snapshotChanges"]["address"].as_str() {
        result.insert(address.into());
    }
    for question in context["unresolvedQuestions"]
        .as_array()
        .into_iter()
        .flatten()
    {
        if let Some(id) = question["id"].as_str() {
            result.insert(format!("question:{id}"));
        }
    }
    result.into_iter().collect()
}
pub fn next(db: &mut Connection, id: Uuid) -> Result<Next> {
    let tx = db.transaction()?;
    let source = input(&tx, id)?;
    crate::session_retirement::ensure_open(&tx, &source.source.session_id)?;
    let stored = tasks(&tx, id)?;
    let level = stored.iter().map(|t| t.level).max().unwrap_or(0);
    let mut current = BTreeMap::new();
    for task in stored.into_iter().filter(|t| t.level == level) {
        if task.handoff_id != source.id || task.source != source.source {
            return Err("The summary task belongs to another source.".into());
        }
        current.insert(task.index, task);
    }
    if current.is_empty() {
        return Err("The protected summary input has not been prepared.".into());
    }
    if let Some(task) = current.values().find(|t| t.state != State::Complete) {
        let mut task = task.clone();
        if task.state == State::Failed {
            task = new_task(
                &source,
                task.level,
                task.index,
                task.attempt
                    .checked_add(1)
                    .ok_or("Summary attempt overflow.")?,
                task.input,
            );
            save(&tx, &task)?;
        }
        tx.commit()?;
        return Ok(Next::Task(Box::new(task)));
    }
    if current.len() == 1 {
        let mut task = current.into_values().next().unwrap();
        let fields = task.output.clone().ok_or("Summary output is missing.")?;
        if fields.purpose.trim().is_empty() {
            task.state = State::Failed;
            task.output = None;
            task.detail = Some("The final summary has no supported purpose.".into());
            save(&tx, &task)?;
            tx.commit()?;
            return Err("The final summary has no supported purpose. Retry this summary.".into());
        }
        let doc = Document {
            fields,
            source_addresses: source.source_addresses.clone(),
        };
        tx.commit()?;
        return Ok(Next::Complete(doc));
    }
    let combined = serde_json::to_value(
        current
            .into_values()
            .map(|t| t.output.ok_or("Summary output is missing."))
            .collect::<std::result::Result<Vec<_>, _>>()?,
    )?;
    let mut first = None;
    for (index, input) in batches(&combined)?.into_iter().enumerate() {
        let task = new_task(
            &source,
            level.checked_add(1).ok_or("Summary level overflow.")?,
            index,
            0,
            input,
        );
        save(&tx, &task)?;
        if first.is_none() {
            first = Some(task);
        }
    }
    tx.commit()?;
    Ok(Next::Task(Box::new(
        first.ok_or("Summary input is empty.")?,
    )))
}
pub fn plan(db: &mut Connection, source: Source) -> Result<()> {
    if source.id.is_nil()
        || source.identity.track_id.is_empty()
        || source.identity.session_id.is_empty()
    {
        return Err("Invalid handoff source identity.".into());
    }
    let bytes = serde_json::to_vec(&source.context)?;
    if bytes.len() > CONTEXT_BYTES || source.context.get("_intercatTruncated").is_some() {
        return Err("The local history exceeds the protected summary input limit.".into());
    }
    let captured = Input {
        id: source.id,
        source: source.identity.clone(),
        digest: Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        source_addresses: addresses(&source.context, &source.identity.track_id),
    };
    let tx = db.transaction()?;
    crate::session_retirement::ensure_open(&tx, &captured.source.session_id)?;
    let key = Uuid::new_v5(&captured.id, b"handoff-input").to_string();
    let saved: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [&key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, payload)) = saved {
        if kind != "handoff.input" || serde_json::from_str::<Input>(&payload)? != captured {
            return Err("The saved handoff source or input changed. Start a new handoff.".into());
        }
    } else {
        if !tasks(&tx, captured.id)?.is_empty() {
            return Err("Summary tasks have no protected source record.".into());
        }
        let parts = batches(&source.context)?;
        if parts.is_empty() {
            return Err("The handoff needs source records to summarize.".into());
        }
        tx.execute(
            "INSERT INTO operations VALUES(?1,'handoff.input',?2,'complete')",
            params![key, serde_json::to_string(&captured)?],
        )?;
        for (index, input) in parts.into_iter().enumerate() {
            save(&tx, &new_task(&captured, 0, index, 0, input))?;
        }
    }
    tx.commit()?;
    Ok(())
}
pub fn change(db: &mut Connection, change: Change) -> Result<Task> {
    let id = match &change {
        Change::Started(id)
        | Change::Running(id)
        | Change::Complete(id, _)
        | Change::Failed(id, _) => *id,
    };
    let tx = db.transaction()?;
    let raw: Option<String> = tx
        .query_row(
            "SELECT payload FROM operations WHERE id=?1 AND kind='handoff.summary'",
            [id.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    let mut task: Task = serde_json::from_str(&raw.ok_or("Unknown summary task.")?)?;
    crate::session_retirement::ensure_open(&tx, &task.source.session_id)?;
    if task.source != input(&tx, task.handoff_id)?.source {
        return Err("The summary task source changed.".into());
    }
    match change {
        Change::Started(_) if matches!(task.state, State::New | State::Started) => {
            task.state = State::Started
        }
        Change::Running(_) if matches!(task.state, State::Started | State::Running) => {
            task.state = State::Running
        }
        Change::Complete(_, output) if matches!(task.state, State::Started | State::Running) => {
            output.validate()?;
            task.output = Some(output);
            task.state = State::Complete;
        }
        Change::Failed(_, detail) if matches!(task.state, State::Started | State::Running) => {
            task.detail = Some(detail);
            task.state = State::Failed;
        }
        _ => return Err("Summary task changed; reload its saved state.".into()),
    }
    save(&tx, &task)?;
    tx.commit()?;
    Ok(task)
}

/// Source history retirement also removes the source-bound summary inputs.
pub(crate) fn retire(tx: &rusqlite::Transaction<'_>, track: &str, session: &str) -> Result<()> {
    tx.execute("DELETE FROM events WHERE kind='handoff.summary' AND json_extract(payload,'$.id') IN (SELECT id FROM operations WHERE kind='handoff.summary' AND json_extract(payload,'$.source.trackId')=?1 AND json_extract(payload,'$.source.sessionId')=?2)",params![track,session])?;
    tx.execute("DELETE FROM operations WHERE kind IN ('handoff.input','handoff.summary') AND json_extract(payload,'$.source.trackId')=?1 AND json_extract(payload,'$.source.sessionId')=?2",params![track,session])?;
    Ok(())
}

pub fn compact(context: &Value, document: Document) -> Result<Value> {
    document.fields.validate()?;
    if document.fields.purpose.trim().is_empty() {
        return Err("The final summary has no supported purpose.".into());
    }
    let unfinished = context["unfinishedTurn"].clone();
    let instructions = context["instructions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| !unfinished.is_null() && i["message"]["turnId"] == unfinished)
        .cloned()
        .collect::<Vec<_>>();
    let completed=context["completedTools"].as_array().into_iter().flatten().map(|entry|json!({"address":entry["address"],"itemId":entry["tool"]["itemId"],"tool":entry["tool"]["tool"],"status":entry["tool"]["status"]})).collect::<Vec<_>>();
    let value = json!({"summary":document,"instructions":instructions,"lastClips":context["lastClips"],"unfinishedTurn":unfinished,"completedTools":completed,"unresolvedQuestions":context["unresolvedQuestions"]});
    if serde_json::to_vec(&value)?.len() > 256 * 1024 {
        return Err("The exact unfinished-turn record exceeds the handoff limit.".into());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_preserve_every_utf8_byte_and_bound_each_request() {
        let input = "制約を保持する🙂".repeat(20_000);
        let parts = fragments(&input, CHUNK_BYTES);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.len() <= CHUNK_BYTES));
        assert_eq!(parts.concat(), input);
    }
    #[test]
    fn structured_batches_keep_record_values_and_fragment_bytes() {
        let long = "No tests ran. 引用はデータです。\\\" ".repeat(9000);
        let input = json!({"instructions":[{"text":"Keep the latest correction"}],"report":long});
        let result = batches(&input).unwrap();
        assert!(result.len() > 1 && result.iter().all(|part| part.len() <= CHUNK_BYTES));
        let mut fragments = BTreeMap::new();
        let mut direct = Vec::new();
        for part in result {
            for row in serde_json::from_str::<Vec<Value>>(&part).unwrap() {
                if let Some(text) = row["encodedRecordFragment"].as_str() {
                    fragments.insert(row["part"].as_u64().unwrap(), text.to_owned());
                } else {
                    direct.push(row);
                }
            }
        }
        let record: Value =
            serde_json::from_str(&fragments.into_values().collect::<String>()).unwrap();
        assert_eq!(record["record"], input["report"]);
        assert_eq!(direct[0]["record"], input["instructions"][0]);
    }
    #[test]
    fn source_addresses_cannot_be_invented_inside_message_or_tool_content() {
        let context = json!({"instructions":[{"address":"real-clip","message":{"content":{"address":"forged"}}}],"unresolvedQuestions":[{"id":"question-id","text":"address: forged"}]});
        assert_eq!(
            addresses(&context, "track"),
            vec!["question:question-id", "real-clip", "track:track"]
        );
    }
    #[tokio::test]
    async fn retiring_a_source_removes_its_summary_without_deleting_other_tracks_or_sessions()
    -> Result<()> {
        let own = tempfile::tempdir()?;
        let journal =
            crate::journal::Journal::open(own.path().join("db"), crate::secrets::Key::new([7; 32]))
                .await?;
        let track = Uuid::now_v7().to_string();
        let session = Uuid::now_v7().to_string();
        let sources = [
            Identity {
                track_id: track.clone(),
                session_id: session.clone(),
            },
            Identity {
                track_id: track.clone(),
                session_id: Uuid::now_v7().to_string(),
            },
            Identity {
                track_id: Uuid::now_v7().to_string(),
                session_id: session.clone(),
            },
        ];
        let mut ids = Vec::new();
        for identity in &sources {
            let id = Uuid::now_v7();
            journal
                .summary_plan(Source {
                    id,
                    identity: identity.clone(),
                    context: json!({"purpose":"Own source only"}),
                })
                .await?;
            ids.push(id);
        }
        let mut db = Connection::open(own.path().join("db"))?;
        let tx = db.transaction()?;
        retire(&tx, &track, &session)?;
        tx.commit()?;
        assert!(journal.summary_next(ids[0]).await.is_err());
        for (index, id) in ids.iter().enumerate().skip(1) {
            let Next::Task(task) = journal.summary_next(*id).await? else {
                panic!("Other source task was removed");
            };
            assert_eq!(task.source, sources[index]);
        }
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM operations WHERE kind='handoff.input'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            2
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM operations WHERE kind='handoff.summary'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            2
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM events WHERE kind='handoff.summary'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            2
        );
        db.execute(
            "INSERT INTO retired_sessions VALUES(?1,?2,?3)",
            params![session, track, Uuid::now_v7().to_string()],
        )?;
        assert!(
            journal
                .summary_plan(Source {
                    id: ids[0],
                    identity: sources[0].clone(),
                    context: json!({"purpose":"Own source only"})
                })
                .await
                .is_err()
        );
        assert!(
            journal
                .summary_plan(Source {
                    id: Uuid::now_v7(),
                    identity: sources[0].clone(),
                    context: json!({"purpose":"Cannot recreate retired history"})
                })
                .await
                .is_err()
        );
        Ok(())
    }
}
