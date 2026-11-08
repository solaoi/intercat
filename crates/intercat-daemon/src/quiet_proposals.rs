//! Observe saved Git trees outside the journal writer before suggesting a
//! review after five quiet minutes. Observations never modify the worktree.
use crate::{Result, clock::Stamp, journal::Journal};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio::{sync::watch, task::JoinHandle};
use uuid::Uuid;

const QUIET_MS: i64 = 5 * 60 * 1000;

#[derive(Serialize, Deserialize)]
struct QuietClock {
    activity: i64,
    since: Stamp,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ticket {
    pub track_id: String,
    pub session_id: String,
    pub turn_id: String,
    pub path: PathBuf,
    pub baseline: String,
    pub checkpoint: String,
    pub activity: i64,
}

#[derive(Serialize, Deserialize)]
struct Observation {
    ticket: Ticket,
    changes: Option<Changes>,
    at: Stamp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Changes {
    pub repository: PathBuf,
    pub paths: Vec<String>,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS proposal_quiet_clock(id INTEGER PRIMARY KEY CHECK(id=1),record TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS proposal_changes(track TEXT PRIMARY KEY,session TEXT NOT NULL,record TEXT NOT NULL);
        DELETE FROM proposal_changes;")?;
    // Tree observations are a cache: recheck filesystem identity on startup.
    // Answers remain durable and are tied to the saved checkpoints.
    Ok(())
}

fn activity(db: &Connection) -> Result<i64> {
    // Polling, usage replies, generated descriptions and our own observations
    // do not extend the quiet interval. Draft edits and actual work do.
    Ok(db.query_row("SELECT coalesce(max(seq),0) FROM events WHERE
        kind GLOB 'utterance.*' OR kind GLOB 'draft.*' OR kind GLOB 'integration.*'
        OR kind GLOB 'history.*' OR kind='worker.state' OR kind='proposal.answered'
        OR (kind='msp.applied' AND (json_extract(payload,'$.method') GLOB 'item/*' OR json_extract(payload,'$.method') GLOB 'turn/*'))",
        [], |row|row.get(0))?)
}

fn clock(db: &Connection) -> Result<Option<QuietClock>> {
    db.query_row(
        "SELECT record FROM proposal_quiet_clock WHERE id=1",
        [],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|record| Ok(serde_json::from_str(&record)?))
    .transpose()
}

fn quiet(db: &Connection, now: &Stamp) -> Result<Option<i64>> {
    let Some(clock) = clock(db)? else {
        return Ok(None);
    };
    if clock.since.boot_id != now.boot_id
        || clock.activity != activity(db)?
        || now.elapsed(&clock.since).0 < QUIET_MS
    {
        return Ok(None);
    }
    Ok(Some(clock.activity))
}

fn scopes(db: &Connection, now: &Stamp) -> Result<Vec<Ticket>> {
    let Some(activity) = quiet(db, now)? else {
        return Ok(vec![]);
    };
    let rows = db
        .prepare(
            "SELECT t.id,t.status,w.record,s.record FROM tracks t JOIN workers w ON w.track_id=t.id
        JOIN workspaces s ON s.track_id=t.id WHERE json_extract(s.record,'$.state')='ready' ORDER BY t.rowid",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut result = vec![];
    for (track, _status, worker, workspace) in rows {
        let worker: Value = serde_json::from_str(&worker)?;
        let workspace: crate::preparation::Workspace = serde_json::from_str(&workspace)?;
        let (Some(session), Some(turn), Some(checkpoint)) = (
            worker["sessionId"].as_str(),
            worker["checkpoint"]["turnId"].as_str(),
            worker["checkpoint"]["sha"].as_str(),
        ) else {
            continue;
        };
        let saved: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind='checkpoint.save' AND state='complete'
            AND json_extract(payload,'$.trackId')=?1 AND json_extract(payload,'$.checkpoint.turnId')=?2 AND json_extract(payload,'$.checkpoint.sha')=?3)",
            params![track,turn,checkpoint],|row|row.get(0))?;
        if !saved {
            continue;
        }
        // A running worker can still contribute its previous saved turn,
        // but a known failed result must not become a Commit suggestion.
        let mut saved_worker = worker.clone();
        saved_worker["activeTurn"] = Value::Null;
        saved_worker["checkpointPending"] = false.into();
        let mut completion = vec![json!({"id":track,"status":"done"})];
        crate::report_result::project(db, &mut completion, &[saved_worker])?;
        if completion[0]["completion"]["result"]["kind"] == "failed" {
            continue;
        }
        let baseline: Option<String> = db.query_row("SELECT json_extract(t.value,'$.checkpoint.sha') FROM events e,json_each(e.payload,'$.tracks') t
            WHERE e.kind='integration.published' AND json_extract(e.payload,'$.status')='done' AND json_extract(t.value,'$.id')=?1
            AND EXISTS(SELECT 1 FROM json_each(e.payload,'$.candidate.commits') c WHERE json_extract(c.value,'$.track')=?1)
            ORDER BY e.seq DESC LIMIT 1",[&track],|row|row.get(0)).optional()?.flatten();
        result.push(Ticket {
            track_id: track,
            session_id: session.into(),
            turn_id: turn.into(),
            path: workspace.path,
            baseline: baseline.unwrap_or(workspace.base),
            checkpoint: checkpoint.into(),
            activity,
        });
    }
    Ok(result)
}

fn eligible(db: &Connection, track: &str) -> Result<bool> {
    let (status, record): (String, String) = db.query_row(
        "SELECT t.status,w.record FROM tracks t JOIN workers w ON w.track_id=t.id WHERE t.id=?1",
        [track],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let worker: Value = serde_json::from_str(&record)?;
    if !matches!(
        worker["phase"].as_str(),
        Some("ready" | "held" | "stopped" | "sleeping")
    ) || !worker["activeTurn"].is_null()
        || worker["checkpointPending"] == true
    {
        return Ok(false);
    }
    let mut completion = vec![json!({"id":track,"status":status})];
    crate::report_result::project(db, &mut completion, std::slice::from_ref(&worker))?;
    if completion[0]["completion"]["result"]["kind"] == "failed" {
        return Ok(false);
    }
    let pending: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM reports WHERE track_id=?1 AND pending=1)
        OR EXISTS(SELECT 1 FROM report_questions WHERE track_id=?1 AND state IN ('open','queued','held'))
        OR EXISTS(SELECT 1 FROM utterances WHERE coalesce(json_extract(decision,'$.trackId'),provisional)=?1 AND state IN ('staged','reading','starting','sent','redirecting'))
        OR EXISTS(SELECT 1 FROM repair_runs WHERE active=1 AND json_extract(record,'$.track')=?1)
        OR EXISTS(SELECT 1 FROM integration_jobs j,json_each(j.record,'$.tracks') t WHERE j.active=1 AND json_extract(t.value,'$.id')=?1 AND json_extract(j.record,'$.status') IN ('preparing','ordering','building','staging','committing'))",
        [track], |row| row.get(0))?;
    Ok(!pending)
}

fn observation(db: &Connection, track: &str) -> Result<Option<Observation>> {
    db.query_row(
        "SELECT record FROM proposal_changes WHERE track=?1",
        [track],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|record| Ok(serde_json::from_str(&record)?))
    .transpose()
}

pub(crate) fn next(db: &mut Connection, now: &Stamp) -> Result<Option<Ticket>> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let current = activity(&tx)?;
    if clock(&tx)?.is_none_or(|old| old.activity != current || old.since.boot_id != now.boot_id) {
        tx.execute("INSERT INTO proposal_quiet_clock VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            [serde_json::to_string(&QuietClock {activity:current,since:now.clone()})?])?;
    }
    let mut next = None;
    for scope in scopes(&tx, now)? {
        if observation(&tx, &scope.track_id)?.is_none_or(|old| {
            old.ticket != scope || old.changes.is_none() && now.elapsed(&old.at).0 >= 30_000
        }) {
            next = Some(scope);
            break;
        }
    }
    tx.commit()?;
    Ok(next)
}

pub fn inspect(ticket: &Ticket) -> Result<Changes> {
    let repo = intercat_git::Repository::open(&ticket.path)?;
    Ok(Changes {
        repository: repo.common_directory()?,
        paths: crate::integration::changes(
            &repo,
            repo.tree_id(ticket.baseline.parse()?)?,
            repo.tree_id(ticket.checkpoint.parse()?)?,
        )?
        .into_iter()
        .map(|change| change.path)
        .collect(),
    })
}

pub(crate) fn finish(
    db: &mut Connection,
    ticket: &Ticket,
    changes: Option<Changes>,
    now: &Stamp,
) -> Result<bool> {
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    if !scopes(&tx, now)?.contains(ticket) {
        return Ok(false);
    }
    let record = Observation {
        ticket: ticket.clone(),
        changes,
        at: now.clone(),
    };
    tx.execute("INSERT INTO proposal_changes VALUES(?1,?2,?3) ON CONFLICT(track) DO UPDATE SET session=excluded.session,record=excluded.record",
        params![ticket.track_id,ticket.session_id,serde_json::to_string(&record)?])?;
    tx.execute("INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'proposal.changes.checked',?3)",
        params![now.epoch_ms,ticket.session_id,json!({"trackId":ticket.track_id,"checkpoint":ticket.checkpoint,"baseline":ticket.baseline,"changes":record.changes}).to_string()])?;
    tx.commit()?;
    Ok(true)
}

fn source(ticket: &Ticket) -> Value {
    json!({"trackId":ticket.track_id,"sessionId":ticket.session_id,"turnId":ticket.turn_id,
        "checkpoint":ticket.checkpoint,"baseline":ticket.baseline})
}

fn answered(db: &Connection, id: Uuid) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM proposal_answers WHERE proposal=?1)",
        [id.to_string()],
        |row| row.get(0),
    )?)
}

fn overlap_groups(observed: &[(Ticket, Observation)]) -> Vec<(Vec<usize>, Vec<String>)> {
    let mut owners: std::collections::BTreeMap<(PathBuf, String), Vec<usize>> =
        std::collections::BTreeMap::new();
    for (index, (_, observation)) in observed.iter().enumerate() {
        let changes = observation.changes.as_ref().unwrap();
        for path in &changes.paths {
            owners
                .entry((changes.repository.clone(), path.clone()))
                .or_default()
                .push(index);
        }
    }
    let mut neighbors = vec![Vec::new(); observed.len()];
    for indices in owners.values().filter(|indices| indices.len() > 1) {
        for &other in &indices[1..] {
            neighbors[indices[0]].push(other);
            neighbors[other].push(indices[0]);
        }
    }
    let mut membership = vec![usize::MAX; observed.len()];
    let mut groups: Vec<(Vec<usize>, Vec<String>)> = vec![];
    for index in 0..observed.len() {
        if membership[index] != usize::MAX {
            continue;
        }
        let mut pending = vec![index];
        let mut members = vec![];
        while let Some(member) = pending.pop() {
            if membership[member] != usize::MAX {
                continue;
            }
            membership[member] = groups.len();
            members.push(member);
            pending.extend(&neighbors[member]);
        }
        members.sort_unstable();
        groups.push((members, vec![]));
    }
    for ((_, path), indices) in owners {
        if indices.len() > 1 {
            groups[membership[indices[0]]].1.push(path);
        }
    }
    groups
}

pub(crate) fn candidates(db: &Connection, completed: &[Value]) -> Result<Vec<Value>> {
    let mut observed = Vec::new();
    for ticket in scopes(db, &Stamp::now())? {
        let Some(record) = observation(db, &ticket.track_id)?
            .filter(|record| record.ticket == ticket && record.changes.is_some())
        else {
            // One scan completes per background tick. Do not offer a partial
            // group while a saved track might still overlap with it.
            return Ok(vec![]);
        };
        if !record.changes.as_ref().unwrap().paths.is_empty() {
            observed.push((ticket, record));
        }
    }
    let groups = overlap_groups(&observed);
    let mut result = vec![];
    for (component, shared) in groups {
        let index = component[0];
        let mut can_offer = false;
        for &member in &component {
            can_offer |= eligible(db, &observed[member].0.track_id)?;
        }
        if !can_offer {
            continue;
        }
        let (ticket, record) = &observed[index];
        if component.len() > 1 {
            let members: Vec<_> = component
                .iter()
                .map(|&member| source(&observed[member].0))
                .collect();
            let track_ids: Vec<_> = component
                .iter()
                .map(|&member| observed[member].0.track_id.clone())
                .collect();
            let source =
                json!({"sessionId":ticket.session_id,"tracks":members,"trackIds":track_ids});
            let id = Uuid::new_v5(
                &Uuid::NAMESPACE_OID,
                &serde_json::to_vec(&("work-overlap", &source))?,
            );
            if answered(db, id)? {
                continue;
            }
            let labels: Vec<String> = component
                .iter()
                .map(|&member| {
                    db.query_row(
                        "SELECT label FROM tracks WHERE id=?1",
                        [&observed[member].0.track_id],
                        |row| row.get(0),
                    )
                })
                .collect::<rusqlite::Result<_>>()?;
            result.push(json!({"id":id,"kind":"work-overlap","trackId":ticket.track_id,"label":labels.join(" + "),
                "source":source,"createdAt":record.at.epoch_ms,
                "text":format!("{} touch the same files", labels.join(" and ")),
                "detail":format!("Five quiet minutes. Saved changes overlap in {}. Review the track order before committing. Running changes are not included.",shared.join(", ")),
                "options":[{"action":"commit","text":"Review order"},{"action":"later","text":"Not now"}]}));
            continue;
        }
        if completed
            .iter()
            .any(|value| value["trackId"] == ticket.track_id)
        {
            continue;
        }
        let source = source(ticket);
        let id = Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            &serde_json::to_vec(&("quiet-changes", &source))?,
        );
        if answered(db, id)? {
            continue;
        }
        let (label, title): (String, String) = db.query_row(
            "SELECT label,title FROM tracks WHERE id=?1",
            [&ticket.track_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        result.push(json!({"id":id,"kind":"quiet-changes","trackId":ticket.track_id,"label":label,"source":source,"createdAt":record.at.epoch_ms,
            "text":format!("{label} has uncommitted changes — review {}?",crate::privacy::truncate(&title,100)),
            "detail":"No work or draft edits were recorded for five minutes. The saved changes differ from the last integrated version. Review the changes and checks before committing.",
            "options":[{"action":"commit","text":"Review for commit"},{"action":"later","text":"Not now"}]}));
    }
    Ok(result)
}

pub struct Service {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}
impl Service {
    pub fn start(journal: Journal) -> Self {
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! { _=stopping.changed()=>break, _=interval.tick()=>{} }
                let Ok(Some(ticket)) = journal.check_quiet_proposal(Stamp::now()).await else {
                    continue;
                };
                let copy = ticket.clone();
                let inspection = tokio::task::spawn_blocking(move || inspect(&copy));
                let changed = tokio::select! {
                    _=stopping.changed()=>break,
                    result=inspection=>result.ok().and_then(std::result::Result::ok),
                };
                let _ = journal
                    .finish_quiet_proposal(ticket, changed, Stamp::now())
                    .await;
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
    fn shared_paths_form_groups_only_within_the_same_repository() {
        let rows: Vec<_> = [
            ("repo", vec!["a"]),
            ("repo", vec!["a", "b"]),
            ("repo", vec!["b"]),
            ("other", vec!["a"]),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (repository, paths))| {
            let ticket = Ticket {
                track_id: index.to_string(),
                session_id: "dummy".into(),
                turn_id: "dummy".into(),
                path: repository.into(),
                baseline: "base".into(),
                checkpoint: "saved".into(),
                activity: 1,
            };
            let observation = Observation {
                ticket: ticket.clone(),
                changes: Some(Changes {
                    repository: repository.into(),
                    paths: paths.into_iter().map(str::to_owned).collect(),
                }),
                at: Stamp::now(),
            };
            (ticket, observation)
        })
        .collect();
        let groups = overlap_groups(&rows);
        assert_eq!(
            groups[0],
            (vec![0, 1, 2], vec!["a".to_owned(), "b".to_owned()])
        );
        assert_eq!(groups[1], (vec![3], vec![]));
    }
}
