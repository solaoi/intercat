//! Explicit continuation requests retain the item identity across turns.
use crate::{
    Result,
    plans::{GenerationState, ItemState, State},
};
use rusqlite::{Connection, params};
use uuid::Uuid;

pub(crate) fn retryable(
    db: &Connection,
    plan: &crate::plans::Plan,
    item: &crate::plans::Item,
) -> Result<bool> {
    if !allowed(db, plan, item)? || has_pending(db, &item.plan_item_id.to_string())? {
        return Ok(false);
    }
    let Some(origin) = crate::plan_execution::item_origin(db, item)? else {
        return Ok(false);
    };
    Ok(crate::plan_execution::settled_outcome(db, &origin)?
        .is_some_and(|(state, _)| state == ItemState::Failed))
}

pub(crate) fn retry(
    db: &rusqlite::Transaction<'_>,
    plan: &crate::plans::Plan,
    item_id: Uuid,
    operation: Uuid,
) -> Result<crate::plans::Plan> {
    let item = plan
        .items
        .iter()
        .find(|item| item.plan_item_id == item_id)
        .ok_or("The plan item is unavailable.")?;
    if !retryable(db, plan, item)? {
        return Err("Retry needs a saved failed result and no pending continuation.".into());
    }
    let input = Uuid::new_v5(&operation, b"retry-plan-item");
    crate::journal::insert_utterance(
        db,
        &input.to_string(),
        "Continue this plan item from its saved work. Address the remaining failure and satisfy its done condition. Keep the language of the original user request in your report; do not switch languages to match these internal retry instructions.",
        &crate::decision::Target::Lock(item.track_ref.clone()),
        Some(&item.track_ref),
        &plan
            .attachments
            .iter()
            .map(|file| file.id)
            .collect::<Vec<_>>(),
        None,
        true,
        Some(&crate::request_sources::PlanItemContext {
            plan_id: plan.id,
            item_id,
            revision: plan.rev,
        }),
    )?;
    crate::plan_store::read(db, plan.id)
}

pub(crate) fn validate_context(
    db: &Connection,
    context: &crate::request_sources::PlanItemContext,
    target: &crate::decision::Target,
    execute_allowed: bool,
) -> Result<()> {
    let plan = crate::plan_store::read(db, context.plan_id)?;
    plan.require_revision(context.revision)?;
    let item = plan
        .items
        .iter()
        .find(|item| item.plan_item_id == context.item_id)
        .ok_or("The plan item is unavailable.")?;
    if !execute_allowed
        || !matches!(target, crate::decision::Target::Lock(track) if track == &item.track_ref)
        || !allowed(db, &plan, item)?
    {
        return Err("This request must keep the selected plan item and its worker.".into());
    }
    Ok(())
}

pub(crate) fn instructions(db: &Connection, input: &str) -> Result<Option<String>> {
    if !is_continuation(db, input)? {
        return Ok(None);
    }
    let (plan_id, item_id) =
        crate::plan_execution::owner(db, input)?.ok_or("The plan item is unavailable.")?;
    let plan = crate::plan_store::read(db, plan_id)?;
    let item = plan
        .items
        .iter()
        .find(|item| item.plan_item_id == item_id)
        .ok_or("The plan item is unavailable.")?;
    Ok(Some(crate::plan_execution::body(&plan, item)))
}

pub(crate) fn is_continuation(db: &Connection, input: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM plan_inputs WHERE input=?1 AND role='continuation')",
        [input],
        |row| row.get(0),
    )?)
}

pub(crate) fn has_pending(db: &Connection, item: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM plan_inputs p JOIN utterances u ON u.id=p.input
         WHERE p.item=?1 AND p.role='continuation'
         AND (u.state IN ('staged','reading','asked','held','starting','sent','redirecting')
           OR (u.state='placed' AND json_extract(u.decision,'$.kind') IN ('extend','schedule','trim'))))
         OR EXISTS(SELECT 1 FROM plan_inputs p JOIN review_fix_inputs i ON i.input=p.input
           JOIN review_fixes f ON f.id=i.fix AND f.input=i.input WHERE p.item=?1
           AND json_extract(f.record,'$.state') IN ('planned','held','reserved','running','awaiting-review','blocked'))",
        [item], |row| row.get(0),
    )?)
}

pub(crate) fn attach(db: &Connection, input: &str, plan_id: Uuid, item_id: Uuid) -> Result<()> {
    let mut plan = crate::plan_store::read(db, plan_id)?;
    let item = plan
        .items
        .iter()
        .find(|item| item.plan_item_id == item_id)
        .ok_or("The plan item is unavailable.")?;
    if !allowed(db, &plan, item)? {
        return Err("This plan item cannot receive a continuation.".into());
    }
    let track: Option<String> = db.query_row(
        "SELECT json_extract(target,'$.trackId') FROM utterances WHERE id=?1 AND json_extract(target,'$.mode')='lock'",
        [input], |row| row.get(0),
    )?;
    if track.as_deref() != Some(&item.track_ref) {
        return Err("The continuation must keep the plan item's track.".into());
    }
    db.execute(
        "INSERT INTO plan_inputs(input,plan,item,role) VALUES(?1,?2,?3,'continuation')",
        params![input, plan_id.to_string(), item_id.to_string()],
    )?;
    let old = plan.rev;
    plan.next_revision()?;
    crate::plan_history::advance_revision(db, plan.id, old, plan.rev)?;
    crate::plan_store::save(db, &plan, "plan.continuation-queued")
}

pub(crate) fn attach_to_turn(
    db: &Connection,
    input: &str,
    session: &str,
    turn: &str,
) -> Result<()> {
    if let Some((plan, item)) = crate::plan_execution::turn_owner(db, session, turn)? {
        attach(db, input, plan, item)?;
    }
    Ok(())
}

pub(crate) fn allowed(
    db: &Connection,
    plan: &crate::plans::Plan,
    item: &crate::plans::Item,
) -> Result<bool> {
    if plan.generation.state != GenerationState::Ready
        || plan.state == State::SingleJob
        || matches!(item.state, ItemState::Cut | ItemState::Editing)
    {
        return Ok(false);
    }
    let Some(input) = item.utterance_id else {
        return Ok(false);
    };
    let retired: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM retired_inputs WHERE input=?1)",
        [input.to_string()],
        |row| row.get(0),
    )?;
    if retired {
        return Ok(false);
    }
    Ok(crate::input_origin::read(db, input)?.is_some())
}

pub(crate) fn input_allowed(
    db: &Connection,
    input: &str,
    plan: &crate::plans::Plan,
    item: &crate::plans::Item,
) -> Result<bool> {
    Ok(allowed(db, plan, item)?
        && db.query_row(
            "SELECT EXISTS(SELECT 1 FROM utterances WHERE id=?1
         AND json_extract(target,'$.mode')='lock' AND json_extract(target,'$.trackId')=?2
         AND json_extract(decision,'$.trackId')=?2
         AND json_extract(decision,'$.kind') IN ('extend','schedule','trim'))",
            params![input, item.track_ref],
            |row| row.get::<_, bool>(0),
        )?)
}
