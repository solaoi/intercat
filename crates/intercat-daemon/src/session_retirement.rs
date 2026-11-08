//! Permanent session boundaries used after restoring an older database.
use crate::{Result, checkpoint::Checkpoint};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Resume {
    pub track_id: String,
    pub session_id: String,
    pub start_command: Uuid,
    pub checkpoint: Checkpoint,
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE retired_sessions(session TEXT PRIMARY KEY,track TEXT NOT NULL,operation TEXT NOT NULL);
      CREATE TABLE retired_inputs(input TEXT PRIMARY KEY,session TEXT NOT NULL);
      CREATE TABLE backup_resumes(track TEXT PRIMARY KEY,record TEXT NOT NULL,opened INTEGER NOT NULL);")?;
    Ok(())
}
pub(crate) fn catalog_bytes(db: &Connection) -> Result<Vec<u8>> {
    let sessions = db
        .prepare("SELECT session,track,operation FROM retired_sessions ORDER BY session")?
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let inputs = db
        .prepare("SELECT input,session FROM retired_inputs ORDER BY input")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let resumes = db
        .prepare("SELECT track,record FROM backup_resumes ORDER BY track")?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(serde_json::to_vec(&(sessions, inputs, resumes))?)
}
pub(crate) fn ensure_open(db: &Connection, session: &str) -> Result<()> {
    if db.query_row(
        "SELECT EXISTS(SELECT 1 FROM retired_sessions WHERE session=?1)",
        [session],
        |r| r.get::<_, bool>(0),
    )? {
        return Err(
            "This session was retired during backup recovery. Its history cannot be reopened."
                .into(),
        );
    }
    Ok(())
}
pub(crate) fn ensure_input(db: &Connection, input: &str) -> Result<()> {
    if db.query_row(
        "SELECT EXISTS(SELECT 1 FROM retired_inputs WHERE input=?1)",
        [input],
        |r| r.get::<_, bool>(0),
    )? {
        return Err("This request belongs to retired history.".into());
    }
    Ok(())
}
pub(crate) fn resume(db: &Connection, track: &str) -> Result<Option<(Resume, bool)>> {
    let row: Option<(String, bool)> = db
        .query_row(
            "SELECT record,opened FROM backup_resumes WHERE track=?1",
            [track],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(record, opened)| {
        let resume: Resume = serde_json::from_str(&record)?;
        if resume.track_id != track {
            return Err("The recovered session has another owner.".into());
        }
        ensure_open(db, &resume.session_id)?;
        Ok((resume, opened))
    })
    .transpose()
}
pub(crate) fn opened(db: &Connection, track: &str, session: &str) -> Result<()> {
    if let Some((resume, _)) = resume(db, track)? {
        if resume.session_id != session {
            return Err("The recovered session identity changed.".into());
        }
        db.execute("UPDATE backup_resumes SET opened=1 WHERE track=?1", [track])?;
    }
    Ok(())
}
pub(crate) fn retire(
    tx: &Transaction<'_>,
    session: &str,
    track: &str,
    operation: Uuid,
) -> Result<()> {
    ensure_open(tx, session)?;
    // Internal fix inputs have no You mark; retire them through their review origin.
    tx.execute("INSERT INTO retired_inputs SELECT i.input,?1 FROM review_fix_inputs i
        JOIN review_fixes f ON f.id=i.fix JOIN review_notes n ON n.id=f.note JOIN review_jobs j ON j.id=n.job
        WHERE json_extract(j.record,'$.target.sessionId')=?1 ON CONFLICT DO NOTHING",[session])?;
    tx.execute("DELETE FROM timeline_clips WHERE id IN (SELECT c.clip FROM review_clips c JOIN review_jobs j ON j.id=c.job WHERE json_extract(j.record,'$.target.sessionId')=?1)",[session])?;
    tx.execute(
        "DELETE FROM review_jobs WHERE json_extract(record,'$.target.sessionId')=?1",
        [session],
    )?;
    crate::proposals::retire(tx, session)?;
    tx.execute("INSERT INTO retired_inputs SELECT id,?1 FROM utterances WHERE id IN (SELECT input FROM timeline_marks WHERE session=?1 AND input IS NOT NULL) OR dispatch_command IN (SELECT command FROM attachment_turns WHERE session=?1) ON CONFLICT DO NOTHING",[session])?;
    crate::plan_store::retire_history(tx, session)?;
    tx.execute(
        "DELETE FROM redirects WHERE input IN (SELECT input FROM retired_inputs WHERE session=?1)",
        [session],
    )?;
    tx.execute("DELETE FROM input_attachments WHERE input IN (SELECT input FROM retired_inputs WHERE session=?1)",[session])?;
    tx.execute(
        "DELETE FROM utterances WHERE id IN (SELECT input FROM retired_inputs WHERE session=?1)",
        [session],
    )?;
    tx.execute("DELETE FROM deletion_snapshots WHERE item IN (SELECT item FROM timeline_items WHERE session=?1)",[session])?;
    tx.execute("DELETE FROM operations WHERE kind='checkpoint.save' AND json_extract(payload,'$.trackId')=?2 AND (json_extract(payload,'$.checkpoint.turnId') IN (SELECT turn FROM timeline_turns WHERE session=?1) OR json_extract(payload,'$.checkpoint.turnId') IN (SELECT json_extract(record,'$.checkpoint.turnId') FROM workers WHERE track_id=?2 AND json_extract(record,'$.sessionId')=?1))",params![session,track])?;
    for table in [
        "timeline_outputs",
        "timeline_marks",
        "timeline_items",
        "timeline_turns",
        "approval_policies",
        "effects",
        "effect_items",
        "effect_gates",
        "msp_applied",
        "msp_gaps",
        "session_parameters",
    ] {
        tx.execute(&format!("DELETE FROM {table} WHERE session=?1"), [session])?;
    }
    tx.execute(
        "DELETE FROM timeline_clips WHERE json_extract(record,'$.sessionId')=?1",
        [session],
    )?;
    tx.execute(
        "DELETE FROM report_questions WHERE json_extract(record,'$.sessionId')=?1",
        [session],
    )?;
    tx.execute(
        "DELETE FROM reports WHERE json_extract(record,'$.sessionId')=?1",
        [session],
    )?;
    tx.execute(
        "DELETE FROM secret_inputs WHERE json_extract(record,'$.sessionId')=?1",
        [session],
    )?;
    // Completed and pending reviews can contain this session's reports and
    // instructions. Remove their records so neither snapshots nor retries can
    // reopen an unverified contribution; the Git commits remain untouched.
    let jobs = tx.prepare("SELECT id FROM integration_jobs WHERE id IN (SELECT job FROM repair_runs WHERE json_extract(record,'$.session')=?1) OR (EXISTS(SELECT 1 FROM workers WHERE track_id=?2 AND json_extract(record,'$.sessionId')=?1) AND EXISTS(SELECT 1 FROM json_each(integration_jobs.record,'$.tracks') t WHERE json_extract(t.value,'$.id')=?2))")?
        .query_map(params![session,track],|row|row.get::<_,String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for job in jobs {
        tx.execute("DELETE FROM integration_settings WHERE job=?1", [&job])?;
        tx.execute("DELETE FROM repair_runs WHERE job=?1", [&job])?;
        tx.execute("DELETE FROM integration_jobs WHERE id=?1", [&job])?;
    }
    tx.execute(
        "DELETE FROM repair_runs WHERE json_extract(record,'$.session')=?1",
        [session],
    )?;
    tx.execute("DELETE FROM msp_positions WHERE session=?1", [session])?;
    tx.execute(
        "DELETE FROM attachment_turn_items WHERE session=?1",
        [session],
    )?;
    tx.execute("DELETE FROM attachment_turns WHERE session=?1", [session])?;
    tx.execute("DELETE FROM operations WHERE json_extract(payload,'$.sessionId')=?1 OR id IN (SELECT input FROM retired_inputs WHERE session=?1)",[session])?;
    tx.execute("DELETE FROM operations WHERE kind='workspace.apply-inheritance' AND json_extract(payload,'$.session')=?1", [session])?;
    tx.execute("DELETE FROM events WHERE session_id=?1 OR json_extract(payload,'$.sessionId')=?1 OR json_extract(payload,'$.session')=?1 OR json_extract(payload,'$.payload.sessionId')=?1 OR json_extract(payload,'$.id') IN (SELECT input FROM retired_inputs WHERE session=?1)",[session])?;
    tx.execute(
        "INSERT INTO retired_sessions VALUES(?1,?2,?3)",
        params![session, track, operation.to_string()],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'history.retired',?2)",
        params![
            crate::journal::timestamp(),
            json!({"sessionId":session,"trackId":track,"operationId":operation}).to_string()
        ],
    )?;
    Ok(())
}
