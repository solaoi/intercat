//! Ordered pasted blocks remain separate from the user's typed request.
use crate::{Result, decision::Target};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    Request,
    Reference,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Source {
    pub attachment_id: Uuid,
    pub role: Role,
    pub after: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Submission {
    pub op_id: Uuid,
    pub text: String,
    pub target: Target,
    pub selected_track_id: Option<String>,
    pub attachment_ids: Vec<Uuid>,
    pub sources: Option<Vec<Source>>,
    #[serde(default = "execution_allowed")]
    pub execute_allowed: bool,
    pub plan_item: Option<PlanItemContext>,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanItemContext {
    pub plan_id: Uuid,
    pub item_id: Uuid,
    pub revision: u64,
}
pub(crate) fn execution_allowed() -> bool {
    true
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Record {
    pub sources: Vec<Source>,
    pub preview: String,
}

pub(crate) fn validate(text: &str, attachments: &[Uuid], sources: &[Source]) -> Result<()> {
    let mut ids = attachments.iter().copied().collect::<BTreeSet<_>>();
    let mut bytes = text.len();
    if attachments.len() + sources.len() > 128 || ids.len() != attachments.len() {
        return Err("Too many or duplicate request references.".into());
    }
    for source in sources {
        if !ids.insert(source.attachment_id) {
            return Err("A pasted reference was included twice.".into());
        }
        bytes = bytes
            .checked_add(source.after.len())
            .ok_or("Request text is too large.")?;
    }
    if bytes > 32 * 1024 {
        return Err("The typed request exceeds 32 KiB.".into());
    }
    Ok(())
}

pub(crate) fn read(db: &Connection, input: &str) -> Result<Option<Record>> {
    let value: Option<String> = db
        .query_row(
            "SELECT record FROM utterance_sources WHERE input=?1",
            [input],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
}

pub(crate) fn write(db: &Connection, input: &str, record: &Record) -> Result<()> {
    db.execute(
        "INSERT INTO utterance_sources VALUES(?1,?2)",
        params![input, serde_json::to_string(record)?],
    )?;
    Ok(())
}

// Called inside the edit lease transaction so source bindings and text commit together.
pub(crate) fn edit(
    db: &Connection,
    input: &str,
    text: &str,
    sources: Option<&[Source]>,
) -> Result<()> {
    let original = read(db, input)?;
    let effective_sources =
        sources.unwrap_or_else(|| original.as_ref().map_or(&[], |record| &record.sources));
    if text.trim().is_empty() && effective_sources.is_empty() {
        return Err("Enter a request of at most 32 KB.".into());
    }
    let mut attachments = crate::attachment_store::records(db, input)?
        .into_iter()
        .map(|stored| stored.attachment.id)
        .collect::<Vec<_>>();
    if let Some(original) = &original {
        attachments.retain(|id| {
            !original
                .sources
                .iter()
                .any(|source| source.attachment_id == *id)
        });
    }
    validate(text, &attachments, effective_sources)?;
    if let Some(sources) = sources {
        if sources.iter().any(|source| {
            !original.as_ref().is_some_and(|record| {
                record
                    .sources
                    .iter()
                    .any(|old| old.attachment_id == source.attachment_id)
            })
        }) {
            return Err("A pasted block cannot be added through this edit.".into());
        }
        attachments.extend(sources.iter().map(|source| source.attachment_id));
        db.execute("DELETE FROM input_attachments WHERE input=?1", [input])?;
        crate::attachment_store::bind(db, input, &attachments)?;
        db.execute("DELETE FROM utterance_sources WHERE input=?1", [input])?;
        if !sources.is_empty() {
            let preview = if text.trim().is_empty() {
                original
                    .as_ref()
                    .map_or_else(String::new, |record| record.preview.clone())
            } else {
                text.trim().lines().next().unwrap_or("").to_owned()
            };
            write(
                db,
                input,
                &Record {
                    sources: sources.to_vec(),
                    preview,
                },
            )?;
        }
    }
    Ok(())
}
