//! Approved delivery revisions keep the sealed pasted original unchanged.
use crate::{Result, attachment_store::Stored};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug)]
pub(crate) struct ReviewRequired;
impl std::fmt::Display for ReviewRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("The pasted reference needs review after the privacy rules changed.")
    }
}
impl std::error::Error for ReviewRequired {}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub attachment_id: Uuid,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Revision {
    pub original: Stored,
    pub copy: Stored,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub attachment_id: Uuid,
    pub sha256: String,
    pub byte_len: u64,
    pub changed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub items: Vec<crate::attachment_store::Attachment>,
    pub next: Option<Uuid>,
}

pub(crate) fn references(db: &Connection, track: &str, after: Option<Uuid>) -> Result<Vec<Stored>> {
    let mut query = db.prepare("SELECT a.record FROM attachments a WHERE a.id>?1 AND (EXISTS(SELECT 1 FROM input_attachments i JOIN utterances u ON u.id=i.input WHERE i.attachment=a.id AND (json_extract(u.decision,'$.trackId')=?2 OR json_extract(u.target,'$.trackId')=?2)) OR EXISTS(SELECT 1 FROM attachment_turn_items i JOIN workers w ON json_extract(w.record,'$.sessionId')=i.session WHERE i.attachment=a.id AND w.track_id=?2)) ORDER BY a.id LIMIT 51")?;
    query
        .query_map(
            params![after.map(|id| id.to_string()).unwrap_or_default(), track],
            |row| row.get::<_, String>(0),
        )?
        .map(|row| serde_json::from_str(&row?).map_err(Into::into))
        .collect()
}

impl Revision {
    pub fn receipt(&self) -> Receipt {
        Receipt {
            attachment_id: self.original.attachment.id,
            sha256: self.copy.attachment.sha256.clone(),
            byte_len: self.copy.attachment.byte_len,
            changed: self.copy != self.original,
        }
    }
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS attachment_screenings(attachment TEXT NOT NULL REFERENCES attachments(id),digest TEXT NOT NULL,record TEXT NOT NULL,PRIMARY KEY(attachment,digest));")?;
    Ok(())
}

pub(crate) fn previous(db: &Connection, request: &Request) -> Result<Option<Revision>> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((kind, payload)) = row else {
        return Ok(None);
    };
    if kind != "attachment.screen" {
        return Err("The reference screening operation was reused.".into());
    }
    let (saved, revision): (Request, Revision) = serde_json::from_str(&payload)?;
    if saved != *request {
        return Err("The reference screening operation was reused.".into());
    }
    Ok(Some(revision))
}

pub(crate) fn read(db: &Connection, id: Uuid, digest: &str) -> Result<Option<Revision>> {
    let record: Option<String> = db
        .query_row(
            "SELECT record FROM attachment_screenings WHERE attachment=?1 AND digest=?2",
            params![id.to_string(), digest],
            |row| row.get(0),
        )
        .optional()?;
    let revision: Option<Revision> = record
        .map(|record| serde_json::from_str(&record))
        .transpose()?;
    if revision.as_ref().is_some_and(|revision| {
        revision.original.attachment.id != id || revision.copy.attachment.sha256 != digest
    }) {
        return Err("The reference screening record is inconsistent.".into());
    }
    Ok(revision)
}

pub(crate) fn all(db: &Connection) -> Result<Vec<Revision>> {
    let revisions: Vec<Revision> = db
        .prepare("SELECT record FROM attachment_screenings ORDER BY attachment,digest LIMIT 50001")?
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|record| serde_json::from_str(&record?).map_err(Into::into))
        .collect::<Result<_>>()?;
    if revisions.len() > 50000 {
        return Err("Too many screened references to review.".into());
    }
    Ok(revisions)
}

pub(crate) fn save(db: &mut Connection, request: Request, revision: Revision) -> Result<Revision> {
    let tx = db.transaction()?;
    if let Some(previous) = previous(&tx, &request)? {
        return Ok(previous);
    }
    if crate::attachment_store::get(&tx, request.attachment_id)? != revision.original {
        return Err("The pasted reference changed during screening.".into());
    }
    if let Some(previous) = read(&tx, request.attachment_id, &revision.copy.attachment.sha256)? {
        if previous != revision {
            return Err("Approved reference copies disagree.".into());
        }
    } else {
        tx.execute(
            "INSERT INTO attachment_screenings VALUES(?1,?2,?3)",
            params![
                request.attachment_id.to_string(),
                revision.copy.attachment.sha256,
                serde_json::to_string(&revision)?,
            ],
        )?;
    }
    tx.execute(
        "INSERT INTO operations VALUES(?1,'attachment.screen',?2,'complete')",
        params![
            request.op_id.to_string(),
            serde_json::to_string(&(&request, &revision))?
        ],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'attachment.screened',?2)",
        params![
            crate::journal::timestamp(),
            serde_json::to_string(&revision.receipt())?
        ],
    )?;
    if revision.copy.attachment.sha256 != revision.original.attachment.sha256 {
        crate::editor_log::pasted_masked(&tx, &format!("masked-paste-review/{}", request.op_id))?;
    }
    tx.commit()?;
    Ok(revision)
}
