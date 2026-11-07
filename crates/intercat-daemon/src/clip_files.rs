//! File warnings use server-authored, item-bound patches, never a later turn's tree.
use crate::{Result, journal::Journal, preparation::Workspace};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
    time::Duration,
};
use tokio::sync::watch;

#[derive(Clone, Debug)]
struct Peer {
    clip: String,
    revision: i64,
    workspace: Workspace,
    baseline: String,
    paths: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct Ticket {
    clip: String,
    revision: i64,
    workspace: Workspace,
    baseline: String,
    paths: Vec<String>,
    peers: Vec<Peer>,
    retried: bool,
}
#[derive(Default)]
pub struct Comparison {
    hits: Vec<(String, String)>,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS clip_file_pending(
        clip TEXT PRIMARY KEY REFERENCES timeline_clips(id) ON DELETE CASCADE,
        revision INTEGER NOT NULL, retry_at INTEGER NOT NULL DEFAULT 0, pending INTEGER NOT NULL DEFAULT 1);
        CREATE INDEX IF NOT EXISTS clip_file_work ON clip_file_pending(pending,retry_at);
        CREATE TABLE IF NOT EXISTS clip_file_scans(
        clip TEXT PRIMARY KEY REFERENCES timeline_clips(id) ON DELETE CASCADE, revision INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS clip_changed_files(
        clip TEXT NOT NULL REFERENCES clip_file_scans(clip) ON DELETE CASCADE,
        path TEXT NOT NULL, PRIMARY KEY(clip,path));
        CREATE INDEX IF NOT EXISTS clip_changed_path ON clip_changed_files(path,clip);
        CREATE TABLE IF NOT EXISTS clip_file_warnings(
        a TEXT NOT NULL REFERENCES timeline_clips(id) ON DELETE CASCADE,
        b TEXT NOT NULL REFERENCES timeline_clips(id) ON DELETE CASCADE,
        path TEXT NOT NULL, PRIMARY KEY(a,b,path));
        CREATE INDEX IF NOT EXISTS clip_file_warning_peer ON clip_file_warnings(b,a,path);")?;
    for event in ["INSERT", "UPDATE"] {
        db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS clip_file_input_{} AFTER {event} ON timeline_versions BEGIN
            INSERT INTO clip_file_pending(clip,revision) SELECT NEW.id,1 FROM timeline_clips
              WHERE id=NEW.id AND json_extract(record,'$.category')='step'
              AND json_extract(record,'$.end') IS NOT NULL
              AND json_extract(record,'$.repairRunId') IS NULL
              ON CONFLICT(clip) DO UPDATE SET revision=revision+1,retry_at=0,pending=1;
            DELETE FROM clip_file_scans WHERE clip=NEW.id;
            DELETE FROM clip_file_warnings WHERE a=NEW.id OR b=NEW.id;
        END;", event.to_lowercase()))?;
        db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS clip_file_output_{} AFTER {event} ON timeline_outputs BEGIN
            INSERT INTO clip_file_pending(clip,revision) SELECT clip,1 FROM timeline_items WHERE session=NEW.session AND item=NEW.item
              ON CONFLICT(clip) DO UPDATE SET revision=revision+1,retry_at=0,pending=1;
            DELETE FROM clip_file_scans WHERE clip IN (SELECT clip FROM timeline_items WHERE session=NEW.session AND item=NEW.item);
            DELETE FROM clip_file_warnings WHERE a IN (SELECT clip FROM timeline_items WHERE session=NEW.session AND item=NEW.item)
              OR b IN (SELECT clip FROM timeline_items WHERE session=NEW.session AND item=NEW.item);
        END;", event.to_lowercase()))?;
        db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS clip_file_warning_{} AFTER {event} ON clip_file_warnings BEGIN
            UPDATE timeline_revision SET revision=revision+1;
            INSERT INTO timeline_warning_versions SELECT NEW.a,revision FROM timeline_revision WHERE true
              ON CONFLICT(clip) DO UPDATE SET version=excluded.version;
            INSERT INTO timeline_warning_versions SELECT NEW.b,revision FROM timeline_revision WHERE true
              ON CONFLICT(clip) DO UPDATE SET version=excluded.version;
        END;", event.to_lowercase()))?;
    }
    db.execute_batch("CREATE TRIGGER IF NOT EXISTS clip_file_warning_removed BEFORE DELETE ON clip_file_warnings BEGIN
        UPDATE timeline_revision SET revision=revision+1;
        INSERT INTO timeline_warning_versions SELECT OLD.a,revision FROM timeline_revision WHERE true
          ON CONFLICT(clip) DO UPDATE SET version=excluded.version;
        INSERT INTO timeline_warning_versions SELECT OLD.b,revision FROM timeline_revision WHERE true
          ON CONFLICT(clip) DO UPDATE SET version=excluded.version;
        END;
        UPDATE clip_file_pending SET retry_at=0;")?;
    Ok(())
}

fn relative(root: &Path, path: &str) -> Result<String> {
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.strip_prefix(root)?
    } else {
        path
    };
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) if name != ".git" => {
                let name = name.to_str().ok_or("Invalid patch path encoding.")?;
                if name.chars().any(char::is_control) || intercat_git::secret_path(name) {
                    return Err("Protected or invalid patch path.".into());
                }
                parts.push(name);
            }
            _ => return Err("Patch path is outside its workspace.".into()),
        }
    }
    if parts.is_empty() {
        return Err("Empty patch path.".into());
    }
    Ok(parts.join("/"))
}

fn patch_paths(root: &Path, reference: &Value, output: &Value) -> Result<Vec<String>> {
    if reference["kind"] != "tool_patch"
        || reference["mediaType"] != "application/json"
        || output["encoding"] != "utf8"
        || output["mediaType"] != "application/json"
        || output["offsetBytes"] != 0
        || output["eof"] != true
        || output.get("_intercatTruncated").is_some()
    {
        return Err("Incomplete structured patch.".into());
    }
    let content = output["content"].as_str().ok_or("Missing patch content.")?;
    if output["byteLen"].as_u64() != Some(content.len() as u64)
        || reference["byteLen"].as_u64() != Some(content.len() as u64)
    {
        return Err("Truncated patch content.".into());
    }
    if let Some(digest) = reference["digest"].as_str() {
        let actual = format!(
            "sha256:{}",
            Sha256::digest(content.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        if digest != actual {
            return Err("Patch digest does not match.".into());
        }
    }
    let patch: Value = serde_json::from_str(content)?;
    let files = patch["files"].as_array().ok_or("Missing patch files.")?;
    if files.len() > 4096 {
        return Err("Too many patch files.".into());
    }
    let mut paths = BTreeSet::new();
    for file in files {
        let path = relative(root, file["path"].as_str().ok_or("Missing patch path.")?)?;
        let hunks = file["hunks"].as_array().ok_or("Missing patch hunks.")?;
        if hunks.iter().any(|hunk| {
            hunk["lines"].as_array().is_some_and(|lines| {
                lines.iter().any(|line| {
                    line.as_str()
                        .is_some_and(|line| line.starts_with('+') || line.starts_with('-'))
                })
            })
        }) {
            paths.insert(path);
        }
    }
    Ok(paths.into_iter().collect())
}

fn baseline(db: &Connection, workspace: &Workspace) -> Result<String> {
    let integrated: Option<String> = db.query_row("SELECT json_extract(t.value,'$.checkpoint.sha')
        FROM events e,json_each(e.payload,'$.tracks') t WHERE e.kind='integration.published'
        AND json_extract(e.payload,'$.status')='done' AND json_extract(t.value,'$.id')=?1
        AND EXISTS(SELECT 1 FROM json_each(e.payload,'$.candidate.commits') c WHERE json_extract(c.value,'$.track')=?1)
        ORDER BY e.seq DESC LIMIT 1", [&workspace.track_id], |row| row.get(0)).optional()?.flatten();
    Ok(integrated.unwrap_or_else(|| workspace.base.clone()))
}

pub(crate) fn next(db: &mut Connection) -> Result<Option<Ticket>> {
    let now = crate::clock::Stamp::now().epoch_ms;
    let tx = db.transaction()?;
    let row: Option<(String,i64,String)> = tx.query_row("SELECT p.clip,p.revision,w.record
        FROM clip_file_pending p JOIN timeline_clips c ON c.id=p.clip
        JOIN workspaces w ON w.track_id=c.track
        WHERE json_extract(c.record,'$.end') IS NOT NULL AND json_extract(c.record,'$.category')='step'
          AND json_extract(c.record,'$.repairRunId') IS NULL AND json_extract(c.record,'$.recovered')=0
          AND json_extract(w.record,'$.state')='ready' AND p.pending=1 AND p.retry_at<=?1
        ORDER BY p.rowid DESC LIMIT 1", [now], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
    let Some((clip, revision, workspace)) = row else {
        return Ok(None);
    };
    let workspace: Workspace = serde_json::from_str(&workspace)?;
    let mut paths = BTreeSet::new();
    let rows = tx.prepare("SELECT i.record,o.record FROM timeline_items i LEFT JOIN timeline_outputs o
        ON o.session=i.session AND o.item=i.item AND o.reference=json_extract(i.record,'$.patchRef.id') WHERE i.clip=?1")?
        .query_map([&clip], |row| Ok((row.get::<_,String>(0)?,row.get::<_,Option<String>>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (item, output) in rows {
        let item: Value = serde_json::from_str(&item)?;
        if item["kind"] != "toolCall" || item["patchRef"]["availability"] != "available" {
            continue;
        }
        if let Some(output) = output {
            let output: Value = serde_json::from_str(&output)?;
            if let Ok(files) = patch_paths(&workspace.path, &item["patchRef"], &output) {
                paths.extend(files);
            }
        }
    }
    let paths: Vec<_> = paths.into_iter().collect();
    let rows = tx.prepare("SELECT DISTINCT f.clip,f.path,c.track,w.record,c.ordinal,s.revision FROM clip_changed_files f
        JOIN timeline_clips c ON c.id=f.clip JOIN workspaces w ON w.track_id=c.track
        JOIN clip_file_scans s ON s.clip=f.clip JOIN clip_file_pending p ON p.clip=f.clip AND p.revision=s.revision AND p.pending=0
        WHERE f.path IN (SELECT value FROM json_each(?1)) AND c.track<>?2 AND json_extract(w.record,'$.state')='ready'
        ORDER BY c.ordinal DESC")?
        .query_map(params![serde_json::to_string(&paths)?,workspace.track_id], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,row.get::<_,i64>(5)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut seen = BTreeSet::new();
    let mut peers = BTreeMap::<String, Peer>::new();
    for (clip, path, track, record, revision) in rows {
        if !seen.insert((track, path.clone())) {
            continue;
        }
        if !peers.contains_key(&clip) {
            let workspace: Workspace = serde_json::from_str(&record)?;
            peers.insert(
                clip.clone(),
                Peer {
                    clip: clip.clone(),
                    revision,
                    baseline: baseline(&tx, &workspace)?,
                    workspace,
                    paths: vec![],
                },
            );
        }
        peers.get_mut(&clip).unwrap().paths.push(path);
    }
    tx.execute(
        "UPDATE clip_file_pending SET retry_at=?2 WHERE clip=?1",
        params![clip, now + 30_000],
    )?;
    let retried: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM clip_file_scans WHERE clip=?1 AND revision=?2)",
        params![clip, revision],
        |row| row.get(0),
    )?;
    let ticket = Ticket {
        baseline: baseline(&tx, &workspace)?,
        clip,
        revision,
        workspace,
        paths,
        peers: peers.into_values().collect(),
        retried,
    };
    tx.commit()?;
    Ok(Some(ticket))
}

pub fn inspect(ticket: &Ticket) -> Result<Comparison> {
    let mut result = Comparison::default();
    if ticket.paths.is_empty() || ticket.peers.is_empty() {
        return Ok(result);
    }
    let repo = intercat_git::Repository::open(&ticket.workspace.path)?.common_directory()?;
    let changed = crate::backup_files::changed_paths(
        &ticket.workspace.path,
        &ticket.baseline,
        &ticket.paths,
    )?;
    for peer in &ticket.peers {
        if intercat_git::Repository::open(&peer.workspace.path)?.common_directory()? != repo {
            continue;
        }
        let paths = peer
            .paths
            .iter()
            .filter(|path| changed.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        for path in
            crate::backup_files::changed_paths(&peer.workspace.path, &peer.baseline, &paths)?
        {
            result.hits.push((peer.clip.clone(), path));
        }
    }
    Ok(result)
}

pub(crate) fn finish(
    db: &mut Connection,
    ticket: &Ticket,
    result: Result<Comparison>,
) -> Result<bool> {
    let tx = db.transaction()?;
    let current: Option<i64> = tx
        .query_row(
            "SELECT revision FROM clip_file_pending WHERE clip=?1",
            [&ticket.clip],
            |row| row.get(0),
        )
        .optional()?;
    if current != Some(ticket.revision) {
        return Ok(false);
    }
    if !same_workspace(&tx, &ticket.workspace, &ticket.baseline)? {
        tx.execute(
            "UPDATE clip_file_pending SET retry_at=0 WHERE clip=?1",
            [&ticket.clip],
        )?;
        tx.commit()?;
        return Ok(false);
    }
    // One retry covers a concurrently changing file. Persistent unsupported
    // inputs wait for another item/output update instead of growing the log.
    let (result, retry) = match result {
        Ok(result) => (result, false),
        Err(_) => (Comparison::default(), !ticket.retried),
    };
    tx.execute("INSERT INTO clip_file_scans VALUES(?1,?2) ON CONFLICT(clip) DO UPDATE SET revision=excluded.revision",params![ticket.clip,ticket.revision])?;
    tx.execute(
        "UPDATE clip_file_pending SET pending=?2 WHERE clip=?1",
        params![ticket.clip, retry],
    )?;
    tx.execute(
        "DELETE FROM clip_changed_files WHERE clip=?1",
        [&ticket.clip],
    )?;
    for path in &ticket.paths {
        tx.execute(
            "INSERT INTO clip_changed_files VALUES(?1,?2)",
            params![ticket.clip, path],
        )?;
    }
    tx.execute(
        "DELETE FROM clip_file_warnings WHERE a=?1 OR b=?1",
        [&ticket.clip],
    )?;
    let mut accepted = Vec::new();
    for (peer, path) in result.hits {
        // A peer may have changed while filesystem IO was outside the writer.
        let revision = ticket
            .peers
            .iter()
            .find(|value| value.clip == peer)
            .map(|value| value.revision);
        let valid: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM clip_changed_files f JOIN clip_file_pending p ON p.clip=f.clip WHERE f.clip=?1 AND f.path=?2 AND p.revision=?3 AND p.pending=0)",
            params![peer,path,revision],
            |row| row.get(0),
        )?;
        let same = ticket
            .peers
            .iter()
            .find(|value| value.clip == peer)
            .map(|peer| same_workspace(&tx, &peer.workspace, &peer.baseline))
            .transpose()?
            .unwrap_or(false);
        if valid && same {
            let (a, b) = if ticket.clip < peer {
                (&ticket.clip, &peer)
            } else {
                (&peer, &ticket.clip)
            };
            tx.execute(
                "INSERT OR IGNORE INTO clip_file_warnings VALUES(?1,?2,?3)",
                params![a, b, path],
            )?;
            accepted.push((peer, path));
        }
    }
    let observed = json!({"clipId":ticket.clip,"revision":ticket.revision,
        "files":ticket.paths,"overlaps":accepted,"retry":retry});
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'clip.files.compared',?2)",
        params![crate::clock::Stamp::now().epoch_ms, observed.to_string()],
    )?;
    tx.commit()?;
    Ok(true)
}

fn same_workspace(db: &Connection, workspace: &Workspace, old_baseline: &str) -> Result<bool> {
    let record: Option<String> = db
        .query_row(
            "SELECT record FROM workspaces WHERE track_id=?1",
            [&workspace.track_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(record
        .map(|record| serde_json::from_str::<Value>(&record))
        .transpose()?
        == Some(serde_json::to_value(workspace)?)
        && baseline(db, workspace)? == old_baseline)
}

pub(crate) fn warnings(db: &Connection, clip: &str) -> Result<Vec<Value>> {
    Ok(db.prepare("SELECT t.label,c.ordinal,min(w.path),count(*) FROM clip_file_warnings w
        JOIN timeline_clips c ON c.id=CASE WHEN w.a=?1 THEN w.b ELSE w.a END
        JOIN tracks t ON t.id=c.track WHERE w.a=?1 OR w.b=?1 GROUP BY c.id ORDER BY c.ordinal LIMIT 8")?
        .query_map([clip],|row|{
            let count=row.get::<_,i64>(3)?;
            let more=if count>1 {format!(" and {} more files",count-1)} else {String::new()};
            Ok(json!({"kind":"same-file","text":format!("Also changed in {}:{} · {}{}. Both tracks have uncommitted changes.",row.get::<_,String>(0)?,row.get::<_,i64>(1)?,row.get::<_,String>(2)?,more)}))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

pub struct Service {
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}
impl Service {
    pub fn start(journal: Journal) -> Self {
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {_=stopping.changed()=>break, _=interval.tick()=>{}}
                let Ok(Some(ticket)) = journal.next_clip_files().await else {
                    continue;
                };
                let copy = ticket.clone();
                let result = tokio::task::spawn_blocking(move || inspect(&copy));
                let result = tokio::select! {_=stopping.changed()=>break, result=result=>result.unwrap_or_else(|error|Err(error.into()))};
                let _ = journal.finish_clip_files(ticket, result).await;
            }
        });
        Self { stop, task }
    }
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_native_patches_distinguish_changed_lines_from_ambiguous_empty_hunks() -> Result<()>
    {
        let cases: Vec<Value> =
            serde_json::from_str(include_str!("../tests/data/clip-patches.json"))?;
        assert_eq!(cases.len(), 5);
        for case in cases {
            assert_eq!(
                serde_json::to_value(patch_paths(
                    Path::new("/dummy/workspace"),
                    &case["reference"],
                    &case["output"]
                )?)?,
                case["expected"],
                "{}",
                case["case"]
            );
        }
        Ok(())
    }

    #[test]
    fn patches_require_complete_bound_bytes_and_actual_changed_lines() -> Result<()> {
        let root = Path::new("/dummy/workspace");
        let content=json!({"files":[{"path":"/dummy/workspace/shared.txt","hunks":[{"lines":["-old","+new"]}]}]}).to_string();
        let reference =
            json!({"kind":"tool_patch","mediaType":"application/json","byteLen":content.len()});
        let output = json!({"content":content,"encoding":"utf8","mediaType":"application/json","offsetBytes":0,"byteLen":content.len(),"eof":true});
        assert_eq!(patch_paths(root, &reference, &output)?, vec!["shared.txt"]);
        for (key, value) in [
            ("eof", json!(false)),
            ("byteLen", json!(1)),
            ("offsetBytes", json!(1)),
            ("encoding", json!("base64")),
        ] {
            let mut changed = output.clone();
            changed[key] = value;
            assert!(patch_paths(root, &reference, &changed).is_err());
        }
        let mut bad_digest = reference.clone();
        bad_digest["digest"] = json!("sha256:invalid");
        assert!(patch_paths(root, &bad_digest, &output).is_err());
        for path in [
            "../escape",
            "/dummy/elsewhere/file.txt",
            ".git/config",
            "nested/.env",
        ] {
            assert!(relative(root, path).is_err());
        }
        // The native host returns this same shape for both a new empty file
        // and a no-op rewrite. It cannot prove a change, so do not infer one.
        let content =
            json!({"files":[{"path":"/dummy/workspace/empty.txt","hunks":[]}]}).to_string();
        let mut reference = reference;
        reference["byteLen"] = json!(content.len());
        let output = json!({"content":content,"encoding":"utf8","mediaType":"application/json","offsetBytes":0,"byteLen":content.len(),"eof":true});
        assert!(patch_paths(root, &reference, &output)?.is_empty());
        Ok(())
    }
}
