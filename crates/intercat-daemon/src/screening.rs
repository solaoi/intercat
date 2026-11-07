//! A transport-ready start keeps its inspected fingerprint set alive through writing.
use crate::{Result, progress::Mask};
use serde_json::Value;
use std::{path::Path, sync::Arc};
use tokio::sync::OwnedRwLockReadGuard;

pub struct Command {
    pub(crate) params: Value,
    pub(crate) permit: Arc<OwnedRwLockReadGuard<()>>,
}

pub struct EditorRequest {
    pub(crate) params: Value,
    pub(crate) seal: crate::secrets::Seal,
    pub(crate) permit: Arc<OwnedRwLockReadGuard<()>>,
}
pub(crate) struct PreparedEditorRequest {
    pub(crate) params: Value,
    pub(crate) seal: crate::secrets::Seal,
}

impl PreparedEditorRequest {
    pub(crate) fn params(&self) -> &Value {
        &self.params
    }

    pub(crate) fn slice_sources(&self, ranges: &[(usize, std::ops::Range<usize>)]) -> Result<Self> {
        let sources: Vec<Value> = serde_json::from_str(
            self.params["tail"]
                .as_str()
                .ok_or("Missing Editor sources.")?,
        )?;
        let mut selected = Vec::with_capacity(ranges.len());
        for (index, range) in ranges {
            let mut source = sources
                .get(*index)
                .cloned()
                .ok_or("Unknown Editor source.")?;
            let text = source["text"]
                .as_str()
                .and_then(|text| text.get(range.clone()))
                .ok_or("Invalid Editor source range.")?;
            source["text"] = text.into();
            selected.push(source);
        }
        let mut params = self.params.clone();
        params["tail"] = serde_json::to_string(&selected)?.into();
        Ok(Self {
            params,
            seal: self.seal.clone(),
        })
    }
}

impl EditorRequest {
    pub fn params(&self) -> &Value {
        &self.params
    }
    /// Relocate already-inspected JSON without releasing its fingerprint lease.
    pub(crate) fn append_tail_to_prefix(mut self) -> Result<Self> {
        let prefix = self.params["prefix"]
            .as_str()
            .ok_or("Missing Editor prefix.")?;
        let tail = self.params["tail"].as_str().ok_or("Missing Editor tail.")?;
        self.params["prefix"] = format!("{prefix}{tail}").into();
        self.params["tail"] = "{}".into();
        Ok(self)
    }
    pub(crate) fn release(self) -> PreparedEditorRequest {
        PreparedEditorRequest {
            params: self.params,
            seal: self.seal,
        }
    }
    pub(crate) fn generation_token_count(&self) -> Result<Self> {
        let prefix = self.params["prefix"]
            .as_str()
            .ok_or("Missing generation prefix.")?;
        let tail = self.params["tail"]
            .as_str()
            .ok_or("Missing generation tail.")?;
        Ok(Self {
            params: serde_json::json!({"text": format!("{prefix}{tail}\nDescription:\n")}),
            permit: self.permit.clone(),
            seal: self.seal.clone(),
        })
    }
    pub(crate) fn reduce_context(&mut self) -> Result<bool> {
        // Reductions retain the screening lease and only remove or shorten inspected data.
        crate::input_context::reduce(&mut self.params)
    }
    pub(crate) fn token_count(&self) -> Result<Self> {
        let text = self.params["tail"]
            .as_str()
            .ok_or("Missing screened Editor tail.")?;
        Ok(Self {
            params: serde_json::json!({"text":text}),
            seal: self.seal.clone(),
            permit: self.permit.clone(),
        })
    }
}
impl Command {
    pub fn params(&self) -> &Value {
        &self.params
    }
}

// Preserve control metadata exactly. Only the user-text fields may be redacted.
fn collect(value: &Value, texts: &mut Vec<String>) {
    match value {
        Value::String(text) => texts.push(text.clone()),
        Value::Array(items) => items.iter().for_each(|item| collect(item, texts)),
        Value::Object(items) => {
            for (name, item) in items {
                texts.push(name.clone());
                collect(item, texts);
            }
        }
        _ => {}
    }
}
fn replace(
    value: &mut Value,
    safe: &mut impl Iterator<Item = String>,
    user_text: bool,
    text_fields: &[&str],
) -> Result<()> {
    match value {
        Value::String(text) => {
            let inspected = safe.next().ok_or("Missing screened request field.")?;
            if !user_text && *text != inspected {
                return Err(
                    "Request metadata contains sensitive text. Review it before sending.".into(),
                );
            }
            *text = inspected;
        }
        Value::Array(items) => {
            for item in items {
                replace(item, safe, user_text, text_fields)?;
            }
        }
        Value::Object(items) => {
            for (name, item) in items {
                if safe.next().as_deref() != Some(name.as_str()) {
                    return Err("A request field name contains sensitive text.".into());
                }
                replace(
                    item,
                    safe,
                    text_fields.contains(&name.as_str()),
                    text_fields,
                )?;
            }
        }
        _ => {}
    }
    Ok(())
}
pub(crate) struct Inspection {
    params: Value,
    fields: &'static [&'static str],
    has_tail: bool,
}

pub(crate) async fn inspect(
    mut params: Value,
    mask: &Mask,
    directory: &Path,
    editor: bool,
) -> Result<(Inspection, crate::privacy::Prepared)> {
    let has_tail = editor && params.get("tail").is_some();
    if has_tail {
        params["tail"] = serde_json::from_str(
            params["tail"]
                .as_str()
                .ok_or("The Editor tail must be a JSON string.")?,
        )?;
    }
    let fields: &'static [&'static str] = if editor {
        &[
            "prefix",
            "title",
            "request",
            "report",
            "text",
            "line",
            "detail",
            "summary",
            "choices",
            "visibleOutput",
            "arguments",
            "failureReason",
        ]
    } else {
        &["text", "arguments"]
    };
    let mut texts = Vec::new();
    collect(&params, &mut texts);
    let mask = mask.clone();
    let masked = tokio::task::spawn_blocking(move || {
        texts
            .iter()
            .map(|text| mask.redact_document(text))
            .collect::<Vec<_>>()
    })
    .await?;
    let prepared = crate::privacy::prepare(masked, directory).await?;
    Ok((
        Inspection {
            params,
            fields,
            has_tail,
        },
        prepared,
    ))
}

impl Inspection {
    pub(crate) fn finish(self, replacements: Vec<String>) -> Result<Value> {
        let mut safe = self.params;
        let mut replacements = replacements.into_iter();
        replace(&mut safe, &mut replacements, false, self.fields)?;
        if replacements.next().is_some() {
            return Err("Unexpected screened request fields.".into());
        }
        let has_tail = self.has_tail;
        if has_tail {
            // Shorten candidate titles only after inspecting their complete text.
            let tracks = safe
                .get_mut("tail")
                .and_then(|tail| tail.get_mut("tracks"))
                .and_then(Value::as_array_mut);
            for track in tracks.into_iter().flatten() {
                if let Some(title) = track["title"].as_str() {
                    track["title"] = crate::privacy::truncate(title, 80).into();
                }
                if let Some(title) = track
                    .get_mut("running")
                    .and_then(|running| running.get_mut("title"))
                    && let Some(text) = title.as_str()
                {
                    *title = crate::privacy::truncate(text, 30).into();
                }
                for planned in track
                    .get_mut("planned")
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                {
                    if let Some(title) = planned["title"].as_str() {
                        planned["title"] = crate::privacy::truncate(title, 30).into();
                    }
                }
            }
            for name in ["recentDecisions", "previousUtterances", "waitingCards"] {
                let records = safe
                    .get_mut("tail")
                    .and_then(|tail| tail.get_mut(name))
                    .and_then(Value::as_array_mut);
                for record in records.into_iter().flatten() {
                    if let Some(title) = record["title"].as_str() {
                        record["title"] = crate::privacy::truncate(title, 30).into();
                    }
                }
            }
            safe["tail"] = crate::input_context::serialize_tail(&safe["tail"])?.into();
        }
        Ok(safe)
    }
}

#[cfg(test)]
mod editor_batch_tests {
    use super::*;

    #[tokio::test]
    async fn prepared_ranges_do_not_hold_fingerprints_and_a_new_catalog_invalidates_them()
    -> Result<()> {
        use crate::{
            journal::Journal,
            secrets::{Key, Registration},
        };
        let root = tempfile::tempdir()?;
        let key = Key::new([7; 32]);
        let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
        let request = journal.screen_editor(serde_json::json!({
            "prefix":"Read this.","tail":"[{\"id\":\"1\",\"text\":\"before 日本語 after\"}]","maxTokens":512,
        }), crate::editor::Priority::Input).await?;
        let prepared = request.release();
        let selected = prepared.slice_sources(&[(0, 7..16)])?;
        assert!(
            selected.params()["tail"]
                .as_str()
                .unwrap()
                .contains("日本語")
        );
        assert!(prepared.slice_sources(&[(0, 8..16)]).is_err());
        assert!(journal.lease_editor(&selected).await?.is_some());
        rusqlite::Connection::open(root.path().join("db"))?.execute(
            "INSERT INTO msp_positions VALUES('batch-session','batch-track',NULL)",
            [],
        )?;
        let hex = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let secret = "private-batch-token";
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            journal.register_fingerprint(Registration {
                op_id: uuid::Uuid::now_v7(),
                track_id: "batch-track".into(),
                session_id: "batch-session".into(),
                name: "TOKEN".into(),
                byte_len: secret.len(),
                tag: hex(&key.tag(secret.as_bytes())),
                key_check: hex(&key.check()),
            }),
        )
        .await??;
        assert!(journal.lease_editor(&selected).await?.is_none());
        Ok(())
    }
}
