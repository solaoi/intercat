//! Reversible plan edits in the session's shared timeline history.
use crate::{
    Result,
    plans::{Item, ItemState, Plan, State},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LockChange {
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Entry {
    pub session: String,
    pub label: String,
    pub at: i64,
    pub plan: Uuid,
    pub before: Vec<Item>,
    pub after: Vec<Item>,
    pub before_state: State,
    pub after_state: State,
    pub lock_change: Option<LockChange>,
    pub position: String,
    pub expected_revision: u64,
}

pub(crate) fn record(db: &Connection, before: &Plan, after: &Plan, label: &str) -> Result<()> {
    if before.state == after.state
        && before.locked_track == after.locked_track
        && before.items.iter().all(|item| {
            after
                .items
                .iter()
                .find(|next| next.plan_item_id == item.plan_item_id)
                .is_some_and(|next| {
                    crate::plan_edit::configuration(item) == crate::plan_edit::configuration(next)
                })
        })
    {
        return Ok(());
    }
    crate::timeline_history::abandon_redo(db)?;
    db.execute("UPDATE operations SET payload=json_set(payload,'$.position','conflicted') WHERE kind='plan.edit' AND json_extract(payload,'$.session')=?1 AND json_extract(payload,'$.plan')=?2 AND json_extract(payload,'$.position')='applied' AND json_extract(payload,'$.expectedRevision')<>?3",params![crate::timeline_history::session(db)?,before.id.to_string(),i64::try_from(before.rev)?])?;
    advance_revision(db, before.id, before.rev, after.rev)?;
    let mut old = Vec::new();
    let mut new = Vec::new();
    for item in &before.items {
        let next = after
            .items
            .iter()
            .find(|next| next.plan_item_id == item.plan_item_id)
            .ok_or("A plan edit removed an item identity.")?;
        if crate::plan_edit::configuration(item) != crate::plan_edit::configuration(next)
            || before.state != after.state && item.state == ItemState::Planned
        {
            old.push(crate::plan_edit::configuration(item));
            new.push(crate::plan_edit::configuration(next));
        }
    }
    let entry = Entry {
        session: crate::timeline_history::session(db)?,
        label: label.into(),
        at: crate::journal::timestamp(),
        plan: before.id,
        before: old,
        after: new,
        before_state: before.state.clone(),
        after_state: after.state.clone(),
        lock_change: (before.locked_track != after.locked_track).then(|| LockChange {
            before: before.locked_track.clone(),
            after: after.locked_track.clone(),
        }),
        position: "applied".into(),
        expected_revision: after.rev,
    };
    db.execute(
        "INSERT INTO operations VALUES(?1,'plan.edit',?2,'complete')",
        params![Uuid::now_v7().to_string(), serde_json::to_string(&entry)?],
    )?;
    Ok(())
}

pub(crate) fn advance_revision(db: &Connection, plan: Uuid, old: u64, new: u64) -> Result<()> {
    db.execute("UPDATE operations SET payload=json_set(payload,'$.expectedRevision',?4) WHERE kind='plan.edit' AND json_extract(payload,'$.session')=?1 AND json_extract(payload,'$.plan')=?2 AND json_extract(payload,'$.position') IN ('applied','undone') AND json_extract(payload,'$.expectedRevision')=?3",params![crate::timeline_history::session(db)?,plan.to_string(),i64::try_from(old)?,i64::try_from(new)?])?;
    Ok(())
}

pub(crate) fn assign_tracks(
    db: &Connection,
    plan: Uuid,
    assigned: &std::collections::BTreeMap<String, Uuid>,
) -> Result<()> {
    let mut query=db.prepare("SELECT id,payload FROM operations WHERE kind='plan.edit' AND json_extract(payload,'$.plan')=?1 AND json_extract(payload,'$.position') IN ('applied','undone')")?;
    let records = query
        .query_map([plan.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, raw) in records {
        let mut entry: Entry = serde_json::from_str(&raw)?;
        for item in entry.before.iter_mut().chain(&mut entry.after) {
            if let Some(track) = assigned.get(&item.track_ref) {
                item.track_ref = track.to_string();
            }
        }
        db.execute(
            "UPDATE operations SET payload=?2 WHERE id=?1",
            params![id, serde_json::to_string(&entry)?],
        )?;
    }
    Ok(())
}

/// Timeline configuration names a logical destination. On restore it follows
/// a continuation; original turn receipts keep their recorded SDK owner.
fn route(db: &Connection, entry: &mut Entry) -> Result<()> {
    for item in entry.before.iter_mut().chain(&mut entry.after) {
        if !item.track_ref.starts_with("new:") {
            item.track_ref = crate::continuation::current(db, &item.track_ref)?;
        }
    }
    if let Some(change) = &mut entry.lock_change {
        for lock in [&mut change.before, &mut change.after]
            .into_iter()
            .flatten()
        {
            *lock = crate::continuation::current(db, lock)?;
        }
    }
    Ok(())
}
fn blocked(db: &Connection, entry: &Entry) -> Result<Option<String>> {
    let plan = match crate::plan_store::read(db, entry.plan) {
        Ok(plan) => plan,
        Err(_) => return Ok(Some("This plan is no longer available.".into())),
    };
    if plan.state == State::SingleJob {
        return Ok(Some(
            "This request is already being handled as one job.".into(),
        ));
    }
    if plan.rewrite.is_none() && plan.generation.state != crate::plans::GenerationState::Ready {
        return Ok(Some(
            "Wait for a ready plan before undoing this change.".into(),
        ));
    }
    if let Some(change) = &entry.lock_change {
        let expected = if entry.position == "undone" {
            &change.before
        } else {
            &change.after
        };
        if &plan.locked_track != expected {
            return Ok(Some(
                "The plan's destination changed after this move.".into(),
            ));
        }
        if let Err(error) = crate::plan_store::require_lock_source(db, &plan) {
            return Ok(Some(error.to_string()));
        }
    }
    let expected = if entry.position == "undone" {
        &entry.before
    } else {
        &entry.after
    };
    for item in expected {
        let Some(current) = plan
            .items
            .iter()
            .find(|current| current.plan_item_id == item.plan_item_id)
        else {
            return Ok(Some("This plan item is no longer available.".into()));
        };
        if let Err(reason) = crate::plan_edit::editable(db, current) {
            return Ok(Some(
                reason.to_string().replace("can’t change:", "can’t undo:"),
            ));
        }
        if crate::plan_edit::configuration(current) != *item {
            return Ok(Some(
                "This plan item changed after the timeline edit.".into(),
            ));
        }
    }
    if entry.position == "conflicted" || plan.rev != entry.expected_revision {
        return Ok(Some("The plan changed after the timeline edit.".into()));
    }
    Ok(None)
}

pub(crate) fn describe(db: &Connection, id: String, mut entry: Entry) -> Result<Value> {
    route(db, &mut entry)?;
    let text = entry
        .before
        .first()
        .map(|item| item.title.as_str())
        .unwrap_or(&entry.label);
    Ok(
        json!({"entryId":id,"label":entry.label,"at":entry.at,"text":text,"blocked":blocked(db,&entry)?}),
    )
}

/// Restore one cut without moving past unrelated entries in the shared stack.
pub(crate) fn undo_cut(db: &Connection, before: &Plan, plan: &mut Plan, item: Uuid) -> Result<()> {
    let current = plan
        .items
        .iter()
        .find(|value| value.plan_item_id == item)
        .ok_or("This plan item is no longer available.")?;
    crate::plan_edit::unstarted(db, current)?;
    if current.state != ItemState::Cut {
        return Err("This plan item is not cut.".into());
    }
    let saved: Option<(String, String)> = db.query_row(
        "SELECT id,payload FROM operations WHERE kind='plan.edit'
         AND json_extract(payload,'$.session')=?1 AND json_extract(payload,'$.plan')=?2
         AND json_extract(payload,'$.position') IN ('applied','conflicted')
         AND EXISTS(SELECT 1 FROM json_each(payload,'$.after') AS item
           WHERE json_extract(item.value,'$.planItemId')=?3 AND json_extract(item.value,'$.state')='cut')
         ORDER BY rowid DESC LIMIT 1",
        params![crate::timeline_history::session(db)?, plan.id.to_string(), item.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    let (id, raw) = saved.ok_or("The original cut is no longer available in this session.")?;
    let mut entry: Entry = serde_json::from_str(&raw)?;
    route(db, &mut entry)?;
    let removed = entry
        .after
        .iter()
        .find(|value| value.plan_item_id == item)
        .ok_or("The original cut no longer identifies this item.")?;
    if crate::plan_edit::configuration(current) != *removed {
        return Err("The cut item changed. Review it before restoring.".into());
    }
    let restored = entry
        .before
        .iter_mut()
        .find(|value| value.plan_item_id == item && value.state == ItemState::Planned)
        .ok_or("The original unstarted item is unavailable.")?;
    // A new explicit start is required even if the cut cancelled Start anyway.
    restored.start_requested = false;
    let current = plan
        .items
        .iter_mut()
        .find(|value| value.plan_item_id == item)
        .unwrap();
    crate::plan_edit::invalidate_input(db, current)?;
    *current = restored.clone();
    plan.state = State::Held;
    crate::plan_edit::finish(db, before, plan)?;
    entry.position = "undone".into();
    entry.expected_revision = plan.rev;
    entry.at = crate::journal::timestamp();
    db.execute(
        "UPDATE operations SET payload=?2 WHERE id=?1 AND kind='plan.edit'",
        params![id, serde_json::to_string(&entry)?],
    )?;
    Ok(())
}

pub(crate) fn restore(db: &Connection, id: &str, mut entry: Entry, redo: bool) -> Result<()> {
    route(db, &mut entry)?;
    if let Some(reason) = blocked(db, &entry)? {
        return Err(reason.into());
    }
    let mut plan = crate::plan_store::read(db, entry.plan)?;
    let old = plan.rev;
    for restored in if redo { &entry.after } else { &entry.before } {
        let current = plan
            .items
            .iter_mut()
            .find(|item| item.plan_item_id == restored.plan_item_id)
            .ok_or("This plan item is no longer available.")?;
        crate::plan_edit::invalidate_input(db, current)?;
        *current = restored.clone();
    }
    if entry.before_state != entry.after_state || plan.state == State::Done {
        plan.state = if redo {
            entry.after_state.clone()
        } else {
            entry.before_state.clone()
        };
    }
    if let Some(change) = &entry.lock_change {
        crate::plan_store::change_lock(
            db,
            &mut plan,
            if redo {
                change.after.clone()
            } else {
                change.before.clone()
            },
        )?;
    }
    let context = crate::plan_store::context(db, plan.id)?;
    crate::plans::validate_edit(&mut plan, &context).map_err(|errors| errors.join("\n"))?;
    plan.next_revision()?;
    if plan.rewrite.is_some() {
        crate::plan_rewrite::changed(&mut plan);
    }
    crate::plan_store::save(db, &plan, "plan.history")?;
    advance_revision(db, entry.plan, old, plan.rev)?;
    entry.expected_revision = plan.rev;
    entry.position = if redo { "applied" } else { "undone" }.into();
    entry.at = crate::journal::timestamp();
    db.execute(
        "UPDATE operations SET payload=?2 WHERE id=?1 AND kind='plan.edit'",
        params![id, serde_json::to_string(&entry)?],
    )?;
    Ok(())
}
