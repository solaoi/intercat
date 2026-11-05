//! Keychain values are only held in memory. SQLite retains their active manifest
//! and historical fingerprints, so removing a value never exposes old output.
use crate::{
    Result,
    progress::Fingerprint,
    secret_input::Value,
    secrets::{Catalog, Key},
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Descriptor {
    pub name: String,
    pub byte_len: usize,
    pub tag: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub project_id: String,
    pub revision: u64,
    pub fingerprints: Vec<Descriptor>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Prepare {
    pub op_id: Uuid,
    pub project_id: String,
    pub revision: u64,
    pub fingerprints: Vec<Descriptor>,
    pub key_check: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub name: String,
    pub value: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Load {
    pub project_id: String,
    pub revision: u64,
    pub entries: Vec<Entry>,
}
#[derive(Clone)]
pub struct Config {
    pub manifest: Manifest,
    pub values: Vec<Value>,
}
#[derive(Clone)]
pub struct Store {
    current: Arc<Mutex<Option<Config>>>,
    changes: watch::Sender<u64>,
}
impl Default for Store {
    fn default() -> Self {
        Self {
            current: Arc::new(Mutex::new(None)),
            changes: watch::channel(0).0,
        }
    }
}
impl Store {
    pub fn ready(&self, state: &serde_json::Value) -> bool {
        state.is_null()
            || state["ready"] == true
                && state["projectId"]
                    .as_str()
                    .and_then(|id| self.revision(id))
                    .is_some_and(|revision| Some(revision) == state["revision"].as_u64())
    }
    pub fn for_host(&self, state: &serde_json::Value) -> Result<Option<Config>> {
        if state.is_null() {
            return Ok(None);
        }
        if !self.ready(state) {
            return Err(crate::journal::StartSuperseded.into());
        }
        let config = state["projectId"]
            .as_str()
            .and_then(|id| self.get(id))
            .ok_or(crate::journal::StartSuperseded)?;
        if Some(config.manifest.revision) != state["revision"].as_u64() {
            return Err(crate::journal::StartSuperseded.into());
        }
        Ok(Some(config))
    }
    pub fn revision(&self, project: &str) -> Option<u64> {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .filter(|v| v.manifest.project_id == project)
            .map(|v| v.manifest.revision)
    }
    pub fn get(&self, project: &str) -> Option<Config> {
        self.current
            .lock()
            .unwrap()
            .as_ref()
            .filter(|v| v.manifest.project_id == project)
            .cloned()
    }
    pub fn publish(&self, config: Config) {
        *self.current.lock().unwrap() = Some(config);
        self.changes
            .send_modify(|revision| *revision = revision.wrapping_add(1));
    }
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
impl Manifest {
    pub fn validate(&self) -> Result<()> {
        crate::secrets::decode(&self.project_id)?;
        if self.revision > 9_007_199_254_740_991 || self.fingerprints.len() > 32 {
            return Err("Invalid project secret manifest.".into());
        }
        let mut previous = None;
        let mut size = 0;
        for value in &self.fingerprints {
            if !crate::secrets::valid_name(&value.name)
                || !(1..=8192).contains(&value.byte_len)
                || previous.is_some_and(|p: &str| p >= value.name.as_str())
            {
                return Err("Invalid project secret manifest.".into());
            }
            crate::secrets::decode(&value.tag)?;
            size += value.byte_len;
            previous = Some(value.name.as_str());
        }
        if size > 65536 {
            return Err("Project secrets exceed 64 KiB.".into());
        }
        Ok(())
    }
    fn digest(&self) -> Result<Vec<u8>> {
        Ok(Sha256::digest(serde_json::to_vec(self)?).to_vec())
    }
}
impl Prepare {
    pub fn metadata(self, key: &Key) -> Result<(Uuid, Manifest)> {
        use subtle::ConstantTimeEq;
        if self.op_id.get_version_num() != 7
            || !bool::from(crate::secrets::decode(&self.key_check)?.ct_eq(&key.check()))
        {
            return Err("Invalid project secret registration.".into());
        }
        let manifest = Manifest {
            project_id: self.project_id,
            revision: self.revision,
            fingerprints: self.fingerprints,
        };
        manifest.validate()?;
        Ok((self.op_id, manifest))
    }
}
impl Load {
    pub fn prepare(self, key: &Key) -> Result<Config> {
        let mut values = Vec::with_capacity(self.entries.len());
        let mut fingerprints = Vec::with_capacity(self.entries.len());
        for entry in self.entries {
            fingerprints.push(Descriptor {
                name: entry.name.clone(),
                byte_len: entry.value.len(),
                tag: hex(&key.tag(entry.value.as_bytes())),
            });
            values.push(Value::new(entry.name, entry.value)?);
        }
        let manifest = Manifest {
            project_id: self.project_id,
            revision: self.revision,
            fingerprints,
        };
        manifest.validate()?;
        Ok(Config { manifest, values })
    }
}
pub fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE project_secret_state(project TEXT PRIMARY KEY,revision INTEGER NOT NULL,manifest TEXT NOT NULL,digest BLOB NOT NULL,ready INTEGER NOT NULL); CREATE TABLE project_fingerprints(project TEXT NOT NULL,name TEXT NOT NULL,byte_len INTEGER NOT NULL,tag BLOB NOT NULL,PRIMARY KEY(project,name,byte_len,tag));")?;
    Ok(())
}
pub fn project_id(db: &Connection) -> Result<Option<String>> {
    let project: Option<String> = db
        .query_row("SELECT value FROM settings WHERE key='project'", [], |r| {
            r.get(0)
        })
        .optional()?;
    project
        .map(|project| {
            let project: crate::project::Project = serde_json::from_str(&project)?;
            use std::os::unix::ffi::OsStrExt;
            Ok(hex(&Sha256::digest(
                project.common_dir.as_os_str().as_bytes(),
            )))
        })
        .transpose()
}
fn validate_project(db: &Connection, project: &str) -> Result<()> {
    if project_id(db)?.as_deref() != Some(project) {
        return Err("Project secret destination changed.".into());
    }
    Ok(())
}
pub fn catalog_bytes(db: &Connection) -> Result<Vec<u8>> {
    let manifests = db
        .prepare(
            "SELECT project,revision,manifest,digest FROM project_secret_state ORDER BY project",
        )?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Vec<u8>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let fingerprints=db.prepare("SELECT project,name,byte_len,tag FROM project_fingerprints ORDER BY project,name,byte_len,tag")?.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,Vec<u8>>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(serde_json::to_vec(&(manifests, fingerprints))?)
}
pub(crate) fn proof(db: &Connection) -> Result<Vec<crate::fingerprint_receipt::Project>> {
    use sha2::{Digest, Sha256};
    let ids = db.prepare("SELECT project FROM project_secret_state UNION SELECT project FROM project_fingerprints ORDER BY project")?
        .query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    ids.into_iter().map(|project_id| {
        let manifest = db.prepare("SELECT revision,manifest,digest FROM project_secret_state WHERE project=?1")?
            .query_map([&project_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Vec<u8>>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let fingerprints = db.prepare("SELECT name,byte_len,tag FROM project_fingerprints WHERE project=?1 ORDER BY name,byte_len,tag")?
            .query_map([&project_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Vec<u8>>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let hash = Sha256::digest(serde_json::to_vec(&(&project_id, manifest, fingerprints))?);
        Ok(crate::fingerprint_receipt::Project { project_id, digest: hash.iter().map(|b| format!("{b:02x}")).collect() })
    }).collect()
}
pub(crate) fn rebuild(tx: &Transaction<'_>, manifest: &Manifest, expected: &str) -> Result<()> {
    for value in &manifest.fingerprints {
        tx.execute(
            "INSERT OR IGNORE INTO project_fingerprints VALUES(?1,?2,?3,?4)",
            params![
                manifest.project_id,
                value.name,
                value.byte_len as i64,
                crate::secrets::decode(&value.tag)?.as_slice()
            ],
        )?;
    }
    let count: i64 = tx.query_row(
        "SELECT count(*) FROM project_fingerprints WHERE project=?1",
        [&manifest.project_id],
        |r| r.get(0),
    )?;
    if count > 256 {
        return Err("Too many retained project fingerprints.".into());
    }
    tx.execute("INSERT INTO project_secret_state VALUES(?1,?2,?3,?4,0) ON CONFLICT(project) DO UPDATE SET revision=excluded.revision,manifest=excluded.manifest,digest=excluded.digest,ready=0", params![manifest.project_id,manifest.revision as i64,serde_json::to_string(manifest)?,manifest.digest()?])?;
    if !proof(tx)?
        .iter()
        .any(|project| project.project_id == manifest.project_id && project.digest == expected)
    {
        return Err("Saved project values cannot reconstruct the confirmed fingerprints. No fingerprints were changed.".into());
    }
    Ok(())
}
pub fn check(db: &mut Connection, id: Uuid, manifest: &Manifest) -> Result<()> {
    let tx = db.transaction()?;
    register_in(&tx, id, manifest)?;
    tx.rollback()?;
    Ok(())
}
pub fn register(db: &mut Connection, id: Uuid, manifest: &Manifest) -> Result<Catalog> {
    let tx = db.transaction()?;
    let result = register_in(&tx, id, manifest)?;
    tx.commit()?;
    Ok(result)
}
pub fn hold(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    if tx.execute("UPDATE project_secret_state SET ready=0 WHERE ready=1", [])? > 0 {
        tx.execute(
            "INSERT INTO events(ts,kind,payload) VALUES(?1,'project.secrets.held','{}')",
            [crate::journal::timestamp()],
        )?;
    }
    tx.commit()?;
    Ok(())
}
fn register_in(tx: &rusqlite::Transaction<'_>, id: Uuid, manifest: &Manifest) -> Result<Catalog> {
    validate_project(tx, &manifest.project_id)?;
    let before = crate::secret_store::catalog(tx)?;
    let digest = manifest.digest()?;
    let old: Option<(i64, Vec<u8>)> = tx
        .query_row(
            "SELECT revision,digest FROM project_secret_state WHERE project=?1",
            [&manifest.project_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((revision, old)) = old {
        if revision == manifest.revision as i64 && old == digest {
            return Ok(before);
        }
        if revision >= manifest.revision as i64 {
            return Err("The project secret revision changed. Reload before editing it.".into());
        }
    }
    if before.sealed.as_ref() != Some(&before.current) {
        return Err("Confirm the previous fingerprint registration first.".into());
    }
    let request = Sha256::digest(serde_json::to_vec(manifest)?);
    if tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM secret_registrations WHERE operation=?1)",
        [id.to_string()],
        |r| r.get::<_, bool>(0),
    )? {
        return Err("The project secret operation ID was reused.".into());
    }
    for fingerprint in &manifest.fingerprints {
        tx.execute(
            "INSERT OR IGNORE INTO project_fingerprints VALUES(?1,?2,?3,?4)",
            params![
                manifest.project_id,
                fingerprint.name,
                fingerprint.byte_len as i64,
                crate::secrets::decode(&fingerprint.tag)?.as_slice()
            ],
        )?;
    }
    let sessions = tx
        .prepare("SELECT DISTINCT session,track FROM secret_fingerprints")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (session, track) in sessions {
        if crate::secret_store::read(tx, &track, &session)?.len() > 256 {
            return Err("Too many secret fingerprints for a session.".into());
        }
    }
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM project_fingerprints WHERE project=?1",
        [&manifest.project_id],
        |r| r.get(0),
    )?;
    if count > 256 {
        return Err("Too many retained project secret fingerprints.".into());
    }
    tx.execute("INSERT INTO project_secret_state VALUES(?1,?2,?3,?4,0) ON CONFLICT(project) DO UPDATE SET revision=excluded.revision,manifest=excluded.manifest,digest=excluded.digest,ready=0",params![manifest.project_id,manifest.revision as i64,serde_json::to_string(manifest)?,digest])?;
    tx.execute(
        "INSERT INTO secret_registrations VALUES(?1,?2)",
        params![id.to_string(), request.as_slice()],
    )?;
    tx.execute("INSERT INTO events(ts,kind,payload) VALUES(?1,'project.secrets.prepared',?2)",params![crate::journal::timestamp(),serde_json::json!({"projectId":manifest.project_id,"revision":manifest.revision,"names":manifest.fingerprints.iter().map(|v|&v.name).collect::<Vec<_>>(),"opId":id}).to_string()])?;
    crate::secret_store::advance(tx, &before)?;
    let result = crate::secret_store::catalog(tx)?;
    Ok(result)
}
pub fn activate(db: &mut Connection, manifest: &Manifest) -> Result<()> {
    let tx = db.transaction()?;
    validate_project(&tx, &manifest.project_id)?;
    let catalog = crate::secret_store::catalog(&tx)?;
    if catalog.sealed.as_ref() != Some(&catalog.current) {
        return Err("Project fingerprints have not been confirmed.".into());
    }
    let saved: Vec<u8> = tx.query_row(
        "SELECT digest FROM project_secret_state WHERE project=?1 AND revision=?2",
        params![manifest.project_id, manifest.revision as i64],
        |r| r.get(0),
    )?;
    if saved != manifest.digest()? {
        return Err("Project secret values do not match their saved fingerprints.".into());
    }
    if tx.execute(
        "UPDATE project_secret_state SET ready=1 WHERE project=?1 AND ready=0",
        [&manifest.project_id],
    )? != 0
    {
        tx.execute(
            "INSERT INTO events(ts,kind,payload) VALUES(?1,'project.secrets.ready',?2)",
            params![
                crate::journal::timestamp(),
                serde_json::json!({"projectId":manifest.project_id,"revision":manifest.revision})
                    .to_string()
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}
pub fn fingerprints(db: &Connection) -> Result<Vec<Fingerprint>> {
    let Some(project) = project_id(db)? else {
        return Ok(vec![]);
    };
    let rows=db.prepare("SELECT name,byte_len,tag FROM project_fingerprints WHERE project=?1 ORDER BY name,byte_len,tag")?.query_map([project],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,Vec<u8>>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(name, byte_len, tag)| {
            Ok(Fingerprint {
                name,
                byte_len: byte_len
                    .try_into()
                    .map_err(|_| "Invalid project fingerprint length.")?,
                tag: tag.try_into().map_err(|_| "Invalid project fingerprint.")?,
            })
        })
        .collect()
}
pub fn status(db: &Connection) -> Result<serde_json::Value> {
    let Some(project) = project_id(db)? else {
        return Ok(serde_json::Value::Null);
    };
    let row: Option<(String, bool)> = db
        .query_row(
            "SELECT manifest,ready FROM project_secret_state WHERE project=?1",
            [&project],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let all_names = db
        .prepare("SELECT DISTINCT name FROM project_fingerprints WHERE project=?1 ORDER BY name")?
        .query_map([&project], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    match row {
        Some((manifest, ready)) => {
            let manifest: Manifest = serde_json::from_str(&manifest)?;
            Ok(
                serde_json::json!({"projectId":project,"revision":manifest.revision,"names":manifest.fingerprints.iter().map(|v|&v.name).collect::<Vec<_>>(),"allNames":all_names,"ready":ready}),
            )
        }
        None => Ok(
            serde_json::json!({"projectId":project,"revision":0,"names":[],"allNames":[],"ready":false}),
        ),
    }
}
pub fn validate_revision(tx: &Transaction<'_>, revision: Option<u64>) -> Result<()> {
    let Some(project) = project_id(tx)? else {
        return Ok(());
    };
    let expected: Option<(i64, bool)> = tx
        .query_row(
            "SELECT revision,ready FROM project_secret_state WHERE project=?1",
            [project],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if !expected.is_some_and(|(wanted, ready)| ready && revision == u64::try_from(wanted).ok()) {
        return Err(crate::journal::StartSuperseded.into());
    }
    Ok(())
}
pub fn validate_dispatch(tx: &Transaction<'_>, track: &str) -> Result<()> {
    let actual: Option<i64> = tx
        .query_row(
            "SELECT json_extract(record,'$.secretRevision') FROM workers WHERE track_id=?1",
            [track],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    validate_revision(tx, actual.and_then(|value| u64::try_from(value).ok()))
}
