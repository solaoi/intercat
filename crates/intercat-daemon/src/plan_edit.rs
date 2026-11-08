//! Atomic changes to unstarted plan items; validation is shared with generation.
use crate::{
    Result,
    plans::{Item, ItemState, Plan},
};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    Earlier,
    Later,
}

pub(crate) fn unstarted(db: &Connection, item: &Item) -> Result<()> {
    if !matches!(item.state, ItemState::Planned | ItemState::Cut) {
        return Err(format!("can’t change: {} has started", item.title).into());
    }
    if let Some(input) = item.utterance_id {
        let state: Option<String> = db
            .query_row(
                "SELECT state FROM utterances WHERE id=?1",
                [input.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if state
            .as_deref()
            .is_some_and(|state| matches!(state, "starting" | "sent" | "landed"))
        {
            return Err(format!("can’t change: {} has started", item.title).into());
        }
    }
    Ok(())
}

pub(crate) fn editable(db: &Connection, item: &Item) -> Result<()> {
    unstarted(db, item)?;
    let preparing_files: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM operations o JOIN plan_inputs p ON p.input=json_extract(o.payload,'$.dependencies.input') WHERE o.kind IN ('workspace.apply-inheritance','workspace.inherit') AND o.state NOT IN ('complete','superseded') AND p.item=?1)",[item.plan_item_id.to_string()],|row|row.get(0))?;
    if preparing_files {
        return Err("This item's inherited files have started preparation.".into());
    }
    let prepared: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM operations o JOIN plan_inputs p ON p.input=json_extract(o.payload,'$.dependencies.input') WHERE o.kind IN ('workspace.apply-inheritance','workspace.inherit') AND o.state='complete' AND p.item=?1 AND json_extract(o.payload,'$.dependencies.track')=?2)",rusqlite::params![item.plan_item_id.to_string(),item.track_ref],|row|row.get(0))?;
    if prepared && !crate::plan_files::has_origin(db, item.plan_item_id, &item.track_ref)? {
        return Err(
            "Wait for the prepared workspace to be recorded before editing this item.".into(),
        );
    }
    Ok(())
}

pub(crate) fn start_without(
    db: &Connection,
    plan: &mut Plan,
    id: Uuid,
    dependency: &str,
) -> Result<()> {
    if crate::dispatch_control::paused(db)?
        || plan.rewrite.is_some()
        || plan.generation.state != crate::plans::GenerationState::Ready
    {
        return Err("Finish editing and resume new work before starting this item.".into());
    }
    let index = plan
        .items
        .iter()
        .position(|item| item.plan_item_id == id)
        .ok_or("The plan item is unavailable.")?;
    let item = &plan.items[index];
    if item.state != ItemState::Planned {
        return Err("Only an unstarted item can ignore a dependency.".into());
    }
    editable(db, item)?;
    let explicit = item.after.iter().any(|after| after.id == dependency);
    let all = crate::plan_store::list(db)?;
    let predecessor = crate::plan_execution::predecessors(plan, item, &all)
        .into_iter()
        .find(|parent| parent.plan_item_id.to_string() == dependency)
        .map(|parent| parent.plan_item_id);
    if !explicit && predecessor.is_none() {
        return Err("The selected dependency is no longer required by this item.".into());
    }
    if crate::plan_dependencies::read(db, &all, dependency)?
        .is_some_and(|parent| !matches!(parent.state, ItemState::Failed | ItemState::Cut))
    {
        return Err(
            "The dependency changed. Review its current result before starting without it.".into(),
        );
    }
    plan.items[index]
        .after
        .retain(|after| after.id != dependency);
    if let Some(predecessor) = predecessor {
        plan.items[index].skip_order.push(predecessor);
    }
    Ok(())
}

pub(crate) fn invalidate_input(db: &Connection, item: &mut Item) -> Result<()> {
    unstarted(db, item)?;
    item.start_requested = false;
    if let Some(input) = item.utterance_id.take() {
        db.execute("UPDATE utterances SET state='cut',revision=revision+1,reason='Plan item changed' WHERE id=?1",[input.to_string()])?;
    }
    item.detail = None;
    Ok(())
}

pub(crate) fn move_order(
    db: &Connection,
    plan: &mut Plan,
    id: Uuid,
    direction: Direction,
) -> Result<()> {
    let index = plan
        .items
        .iter()
        .position(|item| item.plan_item_id == id)
        .ok_or("The plan item is unavailable.")?;
    editable(db, &plan.items[index])?;
    if plan.items[index].state == ItemState::Cut {
        return Err("Restore this item before moving it.".into());
    }
    let mut positions: Vec<_> = plan
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.track_ref == plan.items[index].track_ref && item.state != ItemState::Cut
        })
        .map(|(i, item)| (item.order, i))
        .collect();
    positions.sort_unstable();
    let offset = positions.iter().position(|(_, i)| *i == index).unwrap();
    let next = match direction {
        Direction::Earlier => offset.checked_sub(1),
        Direction::Later => (offset + 1 < positions.len()).then_some(offset + 1),
    }
    .ok_or("This item is already at the end of its track.")?;
    let other = positions[next].1;
    editable(db, &plan.items[other])?;
    let order = plan.items[index].order;
    plan.items[index].order = plan.items[other].order;
    plan.items[other].order = order;
    Ok(())
}

pub(crate) fn move_track(db: &Connection, plan: &mut Plan, id: Uuid, track: &str) -> Result<()> {
    let index = plan
        .items
        .iter()
        .position(|item| item.plan_item_id == id)
        .ok_or("The plan item is unavailable.")?;
    let item = &plan.items[index];
    editable(db, item)?;
    if item.state == ItemState::Cut {
        return Err("Restore this item before moving it.".into());
    }
    if item.track_ref == track {
        return Err("This item is already on that track.".into());
    }
    if crate::plan_files::has_files(db, item.plan_item_id, &item.track_ref)? {
        return Err(
            "Remove this item's prepared dependencies before moving it to another track.".into(),
        );
    }
    let context = crate::plan_store::context(db, plan.id)?;
    let folder = context
        .tracks
        .get(track)
        .ok_or("Choose an active worker track.")?;
    if folder != &item.folder_id {
        let referenced = !item.after.is_empty()
            || crate::plan_store::list(db)?
                .iter()
                .flat_map(|p| &p.items)
                .any(|candidate| {
                    candidate.state != ItemState::Cut
                        && candidate
                            .after
                            .iter()
                            .any(|after| after.id == id.to_string())
                });
        if referenced {
            return Err(
                "Remove this item's dependencies before moving it to another folder.".into(),
            );
        }
    }
    let order = plan
        .items
        .iter()
        .map(|item| item.order)
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or("Plan order overflow.")?;
    plan.items[index].track_ref = track.into();
    plan.items[index].skip_order.clear();
    plan.items[index].folder_id = folder.clone();
    plan.items[index].order = order;
    Ok(())
}

pub(crate) fn finish(db: &Connection, before: &Plan, plan: &mut Plan) -> Result<()> {
    crate::plans::validate_edit(plan, &crate::plan_store::context(db, plan.id)?)
        .map_err(|errors| errors.join("\n"))?;
    let mut changed = before.state != plan.state || before.locked_track != plan.locked_track;
    for item in &mut plan.items {
        let old = before
            .items
            .iter()
            .find(|old| old.plan_item_id == item.plan_item_id)
            .ok_or("The plan item changed.")?;
        if configuration(old) != configuration(item) {
            changed = true;
            invalidate_input(db, item)?;
        }
    }
    if changed {
        plan.next_revision()?;
    }
    Ok(())
}

pub(crate) fn configuration(item: &Item) -> Item {
    let mut value = item.clone();
    value.utterance_id = None;
    value.detail = None;
    value
}

#[cfg(test)]
#[path = "plan_edit_tests.rs"]
mod tests;
