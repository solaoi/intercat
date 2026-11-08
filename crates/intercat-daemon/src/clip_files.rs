//! Exact batch file names, compared with the current unintegrated trees.
use crate::{
    Result,
    journal::Journal,
    preparation::Workspace,
    step_sources::{Active, Source},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::watch;

#[derive(Clone, Debug)]
struct Peer {
    workspace: Workspace,
    baseline: String,
    active: Option<(Active, Option<String>)>,
    owners: BTreeMap<String, String>,
}
#[derive(Clone, Debug)]
pub struct Ticket {
    clip: String,
    revision: i64,
    epoch: i64,
    workspace: Workspace,
    baseline: String,
    paths: Option<Vec<String>>,
    peers: Vec<Peer>,
    retried: bool,
}
struct Hit {
    track: String,
    clip: Option<String>,
    path: String,
}
#[derive(Default)]
pub struct Comparison {
    hits: Vec<Hit>,
    unavailable: bool,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS clip_file_epoch(value INTEGER NOT NULL);
        INSERT INTO clip_file_epoch SELECT 0 WHERE NOT EXISTS(SELECT 1 FROM clip_file_epoch);
        CREATE TABLE IF NOT EXISTS clip_file_pending(
            clip TEXT PRIMARY KEY REFERENCES timeline_clips(id) ON DELETE CASCADE,
            revision INTEGER NOT NULL, retry_at INTEGER NOT NULL DEFAULT 0, pending INTEGER NOT NULL DEFAULT 1);
        CREATE INDEX IF NOT EXISTS clip_file_work ON clip_file_pending(pending,retry_at);
        CREATE TABLE IF NOT EXISTS clip_file_scans(
            clip TEXT PRIMARY KEY REFERENCES timeline_clips(id) ON DELETE CASCADE,
            revision INTEGER NOT NULL, outcome TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS clip_file_warnings(
            clip TEXT NOT NULL REFERENCES timeline_clips(id) ON DELETE CASCADE,
            peer_track TEXT NOT NULL REFERENCES tracks(id) ON DELETE CASCADE,
            peer_clip TEXT REFERENCES timeline_clips(id) ON DELETE CASCADE,
            path TEXT NOT NULL, PRIMARY KEY(clip,peer_track,path));
        CREATE INDEX IF NOT EXISTS clip_file_warning_peer ON clip_file_warnings(peer_clip,clip);")?;
    for event in ["INSERT", "UPDATE"] {
        db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS clip_file_input_{} AFTER {event} ON timeline_versions BEGIN
            UPDATE clip_file_epoch SET value=value+1;
            INSERT INTO clip_file_pending(clip,revision) SELECT NEW.id,1 FROM timeline_clips
              WHERE id=NEW.id AND json_extract(record,'$.category')='step'
              AND json_extract(record,'$.end') IS NOT NULL AND json_extract(record,'$.recovered')=0
              AND json_extract(record,'$.repairRunId') IS NULL
              AND EXISTS(SELECT 1 FROM timeline_items i WHERE i.clip=NEW.id AND json_extract(i.record,'$.kind')='toolCall')
              ON CONFLICT(clip) DO UPDATE SET revision=revision+1,retry_at=0,pending=1;
        END;",event.to_lowercase()))?;
    }
    // Boundary changes revisit old steps; streaming output never starts a scan.
    for table in ["workspaces", "step_capture_active"] {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS clip_file_{table}_{} AFTER {event} ON {table} BEGIN
                UPDATE clip_file_epoch SET value=value+1;
                INSERT INTO clip_file_pending(clip,revision) SELECT b.clip,1 FROM step_source_clips b WHERE true
                ON CONFLICT(clip) DO UPDATE SET revision=revision+1,retry_at=0,pending=1;
            END;",event.to_lowercase()))?;
        }
    }
    db.execute_batch(
        "CREATE TRIGGER IF NOT EXISTS clip_file_integrated AFTER INSERT ON events
        WHEN NEW.kind='integration.published' BEGIN
            UPDATE clip_file_epoch SET value=value+1;
            UPDATE clip_file_pending SET revision=revision+1,pending=1,retry_at=0;
        END;",
    )?;
    for table in ["clip_file_warnings", "clip_file_scans"] {
        for event in ["INSERT", "UPDATE", "DELETE"] {
            let row = if event == "DELETE" { "OLD" } else { "NEW" };
            db.execute_batch(&format!("CREATE TRIGGER IF NOT EXISTS {table}_notice_{} AFTER {event} ON {table} BEGIN
                UPDATE timeline_revision SET revision=revision+1;
                INSERT INTO timeline_warning_versions SELECT {row}.clip,revision FROM timeline_revision WHERE true
                  ON CONFLICT(clip) DO UPDATE SET version=excluded.version;
            END;",event.to_lowercase()))?;
        }
    }
    db.execute("UPDATE clip_file_pending SET retry_at=0", [])?;
    Ok(())
}

pub(crate) fn source_ready(db: &Connection, source: &Source, clip: &str) -> Result<()> {
    db.execute("UPDATE clip_file_epoch SET value=value+1", [])?;
    db.execute("INSERT INTO clip_file_pending(clip,revision)
        SELECT ?1,1 UNION SELECT b.clip,1 FROM step_source_clips b JOIN step_source_paths p ON p.source=b.source
        WHERE p.path IN (SELECT path FROM step_source_paths WHERE source=?2)
        ON CONFLICT(clip) DO UPDATE SET revision=revision+1,retry_at=0,pending=1",params![clip,source.id.to_string()])?;
    Ok(())
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
    let row:Option<(String,i64,String,Option<String>)>=tx.query_row("SELECT p.clip,p.revision,w.record,s.record
        FROM clip_file_pending p JOIN timeline_clips c ON c.id=p.clip JOIN workspaces w ON w.track_id=c.track
        LEFT JOIN step_source_clips b ON b.clip=c.id LEFT JOIN step_file_sources s ON s.id=b.source
        WHERE p.pending=1 AND p.retry_at<=?1 AND json_extract(w.record,'$.state')='ready'
          AND json_extract(c.record,'$.end') IS NOT NULL AND json_extract(c.record,'$.category')='step'
          AND json_extract(c.record,'$.repairRunId') IS NULL AND json_extract(c.record,'$.recovered')=0
          AND EXISTS(SELECT 1 FROM timeline_items i WHERE i.clip=c.id AND json_extract(i.record,'$.kind')='toolCall')
        ORDER BY p.rowid DESC LIMIT 1",[now],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    let Some((clip, revision, workspace, source)) = row else {
        return Ok(None);
    };
    let workspace: Workspace = serde_json::from_str(&workspace)?;
    let source: Option<Source> = source.map(|s| serde_json::from_str(&s)).transpose()?;
    let paths=match source {
        Some(source) if serde_json::to_value(&source.workspace)?==serde_json::to_value(&workspace)? => {
            source.paths.map(|paths| ->Result<_> {
                let mut result=Vec::new();
                for path in paths {
                    let newer:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM step_source_paths p
                        JOIN step_file_sources s ON s.id=p.source JOIN step_source_clips b ON b.source=s.id
                        JOIN timeline_clips c ON c.id=b.clip
                        WHERE s.track=?1 AND p.path=?2 AND c.ordinal>(SELECT ordinal FROM timeline_clips WHERE id=?3))",
                        params![workspace.track_id,path,clip],|r|r.get(0))?;
                    if !newer {result.push(path)}
                }
                Ok(result)
            }).transpose()?
        },
        _=>None,
    };
    let records=tx.prepare("SELECT record FROM workspaces WHERE track_id<>?1 AND json_extract(record,'$.state')='ready'")?
        .query_map([&workspace.track_id],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut peers = Vec::new();
    for record in records {
        let workspace: Workspace = serde_json::from_str(&record)?;
        let active: Option<String> = tx
            .query_row(
                "SELECT a.record FROM step_capture_active a JOIN workers w ON w.track_id=a.track
            WHERE a.track=?1 AND a.session=json_extract(w.record,'$.sessionId') LIMIT 1",
                [&workspace.track_id],
                |r| r.get(0),
            )
            .optional()?;
        let active = if let Some(raw) = active {
            let active: Active = serde_json::from_str(&raw)?;
            let clip:Option<String>=tx.query_row("SELECT DISTINCT i.clip FROM timeline_items i JOIN timeline_clips c ON c.id=i.clip
                WHERE i.session=?1 AND json_extract(c.record,'$.turnId')=?2
                  AND json_extract(i.record,'$.callId') IN (SELECT value FROM json_each(?3)) LIMIT 1",
                params![active.session,active.turn,serde_json::to_string(&active.calls)?],|r|r.get(0)).optional()?;
            Some((active, clip))
        } else {
            None
        };
        let rows=tx.prepare("SELECT p.path,b.clip FROM step_source_paths p JOIN step_file_sources s ON s.id=p.source
            JOIN step_source_clips b ON b.source=s.id JOIN timeline_clips c ON c.id=b.clip
            WHERE s.track=?1 AND p.path IN (SELECT value FROM json_each(?2)) ORDER BY c.ordinal")?
            .query_map(params![workspace.track_id,serde_json::to_string(&paths)?],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        peers.push(Peer {
            baseline: baseline(&tx, &workspace)?,
            workspace,
            active,
            owners: rows.into_iter().collect(),
        });
    }
    tx.execute(
        "UPDATE clip_file_pending SET retry_at=?2 WHERE clip=?1",
        params![clip, now + 30_000],
    )?;
    let retried = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM clip_file_scans WHERE clip=?1 AND revision=?2)",
        params![clip, revision],
        |r| r.get(0),
    )?;
    let ticket = Ticket {
        baseline: baseline(&tx, &workspace)?,
        workspace,
        clip,
        revision,
        epoch: tx.query_row("SELECT value FROM clip_file_epoch", [], |r| r.get(0))?,
        paths,
        peers,
        retried,
    };
    tx.commit()?;
    Ok(Some(ticket))
}

pub fn inspect(ticket: &Ticket) -> Result<Comparison> {
    let mut result = Comparison::default();
    let Some(paths) = &ticket.paths else {
        result.unavailable = true;
        return Ok(result);
    };
    if paths.is_empty() {
        return Ok(result);
    }
    let repository = intercat_git::Repository::open(&ticket.workspace.path)?.common_directory()?;
    let changed =
        crate::backup_files::changed_paths(&ticket.workspace.path, &ticket.baseline, paths)?;
    let paths = changed.into_iter().collect::<Vec<_>>();
    for peer in &ticket.peers {
        let comparison = (|| -> Result<_> {
            if intercat_git::Repository::open(&peer.workspace.path)?.common_directory()?
                != repository
            {
                return Ok(Vec::new());
            }
            let changed =
                crate::backup_files::changed_paths(&peer.workspace.path, &peer.baseline, &paths)?;
            let active_changes = if let Some((active, _)) = &peer.active {
                crate::backup_files::changed_paths(&peer.workspace.path, &active.before, &paths)?
            } else {
                Default::default()
            };
            Ok(changed
                .into_iter()
                .map(|path| {
                    let clip = if active_changes.contains(&path) {
                        peer.active.as_ref().and_then(|(_, clip)| clip.clone())
                    } else {
                        peer.owners.get(&path).cloned()
                    };
                    Hit {
                        track: peer.workspace.track_id.clone(),
                        clip,
                        path,
                    }
                })
                .collect::<Vec<_>>())
        })();
        match comparison {
            Ok(hits) => result.hits.extend(hits),
            Err(_) => result.unavailable = true,
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
    let revision: Option<i64> = tx
        .query_row(
            "SELECT revision FROM clip_file_pending WHERE clip=?1",
            [&ticket.clip],
            |r| r.get(0),
        )
        .optional()?;
    if revision != Some(ticket.revision) {
        return Ok(false);
    }
    let epoch: i64 = tx.query_row("SELECT value FROM clip_file_epoch", [], |r| r.get(0))?;
    if epoch != ticket.epoch {
        tx.execute(
            "UPDATE clip_file_pending SET retry_at=0 WHERE clip=?1",
            [&ticket.clip],
        )?;
        tx.commit()?;
        return Ok(false);
    }
    let (result, failed) = match result {
        Ok(result) => {
            let failed = result.unavailable;
            (result, failed)
        }
        Err(_) => (
            Comparison {
                unavailable: true,
                ..Default::default()
            },
            true,
        ),
    };
    let retry = failed && ticket.paths.is_some() && !ticket.retried;
    tx.execute("INSERT INTO clip_file_scans VALUES(?1,?2,?3) ON CONFLICT(clip) DO UPDATE SET revision=excluded.revision,outcome=excluded.outcome",
        params![ticket.clip,ticket.revision,if result.unavailable {"unavailable"} else {"complete"}])?;
    tx.execute(
        "UPDATE clip_file_pending SET pending=?2 WHERE clip=?1",
        params![ticket.clip, retry],
    )?;
    tx.execute(
        "DELETE FROM clip_file_warnings WHERE clip=?1 OR peer_clip=?1",
        [&ticket.clip],
    )?;
    let mut accepted = Vec::new();
    for hit in result.hits {
        tx.execute(
            "INSERT OR REPLACE INTO clip_file_warnings VALUES(?1,?2,?3,?4)",
            params![ticket.clip, hit.track, hit.clip, hit.path],
        )?;
        if let Some(peer) = &hit.clip {
            tx.execute(
                "INSERT OR REPLACE INTO clip_file_warnings VALUES(?1,?2,?3,?4)",
                params![peer, ticket.workspace.track_id, ticket.clip, hit.path],
            )?;
        }
        accepted.push(json!({"track":hit.track,"clip":hit.clip,"path":hit.path}));
    }
    tx.execute("INSERT INTO events(ts,kind,payload) VALUES(?1,'clip.files.compared',?2)",params![crate::clock::Stamp::now().epoch_ms,
        json!({"clipId":ticket.clip,"revision":ticket.revision,"files":ticket.paths,"overlaps":accepted,"unavailable":result.unavailable,"retry":retry}).to_string()])?;
    tx.commit()?;
    Ok(true)
}

pub(crate) fn warnings(db: &Connection, clip: &str) -> Result<Vec<Value>> {
    let mut warnings=db.prepare("SELECT t.label,c.ordinal,min(w.path),count(*) FROM clip_file_warnings w
        JOIN tracks t ON t.id=w.peer_track LEFT JOIN timeline_clips c ON c.id=w.peer_clip
        WHERE w.clip=?1 GROUP BY w.peer_track,w.peer_clip ORDER BY t.label,c.ordinal LIMIT 8")?
        .query_map([clip],|r|{
            let label:String=r.get(0)?;let ordinal:Option<i64>=r.get(1)?;let count:i64=r.get(3)?;
            let address=ordinal.map(|n|format!("{label}:{n}")).unwrap_or(label);
            let more=if count>1 {format!(" and {} more files",count-1)} else {String::new()};
            Ok(json!({"kind":"same-file","text":format!("Also changed in {address} · {}{more}. Both tracks have uncommitted changes.",r.get::<_,String>(2)?)}))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let unavailable: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM clip_file_scans WHERE clip=?1 AND outcome='unavailable')",
        [clip],
        |r| r.get(0),
    )?;
    if unavailable {
        warnings.push(json!({"kind":"file-comparison-unavailable","text":"Couldn’t compare this step’s file changes."}));
    }
    Ok(warnings)
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
                tokio::select! {_=stopping.changed()=>break,_=interval.tick()=>{}}
                let Ok(Some(ticket)) = journal.next_clip_files().await else {
                    continue;
                };
                let copy = ticket.clone();
                let result = tokio::task::spawn_blocking(move || inspect(&copy));
                let result = tokio::select! {_=stopping.changed()=>break,result=result=>result.unwrap_or_else(|error|Err(error.into()))};
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
