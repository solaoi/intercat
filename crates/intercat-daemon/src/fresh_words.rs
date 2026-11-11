//! Move only future instructions. Historical receipts and explicit app commands
//! retain their source; ordinary later work follows the live continuation.
use crate::Result;
use rusqlite::{Connection, params};
use serde_json::Value;

struct Input {
    id: String,
    state: String,
    decision: Option<String>,
    question: Option<String>,
    revision: i64,
}
fn row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Input> {
    Ok(Input {
        id: row.get(0)?,
        state: row.get(1)?,
        decision: row.get(2)?,
        question: row.get(3)?,
        revision: row.get(4)?,
    })
}
fn input(db: &Connection, id: &str) -> Result<Input> {
    Ok(db.query_row(
        "SELECT id,state,decision,question_id,revision FROM utterances WHERE id=?1",
        [id],
        row,
    )?)
}
pub(crate) fn transfer(db: &Connection, source: &str, target: &str) -> Result<()> {
    // A single-job plan and its source request share one destination change.
    // Move plans first so their source is not collected again as ordinary work.
    for mut plan in crate::plan_store::list(db)? {
        if route_plan(db, &mut plan)? {
            crate::plan_store::save(db, &plan, "plan.continued")?;
        }
    }
    let mut query=db.prepare("SELECT id,state,decision,question_id,revision FROM utterances WHERE state IN ('staged','reading','placed','asked','held','orphaned','redirecting') AND (json_extract(decision,'$.trackId')=?1 OR provisional=?1 OR json_extract(target,'$.trackId')=?1) ORDER BY created_at,id")?;
    let rows = query
        .query_map([source], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for input in rows {
        let revision = input
            .revision
            .checked_add(1)
            .ok_or("Request revision overflow.")?;
        move_input(db, source, target, input, revision)?;
    }
    Ok(())
}
fn move_input(
    db: &Connection,
    source: &str,
    target: &str,
    input: Input,
    revision: i64,
) -> Result<bool> {
    let Input {
        id,
        state,
        decision,
        question,
        revision: previous,
    } = input;
    let mut decision: Option<Value> = decision.map(|raw| serde_json::from_str(&raw)).transpose()?;
    if decision
        .as_ref()
        .is_some_and(|value| matches!(value["kind"].as_str(), Some("command" | "park" | "ask")))
    {
        return Ok(false);
    }
    if crate::review_fix_store::for_input(db, &id)?.is_some() {
        return Ok(false);
    }
    if crate::edit_hold::active(db, &id)? {
        return Err(crate::fresh::Waiting(
            "Finish editing the queued request before transferring this workspace.",
        )
        .into());
    }
    if let Some(value) = &mut decision {
        value["trackId"] = target.into();
    }
    if let Some(question) = question {
        let raw: String = db.query_row(
            "SELECT record FROM report_questions WHERE id=?1",
            [&question],
            |row| row.get(0),
        )?;
        let mut record: crate::reports::Question = serde_json::from_str(&raw)?;
        if record.track_id != source {
            return Err("The earlier question reply changed before handoff.".into());
        }
        let (text, revision): (String, i64) = db.query_row(
            "SELECT text,question_revision FROM utterances WHERE id=?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let original = crate::report_store::original_question(db, &question, revision)?
            .ok_or("The original question revision is unavailable.")?;
        if original.track_id != source || original.session_id != record.session_id {
            return Err("The original question has another source.".into());
        }
        db.execute(
            "UPDATE utterances SET text=?2,question_id=NULL,question_revision=NULL WHERE id=?1",
            params![
                id,
                format!(
                    "Reply to the earlier question at {} (revision {}): {}\n\n{}",
                    original.id, revision, original.text, text
                )
            ],
        )?;
        if record.reply_utterance_id.as_deref() == Some(&id) {
            record.state = "answered-elsewhere".into();
            record.revision += 1;
            db.execute(
                "UPDATE report_questions SET state='answered-elsewhere',record=?2 WHERE id=?1",
                params![question, serde_json::to_string(&record)?],
            )?;
        }
    }
    if revision != previous {
        crate::timeline_history::advance_revision(db, &id, previous)?;
    }
    db.execute("UPDATE utterances SET state=?2,revision=?7,decision=?3,provisional=?4,selected=CASE WHEN selected=?5 THEN ?4 ELSE selected END,target=CASE WHEN json_extract(target,'$.mode')='lock' AND json_extract(target,'$.trackId')=?5 THEN ?6 ELSE target END WHERE id=?1",params![id,if state=="reading"{"staged"}else{&state},decision.map(|value|value.to_string()),target,source,serde_json::to_string(&crate::decision::Target::Lock(target.into()))?,revision])?;
    Ok(true)
}
/// Replies to ended conversations become explicit ordinary follow-ups. No
/// reply is sent to the old SDK question/session; its original address remains.
pub(crate) fn route_reply(db: &Connection, input: &str, source: &str) -> Result<()> {
    let target = crate::continuation::current(db, source)?;
    if target == source {
        return Ok(());
    }
    let input = self::input(db, input)?;
    let revision = input
        .revision
        .checked_add(1)
        .ok_or("Request revision overflow.")?;
    move_input(db, source, &target, input, revision).map(|_| ())
}

/// Restoring text owns the new history revision. Route its saved destination
/// without another revision change, so Undo/Redo keep their exact receipts.
pub(crate) fn restore(db: &Connection, id: &str) -> Result<()> {
    let input = input(db, id)?;
    if input.state == "cut" {
        return Ok(());
    }
    let revision = input.revision;
    let source: Option<String> = db.query_row(
        "SELECT coalesce(json_extract(decision,'$.trackId'),json_extract(target,'$.trackId'),provisional) FROM utterances WHERE id=?1",
        [id],|row| row.get(0),
    )?;
    if let Some(source) = source {
        let target = crate::continuation::current(db, &source)?;
        if target != source {
            if input.decision.as_ref().is_some_and(|raw| {
                serde_json::from_str::<Value>(raw).is_ok_and(|value| value["kind"] == "trim")
            }) {
                return Err(
                    "The original stop boundary is unavailable. Restore this text as a new draft."
                        .into(),
                );
            }
            move_input(db, &source, &target, input, revision)?;
        }
    }
    // A history operation owns a new source version even when its saved frame
    // already names the current writer. Keep the plan's reference in step.
    if let Some(mut plan) = crate::plan_store::source(db, id)?
        && plan.state == crate::plans::State::SingleJob
        && sync_single_request(db, &mut plan, revision)?
    {
        crate::plan_store::save(db, &plan, "plan.continued")?;
    }
    Ok(())
}

fn move_single_request(
    db: &Connection,
    plan: &mut crate::plans::Plan,
    source: &str,
    target: &str,
    input: Input,
    revision: i64,
) -> Result<bool> {
    if !move_input(db, source, target, input, revision)? {
        return Ok(false);
    }
    sync_single_request(db, plan, revision)
}

fn sync_single_request(
    db: &Connection,
    plan: &mut crate::plans::Plan,
    revision: i64,
) -> Result<bool> {
    let raw: String = db.query_row(
        "SELECT target FROM utterances WHERE id=?1",
        [&plan.source_id],
        |row| row.get(0),
    )?;
    let locked_track = match serde_json::from_str::<crate::decision::Target>(&raw)? {
        crate::decision::Target::Lock(track) => Some(track),
        _ => None,
    };
    if plan.source_revision == revision && plan.locked_track == locked_track {
        return Ok(false);
    }
    plan.locked_track = locked_track;
    plan.source_revision = revision;
    let before = plan.rev;
    plan.next_revision()?;
    crate::plan_history::advance_revision(db, plan.id, before, plan.rev)?;
    Ok(true)
}
pub(crate) fn route(db: &Connection, decision: &mut crate::decision::Decision) -> Result<()> {
    if matches!(
        decision.kind,
        crate::decision::Kind::Extend
            | crate::decision::Kind::Fork
            | crate::decision::Kind::Schedule
            | crate::decision::Kind::Trim
            | crate::decision::Kind::Plan
    ) && let Some(track) = &decision.track_id
    {
        decision.track_id = Some(crate::continuation::current(db, track)?);
    }
    Ok(())
}

/// A failed/cut item may be retried/restored; its historical utterance remains
/// the old receipt, while the next attempt uses the current writable segment.
pub(crate) fn route_plan(db: &Connection, plan: &mut crate::plans::Plan) -> Result<bool> {
    if plan.state == crate::plans::State::SingleJob {
        let input = input(db, &plan.source_id)?;
        if !matches!(
            input.state.as_str(),
            "staged" | "reading" | "placed" | "asked" | "held" | "orphaned" | "redirecting"
        ) {
            return Ok(false);
        }
        let source: Option<String> = db.query_row(
            "SELECT coalesce(json_extract(decision,'$.trackId'),json_extract(target,'$.trackId'),provisional) FROM utterances WHERE id=?1",
            [&plan.source_id],|row| row.get(0),
        )?;
        let Some(source) = source else {
            return Ok(false);
        };
        let target = crate::continuation::current(db, &source)?;
        if source == target {
            return Ok(false);
        }
        let revision = input
            .revision
            .checked_add(1)
            .ok_or("Request revision overflow.")?;
        return move_single_request(db, plan, &source, &target, input, revision);
    }
    let editing = plan.rewrite.is_some();
    let mut changed = false;
    for item in &mut plan.items {
        if !matches!(
            item.state,
            crate::plans::ItemState::Running | crate::plans::ItemState::Succeeded
        ) && !item.track_ref.starts_with("new:")
        {
            let current = crate::continuation::current(db, &item.track_ref)?;
            if current != item.track_ref {
                if editing {
                    return Err(crate::fresh::Waiting(
                        "Finish editing the plan item before transferring this workspace.",
                    )
                    .into());
                }
                item.track_ref = current;
                changed = true;
            }
        }
    }
    let old_lock = plan.locked_track.clone();
    if let Some(lock) = &old_lock {
        let current = crate::continuation::current(db, lock)?;
        if current != *lock
            && (changed
                || plan.items.is_empty()
                || plan
                    .items
                    .iter()
                    .any(|item| item.state != crate::plans::ItemState::Succeeded))
        {
            if editing {
                return Err(crate::fresh::Waiting(
                    "Finish editing the plan item before transferring this workspace.",
                )
                .into());
            }
            let actual: (i64, String) = db.query_row(
                "SELECT revision,target FROM utterances WHERE id=?1",
                [&plan.source_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if actual.0 != plan.source_revision
                || serde_json::from_str::<Value>(&actual.1)?
                    != serde_json::to_value(crate::decision::Target::Lock(lock.clone()))?
            {
                return Err("The original plan request changed before its continuation.".into());
            }
            plan.source_revision = plan
                .source_revision
                .checked_add(1)
                .ok_or("Request revision overflow.")?;
            move_input(
                db,
                lock,
                &current,
                input(db, &plan.source_id)?,
                plan.source_revision,
            )?;
            plan.locked_track = Some(current);
            changed = true;
        }
    }
    if changed {
        let old = plan.rev;
        plan.rev = plan.rev.checked_add(1).ok_or("Plan revision overflow.")?;
        crate::plan_history::advance_revision(db, plan.id, old, plan.rev)?;
        // A running Planner used an earlier destination. Invalidate its result
        // and regenerate against the saved current destination.
        if matches!(
            plan.generation.state,
            crate::plans::GenerationState::Queued | crate::plans::GenerationState::Running
        ) {
            plan.generation.id = uuid::Uuid::now_v7();
            plan.generation.state = crate::plans::GenerationState::Queued;
        }
    }
    Ok(changed)
}
