//! Snapshots at the approval boundary, and the exact paths they can recover.
use crate::{Result, journal::Journal, preparation::Workspace};
use intercat_git::{Repository, SavedFile};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub id: Uuid,
    pub track_id: String,
    pub repair_run_id: Option<Uuid>,
    pub item_id: String,
    pub approval_id: String,
    pub sha: String,
    pub reference: String,
    pub created_at: i64,
    pub paths: Vec<String>,
    pub absent: Vec<String>,
    pub directories: Vec<String>,
    pub uncovered: Vec<String>,
    pub complete: bool,
    pub purpose: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub id: Uuid,
    pub sha: String,
    pub files: usize,
    pub complete: bool,
    pub uncovered: usize,
}
impl Snapshot {
    pub fn summary(&self) -> Summary {
        Summary {
            id: self.id,
            sha: self.sha.clone(),
            files: self.paths.len(),
            complete: self.complete,
            uncovered: self.uncovered.len(),
        }
    }
}
pub fn requested(request: &Value, workspace: &Path) -> bool {
    crate::policy::evaluate(
        request,
        &crate::policy::Context {
            stops: Default::default(),
            workspace,
            recoverable_deletion: false,
            remote_hosts: &[],
        },
    )
    .impact
    .is_some_and(|i| i.kind == "delete")
}
fn relative(root: &Path, path: &Path) -> Option<String> {
    let path = if path.is_absolute() {
        path.strip_prefix(root).ok()?
    } else {
        path
    };
    let mut parts = Vec::new();
    for part in path.components() {
        match part {
            Component::Normal(s) => parts.push(s.to_str()?),
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    Some(parts.join("/"))
}
fn matches_root(path: &str, root: &str) -> bool {
    root.is_empty()
        || path == root
        || path
            .strip_prefix(root)
            .is_some_and(|suffix| suffix.starts_with('/'))
}
pub(crate) fn validate_parent(root: &Path, path: &str) -> Result<()> {
    let root_info = std::fs::symlink_metadata(root)?;
    if !root_info.is_dir() || root_info.file_type().is_symlink() || root.canonicalize()? != root {
        return Err("Worker workspace is no longer its original directory.".into());
    }
    if relative(root, Path::new(path)).as_deref() != Some(path)
        || Path::new(path).is_absolute()
        || path.split('/').any(|s| s == ".git")
    {
        return Err("Invalid saved path.".into());
    }
    let mut here = root.to_path_buf();
    let parts: Vec<_> = Path::new(path).components().collect();
    for part in parts.iter().take(parts.len().saturating_sub(1)) {
        let Component::Normal(name) = part else {
            return Err("Invalid saved path.".into());
        };
        here.push(name);
        match std::fs::symlink_metadata(&here) {
            Ok(m) if m.is_dir() && !m.file_type().is_symlink() => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            _ => return Err("A path parent is not a real directory.".into()),
        }
    }
    Ok(())
}
fn current_bytes(root: &Path, path: &str) -> Result<Option<(Vec<u8>, bool, bool)>> {
    validate_parent(root, path)?;
    let path = root.join(path);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    use std::os::unix::fs::PermissionsExt;
    if metadata.file_type().is_symlink() {
        use std::os::unix::ffi::OsStrExt;
        return Ok(Some((
            std::fs::read_link(path)?.as_os_str().as_bytes().to_vec(),
            true,
            false,
        )));
    }
    if !metadata.is_file() {
        return Err("The selected path is not a regular file or symlink.".into());
    }
    Ok(Some((
        std::fs::read(path)?,
        false,
        metadata.permissions().mode() & 0o111 != 0,
    )))
}

enum Selection {
    Paths(Vec<String>, bool),
    Tracked(Vec<String>),
    Clean(Vec<String>, bool, bool),
    Unknown(String),
}
fn selection(root: &Path, request: &Value) -> Selection {
    let index = request["currentRequirementId"]["sourceIndex"]
        .as_u64()
        .unwrap_or(u64::MAX) as usize;
    let Some(stage) = request["subject"]["stages"]
        .as_array()
        .and_then(|s| s.get(index))
    else {
        return Selection::Unknown("Unresolved command".into());
    };
    let Some(values) = stage["argv"]
        .as_array()
        .filter(|_| stage["argvComplete"] == true)
    else {
        return Selection::Unknown("Unresolved arguments".into());
    };
    let Some(args) = values.iter().map(Value::as_str).collect::<Option<Vec<_>>>() else {
        return Selection::Unknown("Unresolved arguments".into());
    };
    let name = args
        .first()
        .and_then(|s| Path::new(s).file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let paths = |values: &[&str]| -> Option<Vec<String>> {
        values
            .iter()
            .map(|s| {
                if s.contains(['*', '?', '[', ']', '{', '}'])
                    || Path::new(s).components().any(|c| c == Component::ParentDir)
                    || s.contains("/./")
                    || s.ends_with("/.")
                {
                    return None;
                }
                let path = relative(root, Path::new(s))?;
                if s.ends_with('/')
                    && std::fs::symlink_metadata(root.join(&path))
                        .is_ok_and(|m| m.file_type().is_symlink())
                {
                    return None;
                }
                Some(path)
            })
            .collect()
    };
    if name == "rm" {
        let mut literals = Vec::new();
        let mut options = true;
        for arg in &args[1..] {
            if options && *arg == "--" {
                options = false;
                continue;
            }
            if options && arg.starts_with('-') {
                if !arg.starts_with("--") && arg[1..].chars().all(|c| "fRrvidI".contains(c))
                    || [
                        "--force",
                        "--recursive",
                        "--verbose",
                        "--dir",
                        "--interactive",
                    ]
                    .contains(arg)
                {
                    continue;
                }
                return Selection::Unknown("Unsupported rm option".into());
            }
            literals.push(*arg);
        }
        return paths(&literals)
            .map(|p| Selection::Paths(p, false))
            .unwrap_or_else(|| {
                Selection::Unknown("Deletion includes external or unresolved paths".into())
            });
    }
    if name == "find" {
        if args.contains(&"-L") || args.contains(&"-H") {
            return Selection::Unknown("find may follow symlinks".into());
        }
        let roots: Vec<_> = args[1..]
            .iter()
            .take_while(|s| !s.starts_with('-'))
            .copied()
            .collect();
        return paths(&roots)
            .filter(|p| !p.is_empty())
            .map(|p| Selection::Paths(p, false))
            .unwrap_or_else(|| Selection::Unknown("Unresolved find roots".into()));
    }
    if name == "git" {
        let Some(command) = crate::policy::git_subcommand(&args[1..]).map(|i| i + 1) else {
            return Selection::Unknown("Unresolved Git command".into());
        };
        // A command that changes its own cwd has a different path namespace.
        if args[..command].iter().any(|s| {
            ["-C", "--work-tree", "--git-dir"].contains(s)
                || s.starts_with("--work-tree=")
                || s.starts_with("--git-dir=")
        }) {
            return Selection::Unknown("Git changes its working directory".into());
        }
        let tail = &args[command + 1..];
        if args[command] == "checkout"
            && tail
                .iter()
                .take_while(|arg| **arg != "--")
                .any(|arg| !arg.starts_with('-'))
        {
            return Selection::Unknown("Checkout uses another source tree".into());
        }
        match args[command] {
            "restore" | "checkout" => {
                let mut roots = Vec::new();
                let mut after = false;
                for arg in tail {
                    if *arg == "--" {
                        after = true;
                        continue;
                    }
                    if !after && ["--worktree", "-W"].contains(arg) {
                        continue;
                    }
                    if !after && arg.starts_with('-') {
                        return Selection::Unknown(
                            "Restore includes index or alternate-source changes".into(),
                        );
                    }
                    roots.push(*arg);
                }
                return paths(&roots)
                    .filter(|p| !p.is_empty())
                    .map(Selection::Tracked)
                    .unwrap_or_else(|| Selection::Unknown("Unresolved Git paths".into()));
            }
            "reset" if tail.iter().all(|s| ["--hard", "HEAD"].contains(s)) => {
                return Selection::Tracked(vec![String::new()]);
            }
            "clean" => {
                let mut roots = Vec::new();
                let (mut ignored, mut only_ignored) = (false, false);
                for arg in tail {
                    if *arg == "--" {
                        continue;
                    }
                    if arg.starts_with('-')
                        && !arg.starts_with("--")
                        && arg[1..].chars().all(|c| "fdxXq".contains(c))
                    {
                        ignored |= arg.contains('x');
                        only_ignored |= arg.contains('X');
                        continue;
                    }
                    if arg.starts_with('-') {
                        return Selection::Unknown("Unsupported clean option".into());
                    }
                    roots.push(*arg);
                }
                if roots.is_empty() {
                    roots.push(".");
                }
                return paths(&roots)
                    .map(|p| Selection::Clean(p, ignored, only_ignored))
                    .unwrap_or_else(|| Selection::Unknown("Unresolved clean paths".into()));
            }
            _ => {}
        }
    }
    Selection::Unknown("The snapshot cannot prove the command’s deletion scope".into())
}

fn collect(
    root: &Path,
    roots: &[String],
    skip_git: bool,
) -> (BTreeSet<String>, BTreeSet<String>, Vec<String>) {
    let (mut files, mut dirs, mut problems) = (BTreeSet::new(), BTreeSet::new(), Vec::new());
    for name in roots {
        let base = root.join(name);
        if let Err(error) = validate_parent(root, name) {
            problems.push(format!("{name}: {error}"));
            continue;
        }
        if !base.exists() && std::fs::symlink_metadata(&base).is_err() {
            files.insert(name.clone());
            continue;
        }
        for entry in walkdir::WalkDir::new(&base)
            .follow_links(false)
            .follow_root_links(false)
            .into_iter()
            .filter_entry(|e| !skip_git || e.file_name() != ".git")
        {
            match entry {
                Ok(entry) => {
                    let Some(path) = relative(root, entry.path()) else {
                        problems.push(name.clone());
                        continue;
                    };
                    if path.split('/').any(|part| part == ".git") {
                        problems.push(path);
                        continue;
                    }
                    if entry.file_type().is_dir() {
                        if !path.is_empty() {
                            dirs.insert(path);
                        }
                    } else {
                        files.insert(path);
                    }
                }
                Err(error) => problems.push(error.to_string()),
            }
        }
    }
    (files, dirs, problems)
}
struct Inventory {
    paths: Vec<String>,
    absent: Vec<String>,
    directories: Vec<String>,
    uncovered: Vec<String>,
}
fn scope(
    root: &Path,
    request: &Value,
    repo: &Repository,
    saved: &BTreeMap<String, SavedFile>,
) -> Result<Inventory> {
    let (files, dirs, mut problems) = match selection(root, request) {
        Selection::Paths(roots, skip) => collect(root, &roots, skip),
        Selection::Tracked(roots) => {
            let tracked = repo
                .tracked_paths()?
                .into_iter()
                .filter(|p| roots.iter().any(|r| matches_root(p, r)))
                .collect::<Vec<_>>();
            collect(root, &tracked, true)
        }
        Selection::Clean(roots, ignored, only_ignored) => {
            let tracked: BTreeSet<_> = repo.tracked_paths()?.into_iter().collect();
            let (files, dirs, problems) = collect(root, &roots, true);
            (
                files
                    .into_iter()
                    .filter(|p| {
                        !tracked.contains(p)
                            && (ignored || only_ignored || saved.contains_key(p))
                            && (!only_ignored || !saved.contains_key(p))
                    })
                    .collect(),
                dirs,
                problems,
            )
        }
        Selection::Unknown(reason) => (
            saved.keys().cloned().collect(),
            BTreeSet::new(),
            vec![reason],
        ),
    };
    let (paths, absent) = check(root, repo, saved, files, &mut problems);
    Ok(Inventory {
        paths,
        absent,
        directories: dirs.into_iter().collect(),
        uncovered: problems,
    })
}
fn check(
    root: &Path,
    repo: &Repository,
    saved: &BTreeMap<String, SavedFile>,
    files: BTreeSet<String>,
    problems: &mut Vec<String>,
) -> (Vec<String>, Vec<String>) {
    let (mut paths, mut absent) = (Vec::new(), Vec::new());
    for path in files {
        match current_bytes(root, &path) {
            Ok(None) => absent.push(path),
            Ok(Some((bytes, link, executable))) => {
                if let Some(file) = saved.get(&path) {
                    if repo.saved_bytes(file).is_ok_and(|saved| saved == bytes)
                        && link == (file.mode == 0o120000)
                        && (!link && executable == (file.mode == 0o100755) || link)
                    {
                        paths.push(path);
                    } else {
                        problems.push(format!("{path}: content changed or is filtered"));
                    }
                } else {
                    problems.push(format!("{path}: not included in the snapshot"));
                }
            }
            Err(error) => problems.push(format!("{path}: {error}")),
        }
    }
    (paths, absent)
}

pub async fn prepare(
    workspace: Workspace,
    repair_run_id: Option<Uuid>,
    request: Value,
    parent: String,
    journal: Journal,
) -> Result<Snapshot> {
    validate_parent(&workspace.path, "")?;
    let captured =
        crate::checkpoint::capture(&workspace, &parent, "Intercat before deletion\n", &journal)
            .await?;
    let path = workspace.path.clone();
    let sha = captured.sha.clone();
    let inspect = request.clone();
    let inventory = tokio::task::spawn_blocking(move || {
        let repo = Repository::open(&path)?;
        let saved = repo.saved_files(sha.parse()?)?;
        scope(&path, &inspect, &repo, &saved)
    })
    .await??;
    let id = Uuid::now_v7();
    let snapshot = Snapshot {
        id,
        track_id: workspace.track_id.clone(),
        repair_run_id,
        item_id: request["itemId"].as_str().unwrap_or("").into(),
        approval_id: request["approvalId"].as_str().unwrap_or("").into(),
        sha: captured.sha,
        reference: format!("{}-deletions/{id}", workspace.reference),
        created_at: crate::journal::timestamp(),
        paths: inventory.paths,
        absent: inventory.absent,
        directories: inventory.directories,
        complete: inventory.uncovered.is_empty(),
        uncovered: inventory.uncovered,
        purpose: "before-delete".into(),
    };
    publish(&workspace, &snapshot, &journal).await?;
    Ok(snapshot)
}
async fn publish(workspace: &Workspace, snapshot: &Snapshot, journal: &Journal) -> Result<()> {
    let operation = Uuid::new_v5(&snapshot.id, b"snapshot").to_string();
    journal
        .intent(
            &operation,
            "deletion.snapshot",
            serde_json::json!({"trackId":workspace.track_id,"snapshot":snapshot}),
        )
        .await?;
    let path = workspace.path.clone();
    let reference = snapshot.reference.clone();
    let sha = snapshot.sha.parse()?;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(path)?;
        repo.compare_and_swap(&reference, None, sha)?;
        repo.sync_ref(&reference)?;
        Ok(())
    })
    .await??;
    journal.finish(&operation, "dispatched").await?;
    journal.save_deletion(snapshot.clone()).await?;
    journal.finish(&operation, "complete").await?;
    Ok(())
}

pub async fn before_restore(
    workspace: &Workspace,
    source: &Snapshot,
    journal: &Journal,
) -> Result<Snapshot> {
    validate_parent(&workspace.path, "")?;
    let captured = crate::checkpoint::capture(
        workspace,
        &source.sha,
        "Intercat before restoring saved files\n",
        journal,
    )
    .await?;
    let path = workspace.path.clone();
    let sha = captured.sha.clone();
    let files = source.paths.iter().chain(&source.absent).cloned().collect();
    let (paths, absent, uncovered) = tokio::task::spawn_blocking(move || -> Result<_> {
        let repo = Repository::open(&path)?;
        let saved = repo.saved_files(sha.parse()?)?;
        let mut uncovered = Vec::new();
        let (paths, absent) = check(&path, &repo, &saved, files, &mut uncovered);
        Ok((paths, absent, uncovered))
    })
    .await??;
    let id = Uuid::now_v7();
    let snapshot = Snapshot {
        id,
        track_id: source.track_id.clone(),
        repair_run_id: source.repair_run_id,
        item_id: source.item_id.clone(),
        approval_id: source.approval_id.clone(),
        sha: captured.sha,
        reference: format!("{}-deletions/{id}", workspace.reference),
        created_at: crate::journal::timestamp(),
        paths,
        absent,
        directories: Vec::new(),
        complete: uncovered.is_empty(),
        uncovered,
        purpose: "before-restore".into(),
    };
    publish(workspace, &snapshot, journal).await?;
    Ok(snapshot)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Restored {
    pub snapshot_id: Uuid,
    pub backup_id: Uuid,
    pub files: usize,
}
pub async fn restore(
    workspace: Workspace,
    repair_run_id: Option<Uuid>,
    source: Snapshot,
    id: Uuid,
    journal: Journal,
) -> Result<Restored> {
    if source.track_id != workspace.track_id || source.repair_run_id != repair_run_id {
        return Err("Snapshot belongs to another worker or repair.".into());
    }
    let backup_id = if let Some(operation) = journal.operation(id).await? {
        if operation["kind"] != "deletion.restore"
            || operation["payload"]["snapshotId"] != source.id.to_string()
        {
            return Err("Operation ID reused.".into());
        }
        let backup: Uuid = serde_json::from_value(operation["payload"]["backupId"].clone())?;
        if operation["state"] == "complete" {
            return Ok(Restored {
                snapshot_id: source.id,
                backup_id: backup,
                files: source.paths.len(),
            });
        }
        let retry_backup = before_restore(&workspace, &source, &journal).await?;
        if !retry_backup.complete {
            return Err(
                "Current files could not all be backed up; the restore was not retried.".into(),
            );
        }
        backup
    } else {
        let backup = before_restore(&workspace, &source, &journal).await?;
        if !backup.complete {
            return Err("Current files could not all be backed up; no files were restored.".into());
        }
        journal.intent(&id.to_string(),"deletion.restore",serde_json::json!({"trackId":workspace.track_id,"repairRunId":repair_run_id,"snapshotId":source.id,"backupId":backup.id})).await?;
        backup.id
    };
    let path = workspace.path;
    let apply = source.clone();
    tokio::task::spawn_blocking(move || crate::restoration::apply(&path, &apply)).await??;
    journal.finish(&id.to_string(), "dispatched").await?;
    journal.finish(&id.to_string(), "complete").await?;
    Ok(Restored {
        snapshot_id: source.id,
        backup_id,
        files: source.paths.len(),
    })
}

pub fn summaries(
    db: &rusqlite::Connection,
    track: Option<&str>,
    item: Option<&str>,
    repair_run_id: Option<Uuid>,
) -> Result<Vec<Snapshot>> {
    let mut query=db.prepare("SELECT record FROM deletion_snapshots WHERE (?1 IS NULL OR track=?1) AND (?2 IS NULL OR item=?2) AND json_extract(record,'$.repairRunId') IS ?3 ORDER BY rowid DESC LIMIT 100")?;
    query
        .query_map(
            rusqlite::params![track, item, repair_run_id.map(|id| id.to_string())],
            |row| row.get::<_, String>(0),
        )?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect()
}
pub fn pending(db: &rusqlite::Connection) -> Result<Vec<Value>> {
    let mut query=db.prepare("SELECT id,payload FROM operations WHERE (kind='repair.restore' OR (kind='deletion.restore' AND json_extract(payload,'$.repairRunId') IS NULL)) AND state IN ('prepared','dispatched') ORDER BY rowid")?;
    query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .map(|row| {
            let (id, payload) = row?;
            let mut value: Value = serde_json::from_str(&payload)?;
            value["operationId"] = id.into();
            Ok(value)
        })
        .collect()
}

pub fn store(db: &mut rusqlite::Connection, snapshot: &Snapshot) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute(
        "INSERT INTO deletion_snapshots VALUES(?1,?2,?3,?4)",
        rusqlite::params![
            snapshot.id.to_string(),
            snapshot.track_id,
            snapshot.item_id,
            serde_json::to_string(snapshot)?
        ],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'deletion.saved',?2)",
        rusqlite::params![
            crate::journal::timestamp(),
            serde_json::to_string(snapshot)?
        ],
    )?;
    tx.commit()?;
    Ok(())
}
pub fn read(db: &rusqlite::Connection, id: Uuid) -> Result<Snapshot> {
    let value: String = db.query_row(
        "SELECT record FROM deletion_snapshots WHERE id=?1",
        [id.to_string()],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str(&value)?)
}
