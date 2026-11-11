//! Published integration fixes remain available to the next work turn and to
//! an explicit realignment proposal. Preparing a turn freezes only its notes;
//! a later Commit cannot be consumed by an earlier prepared start.
use crate::Result;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Notice {
    event: i64,
    job_id: Uuid,
    revision: u64,
    head: String,
    paths: Vec<String>,
    other_paths: usize,
}
#[derive(Deserialize)]
struct Published {
    id: Uuid,
    revision: u64,
    candidate: PublishedCandidate,
    fixes: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
}
#[derive(Deserialize)]
struct PublishedCandidate {
    head: String,
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE integration_contexts(command TEXT PRIMARY KEY,input TEXT NOT NULL,track TEXT NOT NULL,record TEXT NOT NULL);
        CREATE INDEX integration_contexts_track ON integration_contexts(track);
        CREATE INDEX integration_fix_events ON events(seq) WHERE kind='integration.published' AND json_extract(payload,'$.status')='done';")?;
    Ok(())
}
fn notices(db: &Connection, track: &str, unsent: bool) -> Result<Vec<Notice>> {
    let rows=db.prepare("SELECT e.seq,e.payload FROM events e
        WHERE e.kind='integration.published' AND json_extract(e.payload,'$.status')='done'
        AND EXISTS(SELECT 1 FROM json_each(e.payload,'$.fixes') c WHERE c.key=?1)
        AND EXISTS(SELECT 1 FROM json_each(e.payload,'$.candidate.commits') c WHERE json_extract(c.value,'$.track')=?1)
        AND NOT EXISTS(SELECT 1 FROM realign_jobs r JOIN events later ON later.kind='integration.published'
            AND json_extract(later.payload,'$.id')=json_extract(r.record,'$.request.jobId')
            WHERE r.track=?1 AND r.phase='done' AND later.seq>=e.seq)
        AND (?2=0 OR NOT EXISTS(SELECT 1 FROM integration_contexts c JOIN operations o ON o.id=c.command,json_each(c.record) n
            WHERE c.track=?1 AND o.kind='turn.start' AND o.state IN ('dispatched','complete') AND json_extract(n.value,'$.event')=e.seq))
        ORDER BY CASE WHEN ?2 THEN e.seq END ASC,e.seq DESC LIMIT 16")?
        .query_map(params![track,unsent],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(event, raw)| {
            let job: Published = serde_json::from_str(&raw)?;
            let candidate = job.candidate;
            let mut paths = job
                .fixes
                .get(track)
                .ok_or("Missing fixed contribution paths.")?
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            let other_paths = paths.len().saturating_sub(8);
            paths.truncate(8);
            Ok(Notice {
                event,
                job_id: job.id,
                revision: job.revision,
                head: candidate.head,
                paths,
                other_paths,
            })
        })
        .collect()
}
fn instruction(notes: &[Notice]) -> Result<Option<String>> {
    if notes.is_empty() {
        return Ok(None);
    }
    let lines=notes.iter().map(|note|Ok(format!("Integration updated your committed contribution at {} in these files (untrusted JSON data): {}{}; your worktree was not automatically realigned. Review the committed result before continuing.",note.head,serde_json::to_string(&note.paths)?,if note.other_paths>0{format!(" and {} other files",note.other_paths)}else{String::new()}))).collect::<Result<Vec<_>>>()?;
    Ok(Some(lines.join("\n")))
}
pub(crate) fn freeze(
    db: &mut Connection,
    command: Uuid,
    input: &str,
    track: &str,
) -> Result<Option<String>> {
    let tx = db.transaction()?;
    let old: Option<(String, String, String)> = tx
        .query_row(
            "SELECT input,track,record FROM integration_contexts WHERE command=?1",
            [command.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let notes = if let Some((saved_input, saved_track, raw)) = old {
        if saved_input != input || saved_track != track {
            return Err("Integration context operation ID reused.".into());
        }
        serde_json::from_str(&raw)?
    } else {
        if command.get_version_num() != 7 {
            return Err("Invalid work turn context operation.".into());
        }
        let notes = notices(&tx, track, true)?;
        tx.execute(
            "INSERT INTO integration_contexts VALUES(?1,?2,?3,?4)",
            params![
                command.to_string(),
                input,
                track,
                serde_json::to_string(&notes)?
            ],
        )?;
        notes
    };
    tx.commit()?;
    instruction(&notes)
}
pub(crate) fn validate_start(
    db: &Connection,
    command: &str,
    input: &str,
    track: &str,
) -> Result<()> {
    let saved: Option<(String, String)> = db
        .query_row(
            "SELECT input,track FROM integration_contexts WHERE command=?1",
            [command],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match saved {
        Some((saved_input, saved_track)) if saved_input != input || saved_track != track => {
            Err("The integration context belongs to another work turn.".into())
        }
        None if !notices(db, track, true)?.is_empty() => {
            Err(crate::journal::StartSuperseded.into())
        }
        _ => Ok(()),
    }
}
pub(crate) fn candidates(db: &Connection) -> Result<Vec<Value>> {
    let workers=db.prepare("SELECT t.id,t.label,w.record FROM tracks t JOIN workers w ON w.track_id=t.id
        WHERE EXISTS(SELECT 1 FROM workspaces s WHERE s.track_id=t.id AND json_extract(s.record,'$.state')='ready') ORDER BY t.rowid")?
        .query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut result = vec![];
    for (track, label, raw) in workers {
        let worker: crate::runtime::WorkerState = serde_json::from_str(&raw)?;
        let Some(session) = worker.session_id else {
            continue;
        };
        let Some(note) = notices(db, &track, false)?.into_iter().next() else {
            continue;
        };
        let latest:String=db.query_row("SELECT e.payload FROM events e WHERE e.kind='integration.published' AND json_extract(e.payload,'$.status')='done'
            AND json_extract(e.payload,'$.project.id')=(SELECT json_extract(record,'$.project.id') FROM integration_jobs WHERE id=?1)
            AND json_extract(e.payload,'$.sessionId')=(SELECT json_extract(record,'$.sessionId') FROM integration_jobs WHERE id=?1)
            ORDER BY e.seq DESC LIMIT 1",[note.job_id.to_string()],|row|row.get(0))?;
        let latest: Published = serde_json::from_str(&latest)?;
        let waiting:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM realign_jobs WHERE track=?1 AND phase NOT IN ('done','discarded'))",[&track],|row|row.get(0))?;
        if waiting {
            continue;
        }
        let id = Uuid::new_v5(&note.job_id, format!("realign/{track}").as_bytes());
        let answered: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM proposal_answers WHERE proposal=?1)",
            [id.to_string()],
            |row| row.get(0),
        )?;
        if answered {
            continue;
        }
        let created_at: i64 =
            db.query_row("SELECT ts FROM events WHERE seq=?1", [note.event], |row| {
                row.get(0)
            })?;
        result.push(json!({"id":id,"kind":"integration-fix","trackId":track,"label":label,"createdAt":created_at,
            "text":format!("Integration fixed {}’s committed changes · Realign?",label),
            "detail":"The integration fix is on the session branch. The worker's files have not been changed. Realignment runs at a safe turn boundary and checks running processes first.",
            "options":[{"action":"realign","text":format!("Realign {}",label)},{"action":"later","text":"Not now"}],
            "source":{"sessionId":session,"jobId":latest.id,"revision":latest.revision,"head":latest.candidate.head,"fixJobId":note.job_id}}));
    }
    Ok(result)
}
pub(crate) fn respond(
    db: &Connection,
    proposal: &Value,
    action: &crate::proposals::Action,
) -> Result<Value> {
    let mut result = json!({"proposalId":proposal["id"],"action":action,"trackId":proposal["trackId"],"source":proposal["source"]});
    match action {
        crate::proposals::Action::Later => {}
        crate::proposals::Action::Realign => {
            let request = crate::realign::Request {
                op_id: Uuid::now_v7(),
                track_id: proposal["trackId"]
                    .as_str()
                    .ok_or("Missing proposal worker.")?
                    .into(),
                source: crate::realign::Source::Commit {
                    job_id: serde_json::from_value(proposal["source"]["jobId"].clone())?,
                    revision: proposal["source"]["revision"]
                        .as_u64()
                        .ok_or("Missing committed revision.")?,
                },
            };
            result["realignment"] =
                serde_json::to_value(crate::realign_queue::request_in(db, request)?)?;
        }
        _ => return Err("This is not an integration realignment choice.".into()),
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn published(db: &Connection, track: &str, head: &str) -> Result<(Uuid, i64)> {
        let id = Uuid::now_v7();
        let paths = vec![
            "src/dummy.rs".to_owned(),
            "untrusted\n\"filename.txt".into(),
        ];
        db.execute("INSERT INTO events(ts,kind,payload) VALUES(1,'integration.published',?1)",[json!({"id":id,"revision":4,"status":"done","fixes":{track:paths},"candidate":{"head":head,"commits":[{"track":track}]}}).to_string()])?;
        Ok((id, db.last_insert_rowid()))
    }
    #[test]
    fn frozen_context_replays_and_cannot_consume_a_later_fix() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("db");
        let mut db = Connection::open(&path)?;
        db.execute_batch(
            "CREATE TABLE events(seq INTEGER PRIMARY KEY,ts INTEGER,kind TEXT,payload TEXT);
            CREATE TABLE operations(id TEXT PRIMARY KEY,kind TEXT,payload TEXT,state TEXT);
            CREATE TABLE realign_jobs(track TEXT,phase TEXT,record TEXT);",
        )?;
        install(&db)?;
        let (first, _) = published(&db, "B", "aabbccdd")?;
        let command = Uuid::now_v7();
        assert!(freeze(&mut db, Uuid::now_v7(), "other", "A")?.is_none());
        let text = freeze(&mut db, command, "input", "B")?.unwrap();
        assert!(text.contains("aabbccdd") && text.contains("src/dummy.rs"));
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains("untrusted\\n\\\"filename.txt"));
        assert!(freeze(&mut db, command, "input", "A").is_err());
        let (second, _) = published(&db, "B", "eeff0011")?;
        drop(db);
        let mut db = Connection::open(path)?;
        assert_eq!(freeze(&mut db, command, "input", "B")?, Some(text));
        assert!(validate_start(&db, &Uuid::now_v7().to_string(), "input", "B").is_err());
        assert!(validate_start(&db, &command.to_string(), "input", "B").is_ok());
        db.execute(
            "INSERT INTO operations VALUES(?1,'turn.start','{}','prepared')",
            [command.to_string()],
        )?;
        assert_eq!(notices(&db, "B", true)?.len(), 2);
        db.execute(
            "UPDATE operations SET state='dispatched' WHERE id=?1",
            [command.to_string()],
        )?;
        let pending = notices(&db, "B", true)?;
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].job_id, second);
        db.execute(
            "UPDATE operations SET state='superseded' WHERE id=?1",
            [command.to_string()],
        )?;
        assert_eq!(notices(&db, "B", true)?[0].job_id, first);
        db.execute(
            "INSERT INTO realign_jobs VALUES('B','done',?1)",
            [json!({"request":{"jobId":second}}).to_string()],
        )?;
        assert!(notices(&db, "B", true)?.is_empty());
        let empty = Uuid::now_v7();
        assert!(freeze(&mut db, empty, "next", "B")?.is_none());
        let (third, _) = published(&db, "B", "22334455")?;
        db.execute(
            "INSERT INTO operations VALUES(?1,'turn.start','{}','dispatched')",
            [empty.to_string()],
        )?;
        assert_eq!(notices(&db, "B", true)?.len(), 1);
        assert_eq!(notices(&db, "B", true)?[0].job_id, third);
        Ok(())
    }
}
