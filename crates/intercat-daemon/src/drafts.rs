//! Session-scoped UI drafts share the journal's database and single writer.
use crate::{Result, attachment_store::Attachment, decision::Target};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Entry {
    text: String,
    target: Target,
    attachments: Vec<Attachment>,
    context: Value,
    pending: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sources: Option<Vec<Paste>>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Paste {
    id: Uuid,
    role: crate::request_sources::Role,
    after: String,
    words: usize,
    attachment: Option<Attachment>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Book {
    active: String,
    stack: Vec<String>,
    entries: BTreeMap<String, Entry>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Saved {
    revision: i64,
    op_id: Option<Uuid>,
    parent_op_id: Option<Uuid>,
    book: Book,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Save {
    session: String,
    revision: i64,
    op_id: Uuid,
    parent_op_id: Option<Uuid>,
    book: Book,
}
fn key(session: &str) -> Result<String> {
    if session.is_empty() || session.len() > 256 {
        return Err("Invalid draft session.".into());
    }
    Ok(format!("drafts/{session}"))
}
fn empty() -> Saved {
    Saved {
        revision: 0,
        op_id: None,
        parent_op_id: None,
        book: Book {
            active: "work".into(),
            stack: vec![],
            entries: BTreeMap::from([(
                "work".into(),
                Entry {
                    text: String::new(),
                    target: Target::None,
                    attachments: vec![],
                    context: json!({"kind":"work"}),
                    pending: None,
                    sources: None,
                },
            )]),
        },
    }
}
fn validate(book: &Book) -> Result<()> {
    let mut seen = BTreeSet::new();
    if !book.entries.contains_key("work")
        || !book.entries.contains_key(&book.active)
        || book
            .stack
            .iter()
            .any(|id| id == &book.active || !book.entries.contains_key(id) || !seen.insert(id))
    {
        return Err("Invalid draft history.".into());
    }
    if matches!(
        book.entries[&book.active].context["kind"].as_str(),
        Some("approval" | "rephrase")
    ) || book.stack.iter().any(|id| {
        matches!(
            book.entries[id].context["kind"].as_str(),
            Some("approval" | "rephrase")
        )
    }) {
        return Err("Card drafts stay separate from the request composer.".into());
    }
    for entry in book.entries.values() {
        if let Some(sources) = &entry.sources {
            let mut ids = entry
                .attachments
                .iter()
                .map(|attachment| attachment.id)
                .collect::<BTreeSet<_>>();
            if sources.len() + entry.attachments.len() > 128
                || sources.iter().any(|source| {
                    !ids.insert(source.id)
                        || source.words > 2 * 1024 * 1024
                        || source
                            .attachment
                            .as_ref()
                            .is_some_and(|attachment| attachment.id != source.id)
                })
                || entry.text.len()
                    + sources
                        .iter()
                        .map(|source| source.after.len())
                        .sum::<usize>()
                    > 32 * 1024
            {
                return Err("Invalid pasted draft.".into());
            }
        }
        let valid_context = match entry.context["kind"].as_str() {
            Some("work") => true,
            Some("plan-reply") => {
                let context = &entry.context;
                ["planId", "questionId"].iter().all(|key| {
                    context[key]
                        .as_str()
                        .is_some_and(|value| Uuid::parse_str(value).is_ok())
                }) && context["revision"]
                    .as_u64()
                    .is_some_and(|revision| revision > 0)
                    && context["question"]
                        .as_str()
                        .is_some_and(|text| text.len() <= 8192)
                    && context["label"].is_string()
                    && entry.text.len() <= 32 * 1024
                    && matches!(entry.target, Target::None)
                    && entry.attachments.is_empty()
                    && entry.sources.as_ref().is_none_or(Vec::is_empty)
            }
            Some("continuation") => {
                let context = &entry.context;
                ["planId", "itemId", "trackId"].iter().all(|key| {
                    context[key]
                        .as_str()
                        .is_some_and(|id| Uuid::parse_str(id).is_ok())
                }) && context["revision"]
                    .as_u64()
                    .is_some_and(|revision| revision > 0)
                    && context["title"].is_string()
                    && serde_json::to_value(&entry.target).is_ok_and(|target| {
                        target["mode"] == "lock" && target["trackId"] == context["trackId"]
                    })
            }
            Some("reply") => {
                entry.context["question"]["id"].is_string()
                    && entry.context["question"]["trackId"].is_string()
                    && entry.context["question"]["revision"]
                        .as_i64()
                        .is_some_and(|revision| revision >= 1)
            }
            Some("repair") => entry.context["id"].is_string(),
            Some("approval") => {
                let context = &entry.context;
                ["trackId", "sessionId", "approvalId", "label"]
                    .iter()
                    .all(|key| {
                        context[key]
                            .as_str()
                            .is_some_and(|value| !value.is_empty() && value.len() <= 256)
                    })
                    && !context["requirement"].is_null()
                    && serde_json::from_value::<crate::secret_input::Owner>(
                        context["owner"].clone(),
                    )
                    .is_ok()
                    && serde_json::from_value::<crate::approval::ReplyMode>(context["mode"].clone())
                        .is_ok()
                    && entry.text.len() <= 16 * 1024
            }
            Some("rephrase") => {
                let context = &entry.context;
                context["inputId"].is_string()
                    && context["revision"]
                        .as_i64()
                        .is_some_and(|revision| revision > 0)
                    && context["touched"].is_boolean()
                    && context["seeded"].is_boolean()
                    && entry.text.len() <= 32 * 1024
                    && entry.attachments.is_empty()
                    && entry.sources.is_none()
            }
            Some("plan-item") => {
                let context = &entry.context;
                ["planId", "itemId", "token"].iter().all(|key| {
                    context[key]
                        .as_str()
                        .is_some_and(|id| Uuid::parse_str(id).is_ok())
                }) && context["title"].is_string()
                    && context["revision"]
                        .as_u64()
                        .is_some_and(|revision| revision > 0)
                    && entry.text.len() <= 32 * 1024
                    && entry.attachments.is_empty()
                    && entry.sources.is_none()
            }
            Some("edit") => {
                let context = &entry.context;
                context["inputId"].is_string()
                    && context["originalText"].is_string()
                    && matches!(context["state"].as_str(), Some("editing" | "sent"))
                    && (context["lease"].is_null()
                        || context["lease"]["token"]
                            .as_str()
                            .is_some_and(|id| Uuid::parse_str(id).is_ok())
                            && context["lease"]["owner"]
                                .as_str()
                                .is_some_and(|id| Uuid::parse_str(id).is_ok())
                            && context["lease"]["revision"]
                                .as_i64()
                                .is_some_and(|revision| revision >= 1))
            }
            Some("commit") => {
                entry.context["context"]["jobId"].is_string()
                    && entry.context["context"]["track"].is_string()
                    && entry.context["context"]["selection"]["path"].is_string()
            }
            _ => false,
        };
        if !valid_context
            || entry.pending.as_ref().is_some_and(|pending| {
                if entry.context["kind"] == "plan-reply" {
                    let request = &pending["request"];
                    if pending["kind"] != "plan-reply"
                        || request["text"] != entry.text
                        || ["planId", "questionId", "revision", "question"]
                            .iter()
                            .any(|key| request[key] != entry.context[key])
                    {
                        return true;
                    }
                } else if pending["kind"] == "plan-reply" {
                    return true;
                }
                if entry.context["kind"] == "continuation" {
                    let request = &pending["request"];
                    if pending["kind"] != "work"
                        || request["text"] != entry.text
                        || ["planId", "itemId", "revision"]
                            .iter()
                            .any(|key| request["planItem"][key] != entry.context[key])
                        || request["target"]["mode"] != "lock"
                        || request["target"]["trackId"] != entry.context["trackId"]
                        || request["executeAllowed"] == false
                    {
                        return true;
                    }
                } else if pending["request"]["planItem"].is_object() {
                    return true;
                }
                if pending["kind"] == "plan-item" {
                    let request = &pending["request"];
                    if entry.context["kind"] != "plan-item"
                        || request["kind"] != "submit"
                        || request["text"] != entry.text
                        || ["planId", "token", "revision"]
                            .iter()
                            .any(|key| request[key] != entry.context[key])
                    {
                        return true;
                    }
                }
                if pending["kind"] == "rephrase" {
                    let request = &pending["request"];
                    if entry.context["kind"] != "rephrase"
                        || request["utteranceId"] != entry.context["inputId"]
                        || request["revision"] != entry.context["revision"]
                        || request["text"] != entry.text
                    {
                        return true;
                    }
                }
                if pending["kind"] == "approval" {
                    let request = &pending["request"];
                    if entry.context["kind"] != "approval"
                        || ["owner", "trackId", "sessionId", "approvalId", "requirement"]
                            .iter()
                            .any(|key| request[key] != entry.context[key])
                        || request["response"]["action"] != "reply"
                        || request["response"]["mode"] != entry.context["mode"]
                        || request["response"]["text"] != entry.text
                    {
                        return true;
                    }
                }
                let identifier = if pending["kind"] == "edit" {
                    if pending["request"]["kind"] != "end" {
                        return true;
                    }
                    &pending["request"]["token"]
                } else {
                    if !matches!(
                        pending["kind"].as_str(),
                        Some(
                            "work"
                                | "repair"
                                | "commit"
                                | "approval"
                                | "rephrase"
                                | "plan-item"
                                | "plan-reply"
                        )
                    ) {
                        return true;
                    }
                    &pending["request"]["opId"]
                };
                identifier
                    .as_str()
                    .is_none_or(|id| Uuid::parse_str(id).is_err())
            })
        {
            return Err("Invalid draft context.".into());
        }
    }
    Ok(())
}
pub fn read(db: &Connection, session: &str) -> Result<Saved> {
    let value: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            [key(session)?],
            |row| row.get(0),
        )
        .optional()?;
    let saved = value
        .map(|value| serde_json::from_str(&value))
        .transpose()?
        .unwrap_or_else(empty);
    validate(&saved.book)?;
    Ok(saved)
}
pub(crate) fn contains_pending_edit(
    db: &Connection,
    owner: Uuid,
    input: &str,
    request: &Value,
) -> Result<bool> {
    let mut query = db.prepare("SELECT value FROM settings WHERE key LIKE 'drafts/%'")?;
    for value in query.query_map([], |row| row.get::<_, String>(0))? {
        let saved: Saved = serde_json::from_str(&value?)?;
        if saved.book.entries.values().any(|entry| {
            entry.context["kind"] == "edit"
                && entry.context["inputId"] == input
                && entry.context["lease"]["owner"] == owner.to_string()
                && entry.context["lease"]["token"] == request["token"]
                && entry.context["lease"]["revision"] == request["revision"]
                && entry.pending.as_ref().is_some_and(|pending| {
                    pending["kind"] == "edit" && pending["request"] == *request
                })
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}
pub fn save(db: &mut Connection, request: Save) -> Result<Value> {
    validate(&request.book)?;
    if request.revision < 0 {
        return Err("Invalid draft revision.".into());
    }
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let current = read(&tx, &request.session)?;
    let result = |status, saved: Saved| -> Result<Value> {
        let mut value = serde_json::to_value(saved)?;
        value["status"] = Value::String(status);
        Ok(value)
    };
    if current.op_id == Some(request.op_id) {
        if current.revision != request.revision + 1
            || current.parent_op_id != request.parent_op_id
            || serde_json::to_value(&current.book)? != serde_json::to_value(&request.book)?
        {
            return Err("Draft operation ID reused.".into());
        }
        return result("saved".into(), current);
    }
    if current.revision != request.revision || current.op_id != request.parent_op_id {
        return result("conflict".into(), current);
    }
    let saved = Saved {
        revision: current.revision + 1,
        op_id: Some(request.op_id),
        parent_op_id: request.parent_op_id,
        book: request.book,
    };
    tx.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key(&request.session)?, serde_json::to_string(&saved)?])?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'draft.saved',?2)",
        params![
            crate::journal::timestamp(),
            json!({"session":request.session,"revision":saved.revision}).to_string()
        ],
    )?;
    let next = &saved.book.entries[&saved.book.active].context;
    let old = &current.book.entries[&current.book.active].context;
    if next["kind"] == "reply"
        && (current.book.active != saved.book.active
            || old["question"]["id"] != next["question"]["id"]
            || old["question"]["revision"] != next["question"]["revision"])
    {
        crate::editor_log::reply_opened(
            &tx,
            &format!("reply-draft/{}", request.op_id),
            &next["question"],
        )?;
    }
    tx.commit()?;
    result("saved".into(), saved)
}
