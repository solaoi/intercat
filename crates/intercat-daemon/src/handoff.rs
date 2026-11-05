//! Bounded, durable map/reduce summaries of already-masked local history.
use crate::{
    Result,
    pickup::{self, Phase},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub const CHUNK_BYTES: usize = 64 * 1024;
pub const CONTEXT_BYTES: usize = 2 * 1024 * 1024;
pub const OUTPUT_BYTES: usize = 16 * 1024;

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
#[derive(Clone, Debug, Serialize)]
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
    pub recovery_id: Uuid,
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

pub fn task_ids(db: &Connection, recovery: Uuid) -> Result<Vec<Uuid>> {
    let mut query = db.prepare("SELECT id FROM operations WHERE kind='handoff.summary' AND json_extract(payload,'$.recoveryId')=?1 ORDER BY rowid")?;
    query
        .query_map([recovery.to_string()], |row| row.get::<_, String>(0))?
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
fn new_task(recovery_id: Uuid, level: u32, index: usize, attempt: u32, input: String) -> Task {
    Task {
        id: Uuid::now_v7(),
        recovery_id,
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
    db.execute("INSERT INTO events(ts,kind,payload) VALUES(?1,'handoff.summary',?2)",params![crate::journal::timestamp(),json!({"id":task.id,"recoveryId":task.recovery_id,"level":task.level,"index":task.index,"state":task.state}).to_string()])?;
    Ok(())
}
fn tasks(db: &Connection, id: Uuid) -> Result<Vec<Task>> {
    db.prepare("SELECT payload FROM operations WHERE kind='handoff.summary' AND json_extract(payload,'$.recoveryId')=?1 ORDER BY rowid")?
        .query_map([id.to_string()],|r|r.get::<_,String>(0))?.map(|r|Ok(serde_json::from_str(&r?)?)).collect()
}
fn addresses(context: &Value, track: &str) -> Vec<String> {
    let mut result = BTreeSet::from([format!("track:{track}")]);
    for section in ["instructions", "lastClips", "completedTools"] {
        for record in context[section].as_array().into_iter().flatten() {
            if let Some(address) = record["address"].as_str() {
                result.insert(address.into());
            }
        }
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
    let recovery = pickup::read(&tx, id)?.ok_or("Unknown file recovery.")?;
    if recovery.phase != Phase::Summarizing {
        return Err("File recovery is not ready for its summary.".into());
    }
    let stored = tasks(&tx, id)?;
    let level = stored.iter().map(|t| t.level).max().unwrap_or(0);
    let mut current = BTreeMap::new();
    for task in stored.into_iter().filter(|t| t.level == level) {
        current.insert(task.index, task);
    }
    if current.is_empty() {
        return Err("The protected summary input has not been prepared.".into());
    }
    if let Some(task) = current.values().find(|t| t.state != State::Complete) {
        let mut task = task.clone();
        if task.state == State::Failed {
            task = new_task(
                id,
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
            source_addresses: addresses(&recovery.context, &recovery.request.track_id),
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
            id,
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
pub fn plan(db: &mut Connection, id: Uuid, input: Value) -> Result<()> {
    let tx = db.transaction()?;
    let recovery = pickup::read(&tx, id)?.ok_or("Unknown file recovery.")?;
    if recovery.phase != Phase::Summarizing {
        return Err("File recovery is not ready for a summary.".into());
    }
    if tasks(&tx, id)?.is_empty() {
        if serde_json::to_vec(&input)?.len() > CONTEXT_BYTES {
            return Err("The local history exceeds the summary input limit.".into());
        }
        for (index, input) in batches(&input)?.into_iter().enumerate() {
            save(&tx, &new_task(id, 0, index, 0, input))?;
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
}
