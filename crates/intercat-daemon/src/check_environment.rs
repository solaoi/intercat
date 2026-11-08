//! Explicit Check inputs. Persist paths and bindings while values stay in memory.
use crate::{Result, project::Folder};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};
use uuid::Uuid;

const LIMIT: usize = 1024 * 1024;
const SETTING: &str = "check_environment";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Selection {
    pub revision: u64,
    pub file: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Change {
    pub op_id: Uuid,
    pub revision: u64,
    pub file: Option<PathBuf>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Frozen {
    pub selection: Selection,
    digest: String,
}

fn read_values(path: &Path) -> Result<(Vec<u8>, BTreeMap<String, String>)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "The selected Check environment file could not be opened.")?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > LIMIT as u64 {
        return Err("Check environment must be a regular file no larger than 1 MiB.".into());
    }
    let mut bytes = Vec::new();
    file.take((LIMIT + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > LIMIT {
        return Err("Check environment exceeds 1 MiB.".into());
    }
    let mut values = BTreeMap::new();
    for item in dotenvy::from_read_iter(bytes.as_slice()) {
        // Parser errors may include the original line, which can contain a secret.
        let (name, value) =
            item.map_err(|_| "Check environment contains an invalid dotenv entry.")?;
        if !crate::secrets::valid_environment_name(&name) || value.contains('\0') {
            return Err("Check environment contains a reserved or invalid variable.".into());
        }
        values.insert(name, value);
    }
    if values.len() > 256 || values.values().map(String::len).sum::<usize>() > LIMIT {
        return Err("Check environment exceeds the variable limit.".into());
    }
    Ok((bytes, values))
}

fn digest(bytes: &[u8], values: &BTreeMap<String, String>) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.update(serde_json::to_vec(values)?);
    Ok(hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

impl Frozen {
    pub fn capture(mut selection: Selection) -> Result<Self> {
        if let Some(path) = &selection.file {
            selection.file = Some(
                path.canonicalize()
                    .map_err(|_| "The selected Check environment file is unavailable.")?,
            );
        }
        let digest = match &selection.file {
            Some(path) => {
                let (bytes, values) = read_values(path)?;
                digest(&bytes, &values)?
            }
            None => String::new(),
        };
        Ok(Self { selection, digest })
    }

    pub fn values(&self) -> Result<BTreeMap<String, String>> {
        let Some(path) = &self.selection.file else {
            return Ok(BTreeMap::new());
        };
        let (bytes, values) = read_values(path)?;
        if digest(&bytes, &values)? != self.digest {
            return Err("Check environment changed. Re-check with the current file.".into());
        }
        Ok(values)
    }
}

pub fn validate_file(path: &Path, project: &Folder) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .map_err(|_| "The selected Check environment file is unavailable.")?;
    if path.starts_with(&project.common_dir) {
        return Err("Choose a Check environment file outside Git metadata.".into());
    }
    if let Ok(relative) = path.strip_prefix(&project.root) {
        let tracked = intercat_git::Repository::open(&project.root)?.tracked_paths()?;
        if tracked.iter().any(|entry| Path::new(entry) == relative) {
            return Err("Choose an untracked Check environment file so it stays outside the candidate tree.".into());
        }
    }
    read_values(&path)?;
    Ok(path)
}

pub fn validate_project_file(path: &Path, project: &crate::project::Project) -> Result<PathBuf> {
    let mut checked = path.to_path_buf();
    for folder in &project.folders {
        checked = validate_file(&checked, folder)?;
    }
    Ok(checked)
}

pub(crate) fn selection(db: &Connection) -> Result<Selection> {
    let value: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            [SETTING],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
        .map(Option::unwrap_or_default)
}

pub(crate) fn change(
    db: &mut Connection,
    request: &Change,
    project_id: Uuid,
    file: Option<PathBuf>,
) -> Result<Selection> {
    if request.op_id.get_version_num() != 7 {
        return Err("Invalid Check environment operation.".into());
    }
    let tx = db.transaction()?;
    let payload = serde_json::to_string(request)?;
    let previous: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, saved)) = previous {
        if kind != "check-environment.change" || saved != payload {
            return Err("Operation ID was used for a different Check environment change.".into());
        }
        return selection(&tx);
    }
    let project: String = tx.query_row(
        "SELECT value FROM settings WHERE key='project'",
        [],
        |row| row.get(0),
    )?;
    if serde_json::from_str::<crate::project::Project>(&project)?.id != project_id {
        return Err("The project changed before saving Check environment.".into());
    }
    let current = selection(&tx)?;
    if request.revision != current.revision {
        return Err("Check environment changed in another window. Reload before saving.".into());
    }
    let next = Selection {
        revision: current
            .revision
            .checked_add(1)
            .ok_or("Check environment revision overflow.")?,
        file,
    };
    tx.execute(
        "INSERT INTO settings VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![SETTING, serde_json::to_string(&next)?],
    )?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'check-environment.change',?2,'complete')",
        params![request.op_id.to_string(), payload],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'check-environment.changed',?2)",
        params![crate::journal::timestamp(), serde_json::to_string(&next)?],
    )?;
    tx.commit()?;
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(db: &mut Connection, request: &Change, _root: &Path) -> Result<Selection> {
        let project = crate::folders::project(db)?.unwrap();
        change(db, request, project.id, request.file.clone())
    }

    #[test]
    fn frozen_values_parse_quotes_multiline_and_detect_replacements() -> Result<()> {
        let root = tempfile::tempdir()?;
        let file = root.path().join("test.env");
        std::fs::write(
            &file,
            "export API_KEY='private fixture value'\nMULTILINE=\"first\\nsecond\"\nlower_case=yes\nEMPTY=\n",
        )?;
        let frozen = Frozen::capture(Selection {
            revision: 1,
            file: Some(file.clone()),
        })?;
        let values = frozen.values()?;
        assert_eq!(values["API_KEY"], "private fixture value");
        assert_eq!(values["MULTILINE"], "first\nsecond");
        assert_eq!(values["lower_case"], "yes");
        assert_eq!(values["EMPTY"], "");
        assert!(!serde_json::to_string(&frozen)?.contains("private fixture value"));
        std::fs::write(&file, "API_KEY=changed\n")?;
        assert!(frozen.values().is_err());
        assert_eq!(Frozen::capture(Selection::default())?.values()?.len(), 0);
        Ok(())
    }

    #[test]
    fn invalid_entries_and_reserved_variables_never_echo_values() -> Result<()> {
        let root = tempfile::tempdir()?;
        let file = root.path().join("test.env");
        for content in [
            "API_KEY=\"sensitive fixture",
            "PATH=sensitive_fixture",
            "NODE_OPTIONS=sensitive_fixture",
            "GIT_CONFIG_COUNT=1",
        ] {
            std::fs::write(&file, content)?;
            let error = Frozen::capture(Selection {
                revision: 1,
                file: Some(file.clone()),
            })
            .err()
            .ok_or("Expected rejection")?
            .to_string();
            assert!(!error.contains("sensitive"));
        }
        std::os::unix::fs::symlink(&file, root.path().join("link"))?;
        assert!(read_values(&root.path().join("link")).is_err());
        Ok(())
    }

    #[test]
    fn selection_is_transactional_and_replay_keeps_newer_selection() -> Result<()> {
        let root = tempfile::tempdir()?;
        let database = root.path().join("settings.sqlite");
        let mut db = Connection::open(&database)?;
        db.execute_batch("CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE operations(id TEXT PRIMARY KEY,kind TEXT,payload TEXT,state TEXT); CREATE TABLE events(ts INTEGER,kind TEXT,payload TEXT);")?;
        let folder = Folder {
            id: "fixture-folder".into(),
            name: "fixture".into(),
            path: root.path().into(),
            root: root.path().into(),
            common_dir: root.path().join(".git"),
            base: "base".into(),
            base_branch: None,
        };
        let project = crate::project::Project {
            id: Uuid::now_v7(),
            name: "fixture".into(),
            folders: vec![folder],
        };
        db.execute(
            "INSERT INTO settings VALUES('project',?1)",
            [serde_json::to_string(&project)?],
        )?;
        assert_eq!(selection(&db)?, Selection::default());
        let request = Change {
            op_id: Uuid::now_v7(),
            revision: 0,
            file: Some(root.path().join("test.env")),
        };
        let first = save(&mut db, &request, root.path())?;
        assert_eq!(first.revision, 1);
        assert_eq!(save(&mut db, &request, root.path())?, first);
        assert!(
            save(
                &mut db,
                &Change {
                    op_id: Uuid::now_v7(),
                    ..request.clone()
                },
                root.path()
            )
            .is_err()
        );
        let next = save(
            &mut db,
            &Change {
                op_id: Uuid::now_v7(),
                revision: 1,
                file: None,
            },
            root.path(),
        )?;
        assert_eq!(save(&mut db, &request, root.path())?, next);
        assert!(
            save(
                &mut db,
                &Change {
                    file: None,
                    ..request
                },
                root.path()
            )
            .is_err()
        );
        db.execute_batch("CREATE TEMP TRIGGER fail_environment_event BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT, 'simulated write failure'); END;")?;
        let failed = Change {
            op_id: Uuid::now_v7(),
            revision: 2,
            file: Some(root.path().join("other.env")),
        };
        assert!(save(&mut db, &failed, root.path()).is_err());
        assert_eq!(selection(&db)?, next);
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM operations WHERE id=?1",
                [failed.op_id.to_string()],
                |row| row.get::<_, i64>(0)
            )?,
            0
        );
        drop(db);
        assert_eq!(selection(&Connection::open(database)?)?, next);
        Ok(())
    }
}
