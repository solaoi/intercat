//! Reversible timeline edits share the journal's transaction and operation log.
use crate::{Result, redirect::Redirect};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Frame {
    input: String,
    text: String,
    target: String,
    selected: Option<String>,
    state: String,
    revision: i64,
    decision: Option<String>,
    reason: Option<String>,
    provisional: Option<String>,
    question: Option<String>,
    question_revision: Option<i64>,
    redirect: Option<Redirect>,
    start_condition: crate::start_condition::Context,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    session: String,
    label: String,
    at: i64,
    before: Frame,
    after: Frame,
    position: String,
    expected_revision: i64,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub action: Action,
}
#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Action {
    Undo {
        #[serde(rename = "entryId")]
        entry: String,
    },
    Redo {
        #[serde(rename = "entryId")]
        entry: String,
    },
    Restore {
        #[serde(rename = "utteranceId")]
        input: String,
        revision: i64,
    },
    RestoreDraft {
        #[serde(rename = "utteranceId")]
        input: String,
        revision: i64,
    },
}
pub(crate) fn session(db: &Connection) -> Result<String> {
    Ok(db
        .query_row(
            "SELECT json_extract(value,'$.id') FROM settings WHERE key='session'",
            [],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_else(|| "home".into()))
}
pub(crate) fn capture(db: &Connection, input: &str) -> Result<Frame> {
    let mut frame = db.query_row("SELECT text,target,selected,state,revision,decision,reason,provisional,question_id,question_revision FROM utterances WHERE id=?1", [input], |row| Ok(Frame {
        input: input.into(), text: row.get(0)?, target: row.get(1)?, selected: row.get(2)?, state: row.get(3)?, revision: row.get(4)?, decision: row.get(5)?, reason: row.get(6)?, provisional: row.get(7)?, question: row.get(8)?, question_revision: row.get(9)?, redirect: None, start_condition: Default::default(),
    }))?;
    frame.redirect = crate::redirect::read(db, input)?;
    frame.start_condition = crate::start_condition::read(db, input)?;
    Ok(frame)
}
enum Edit {
    Request(Box<Entry>),
    Plan(Box<crate::plan_history::Entry>),
}
fn head(db: &Connection, position: &str) -> Result<Option<(String, Edit)>> {
    let order = if position == "applied" { "DESC" } else { "ASC" };
    let query = format!(
        "SELECT id,kind,payload FROM operations WHERE kind IN ('timeline.edit','plan.edit') AND json_extract(payload,'$.session')=?1 AND (json_extract(payload,'$.position')=?2 OR (?2='applied' AND json_extract(payload,'$.position')='conflicted')) ORDER BY rowid {order} LIMIT 1"
    );
    let row: Option<(String, String, String)> = db
        .query_row(&query, params![session(db)?, position], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .optional()?;
    row.map(|(id, kind, value)| {
        Ok((
            id,
            if kind == "plan.edit" {
                Edit::Plan(serde_json::from_str(&value)?)
            } else {
                Edit::Request(serde_json::from_str(&value)?)
            },
        ))
    })
    .transpose()
}
fn save(db: &Connection, id: &str, entry: &Entry) -> Result<()> {
    db.execute(
        "UPDATE operations SET payload=?2 WHERE id=?1 AND kind='timeline.edit'",
        params![id, serde_json::to_string(entry)?],
    )?;
    Ok(())
}
pub(crate) fn abandon_redo(db: &Connection) -> Result<()> {
    db.execute("UPDATE operations SET payload=json_set(payload,'$.position','abandoned') WHERE kind IN ('timeline.edit','plan.edit') AND json_extract(payload,'$.session')=?1 AND json_extract(payload,'$.position')='undone'", [session(db)?])?;
    Ok(())
}
pub(crate) fn advance_revision(db: &Connection, input: &str, before: i64) -> Result<()> {
    db.execute("UPDATE operations SET payload=json_set(payload,'$.expectedRevision',?3) WHERE kind='timeline.edit' AND json_extract(payload,'$.session')=?1 AND json_extract(payload,'$.before.input')=?2 AND json_extract(payload,'$.expectedRevision')=?4 AND json_extract(payload,'$.position') IN ('applied','undone')", params![session(db)?, input, before + 1, before])?;
    Ok(())
}
pub(crate) fn record(db: &Connection, before: Frame, label: &str) -> Result<()> {
    abandon_redo(db)?;
    let after = capture(db, &before.input)?;
    db.execute("UPDATE operations SET payload=json_set(payload,'$.position','conflicted') WHERE kind='timeline.edit' AND json_extract(payload,'$.session')=?1 AND json_extract(payload,'$.before.input')=?2 AND json_extract(payload,'$.position')='applied' AND json_extract(payload,'$.expectedRevision')<>?3", params![session(db)?, before.input, before.revision])?;
    db.execute("UPDATE operations SET payload=json_set(payload,'$.expectedRevision',?3) WHERE kind='timeline.edit' AND json_extract(payload,'$.session')=?1 AND json_extract(payload,'$.before.input')=?2 AND json_extract(payload,'$.position')='applied'", params![session(db)?, before.input, after.revision])?;
    let entry = Entry {
        session: session(db)?,
        label: label.into(),
        at: crate::journal::timestamp(),
        before,
        expected_revision: after.revision,
        after,
        position: "applied".into(),
    };
    db.execute(
        "INSERT INTO operations VALUES(?1,'timeline.edit',?2,'complete')",
        params![Uuid::now_v7().to_string(), serde_json::to_string(&entry)?],
    )?;
    Ok(())
}
fn blocked(db: &Connection, entry: &Entry) -> Result<Option<String>> {
    if !db.query_row(
        "SELECT EXISTS(SELECT 1 FROM utterances WHERE id=?1)",
        [&entry.before.input],
        |row| row.get::<_, bool>(0),
    )? {
        return Ok(Some("This request is no longer available.".into()));
    }
    let current = capture(db, &entry.before.input)?;
    if ["starting", "sent", "landed"].contains(&current.state.as_str())
        || crate::redirect::interrupt_started(db, &current.input)?
        || entry.before.redirect.as_ref().is_some_and(|redirect| {
            matches!(
                redirect.phase,
                crate::redirect::Phase::Interrupting
                    | crate::redirect::Phase::Sent
                    | crate::redirect::Phase::Done
            )
        })
    {
        return Ok(Some(format!("can’t undo: {} has started", current.text)));
    }
    if crate::edit_hold::active(db, &current.input)? {
        return Ok(Some("Finish editing this request first.".into()));
    }
    if entry.position == "conflicted" || current.revision != entry.expected_revision {
        return Ok(Some("This request changed after the timeline edit.".into()));
    }
    Ok(None)
}
pub(crate) fn snapshot(db: &Connection) -> Result<Value> {
    let describe = |(id, edit): (String, Edit)| -> Result<Value> {
        match edit {
            Edit::Request(entry) => Ok(
                json!({"entryId":id,"label":entry.label,"at":entry.at,"text":entry.before.text,"blocked":blocked(db,&entry)?}),
            ),
            Edit::Plan(entry) => crate::plan_history::describe(db, id, *entry),
        }
    };
    let undo = head(db, "applied")?.map(describe).transpose()?;
    let redo = head(db, "undone")?.map(describe).transpose()?;
    let taken_back = db.prepare("SELECT id,text,revision FROM utterances WHERE state='cut' AND NOT EXISTS(SELECT 1 FROM plan_inputs WHERE input=utterances.id) AND NOT EXISTS(SELECT 1 FROM review_fix_inputs WHERE input=utterances.id) ORDER BY created_at DESC,id DESC")?
        .query_map([], |row| Ok(json!({"id":row.get::<_,String>(0)?,"text":row.get::<_,String>(1)?,"revision":row.get::<_,i64>(2)?})))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({"undo":undo,"redo":redo,"takenBack":taken_back}))
}
fn restore_frame(tx: &Transaction<'_>, frame: &Frame, revision: i64, held: bool) -> Result<()> {
    if (held || frame.state != "cut")
        && frame.redirect.is_none()
        && frame.decision.as_ref().is_some_and(|decision| {
            serde_json::from_str::<Value>(decision).is_ok_and(|value| value["kind"] == "trim")
        })
    {
        return Err(
            "The original stop boundary is unavailable. Restore this text as a new draft.".into(),
        );
    }
    let mut state = frame.state.as_str();
    let mut reason = frame.reason.clone();
    if state == "reading" {
        state = "staged";
    }
    if held || state != "cut" && crate::dispatch_control::paused(tx)? {
        state = "held";
        reason = Some("Restored by you".into());
    }
    if state == "cut" {
        crate::inference_store::take_back(tx, &frame.input)?;
    }
    tx.execute("UPDATE utterances SET text=?2,target=?3,selected=?4,state=?5,revision=?6,decision=?7,reason=?8,provisional=?9,question_id=?10,question_revision=?11 WHERE id=?1", params![frame.input,frame.text,frame.target,frame.selected,state,revision,frame.decision,reason,frame.provisional,frame.question,frame.question_revision])?;
    crate::fresh_words::restore(tx, &frame.input)?;
    crate::review_fix_store::history(tx, &frame.input, state)?;
    crate::start_condition::save(tx, &frame.input, &frame.start_condition)?;
    crate::request_graph::validate(tx)?;
    crate::redirect::forget(tx, &frame.input)?;
    if state == "cut" {
        crate::report_store::input_state(tx, &frame.input, "cut")?;
    } else {
        crate::report_store::restore_reply(tx, &frame.input)?;
        crate::plan_feedback::restore_reply(tx, &frame.input)?;
        if let Some(redirect) = &frame.redirect {
            crate::redirect::restore_history(tx, redirect.clone(), revision, state == "held")?;
        }
    }
    Ok(())
}
pub(crate) fn change(db: &mut Connection, request: Request) -> Result<()> {
    if request.op_id.get_version_num() != 7 {
        return Err("Invalid history operation.".into());
    }
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let payload = serde_json::to_string(&request.action)?;
    if let Some((kind, saved)) = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if kind != "timeline.history" || saved != payload {
            return Err("History operation ID reused.".into());
        }
        return Ok(());
    }
    match &request.action {
        Action::Restore { input, revision } | Action::RestoreDraft { input, revision } => {
            crate::session_retirement::ensure_input(&tx, input)?;
            let before = capture(&tx, input)?;
            if before.state != "cut" || before.revision != *revision {
                return Err("This request is no longer taken back.".into());
            }
            let previous: Option<String> = tx.query_row("SELECT payload FROM operations WHERE kind='timeline.edit' AND json_extract(payload,'$.session')=?1 AND json_extract(payload,'$.before.input')=?2 AND json_extract(payload,'$.label')='Take back' ORDER BY rowid DESC LIMIT 1", params![session(&tx)?, input], |row| row.get(0)).optional()?;
            let mut source = previous
                .map(|value| serde_json::from_str::<Entry>(&value).map(|entry| entry.before))
                .transpose()?
                .unwrap_or_else(|| before.clone());
            if matches!(request.action, Action::RestoreDraft { .. }) {
                source.state = "held".into();
                source.decision = None;
                source.redirect = None;
            }
            restore_frame(&tx, &source, revision + 1, true)?;
            record(&tx, before, "Restore")?;
        }
        Action::Undo { entry } | Action::Redo { entry } => {
            let redo = matches!(request.action, Action::Redo { .. });
            let Some((id, current)) =
                head(&tx, if redo { "undone" } else { "applied" })?.filter(|(id, _)| id == entry)
            else {
                return Err("The edit history changed. Refresh before trying again.".into());
            };
            match current {
                Edit::Plan(entry) => crate::plan_history::restore(&tx, &id, *entry, redo)?,
                Edit::Request(mut current) => {
                    crate::session_retirement::ensure_input(&tx, &current.before.input)?;
                    if let Some(reason) = blocked(&tx, &current)? {
                        return Err(reason.into());
                    }
                    let frame = if redo {
                        &current.after
                    } else {
                        &current.before
                    };
                    let input = frame.input.clone();
                    let revision = current.expected_revision + 1;
                    restore_frame(&tx, frame, revision, false)?;
                    let revision = capture(&tx, &input)?.revision;
                    current.position = if redo { "applied" } else { "undone" }.into();
                    current.expected_revision = revision;
                    current.at = crate::journal::timestamp();
                    save(&tx, &id, &current)?;
                    tx.execute("UPDATE operations SET payload=json_set(payload,'$.expectedRevision',?3) WHERE kind='timeline.edit' AND json_extract(payload,'$.session')=?1 AND json_extract(payload,'$.before.input')=?2 AND json_extract(payload,'$.position') IN ('applied','undone')", params![session(&tx)?, input, revision])?;
                }
            }
        }
    }
    tx.execute(
        "INSERT INTO operations VALUES(?1,'timeline.history',?2,'complete')",
        params![request.op_id.to_string(), payload],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'timeline.history',?2)",
        params![crate::journal::timestamp(), payload],
    )?;
    tx.commit()?;
    Ok(())
}
