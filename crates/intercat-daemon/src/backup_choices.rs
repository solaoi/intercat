//! Recovery decisions are staged separately from history mutation and receipt publication.
use crate::{Result, fingerprint_receipt::Receipt};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Choice {
    PickUp,
    DiscardHistory,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub session_id: String,
    pub choice: Choice,
    pub review_id: Option<Uuid>,
    pub acknowledge_unread: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Record {
    pub request: Request,
    pub track_id: String,
    pub binding: String,
    pub review_seal: Option<String>,
    pub seal: String,
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE backup_session_choices(session TEXT PRIMARY KEY,record TEXT NOT NULL);",
    )?;
    Ok(())
}
pub(crate) fn load(db: &Connection, key: &crate::secrets::Key) -> Result<Vec<Record>> {
    use subtle::ConstantTimeEq;
    let mut query = db.prepare(
        "SELECT session,record FROM backup_session_choices ORDER BY session LIMIT 10001",
    )?;
    let records = query
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .map(|r| {
            let (session, text) = r?;
            let record: Record = serde_json::from_str(&text)?;
            if record.request.session_id != session
                || !bool::from(record.seal.as_bytes().ct_eq(record.tag(key)?.as_bytes()))
            {
                return Err("The staged recovery decision changed.".into());
            }
            Ok(record)
        })
        .collect::<Result<Vec<_>>>()?;
    if records.len() > 10000 {
        return Err("Too many staged recovery decisions.".into());
    }
    Ok(records)
}
impl Record {
    fn tag(&self, key: &crate::secrets::Key) -> Result<String> {
        let mut material = b"intercat.backup-choice.v1\0".to_vec();
        material.extend(serde_json::to_vec(&(
            &self.request,
            &self.track_id,
            &self.binding,
            &self.review_seal,
        ))?);
        Ok(key
            .tag(&material)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }
    pub(crate) fn display(&self, binding: &str) -> Value {
        json!({"opId":self.request.op_id,"choice":self.request.choice,"reviewId":self.request.review_id,"acknowledgeUnread":self.request.acknowledge_unread,"needsRecheck":true,"stale":self.binding!=binding})
    }
}
pub(crate) fn save(
    db: &mut Connection,
    receipt: &Receipt,
    mut record: Record,
    key: &crate::secrets::Key,
) -> Result<()> {
    let tx = db.transaction()?;
    let plan = crate::backup_review::plan(&tx, receipt, key)?;
    if plan.binding(receipt)? != record.binding
        || !plan.owns_affected_session(&record.request.session_id, &record.track_id)
    {
        return Err("The recovery target changed. Review it again.".into());
    }
    if let Some(review) = record.request.review_id {
        let seal:Option<String>=tx.query_row("SELECT json_extract(payload,'$.review.seal') FROM operations WHERE id=?1 AND kind='backup.files.review' AND state='complete'",[review.to_string()],|r|r.get(0)).optional()?;
        if seal != record.review_seal {
            return Err("The saved file review changed.".into());
        }
    }
    record.seal = record.tag(key)?;
    let encoded = serde_json::to_string(&record)?;
    let old: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [record.request.op_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, old)) = old {
        let current: Option<String> = tx
            .query_row(
                "SELECT record FROM backup_session_choices WHERE session=?1",
                [&record.request.session_id],
                |r| r.get(0),
            )
            .optional()?;
        if kind != "backup.session.choice" || old != encoded {
            return Err("Recovery decision operation ID reused.".into());
        }
        if current.as_deref() != Some(&encoded) {
            return Err("A newer recovery decision replaced this request.".into());
        }
        return Ok(());
    }
    tx.execute("INSERT INTO backup_session_choices VALUES(?1,?2) ON CONFLICT(session) DO UPDATE SET record=excluded.record",params![record.request.session_id,encoded])?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'backup.session.choice',?2,'complete')",
        params![record.request.op_id.to_string(), encoded],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'backup.session.choice',?2)",
        params![crate::journal::timestamp(), encoded],
    )?;
    tx.commit()?;
    Ok(())
}
