//! Fitted temperatures are versioned independently from requests already in flight.
use crate::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

pub fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Identity {
    backend: String,
    cache_prefix: bool,
    model_revision: String,
    prompt_version: String,
}

impl Identity {
    pub fn for_prefix(prefix: &str, cache_prefix: bool) -> Self {
        let model: Value = serde_json::from_str(include_str!(
            "../../../services/editor/src/intercat_editor/model_manifest.json"
        ))
        .expect("bundled model manifest");
        Self {
            backend: "GlimmerLogits(local)".into(),
            cache_prefix,
            model_revision: model["revision"].as_str().unwrap().into(),
            prompt_version: hash(prefix.as_bytes()),
        }
    }

    pub fn id(&self) -> String {
        hash(&serde_json::to_vec(self).expect("calibration identity"))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Artifact {
    format: u32,
    dataset_hash: String,
    profiles: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Applied {
    pub temperature: f64,
    pub profile: String,
    pub fit_hash: String,
    pub dataset_hash: String,
}

impl Artifact {
    fn profiles(&self) -> Result<BTreeMap<String, Applied>> {
        if self.format != 1 || !valid_hash(&self.dataset_hash) || self.profiles.is_empty() {
            return Err("Invalid calibration artifact.".into());
        }
        let mut profiles = BTreeMap::new();
        for (id, profile) in &self.profiles {
            let identity: Identity = serde_json::from_value(profile["identity"].clone())?;
            let pinned = Identity::for_prefix("", false);
            let temperature = profile["fit"]["temperature"]
                .as_f64()
                .ok_or("Missing temperature.")?;
            let calibration = profile["calibrationHash"]
                .as_str()
                .ok_or("Missing calibration hash.")?;
            let evaluation = profile["evaluationHash"]
                .as_str()
                .ok_or("Missing evaluation hash.")?;
            if identity.backend != pinned.backend
                || identity.model_revision != pinned.model_revision
                || !valid_hash(&identity.prompt_version)
                || identity.id() != *id
                || !temperature.is_finite()
                || !(0.05..=20.0).contains(&temperature)
                || !valid_hash(calibration)
                || !valid_hash(evaluation)
                || calibration == evaluation
                || profile["fit"]["groups"].as_u64().unwrap_or(0) == 0
                || profile["fit"]["observations"].as_u64().unwrap_or(0) == 0
                || profile["evaluationAfter"]["observations"]
                    .as_u64()
                    .unwrap_or(0)
                    == 0
            {
                return Err(
                    "Calibration does not match the pinned model or independent data.".into(),
                );
            }
            profiles.insert(
                id.clone(),
                Applied {
                    temperature,
                    profile: id.clone(),
                    dataset_hash: self.dataset_hash.clone(),
                    fit_hash: hash(&serde_json::to_vec(profile)?),
                },
            );
        }
        Ok(profiles)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct State {
    pub revision: u64,
    pub profiles: BTreeMap<String, Applied>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Change {
    pub op_id: Uuid,
    pub revision: u64,
    pub artifact: Artifact,
}

pub(crate) fn state(db: &Connection) -> Result<State> {
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key='editor-calibration'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
        .map(Option::unwrap_or_default)
}

pub(crate) fn resolve(db: &Connection, prefix: &str, cache_prefix: bool) -> Result<Applied> {
    let id = Identity::for_prefix(prefix, cache_prefix).id();
    if let Some(applied) = state(db)?.profiles.remove(&id) {
        return Ok(applied);
    }
    let bundled: Artifact = serde_json::from_str(include_str!(
        "../../../services/editor/src/intercat_editor/calibration_profiles.json"
    ))?;
    bundled
        .profiles()?
        .remove(&id)
        .ok_or_else(|| "No calibration matches this Editor prompt.".into())
}

pub(crate) fn apply(db: &mut Connection, change: Change) -> Result<State> {
    if change.op_id.get_version_num() != 7 {
        return Err("A version 7 operation ID is required.".into());
    }
    let profiles = change.artifact.profiles()?;
    db.pragma_update(None, "synchronous", "FULL")?;
    let transaction = db.transaction()?;
    let payload = serde_json::to_string(&change)?;
    let prior: Option<(String, String)> = transaction
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [change.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, prior)) = prior {
        if kind != "editor.calibrate" || prior != payload {
            return Err("Operation ID reused with different contents.".into());
        }
        return state(&transaction);
    }
    let mut current = state(&transaction)?;
    if current.revision != change.revision {
        return Err("Calibration changed. Reload before applying this fit.".into());
    }
    current.revision = current
        .revision
        .checked_add(1)
        .ok_or("Calibration revision exhausted.")?;
    current.profiles.extend(profiles);
    transaction.execute("INSERT INTO settings VALUES('editor-calibration',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [serde_json::to_string(&current)?])?;
    transaction.execute(
        "INSERT INTO operations VALUES(?1,'editor.calibrate',?2,'complete')",
        params![change.op_id.to_string(), payload],
    )?;
    transaction.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'editor.calibrated',?2)",
        params![
            crate::journal::timestamp(),
            serde_json::to_string(
                &json!({"revision":current.revision,"artifact":change.artifact})
            )?
        ],
    )?;
    transaction.commit()?;
    Ok(current)
}
