//! Rebuild only fingerprints that can be proved against the external receipt.
//! History and worker startup remain blocked until recovery review is complete.
use crate::{Result, fingerprint_receipt::Receipt, secrets::Seal};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const SETTING: &str = "fingerprint-recovery";

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub receipt: Receipt,
    pub manifest: crate::project_secrets::Manifest,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct State {
    source: Receipt,
    current: Seal,
    rebuilt_projects: Vec<String>,
}
fn read(db: &Connection) -> Result<Option<State>> {
    db.query_row("SELECT value FROM settings WHERE key=?1", [SETTING], |r| {
        r.get::<_, String>(0)
    })
    .optional()?
    .map(|s| serde_json::from_str(&s).map_err(Into::into))
    .transpose()
}
fn report(db: &Connection, state: &State) -> Result<Value> {
    let current = crate::secret_store::catalog(db)?;
    if current.current != state.current {
        return Err("The database changed during fingerprint recovery.".into());
    }
    let mut result = serde_json::to_value(current.proof.difference(&state.source.proof))?;
    result["rebuiltProjects"] = json!(state.rebuilt_projects);
    result["historyReviewRequired"] = json!(true);
    Ok(result)
}
pub fn status(db: &Connection, receipt: Option<&Receipt>) -> Result<Option<Value>> {
    let Some(state) = read(db)? else {
        return Ok(None);
    };
    if receipt != Some(&state.source) {
        return Err("The saved confirmation changed during fingerprint recovery.".into());
    }
    Ok(Some(report(db, &state)?))
}

pub fn rebuild(db: &mut Connection, request: Request) -> Result<Value> {
    request.receipt.validate()?;
    request.manifest.validate()?;
    if request.op_id.get_version_num() != 7 {
        return Err("Invalid fingerprint recovery operation.".into());
    }
    let expected = request
        .receipt
        .proof
        .projects
        .iter()
        .find(|project| project.project_id == request.manifest.project_id)
        .ok_or("This project is not in the saved confirmation.")?;
    let tx = db.transaction()?;
    let before = crate::secret_store::catalog(&tx)?;
    let mut state = if let Some(state) = read(&tx)? {
        if state.source != request.receipt || state.current != before.current {
            return Err("The database or saved confirmation changed during recovery.".into());
        }
        state
    } else {
        if before.accept(Some(&request.receipt)).is_ok()
            || before.current.revision > request.receipt.catalog.revision
        {
            return Err(
                "This database is not an older copy requiring fingerprint recovery.".into(),
            );
        }
        State {
            source: request.receipt.clone(),
            current: before.current.clone(),
            rebuilt_projects: vec![],
        }
    };
    let request_hash: String =
        Sha256::digest(serde_json::to_vec(&(&request.receipt, &request.manifest))?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
    let payload =
        json!({"requestHash":request_hash,"projectId":request.manifest.project_id}).to_string();
    let old: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, old)) = old {
        if kind != "fingerprints.rebuild" || old != payload {
            return Err("The recovery operation ID was already used for another request.".into());
        }
        return report(&tx, &state);
    }
    crate::project_secrets::rebuild(&tx, &request.manifest, &expected.digest)?;
    if !state.rebuilt_projects.contains(&expected.project_id) {
        state.rebuilt_projects.push(expected.project_id.clone());
        state.rebuilt_projects.sort();
        state.current =
            crate::secret_store::advance_recovery(&tx, &before, request.receipt.catalog.revision)?
                .current;
    }
    tx.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![SETTING,serde_json::to_string(&state)?])?;
    tx.execute("INSERT INTO operations(id,kind,payload,state) VALUES(?1,'fingerprints.rebuild',?2,'complete')", params![request.op_id.to_string(),payload])?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'fingerprints.rebuilt',?2)",
        params![
            crate::journal::timestamp(),
            json!({"projectId":expected.project_id,"opId":request.op_id}).to_string()
        ],
    )?;
    let result = report(&tx, &state)?;
    tx.commit()?;
    Ok(result)
}
