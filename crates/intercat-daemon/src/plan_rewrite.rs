//! A window-owned edit holds one item until its reviewed replacement is accepted.
use crate::{
    Result,
    clock::Stamp,
    plans::{DraftItem, Generation, GenerationState, Item, ItemState, Plan, State},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const TIMEOUT_MS: i64 = 30_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Edit {
    pub owner: Uuid,
    pub token: Uuid,
    pub item_id: Uuid,
    pub revision: u64,
    pub renewed: Stamp,
    pub before: Item,
    pub words: Option<String>,
    pub proposal: Option<DraftItem>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub owner: Uuid,
    pub action: Action,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Action {
    Begin {
        plan_id: Uuid,
        item_id: Uuid,
        token: Uuid,
        revision: u64,
    },
    Renew {
        plan_id: Uuid,
        token: Uuid,
    },
    Cancel {
        plan_id: Uuid,
        token: Uuid,
    },
    Submit {
        plan_id: Uuid,
        token: Uuid,
        revision: u64,
        op_id: Uuid,
        text: String,
    },
    Apply {
        plan_id: Uuid,
        token: Uuid,
        revision: u64,
        op_id: Uuid,
    },
    Dependencies {
        plan_id: Uuid,
        token: Uuid,
        revision: u64,
        op_id: Uuid,
        after: Vec<crate::plans::Dependency>,
    },
    Retry {
        plan_id: Uuid,
        token: Uuid,
        revision: u64,
        op_id: Uuid,
    },
    Disconnect,
}
impl Action {
    fn plan(&self) -> Option<Uuid> {
        match self {
            Self::Begin { plan_id, .. }
            | Self::Renew { plan_id, .. }
            | Self::Cancel { plan_id, .. }
            | Self::Submit { plan_id, .. }
            | Self::Apply { plan_id, .. }
            | Self::Dependencies { plan_id, .. }
            | Self::Retry { plan_id, .. } => Some(*plan_id),
            Self::Disconnect => None,
        }
    }
    fn receipt(&self) -> Option<Uuid> {
        match self {
            Self::Submit { op_id, .. }
            | Self::Apply { op_id, .. }
            | Self::Dependencies { op_id, .. }
            | Self::Retry { op_id, .. } => Some(*op_id),
            _ => None,
        }
    }
}

fn ready_generation() -> Generation {
    Generation {
        id: Uuid::now_v7(),
        state: GenerationState::Ready,
        attempt: 0,
        detail: None,
    }
}

fn close(db: &Connection, plan: &mut Plan, interrupted: bool) -> Result<()> {
    let edit = plan
        .rewrite
        .take()
        .ok_or("This item is no longer being edited.")?;
    let item = plan
        .items
        .iter_mut()
        .find(|item| item.plan_item_id == edit.item_id)
        .ok_or("The edited item is unavailable.")?;
    if item.state != ItemState::Editing {
        return Err("The item changed while it was being edited.".into());
    }
    *item = edit.before;
    plan.generation = ready_generation();
    let before = plan.rev;
    plan.next_revision()?;
    if interrupted {
        plan.state = State::Held;
        item_detail(
            plan,
            edit.item_id,
            "Editing was interrupted · review before starting",
        );
    }
    crate::plan_history::advance_revision(db, plan.id, before, plan.rev)?;
    crate::plan_store::save(
        db,
        plan,
        if interrupted {
            "plan.edit-interrupted"
        } else {
            "plan.edit-cancelled"
        },
    )
}
fn item_detail(plan: &mut Plan, id: Uuid, message: &str) {
    if let Some(item) = plan.items.iter_mut().find(|item| item.plan_item_id == id) {
        item.detail = Some(message.into());
    }
}

pub(crate) fn expire(db: &mut Connection, now: &Stamp) -> Result<()> {
    let expired: Vec<_> = crate::plan_store::list(db)?
        .into_iter()
        .filter(|plan| {
            plan.rewrite.as_ref().is_some_and(|edit| {
                edit.words.is_none()
                    && (edit.renewed.boot_id != now.boot_id
                        || now.elapsed(&edit.renewed).0 >= TIMEOUT_MS)
            })
        })
        .collect();
    if expired.is_empty() {
        return Ok(());
    }
    let tx = db.transaction()?;
    for mut plan in expired {
        close(&tx, &mut plan, true)?;
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn change(db: &mut Connection, request: Request, now: Stamp) -> Result<Option<Plan>> {
    expire(db, &now)?;
    let tx = db.transaction()?;
    let Some(id) = request.action.plan() else {
        for mut plan in crate::plan_store::list(&tx)?.into_iter().filter(|plan| {
            plan.rewrite
                .as_ref()
                .is_some_and(|edit| edit.owner == request.owner && edit.words.is_none())
        }) {
            close(&tx, &mut plan, true)?;
        }
        tx.commit()?;
        return Ok(None);
    };
    let mut plan = crate::plan_store::read(&tx, id)?;
    let receipt = request.action.receipt();
    let payload = serde_json::to_string(&request.action)?;
    if let Some(op) = receipt {
        if op.get_version_num() != 7 {
            return Err("Invalid item edit operation.".into());
        }
        use rusqlite::OptionalExtension;
        if let Some((kind, saved)) = tx
            .query_row(
                "SELECT kind,payload FROM operations WHERE id=?1",
                [op.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            if kind != "plan.item-edit" || saved != payload {
                return Err("Item edit operation ID reused.".into());
            }
            return Ok(Some(plan));
        }
    }
    let old_revision = plan.rev;
    match request.action {
        Action::Begin {
            item_id,
            token,
            revision,
            ..
        } => {
            if token.get_version_num() != 7 {
                return Err("Invalid item edit token.".into());
            }
            if let Some(edit) = &plan.rewrite {
                if edit.owner == request.owner && edit.token == token && edit.item_id == item_id {
                    return Ok(Some(plan));
                }
                return Err("This plan is being edited in another window.".into());
            }
            plan.require_revision(revision)?;
            if plan.generation.state != GenerationState::Ready {
                return Err("Wait for planning to finish before editing an item.".into());
            }
            let item = plan
                .items
                .iter_mut()
                .find(|item| item.plan_item_id == item_id)
                .ok_or("This item is unavailable.")?;
            if item.state != ItemState::Planned {
                return Err("Only an unstarted item can be edited.".into());
            }
            crate::plan_edit::editable(&tx, item)?;
            let before = crate::plan_edit::configuration(item);
            crate::plan_edit::invalidate_input(&tx, item)?;
            item.state = ItemState::Editing;
            plan.next_revision()?;
            plan.rewrite = Some(Edit {
                owner: request.owner,
                token,
                item_id,
                revision: plan.rev,
                renewed: now,
                before,
                words: None,
                proposal: None,
            });
        }
        Action::Renew { token, .. } => {
            let edit = plan
                .rewrite
                .as_mut()
                .ok_or("Editing was interrupted. Reopen this item.")?;
            if edit.owner != request.owner || edit.token != token {
                return Err("This window does not own the item edit.".into());
            }
            edit.renewed = now;
        }
        Action::Cancel { token, .. } => {
            if plan.rewrite.is_none() {
                return Ok(Some(plan));
            }
            let edit = plan
                .rewrite
                .as_ref()
                .ok_or("This item is no longer being edited.")?;
            if edit.token != token || edit.words.is_none() && edit.owner != request.owner {
                return Err("This window does not own the item edit.".into());
            }
            close(&tx, &mut plan, false)?;
        }
        Action::Submit {
            token,
            revision,
            text,
            ..
        } => {
            plan.require_revision(revision)?;
            let edit = plan
                .rewrite
                .as_mut()
                .ok_or("Editing was interrupted. Reopen this item.")?;
            if edit.owner != request.owner || edit.token != token {
                return Err("This window does not own the item edit.".into());
            }
            if text.trim().is_empty() || text.len() > 32 * 1024 {
                return Err("Write a revision of up to 32 KiB.".into());
            }
            if edit.words.is_some() {
                return Err("Review or cancel the existing revision first.".into());
            }
            edit.words = Some(text);
            edit.proposal = None;
            plan.next_revision()?;
            plan.rewrite.as_mut().unwrap().revision = plan.rev;
            plan.generation = Generation {
                id: Uuid::now_v7(),
                state: GenerationState::Queued,
                attempt: 0,
                detail: None,
            };
        }
        Action::Retry {
            token, revision, ..
        } => {
            plan.require_revision(revision)?;
            let edit = plan
                .rewrite
                .as_mut()
                .ok_or("This item is no longer being edited.")?;
            if edit.token != token
                || edit.words.is_none()
                || matches!(
                    plan.generation.state,
                    GenerationState::Queued | GenerationState::Running
                )
            {
                return Err("This item revision cannot be retried.".into());
            }
            edit.proposal = None;
            plan.next_revision()?;
            plan.rewrite.as_mut().unwrap().revision = plan.rev;
            plan.generation = Generation {
                id: Uuid::now_v7(),
                state: GenerationState::Queued,
                attempt: 0,
                detail: None,
            };
        }
        Action::Dependencies {
            token,
            revision,
            after,
            ..
        } => {
            plan.require_revision(revision)?;
            let edit = plan
                .rewrite
                .clone()
                .ok_or("Editing was interrupted. Reopen this item.")?;
            if edit.owner != request.owner || edit.token != token || edit.words.is_some() {
                return Err("This window does not own the dependency edit.".into());
            }
            let item = plan
                .items
                .iter_mut()
                .find(|item| item.plan_item_id == edit.item_id)
                .ok_or("The item is unavailable.")?;
            *item = edit.before;
            let before = plan.clone();
            plan.items
                .iter_mut()
                .find(|item| item.plan_item_id == edit.item_id)
                .unwrap()
                .after = after;
            plan.rewrite = None;
            plan.generation = ready_generation();
            crate::plan_edit::finish(&tx, &before, &mut plan)?;
            if plan.rev == old_revision {
                plan.next_revision()?;
            }
            crate::plan_history::record(&tx, &before, &plan, "Change dependency")?;
        }
        Action::Apply {
            token, revision, ..
        } => {
            plan.require_revision(revision)?;
            let edit = plan
                .rewrite
                .clone()
                .ok_or("This item is no longer being edited.")?;
            if edit.token != token {
                return Err("This item revision changed.".into());
            }
            let proposal = edit
                .proposal
                .ok_or("Wait for the revised item before accepting it.")?;
            let mut before = plan.clone();
            *before
                .items
                .iter_mut()
                .find(|item| item.plan_item_id == edit.item_id)
                .ok_or("The item is unavailable.")? = edit.before;
            replace(&mut plan, edit.item_id, &proposal)?;
            plan.rewrite = None;
            plan.generation = ready_generation();
            crate::plan_edit::finish(&tx, &before, &mut plan)?;
            if plan.rev == old_revision {
                plan.next_revision()?;
            }
            crate::plan_history::record(&tx, &before, &plan, "Revise plan item")?;
        }
        Action::Disconnect => unreachable!(),
    }
    crate::plan_history::advance_revision(&tx, plan.id, old_revision, plan.rev)?;
    crate::plan_store::save(&tx, &plan, "plan.item-edit")?;
    if let Some(op) = receipt {
        tx.execute(
            "INSERT INTO operations VALUES(?1,'plan.item-edit',?2,'complete')",
            params![op.to_string(), payload],
        )?;
    }
    tx.commit()?;
    Ok(Some(plan))
}

fn replace(plan: &mut Plan, id: Uuid, proposal: &DraftItem) -> Result<()> {
    if proposal.id != id.to_string() {
        return Err("The revision replaced a different item.".into());
    }
    let item = plan
        .items
        .iter_mut()
        .find(|item| item.plan_item_id == id)
        .ok_or("The item is unavailable.")?;
    if proposal.track_ref != item.track_ref
        || proposal.folder_id != item.folder_id
        || proposal.after != item.after
    {
        return Err("Keep this item's destination and dependencies. Use the move and dependency actions to change them.".into());
    }
    item.title = proposal.title.clone();
    item.instruction = proposal.instruction.clone();
    item.track_ref = proposal.track_ref.clone();
    item.folder_id = proposal.folder_id.clone();
    item.constraints = proposal.constraints.clone();
    item.done_when = proposal.done_when.clone();
    item.after = proposal.after.clone();
    item.state = ItemState::Planned;
    item.detail = None;
    Ok(())
}

pub(crate) fn generated(
    db: &Connection,
    plan: &mut Plan,
    result: crate::plan_store::Generated,
) -> Result<()> {
    match result {
        crate::plan_store::Generated::Draft(draft) => {
            let edit = plan
                .rewrite
                .as_ref()
                .ok_or("No item revision is pending.")?;
            let valid = if !draft.conflicts.is_empty() || !draft.assumptions.is_empty() {
                Err("An item revision must not replace the plan conflict review.".into())
            } else if let [proposal] = draft.items.as_slice() {
                let mut candidate = plan.clone();
                replace(&mut candidate, edit.item_id, proposal).and_then(|()| {
                    crate::plans::validate_edit(
                        &mut candidate,
                        &crate::plan_store::context(db, plan.id)?,
                    )
                    .map_err(|errors| errors.join("\n").into())
                })
            } else {
                Err("Return exactly one revised item.".into())
            };
            match valid {
                Ok(()) => {
                    plan.rewrite.as_mut().unwrap().proposal = Some(draft.items[0].clone());
                    plan.generation.state = GenerationState::Ready;
                    plan.generation.detail = None;
                }
                Err(error) => crate::plan_store::invalid(plan, error.to_string()),
            }
        }
        crate::plan_store::Generated::Invalid(error) => crate::plan_store::invalid(plan, error),
        crate::plan_store::Generated::TimedOut => {
            plan.generation.state = GenerationState::TimedOut;
            plan.generation.detail = Some("The Planner did not finish within 60 seconds.".into());
        }
        crate::plan_store::Generated::Failed(error) => {
            plan.generation.state = GenerationState::Failed;
            plan.generation.detail = Some(error);
        }
    }
    Ok(())
}

pub(crate) fn changed(plan: &mut Plan) {
    if let Some(edit) = plan.rewrite.as_mut() {
        edit.proposal = None;
        if edit.words.is_some() {
            plan.generation = Generation {
                id: Uuid::now_v7(),
                state: GenerationState::Failed,
                attempt: 0,
                detail: Some("The plan changed · review before retrying this revision".into()),
            };
        }
    }
}

pub(crate) fn cancel_for_cut(plan: &mut Plan, item_id: Uuid) {
    if plan
        .rewrite
        .as_ref()
        .is_some_and(|edit| edit.item_id == item_id)
    {
        let edit = plan.rewrite.take().unwrap();
        if let Some(item) = plan
            .items
            .iter_mut()
            .find(|item| item.plan_item_id == item_id)
        {
            *item = edit.before;
        }
        plan.generation = ready_generation();
    }
}

#[cfg(test)]
#[path = "plan_rewrite_tests.rs"]
mod tests;
