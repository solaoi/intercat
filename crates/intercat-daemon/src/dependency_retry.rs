//! Explicit attempts of a dependency which predates the current plan.
use crate::{
    Result,
    input_origin::{Identity, Origin, Phase},
    plans::{ItemState, Plan},
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Attempt {
    pub input: Uuid,
    pub clip: String,
    pub task_input: Uuid,
    pub track: String,
    pub title: String,
    pub source: Identity,
    pub checkpoint: String,
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS dependency_retries(input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE,clip TEXT NOT NULL,record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS dependency_retry_clip ON dependency_retries(clip);")?;
    Ok(())
}
pub(crate) fn for_input(db: &Connection, input: &str) -> Result<Option<Attempt>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM dependency_retries WHERE input=?1",
            [input],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}
pub(crate) fn latest(db: &Connection, clip: &str) -> Result<Option<Attempt>> {
    let raw: Option<String> = db.query_row("SELECT r.record FROM dependency_retries r JOIN utterances u ON u.id=r.input WHERE r.clip=?1 AND u.state<>'cut'
        AND EXISTS(SELECT 1 FROM utterances WHERE id=json_extract(r.record,'$.taskInput'))
        AND NOT EXISTS(SELECT 1 FROM retired_inputs WHERE input IN (r.input,json_extract(r.record,'$.taskInput'),json_extract(r.record,'$.source.utteranceId')))
        ORDER BY r.rowid DESC LIMIT 1", [clip], |r| r.get(0)).optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}
fn origin_of_turn(db: &Connection, session: &str, turn: Uuid) -> Result<Option<Origin>> {
    let inputs = db.prepare("SELECT DISTINCT u.id FROM utterances u
        JOIN operations o ON o.id=u.dispatch_command AND o.kind='turn.start'
        JOIN attachment_turns a ON a.command=o.id AND a.session=json_extract(o.payload,'$.sessionId')
        WHERE a.session=?1 AND a.turn=?2 AND a.inherited=0")?
        .query_map(params![session,turn.to_string()], |r| r.get::<_,String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if inputs.len() != 1 {
        return Ok(None);
    }
    if db.query_row(
        "SELECT EXISTS(SELECT 1 FROM retired_inputs WHERE input=?1)",
        [&inputs[0]],
        |r| r.get::<_, bool>(0),
    )? {
        return Ok(None);
    }
    crate::input_origin::read(db, inputs[0].parse()?)
}
pub(crate) fn current_clip(db: &Connection, origin: &Origin) -> Result<Option<String>> {
    Ok(db.query_row("SELECT json_extract(record,'$.current') FROM timeline_turns WHERE session=?1 AND turn=?2", params![origin.identity.session_id,origin.identity.turn_id.to_string()], |r| r.get(0)).optional()?.flatten())
}
struct Candidate {
    origin: Origin,
    task_input: Uuid,
    title: String,
    clip: String,
    plan_item: Option<crate::request_sources::PlanItemContext>,
}
fn candidate(db: &Connection, plans: &[Plan], clip: &str) -> Result<Option<Candidate>> {
    if plans
        .iter()
        .flat_map(|p| &p.items)
        .any(|item| item.plan_item_id.to_string() == clip)
    {
        return Ok(None);
    }
    let Some(parent) = crate::plan_dependencies::read(db, plans, clip)? else {
        return Ok(None);
    };
    if parent.state != ItemState::Failed {
        return Ok(None);
    }
    let Some(turn) = parent.turn else {
        return Ok(None);
    };
    let plan_item = if let Some((plan_id, item_id)) =
        crate::plan_execution::turn_owner(db, &turn.session, &turn.id.to_string())?
    {
        let Some(plan) = plans.iter().find(|plan| plan.id == plan_id) else {
            return Ok(None);
        };
        let Some(item) = plan.items.iter().find(|item| item.plan_item_id == item_id) else {
            return Ok(None);
        };
        if !crate::plan_continuation::allowed(db, plan, item)?
            || crate::plan_continuation::has_pending(db, &item_id.to_string())?
        {
            return Ok(None);
        }
        Some(crate::request_sources::PlanItemContext {
            plan_id,
            item_id,
            revision: plan.rev,
        })
    } else {
        None
    };
    let Some(origin) = origin_of_turn(db, &turn.session, turn.id)? else {
        return Ok(None);
    };
    if !matches!(origin.phase, Phase::Finished | Phase::MovedOn) || origin.checkpoint.is_none() {
        return Ok(None);
    }
    let previous = latest(db, clip)?;
    let task_input = previous
        .as_ref()
        .map_or(origin.identity.utterance_id, |attempt| attempt.task_input);
    if db.query_row(
        "SELECT EXISTS(SELECT 1 FROM retired_inputs WHERE input=?1)",
        [task_input.to_string()],
        |r| r.get::<_, bool>(0),
    )? {
        return Ok(None);
    }
    let current = if previous.is_some() {
        current_clip(db, &origin)?
    } else {
        Some(clip.to_owned())
    };
    Ok(current.map(|clip| Candidate {
        origin,
        task_input,
        title: parent.title,
        clip,
        plan_item,
    }))
}
pub(crate) fn retryable(db: &Connection, plans: &[Plan], clip: &str) -> Result<bool> {
    Ok(candidate(db, plans, clip)?.is_some())
}
pub(crate) fn retry(
    db: &Transaction<'_>,
    plan: &Plan,
    item: Uuid,
    clip: &str,
    expected_clip: &str,
    operation: Uuid,
) -> Result<()> {
    let item = plan
        .items
        .iter()
        .find(|value| value.plan_item_id == item)
        .ok_or("The dependent item is unavailable.")?;
    if plan.generation.state != crate::plans::GenerationState::Ready
        || item.state != ItemState::Planned
        || !item.after.iter().any(|after| after.id == clip)
    {
        return Err("This item no longer waits for that dependency.".into());
    }
    let source = candidate(db, &crate::plan_store::list(db)?, clip)?
        .ok_or("Retry needs the dependency's saved failed turn and no pending attempt.")?;
    if source.clip != expected_clip {
        return Err("The dependency changed. Review its latest result before retrying.".into());
    }
    let input = Uuid::new_v5(&operation, b"retry-dependency");
    let attachments = crate::attachment_store::records(db, &source.task_input.to_string())?
        .into_iter()
        .map(|value| value.attachment.id)
        .collect::<Vec<_>>();
    let sources = crate::request_sources::read(db, &source.task_input.to_string())?;
    crate::journal::insert_utterance(
        db,
        &input.to_string(),
        "Retry the selected saved task. Keep completed work and address its remaining failure. Keep the original user's language in the report.",
        &crate::decision::Target::Lock(source.origin.track_id.clone()),
        Some(&source.origin.track_id),
        &attachments,
        sources.as_ref(),
        true,
        source.plan_item.as_ref(),
    )?;
    let attempt = Attempt {
        input,
        clip: clip.into(),
        task_input: source.task_input,
        track: source.origin.track_id,
        title: source.title,
        checkpoint: source.origin.checkpoint.unwrap(),
        source: source.origin.identity,
    };
    db.execute(
        "INSERT INTO dependency_retries VALUES(?1,?2,?3)",
        params![input.to_string(), clip, serde_json::to_string(&attempt)?],
    )?;
    let decision = serde_json::json!({"kind":"extend","trackId":attempt.track,"boundary":"after-turn","command":null,"confidence":1.0,"inferenceMs":0.0,"timedOut":false,"scores":{}});
    db.execute("UPDATE utterances SET decision=?2,state=CASE WHEN state='held' THEN state ELSE 'placed' END WHERE id=?1", params![input.to_string(),decision.to_string()])?;
    Ok(())
}
pub(crate) fn allowed(db: &Connection, input: &str) -> Result<bool> {
    let Some(attempt) = for_input(db, input)? else {
        return Ok(true);
    };
    if latest(db, &attempt.clip)?.is_none_or(|current| current.input != attempt.input) {
        return Ok(false);
    }
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM timeline_clips WHERE id=?1) AND NOT EXISTS(SELECT 1 FROM retired_inputs WHERE input IN (?2,?3))", params![attempt.clip,attempt.task_input.to_string(),attempt.source.utterance_id.to_string()], |r| r.get(0))?;
    if !exists {
        return Ok(false);
    }
    let Some(source) = crate::input_origin::read(db, attempt.source.utterance_id)? else {
        return Ok(false);
    };
    if source.identity != attempt.source
        || source.checkpoint.as_deref() != Some(&attempt.checkpoint)
        || crate::plan_execution::settled_outcome(db, &source)?
            .is_none_or(|(state, _)| state != ItemState::Failed)
    {
        return Ok(false);
    }
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM utterances WHERE id=?1 AND json_extract(target,'$.mode')='lock' AND json_extract(target,'$.trackId')=?2 AND json_extract(decision,'$.trackId')=?2 AND json_extract(decision,'$.kind') IN ('extend','schedule','trim'))", params![input,attempt.track], |r| r.get(0))?)
}
pub(crate) fn instructions(db: &Connection, input: &str) -> Result<Option<String>> {
    let Some(attempt) = for_input(db, input)? else {
        return Ok(None);
    };
    if crate::plan_execution::owner(db, input)?.is_some() {
        return Ok(None);
    }
    if !allowed(db, input)? {
        return Err(crate::journal::StartSuperseded.into());
    }
    let text: String = db.query_row(
        "SELECT text FROM utterances WHERE id=?1",
        [attempt.task_input.to_string()],
        |r| r.get(0),
    )?;
    Ok(Some(format!(
        "This explicitly retries the task from saved turn {} in session {}. It is not permission to repeat unrelated work on this track. Keep existing files and complete the original task below.\n\nOriginal task:\n{text}",
        attempt.source.turn_id, attempt.source.session_id
    )))
}
