//! Plans are written by the journal's single writer. Generation receipts name
//! both the plan revision and the particular attempt that produced a result.
pub use crate::plan_edit::Direction;
use crate::{
    Result,
    plans::{self, Generation, GenerationState, Plan, State},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const AVAILABLE_TRACKS: &str = "SELECT t.id FROM tracks t WHERE t.status<>'lost'
    AND NOT EXISTS(SELECT 1 FROM workers w WHERE w.track_id=t.id
      AND json_extract(w.record,'$.phase') IN ('lost','needs-recovery'))";

pub(crate) fn track_available(db: &Connection, track: &str) -> Result<bool> {
    Ok(db.query_row(
        &format!("SELECT EXISTS(SELECT 1 FROM ({AVAILABLE_TRACKS}) WHERE id=?1)"),
        [track],
        |row| row.get(0),
    )?)
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS plans(id TEXT PRIMARY KEY,source TEXT NOT NULL UNIQUE REFERENCES utterances(id) ON DELETE CASCADE,record TEXT NOT NULL,queue_position INTEGER UNIQUE CHECK(queue_position IS NULL OR (typeof(queue_position)='integer' AND queue_position>0)));")?;
    Ok(())
}
pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Plan> {
    Ok(serde_json::from_str(&db.query_row(
        "SELECT record FROM plans WHERE id=?1",
        [id.to_string()],
        |row| row.get::<_, String>(0),
    )?)?)
}
pub(crate) fn source(db: &Connection, input: &str) -> Result<Option<Plan>> {
    let raw: Option<String> = db
        .query_row("SELECT record FROM plans WHERE source=?1", [input], |row| {
            row.get(0)
        })
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}
pub(crate) fn list(db: &Connection) -> Result<Vec<Plan>> {
    let mut query = db
        .prepare("SELECT record FROM plans ORDER BY queue_position IS NULL,queue_position,rowid")?;
    query
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect()
}
pub(crate) fn context(db: &Connection, excluding: Uuid) -> Result<plans::Context> {
    let mut context = plans::Context::default();
    context.edges.extend(crate::request_graph::edges(db)?);
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key='project'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(context);
    };
    let project: crate::project::Project = serde_json::from_str(&raw)?;
    context
        .folders
        .extend(project.folders.iter().map(|folder| folder.id()));
    let folders = crate::folders::track_ids(db)?;
    let mut query = db.prepare(AVAILABLE_TRACKS)?;
    for id in query.query_map([], |row| row.get::<_, String>(0))? {
        let id = id?;
        if let Some(folder) = folders.get(&id) {
            context.tracks.insert(id, folder.clone());
        }
    }
    let mut query = db.prepare(
        "SELECT c.id,c.track FROM timeline_clips c JOIN tracks t ON t.id=c.track WHERE json_extract(c.record,'$.repairRunId') IS NULL",
    )?;
    for row in query.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, track_id) = row?;
        context.external.insert(
            id,
            plans::External {
                track_id: track_id.clone(),
                folder_id: folders
                    .get(&track_id)
                    .ok_or("The checkpoint has no project folder.")?
                    .clone(),
            },
        );
    }
    let mut following = false;
    for plan in list(db)? {
        if plan.id == excluding {
            following = true;
            continue;
        }
        context
            .plan_items
            .extend(plan.items.iter().map(|item| item.plan_item_id.to_string()));
        for item in &plan.items {
            context.external.insert(
                item.plan_item_id.to_string(),
                plans::External {
                    track_id: plans::track_key(plan.id, &item.track_ref),
                    folder_id: item.folder_id.clone(),
                },
            );
        }
        let mut items = plan
            .items
            .iter()
            .filter(|item| item.state != plans::ItemState::Cut)
            .collect::<Vec<_>>();
        items.sort_by_key(|item| item.order);
        for item in items {
            let id = item.plan_item_id.to_string();
            let node = plans::OrderedItem {
                id: id.clone(),
                track: plans::track_key(plan.id, &item.track_ref),
                skipped: item.skip_order.iter().map(ToString::to_string).collect(),
            };
            if following {
                context.order_after.push(node);
            } else {
                context.order_before.push(node);
            }
            context.edges.extend(
                item.after
                    .iter()
                    .map(|after| (id.clone(), after.id.clone())),
            );
        }
    }
    let mut ordered = context.order_before.clone();
    ordered.extend(context.order_after.iter().cloned());
    context.edges.extend(plans::order_edges(&ordered));
    for plan in list(db)?.into_iter().filter(|plan| plan.id != excluding) {
        for item in plan
            .items
            .iter()
            .filter(|item| item.state != plans::ItemState::Cut)
        {
            for dependency in item.after.iter().filter(|dependency| dependency.inherit) {
                context
                    .inherited_edges
                    .push((item.plan_item_id.to_string(), dependency.id.clone()));
            }
        }
    }
    Ok(context)
}
pub(crate) fn save(db: &Connection, plan: &Plan, event: &str) -> Result<()> {
    db.execute(
        "INSERT INTO plans(id,source,record,queue_position) VALUES(?1,?2,?3,
         CASE WHEN ?4 THEN (SELECT coalesce(max(queue_position),0)+1 FROM plans) END)
         ON CONFLICT(id) DO UPDATE SET record=excluded.record,
         queue_position=coalesce(plans.queue_position,excluded.queue_position)",
        params![
            plan.id.to_string(),
            plan.source_id,
            serde_json::to_string(plan)?,
            plan.generation.state == GenerationState::Ready
                && plan
                    .items
                    .iter()
                    .any(|item| item.state != plans::ItemState::Cut)
        ],
    )?;
    db.execute("INSERT INTO events(ts,kind,payload) VALUES(?1,?2,?3)",params![crate::journal::timestamp(),event,serde_json::json!({"planId":plan.id,"revision":plan.rev,"generation":plan.generation.id}).to_string()])?;
    Ok(())
}
/// Retiring a worker's unverified history invalidates its plan results. Hold
/// the remaining inputs before their source/link rows can be removed.
pub(crate) fn retire_history(db: &Connection, session: &str) -> Result<()> {
    let mut query = db.prepare("SELECT id FROM plans WHERE source IN (SELECT input FROM retired_inputs WHERE session=?1) OR id IN (SELECT plan FROM plan_inputs WHERE input IN (SELECT input FROM retired_inputs WHERE session=?1))")?;
    let ids = query
        .query_map([session], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in ids {
        let mut plan = read(db, id.parse()?)?;
        for item in &mut plan.items {
            if let Some(input) = item.utterance_id {
                let retired: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM retired_inputs WHERE session=?1 AND input=?2)",
                    params![session, input.to_string()],
                    |row| row.get(0),
                )?;
                if retired {
                    item.state = plans::ItemState::Failed;
                    item.detail = Some("This result was retired during history recovery.".into());
                }
            }
        }
        plan.next_revision()?;
        plan.state = State::Held;
        db.execute("UPDATE utterances SET state='held',reason='Plan history needs review',revision=revision+1 WHERE id IN (SELECT input FROM plan_inputs WHERE plan=?1) AND state IN ('staged','reading','placed','held')", [&id])?;
        save(db, &plan, "plan.history-retired")?;
    }
    Ok(())
}
pub(crate) fn queue(
    db: &Connection,
    input: &str,
    revision: i64,
    execute_allowed: bool,
) -> Result<Plan> {
    if let Some(plan) = source(db, input)? {
        if plan.state == State::SingleJob {
            return Err("This request must stay one job.".into());
        }
        return Ok(plan);
    }
    let target: String = db.query_row(
        "SELECT target FROM utterances WHERE id=?1 AND revision=?2",
        params![input, revision],
        |row| row.get(0),
    )?;
    let locked = match serde_json::from_str::<crate::decision::Target>(&target)? {
        crate::decision::Target::Lock(track) => Some(track),
        _ => None,
    };
    let mut plan = Plan::new(input.parse()?, revision, execute_allowed, locked);
    plan.start_mode = crate::plan_start::read(db)?.mode;
    plan.attachments = crate::attachment_store::records(db, input)?
        .into_iter()
        .map(|stored| stored.attachment)
        .collect();
    save(db, &plan, "plan.queued")?;
    Ok(plan)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Action {
    KeepAssumption {
        assumption_id: Uuid,
    },
    Review,
    Start,
    Hold,
    Retry,
    ResolveLock {
        unlock: bool,
    },
    RetryItem {
        item_id: Uuid,
    },
    RetryDependency {
        item_id: Uuid,
        dependency_id: String,
        clip_id: String,
    },
    StartWithout {
        item_id: Uuid,
        dependency_id: String,
    },
    RunAsOne,
    Cut {
        item_id: Uuid,
    },
    Restore {
        item_id: Uuid,
    },
    Move {
        item_id: Uuid,
        direction: Direction,
    },
    MoveTo {
        item_id: Uuid,
        track_id: String,
    },
    UnlockAndMove {
        item_id: Uuid,
        track_id: String,
    },
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Command {
    pub op_id: Uuid,
    pub plan_id: Uuid,
    pub revision: u64,
    pub action: Action,
}
fn hold(plan: &mut Plan) -> Result<()> {
    plan.next_revision()?;
    plan.state = State::Held;
    for item in &mut plan.items {
        item.start_requested = false;
    }
    if matches!(
        plan.generation.state,
        GenerationState::Queued | GenerationState::Running
    ) {
        plan.generation.id = Uuid::now_v7();
        plan.generation.state = GenerationState::Failed;
        plan.generation.detail = Some("Planning was stopped.".into());
    }
    Ok(())
}
fn advance_source(db: &Connection, plan: &mut Plan) -> Result<()> {
    if crate::edit_hold::active(db, &plan.source_id)? {
        return Err("Finish editing the original request first.".into());
    }
    let next = plan
        .source_revision
        .checked_add(1)
        .ok_or("Request revision overflow.")?;
    let changed = db.execute("UPDATE utterances SET revision=?3 WHERE id=?1 AND revision=?2 AND state IN ('landed','placed','held')", params![plan.source_id,plan.source_revision,next])?;
    if changed != 1 {
        return Err("The original request changed. Review it before retrying.".into());
    }
    plan.source_revision = next;
    Ok(())
}
pub(crate) fn require_lock_source(db: &Connection, plan: &Plan) -> Result<()> {
    if crate::edit_hold::active(db, &plan.source_id)? {
        return Err("Finish editing the original request first.".into());
    }
    let (revision, state, target): (i64, String, String) = db.query_row(
        "SELECT revision,state,target FROM utterances WHERE id=?1",
        [&plan.source_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let expected = plan
        .locked_track
        .as_ref()
        .map_or(crate::decision::Target::None, |track| {
            crate::decision::Target::Lock(track.clone())
        });
    if revision != plan.source_revision
        || !matches!(state.as_str(), "landed" | "placed" | "held")
        || serde_json::from_str::<serde_json::Value>(&target)? != serde_json::to_value(expected)?
    {
        return Err("The original request's destination changed. Review it before changing this plan's lock.".into());
    }
    Ok(())
}
pub(crate) fn change_lock(db: &Connection, plan: &mut Plan, track: Option<String>) -> Result<()> {
    require_lock_source(db, plan)?;
    advance_source(db, plan)?;
    plan.locked_track = track;
    let target = plan
        .locked_track
        .as_ref()
        .map_or(crate::decision::Target::None, |track| {
            crate::decision::Target::Lock(track.clone())
        });
    db.execute(
        "UPDATE utterances SET target=?2 WHERE id=?1",
        params![plan.source_id, serde_json::to_string(&target)?],
    )?;
    Ok(())
}
pub(crate) fn retry_generation(db: &Connection, plan: &mut Plan) -> Result<()> {
    if plan.rewrite.is_some() {
        return Err("Retry the item revision or keep the current item first.".into());
    }
    for item in &plan.items {
        crate::plan_edit::editable(db, item)?;
    }
    for item in &mut plan.items {
        crate::plan_edit::invalidate_input(db, item)?;
    }
    advance_source(db, plan)?;
    plan.next_revision()?;
    plan.state = State::Draft;
    plan.generation = Generation {
        id: Uuid::now_v7(),
        state: GenerationState::Queued,
        attempt: 0,
        detail: None,
    };
    plan.based_on.clear();
    plan.unlock_proposal = None;
    Ok(())
}
fn run_as_one(db: &Connection, plan: &mut Plan) -> Result<()> {
    if !plan.execute_allowed
        || plan.rewrite.is_some()
        || !matches!(plan.state, State::Draft | State::Held)
    {
        return Err("Only an unfinished executable plan can run as one job.".into());
    }
    if crate::dispatch_control::paused(db)? {
        return Err("Resume new work before running this request.".into());
    }
    crate::plan_feedback::capture_fallback(plan);
    for item in &mut plan.items {
        crate::plan_edit::invalidate_input(db, item)?;
        item.state = plans::ItemState::Cut;
    }
    advance_source(db, plan)?;
    db.execute(
        "UPDATE utterances SET state='staged',decision=NULL,reason=NULL WHERE id=?1",
        [&plan.source_id],
    )?;
    plan.next_revision()?;
    plan.state = State::SingleJob;
    plan.generation = Generation {
        id: Uuid::now_v7(),
        state: GenerationState::Ready,
        attempt: 0,
        detail: None,
    };
    plan.unlock_proposal = None;
    Ok(())
}
pub(crate) fn hold_all(db: &Connection) -> Result<()> {
    for mut plan in list(db)? {
        if matches!(plan.state, State::Draft | State::Running)
            || plan.items.iter().any(|item| item.start_requested)
        {
            hold(&mut plan)?;
            save(db, &plan, "plan.held")?;
        }
    }
    Ok(())
}
pub(crate) fn start_unavailable(db: &Connection, plan: &Plan) -> Result<Option<&'static str>> {
    if !matches!(plan.state, State::Draft | State::Held)
        || plan.rewrite.is_some()
        || plan.generation.state != GenerationState::Ready
        || plan.seen_revision != Some(plan.rev)
        || !plan
            .items
            .iter()
            .any(|item| item.state == plans::ItemState::Planned)
    {
        return Ok(Some("Review this version of the plan before starting it."));
    }
    let source: Option<(i64, String)> = db
        .query_row(
            "SELECT revision,state FROM utterances WHERE id=?1",
            [&plan.source_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if source.is_none_or(|(revision, state)| revision != plan.source_revision || state == "cut")
        || crate::edit_hold::active(db, &plan.source_id)?
    {
        return Ok(Some("The original request changed. Review the plan again."));
    }
    if crate::dispatch_control::paused(db)? {
        return Ok(Some("Resume new work before starting this plan."));
    }
    let mut checked = plan.clone();
    if crate::plans::validate_edit(&mut checked, &context(db, plan.id)?).is_err() {
        return Ok(Some(
            "The plan's tracks or dependencies changed. Review the plan before starting it.",
        ));
    }
    Ok(None)
}
pub(crate) fn start(db: &Connection, plan: &mut Plan) -> Result<()> {
    if let Some(reason) = start_unavailable(db, plan)? {
        return Err(reason.into());
    }
    crate::plan_execution::assign_tracks(db, plan)?;
    plan.next_revision()?;
    plan.state = State::Running;
    Ok(())
}
pub(crate) fn command(db: &mut Connection, command: Command) -> Result<Plan> {
    if command.op_id.get_version_num() != 7 {
        return Err("Invalid plan operation.".into());
    }
    let tx = db.transaction()?;
    let payload = serde_json::to_string(&command)?;
    if let Some((kind, old)) = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [command.op_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if kind != "plan.command" || old != payload {
            return Err("Operation ID reused.".into());
        }
        return read(&tx, command.plan_id);
    }
    let mut plan = read(&tx, command.plan_id)?;
    plan.require_revision(command.revision)?;
    if plan.state == State::SingleJob && !matches!(command.action, Action::Review) {
        return Err("This request is already being handled as one job.".into());
    }
    if let Action::Cut { item_id } = &command.action {
        crate::plan_rewrite::cancel_for_cut(&mut plan, *item_id);
    }
    let before = plan.clone();
    let history_label = match &command.action {
        Action::Hold => Some("Hold plan"),
        Action::Cut { .. } => Some("Cut plan item"),
        Action::Move { .. } | Action::MoveTo { .. } => Some("Move plan item"),
        Action::UnlockAndMove { .. } => Some("Unlock and move plan item"),
        Action::StartWithout { .. } => Some("Start without dependency"),
        _ => None,
    };
    match command.action {
        Action::KeepAssumption { assumption_id } => {
            crate::plan_assumptions::keep(&tx, &mut plan, assumption_id)?;
        }
        Action::Review => {
            plan.seen_revision = Some(plan.rev);
        }
        Action::Start => {
            start(&tx, &mut plan)?;
        }
        Action::Hold => {
            if plan.state != State::Held || plan.items.iter().any(|item| item.start_requested) {
                hold(&mut plan)?;
            }
        }
        Action::Retry => {
            if plan.generation.state == GenerationState::UnlockRequired {
                return Err("Choose whether to unlock the plan or keep its current track.".into());
            }
            retry_generation(&tx, &mut plan)?;
        }
        Action::ResolveLock { unlock } => {
            if plan.generation.state != GenerationState::UnlockRequired
                || plan.locked_track.is_none()
            {
                return Err("This plan is not waiting for an unlock choice.".into());
            }
            if unlock {
                let proposal = plan
                    .unlock_proposal
                    .clone()
                    .ok_or("The proposed split is unavailable. Review the request again.")?;
                change_lock(&tx, &mut plan, None)?;
                let conflicts = proposal.conflicts.clone();
                plan.apply_draft(proposal, &context(&tx, plan.id)?)
                    .map_err(|errors| errors.join("\n"))?;
                plan.state = State::Draft;
                crate::plan_conflicts::replace(&tx, &plan, &conflicts)?;
            } else {
                run_as_one(&tx, &mut plan)?;
            }
        }

        Action::RetryItem { item_id } => {
            plan = crate::plan_continuation::retry(&tx, &plan, item_id, command.op_id)?;
        }
        Action::RetryDependency {
            item_id,
            dependency_id,
            clip_id,
        } => {
            crate::dependency_retry::retry(
                &tx,
                &plan,
                item_id,
                &dependency_id,
                &clip_id,
                command.op_id,
            )?;
            let mut current = read(&tx, command.plan_id)?;
            if current.rev == plan.rev {
                current.next_revision()?;
            }
            plan = current;
        }
        Action::StartWithout {
            item_id,
            dependency_id,
        } => {
            crate::plan_edit::start_without(&tx, &mut plan, item_id, &dependency_id)?;
            crate::plan_edit::finish(&tx, &before, &mut plan)?;
            plan.items
                .iter_mut()
                .find(|item| item.plan_item_id == item_id)
                .unwrap()
                .start_requested = true;
        }
        Action::RunAsOne => {
            if !matches!(
                plan.generation.state,
                GenerationState::TimedOut | GenerationState::Invalid | GenerationState::Failed
            ) {
                return Err("Only an unfinished executable plan can run as one job.".into());
            }
            run_as_one(&tx, &mut plan)?;
        }
        Action::Restore { item_id } => {
            crate::plan_history::undo_cut(&tx, &before, &mut plan, item_id)?;
        }
        Action::Cut { item_id } => {
            let item = plan
                .items
                .iter_mut()
                .find(|item| item.plan_item_id == item_id)
                .ok_or("This plan item is no longer available.")?;
            crate::plan_edit::unstarted(&tx, item)?;
            if item.state != plans::ItemState::Planned {
                return Err("Only an unstarted plan item can be cut.".into());
            }
            item.state = plans::ItemState::Cut;
            crate::plan_edit::finish(&tx, &before, &mut plan)?;
        }
        Action::Move { item_id, direction } => {
            crate::plan_edit::move_order(&tx, &mut plan, item_id, direction)?;
            crate::plan_edit::finish(&tx, &before, &mut plan)?;
        }
        Action::MoveTo { item_id, track_id } => {
            crate::plan_edit::move_track(&tx, &mut plan, item_id, &track_id)?;
            crate::plan_edit::finish(&tx, &before, &mut plan)?;
        }
        Action::UnlockAndMove { item_id, track_id } => {
            if plan
                .locked_track
                .as_ref()
                .is_none_or(|track| track == &track_id)
                || plan.generation.state != GenerationState::Ready
                || plan.rewrite.is_some()
            {
                return Err("This move does not require unlocking a ready plan.".into());
            }
            crate::plan_edit::move_track(&tx, &mut plan, item_id, &track_id)?;
            change_lock(&tx, &mut plan, None)?;
            crate::plan_edit::finish(&tx, &before, &mut plan)?;
        }
    }
    if plan.rev != before.rev && plan.rewrite.is_some() {
        crate::plan_rewrite::changed(&mut plan);
    }
    save(&tx, &plan, "plan.changed")?;
    if let Some(label) = history_label {
        crate::plan_history::record(&tx, &before, &plan, label)?;
    } else {
        crate::plan_history::advance_revision(&tx, plan.id, before.rev, plan.rev)?;
    }
    tx.execute(
        "INSERT INTO operations VALUES(?1,'plan.command',?2,'complete')",
        params![command.op_id.to_string(), payload],
    )?;
    tx.commit()?;
    Ok(plan)
}
pub(crate) fn begin(
    db: &mut Connection,
    id: Uuid,
    generation: Uuid,
    revision: u64,
) -> Result<bool> {
    let tx = db.transaction()?;
    let mut plan = read(&tx, id)?;
    if plan.generation.id != generation
        || plan.rev != revision
        || plan.generation.state != GenerationState::Queued
        || !plan.can_plan()
        || crate::dispatch_control::paused(&tx)?
    {
        return Ok(false);
    }
    plan.generation.state = GenerationState::Running;
    save(&tx, &plan, "plan.started")?;
    tx.commit()?;
    Ok(true)
}
pub(crate) fn recover(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    for mut plan in list(&tx)? {
        if plan.generation.state == GenerationState::Running {
            plan.next_revision()?;
            plan.generation.id = Uuid::now_v7();
            plan.generation.state = GenerationState::Failed;
            plan.generation.detail =
                Some("Planning was interrupted. Retry to prepare a new plan.".into());
            save(&tx, &plan, "plan.interrupted")?;
        }
    }
    tx.commit()?;
    Ok(())
}
pub(crate) fn based_on(
    db: &mut Connection,
    id: Uuid,
    generation: Uuid,
    revision: u64,
    values: Vec<plans::BasedOn>,
) -> Result<bool> {
    let tx = db.transaction()?;
    let mut plan = read(&tx, id)?;
    if plan.rev != revision
        || plan.generation.id != generation
        || plan.generation.state != GenerationState::Running
        || !plan.can_plan()
    {
        return Ok(false);
    }
    if plan.based_on != values {
        plan.based_on = values;
        save(&tx, &plan, "plan.context")?;
    }
    tx.commit()?;
    Ok(true)
}
pub enum Generated {
    Draft(plans::Draft),
    Invalid(String),
    Failed(String),
    TimedOut,
}
pub(crate) fn generated(
    db: &mut Connection,
    id: Uuid,
    generation: Uuid,
    revision: u64,
    result: Generated,
    based_on: Vec<plans::BasedOn>,
) -> Result<bool> {
    let tx = db.transaction()?;
    let mut plan = read(&tx, id)?;
    if plan.generation.id != generation
        || plan.rev != revision
        || plan.generation.state != GenerationState::Running
        || !plan.can_plan()
    {
        return Ok(false);
    }
    if plan.rewrite.is_some() {
        crate::plan_rewrite::generated(&tx, &mut plan, result)?;
        save(&tx, &plan, "plan.item-revised")?;
        tx.commit()?;
        return Ok(true);
    }
    match result {
        Generated::Draft(draft) => {
            let context = context(&tx, id)?;
            let only_outside_lock = plan
                .locked_track
                .as_ref()
                .is_some_and(|track| draft.items.iter().any(|item| &item.track_ref != track))
                && plans::materialize(plan.id, None, &draft, &context).is_ok();
            let proposal = only_outside_lock.then(|| draft.clone());
            let conflicts = draft.conflicts.clone();
            match plan.apply_draft(draft, &context) {
                Ok(()) => {
                    plan.based_on = based_on;
                    crate::plan_conflicts::replace(&tx, &plan, &conflicts)?;
                }
                Err(errors) => {
                    invalid(
                        &mut plan,
                        errors.into_iter().take(8).collect::<Vec<_>>().join("\n"),
                    );
                    if only_outside_lock && plan.generation.state == GenerationState::Invalid {
                        plan.generation.state = GenerationState::UnlockRequired;
                        plan.unlock_proposal = proposal;
                        plan.based_on = based_on;
                    }
                }
            }
        }
        Generated::Invalid(error) => invalid(&mut plan, error),
        Generated::Failed(error) => {
            plan.generation.state = GenerationState::Failed;
            plan.generation.detail = Some(error);
        }
        Generated::TimedOut => {
            plan.generation.state = GenerationState::TimedOut;
            plan.generation.detail = Some("The Planner did not finish within 60 seconds.".into());
        }
    }
    save(&tx, &plan, "plan.generated")?;
    tx.commit()?;
    Ok(true)
}
pub(crate) fn invalid(plan: &mut Plan, error: String) {
    plan.generation.detail = Some(error);
    if plan.generation.attempt == 0 {
        plan.generation.attempt = 1;
        plan.generation.id = Uuid::now_v7();
        plan.generation.state = GenerationState::Queued;
    } else {
        plan.generation.state = GenerationState::Invalid;
    }
}
