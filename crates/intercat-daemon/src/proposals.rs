//! One proposal slot for saved work results and Planner findings.
//! Answers retain their source identity for replay and retirement.
use crate::{
    Result,
    decision::{Decision, Kind, Target},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Commit,
    Continue,
    Later,
    KeepPlan,
    ParallelPlan,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Command {
    pub op_id: Uuid,
    pub proposal_id: Uuid,
    pub action: Action,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    crate::quiet_proposals::install(db)?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS proposal_answers(
        proposal TEXT PRIMARY KEY, session TEXT NOT NULL,
        operation TEXT NOT NULL UNIQUE REFERENCES operations(id) ON DELETE CASCADE,
        record TEXT NOT NULL);",
    )?;
    Ok(())
}

fn candidates(db: &Connection) -> Result<Vec<Value>> {
    let rows = db
        .prepare(
            "SELECT t.id,t.label,t.title,t.status,w.record FROM tracks t
        JOIN workers w ON w.track_id=t.id JOIN workspaces s ON s.track_id=t.id
        WHERE t.status='done' AND json_extract(s.record,'$.state')='ready' ORDER BY t.rowid",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut proposals = Vec::new();
    for (id, label, title, status, record) in rows {
        let worker: Value = serde_json::from_str(&record)?;
        let mut tracks = vec![json!({"id":id,"status":status})];
        crate::report_result::project(db, &mut tracks, std::slice::from_ref(&worker))?;
        let completion = &tracks[0]["completion"];
        let kind = completion["result"]["kind"].as_str().unwrap_or("");
        if !matches!(kind, "done" | "partial") {
            continue;
        }
        let session = completion["sessionId"].as_str().unwrap();
        let turn = completion["turnId"].as_str().unwrap();
        let checkpoint = worker["checkpoint"]["sha"]
            .as_str()
            .ok_or("Missing saved result checkpoint.")?;
        // A prepared checkpoint is not saved yet. Do not offer a commit during
        // publication, or revive a result whose changes were already integrated.
        let saved: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM operations
            WHERE kind='checkpoint.save' AND state='complete' AND json_extract(payload,'$.trackId')=?1
            AND json_extract(payload,'$.checkpoint.turnId')=?2 AND json_extract(payload,'$.checkpoint.sha')=?3)",
            params![id,turn,checkpoint], |row| row.get(0))?;
        let integrated: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM events e, json_each(e.payload,'$.tracks') t
            WHERE e.kind='integration.published' AND json_extract(e.payload,'$.status')='done'
            AND json_extract(t.value,'$.id')=?1 AND json_extract(t.value,'$.checkpoint.sha')=?2
            AND EXISTS(SELECT 1 FROM json_each(e.payload,'$.candidate.commits') c WHERE json_extract(c.value,'$.track')=?1))",
            params![id,checkpoint], |row| row.get(0))?;
        let waiting: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM report_questions
            WHERE track_id=?1 AND state IN ('open','queued','held')) OR EXISTS(SELECT 1 FROM reports
            WHERE track_id=?1 AND pending=1)",
            [&id],
            |row| row.get(0),
        )?;
        if !saved || integrated || waiting {
            continue;
        }
        let created_at: Option<i64> = db.query_row(
            "SELECT max(json_extract(record,'$.end.epochMs'))
            FROM timeline_clips WHERE track=?1 AND json_extract(record,'$.sessionId')=?2
            AND json_extract(record,'$.turnId')=?3",
            params![id, session, turn],
            |row| row.get(0),
        )?;
        let Some(created_at) = created_at else {
            continue;
        };
        let source = json!({"trackId":id,"sessionId":session,"turnId":turn,"checkpoint":checkpoint,"result":completion["result"]});
        let proposal_id = Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            &serde_json::to_vec(&("work-completed", &source))?,
        );
        let answered: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM proposal_answers WHERE proposal=?1)",
            [proposal_id.to_string()],
            |row| row.get(0),
        )?;
        if answered {
            continue;
        }
        let text = if kind == "partial" {
            format!("{label} finished part of it")
        } else {
            format!(
                "{label} is done — commit {} now?",
                crate::privacy::truncate(&title, 100)
            )
        };
        let options = if kind == "partial" {
            json!([{"action":"continue","text":format!("Ask {label} to continue")},{"action":"commit","text":"Commit what’s there"}])
        } else {
            json!([{"action":"commit","text":"Review for commit"},{"action":"later","text":"Not now"}])
        };
        proposals.push(json!({"id":proposal_id,"kind":"work-completed","trackId":id,"label":label,
            "createdAt":created_at,"text":text,"options":options,"source":source,
            "detail":format!("Worker report: {} Review the changes and checks before committing.", completion["result"]["reason"].as_str().unwrap_or(""))}));
    }
    let quiet = crate::quiet_proposals::candidates(db, &proposals)?;
    proposals.extend(quiet);
    proposals.extend(crate::plan_conflicts::candidates(db)?);
    proposals.sort_by_key(|value| {
        (
            value["kind"] != "work-overlap",
            value["createdAt"].as_i64().unwrap(),
        )
    });
    Ok(proposals)
}

pub(crate) fn current(db: &Connection) -> Result<Option<Value>> {
    Ok(candidates(db)?.into_iter().next())
}

pub(crate) fn respond(db: &mut Connection, command: &Command) -> Result<Value> {
    if command.op_id.get_version_num() != 7 {
        return Err("Invalid proposal operation ID.".into());
    }
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let payload = serde_json::to_string(command)?;
    let prior: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [command.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, previous)) = prior {
        if kind != "proposal.respond" || previous != payload {
            return Err("Operation ID reused.".into());
        }
        let result: String = tx.query_row(
            "SELECT record FROM proposal_answers WHERE operation=?1",
            [command.op_id.to_string()],
            |row| row.get(0),
        )?;
        return Ok(serde_json::from_str(&result)?);
    }
    let proposal = candidates(&tx)?
        .into_iter()
        .find(|p| p["id"] == command.proposal_id.to_string())
        .ok_or("The proposal changed or was already answered.")?;
    let selected = proposal["options"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["action"] == serde_json::to_value(&command.action).unwrap())
        .ok_or("This action is not offered by the proposal.")?;
    if let Some(reason) = selected["disabledReason"].as_str() {
        return Err(reason.into());
    }
    let result = if proposal["kind"] == "plan-conflict" {
        crate::plan_conflicts::respond(&tx, &proposal, &command.action)?
    } else {
        let track = proposal["trackId"].as_str().unwrap();
        let mut result = json!({"proposalId":command.proposal_id,"action":command.action,"trackId":track,"source":proposal["source"]});
        match command.action {
            Action::Continue => {
                let input = Uuid::now_v7().to_string();
                let text = format!(
                    "Continue the unfinished work from turn {}. Use its original request and constraints. Keep the language of the original user request in your report; do not switch languages to match these internal continuation instructions. The previous result, quoted as JSON, was: {}",
                    proposal["source"]["turnId"].as_str().unwrap(),
                    proposal["source"]["result"]
                );
                let session = proposal["source"]["sessionId"].as_str().unwrap();
                let turn = proposal["source"]["turnId"].as_str().unwrap();
                let owner = crate::plan_execution::turn_owner(&tx, session, turn)?;
                crate::journal::insert_utterance(
                    &tx,
                    &input,
                    &text,
                    &Target::Lock(track.into()),
                    None,
                    &[],
                    None,
                    true,
                    None,
                )?;
                if let Some((plan_id, item_id)) = owner {
                    crate::plan_continuation::attach(&tx, &input, plan_id, item_id)?;
                    let decision = json!({"kind":"extend","trackId":track,"boundary":"after-turn","command":null,"confidence":1.0,"inferenceMs":0.0,"timedOut":false,"scores":{}});
                    tx.execute("UPDATE utterances SET state=CASE WHEN state='held' THEN state ELSE 'placed' END,decision=?2 WHERE id=?1",
                    params![input,decision.to_string()])?;
                }
                result["inputId"] = input.into();
            }
            Action::Commit => {
                let input = Uuid::now_v7().to_string();
                crate::journal::insert_utterance(
                    &tx,
                    &input,
                    &format!(
                        "Review Worker {} for commit.",
                        proposal["label"].as_str().unwrap()
                    ),
                    &Target::Lock(track.into()),
                    None,
                    &[],
                    None,
                    true,
                    None,
                )?;
                let mut decision = Decision {
                    kind: Kind::Command,
                    track_id: Some(track.into()),
                    boundary: None,
                    command: Some("commit".into()),
                    confidence: 1.,
                    inference_ms: 0.,
                    timed_out: false,
                    scores: json!({}),
                };
                crate::app_commands::prepare_presentation(&mut decision)?;
                if proposal["kind"] == "work-overlap" {
                    decision.scores["presentation"]["commitTracks"] =
                        proposal["source"]["trackIds"].clone();
                }
                tx.execute("UPDATE utterances SET decision=?2,state=CASE WHEN state='held' THEN 'held' ELSE 'placed' END WHERE id=?1",
                params![input,serde_json::to_string(&decision)?])?;
                result["inputId"] = input.into();
            }
            Action::Later => {}
            Action::KeepPlan | Action::ParallelPlan => {
                return Err("This is not a plan conflict.".into());
            }
        }
        result
    };
    tx.execute(
        "INSERT INTO operations VALUES(?1,'proposal.respond',?2,'complete')",
        params![command.op_id.to_string(), payload],
    )?;
    tx.execute(
        "INSERT INTO proposal_answers VALUES(?1,?2,?3,?4)",
        params![
            command.proposal_id.to_string(),
            proposal["source"]["sessionId"].as_str().unwrap(),
            command.op_id.to_string(),
            result.to_string()
        ],
    )?;
    tx.execute(
        "INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'proposal.answered',?3)",
        params![
            crate::journal::timestamp(),
            proposal["source"]["sessionId"].as_str().unwrap(),
            json!({"proposal":proposal,"result":result}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(result)
}

pub(crate) fn retire(db: &Connection, session: &str) -> Result<()> {
    db.execute("DELETE FROM proposal_changes WHERE session=?1", [session])?;
    let scope =
        "(session=?1 OR EXISTS(SELECT 1 FROM json_each(proposal_answers.record,'$.source.tracks')
        WHERE json_extract(value,'$.sessionId')=?1))";
    db.execute(&format!("INSERT INTO retired_inputs SELECT json_extract(record,'$.inputId'),?1 FROM proposal_answers
        WHERE {scope} AND json_type(record,'$.inputId')='text' ON CONFLICT DO NOTHING"), [session])?;
    db.execute(&format!("DELETE FROM operations WHERE id IN (SELECT operation FROM proposal_answers WHERE {scope})"), [session])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn retiring_any_group_member_retires_its_pending_commit_review() -> crate::Result<()> {
        let db = rusqlite::Connection::open_in_memory()?;
        db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE operations(id TEXT PRIMARY KEY); CREATE TABLE retired_inputs(input TEXT PRIMARY KEY,session TEXT);
            INSERT INTO operations VALUES('group-op'),('solo-op');")?;
        super::install(&db)?;
        db.execute_batch("INSERT INTO proposal_answers VALUES('group','session-a','group-op','{\"inputId\":\"group-review\",\"source\":{\"tracks\":[{\"sessionId\":\"session-a\"},{\"sessionId\":\"session-b\"}]}}'),
            ('solo','session-a','solo-op','{\"inputId\":\"solo-review\"}');")?;
        super::retire(&db, "session-b")?;
        assert_eq!(
            db.query_row("SELECT input FROM retired_inputs", [], |row| row
                .get::<_, String>(0))?,
            "group-review"
        );
        assert_eq!(
            db.query_row("SELECT proposal FROM proposal_answers", [], |row| row
                .get::<_, String>(0))?,
            "solo"
        );
        Ok(())
    }

    #[test]
    fn retiring_history_also_retires_unstarted_proposal_work_and_answers() -> crate::Result<()> {
        let db = rusqlite::Connection::open_in_memory()?;
        db.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE operations(id TEXT PRIMARY KEY); CREATE TABLE retired_inputs(input TEXT PRIMARY KEY,session TEXT);
            INSERT INTO operations VALUES('answer-one'),('answer-two');")?;
        super::install(&db)?;
        db.execute_batch("INSERT INTO proposal_answers VALUES('one','old-session','answer-one','{\"inputId\":\"pending-work\"}'),('two','other-session','answer-two','{}');")?;
        db.execute_batch("INSERT INTO proposal_changes VALUES('old','old-session','{}'),('other','other-session','{}');")?;
        super::retire(&db, "old-session")?;
        assert_eq!(
            db.query_row("SELECT session FROM proposal_changes", [], |row| row
                .get::<_, String>(0))?,
            "other-session"
        );
        assert_eq!(
            db.query_row(
                "SELECT session FROM retired_inputs WHERE input='pending-work'",
                [],
                |row| row.get::<_, String>(0)
            )?,
            "old-session"
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM proposal_answers", [], |row| row
                .get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            db.query_row("SELECT id FROM operations", [], |row| row
                .get::<_, String>(0))?,
            "answer-two"
        );
        Ok(())
    }
}
