//! A request waits for an explicitly identified turn, clip or plan item.
use crate::{
    Result,
    decision::{Decision, Kind},
    decision_review::{Review, Stage},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Turn {
    pub track_id: String,
    pub label: String,
    pub session_id: String,
    pub turn_id: Uuid,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlanItem {
    pub plan_id: Uuid,
    pub item_id: Uuid,
    pub title: String,
    pub label: String,
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub enum Source {
    Turn(Turn),
    Clip {
        id: String,
        ordinal: i64,
        turn: Turn,
    },
    PlanItem(PlanItem),
}
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Target {
    Clip {
        clip_id: String,
    },
    PlanItem {
        plan_id: Uuid,
        item_id: Uuid,
        revision: u64,
    },
}
#[derive(Clone, Default, Deserialize, Serialize)]
pub(crate) struct Context {
    pub candidates: Vec<Source>,
    pub source: Option<Source>,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS input_start_conditions(input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE,record TEXT NOT NULL);")?;
    Ok(())
}
pub(crate) fn capture(db: &Connection, input: &str) -> Result<()> {
    let rows = db.prepare("SELECT w.track_id,t.label,json_extract(w.record,'$.sessionId'),json_extract(w.record,'$.activeTurn') FROM workers w JOIN tracks t ON t.id=w.track_id WHERE json_type(w.record,'$.sessionId')='text' AND json_type(w.record,'$.activeTurn')='text' ORDER BY t.rowid")?
        .query_map([], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get::<_,String>(3)?)))?
        .collect::<rusqlite::Result<Vec<(String,String,String,String)>>>()?;
    let mut candidates = rows
        .into_iter()
        .map(|(track_id, label, session_id, turn)| {
            Ok(Source::Turn(Turn {
                track_id,
                label,
                session_id,
                turn_id: turn.parse()?,
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    for plan in crate::plan_store::list(db)? {
        if plan.generation.state != crate::plans::GenerationState::Ready {
            continue;
        }
        for item in plan.items.iter().filter(|item| {
            matches!(
                item.state,
                crate::plans::ItemState::Planned | crate::plans::ItemState::Editing
            )
        }) {
            let label = db
                .query_row(
                    "SELECT label FROM tracks WHERE id=?1",
                    [&item.track_ref],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_else(|| format!("New {}", item.track_ref.trim_start_matches("new:")));
            candidates.push(Source::PlanItem(PlanItem {
                plan_id: plan.id,
                item_id: item.plan_item_id,
                title: item.title.clone(),
                label,
            }));
        }
    }
    save(
        db,
        input,
        &Context {
            candidates,
            source: None,
        },
    )
}
pub(crate) fn read(db: &Connection, input: &str) -> Result<Context> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM input_start_conditions WHERE input=?1",
            [input],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
        .map(|value| value.unwrap_or_default())
}
pub(crate) fn save(db: &Connection, input: &str, context: &Context) -> Result<()> {
    db.execute("INSERT INTO input_start_conditions VALUES(?1,?2) ON CONFLICT(input) DO UPDATE SET record=excluded.record",params![input,serde_json::to_string(context)?])?;
    Ok(())
}
fn waits(decision: &Decision, context: &Context) -> bool {
    decision.boundary.as_deref() == Some("after-clip")
        || decision.boundary.as_deref() == Some("after-turn")
            && matches!(decision.kind, Kind::Fork | Kind::Extend | Kind::Schedule)
            && (decision.kind == Kind::Fork || context.source.is_some())
}
pub(crate) fn prepare(db: &Connection, input: &str, decision: &mut Decision) -> Result<()> {
    let mut context = read(db, input)?;
    if !waits(decision, &context) || decision.timed_out {
        return Ok(());
    }
    if context.source.is_none()
        && decision.boundary.as_deref() != Some("after-clip")
        && context.candidates.len() == 1
        && matches!(context.candidates[0], Source::Turn(_))
    {
        context.source = context.candidates.first().cloned();
        save(db, input, &context)?;
    }
    if context.source.is_none() && crate::decision_review::read(decision)?.is_none() {
        crate::decision_review::write(
            decision,
            Review {
                stage: Stage::StartTurn,
                turn: None,
            },
        )?;
    }
    Ok(())
}
pub(crate) fn choose(db: &Connection, input: &str, source: &Source) -> Result<()> {
    let mut context = read(db, input)?;
    if !context.candidates.contains(source) {
        return Err("Choose work recorded with this request.".into());
    }
    if let Source::PlanItem(item) = source {
        let plan = crate::plan_store::read(db, item.plan_id)?;
        if !plan.items.iter().any(|current| {
            current.plan_item_id == item.item_id && current.state != crate::plans::ItemState::Cut
        }) {
            return Err("This plan item was removed. Edit the request to choose again.".into());
        }
    }
    context.source = Some(source.clone());
    save(db, input, &context)
}
pub(crate) fn clear(db: &Connection, input: &str) -> Result<()> {
    let mut context = read(db, input)?;
    context.source = None;
    save(db, input, &context)
}
pub(crate) fn bind_turn(
    db: &Connection,
    input: &str,
    track: &str,
    turn: Option<&crate::decision_review::Turn>,
) -> Result<()> {
    let Some(turn) = turn else {
        return clear(db, input);
    };
    let label:Option<String>=db.query_row("SELECT t.label FROM tracks t JOIN timeline_turns s ON json_extract(s.record,'$.track')=t.id WHERE t.id=?1 AND s.session=?2 AND s.turn=?3",params![track,turn.session_id,turn.turn_id.to_string()],|row|row.get(0)).optional()?;
    let label = label.ok_or("The selected turn no longer belongs to this worker.")?;
    let mut context = read(db, input)?;
    context.source = Some(Source::Turn(Turn {
        track_id: track.into(),
        label,
        session_id: turn.session_id.clone(),
        turn_id: turn.turn_id,
    }));
    save(db, input, &context)
}
pub(crate) fn select(db: &Connection, input: &str, target: &Target) -> Result<()> {
    let source = match target {
        Target::Clip { clip_id } => {
            let row:Option<(String,i64,String,String,String)>=db.query_row("SELECT c.track,c.ordinal,t.label,json_extract(c.record,'$.sessionId'),json_extract(c.record,'$.turnId') FROM timeline_clips c JOIN tracks t ON t.id=c.track JOIN timeline_turns s ON s.session=json_extract(c.record,'$.sessionId') AND s.turn=json_extract(c.record,'$.turnId') WHERE c.id=?1 AND json_extract(c.record,'$.end') IS NULL AND json_extract(c.record,'$.repairRunId') IS NULL AND coalesce(json_extract(s.record,'$.terminal'),0)=0",[clip_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).optional()?;
            let (track_id, ordinal, label, session_id, turn_id) =
                row.ok_or("Choose a running clip. Completed clips cannot be a new wait target.")?;
            Source::Clip {
                id: clip_id.clone(),
                ordinal,
                turn: Turn {
                    track_id,
                    label,
                    session_id,
                    turn_id: turn_id.parse()?,
                },
            }
        }
        Target::PlanItem {
            plan_id,
            item_id,
            revision,
        } => {
            let plan = crate::plan_store::read(db, *plan_id)?;
            plan.require_revision(*revision)?;
            let item = plan
                .items
                .iter()
                .find(|item| {
                    item.plan_item_id == *item_id
                        && matches!(
                            item.state,
                            crate::plans::ItemState::Planned | crate::plans::ItemState::Editing
                        )
                })
                .ok_or("Choose an unstarted plan item.")?;
            let label = db
                .query_row(
                    "SELECT label FROM tracks WHERE id=?1",
                    [&item.track_ref],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .unwrap_or_else(|| format!("New {}", item.track_ref.trim_start_matches("new:")));
            Source::PlanItem(PlanItem {
                plan_id: *plan_id,
                item_id: *item_id,
                title: item.title.clone(),
                label,
            })
        }
    };
    let mut context = read(db, input)?;
    context.source = Some(source);
    save(db, input, &context)
}
fn completed(db: &Connection, source: &Turn) -> Result<bool> {
    Ok(db.query_row("SELECT coalesce(json_extract(record,'$.terminal'),0) FROM timeline_turns WHERE session=?1 AND turn=?2 AND json_extract(record,'$.track')=?3",params![source.session_id,source.turn_id.to_string(),source.track_id],|row|row.get(0)).optional()?.unwrap_or(false))
}
fn outcome(db: &Connection, source: &Source) -> Result<(bool, Option<String>)> {
    match source {
        Source::Turn(turn) | Source::Clip { turn, .. } => Ok((completed(db, turn)?, None)),
        Source::PlanItem(item) => {
            let plans = crate::plan_store::list(db)?;
            let parent = crate::plan_dependencies::read(db, &plans, &item.item_id.to_string())?;
            let ready = parent
                .as_ref()
                .is_some_and(|parent| parent.state == crate::plans::ItemState::Succeeded)
                && !crate::plan_continuation::has_pending(db, &item.item_id.to_string())?;
            let detail = match parent.map(|p| p.state) {
                None => Some("Dependency unavailable"),
                Some(crate::plans::ItemState::Cut) => Some("Dependency was cut"),
                Some(crate::plans::ItemState::Failed) => Some("Dependency did not finish"),
                _ => None,
            };
            Ok((ready, detail.map(str::to_owned)))
        }
    }
}
pub(crate) fn projection(
    db: &Connection,
    input: &str,
    decision: &Decision,
) -> Result<Option<Value>> {
    let context = read(db, input)?;
    if !waits(decision, &context) {
        return Ok(None);
    }
    let (ready, detail) = context
        .source
        .as_ref()
        .map(|source| outcome(db, source))
        .transpose()?
        .unwrap_or((false, None));
    Ok(Some(
        json!({"source":context.source,"candidates":context.candidates,"ready":ready,"detail":detail}),
    ))
}
pub(crate) fn validate_dispatch(db: &Connection, input: &str) -> Result<()> {
    let raw: Option<String> = db
        .query_row(
            "SELECT decision FROM utterances WHERE id=?1",
            [input],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let decision = serde_json::from_str(&raw.ok_or(crate::journal::StartSuperseded)?)?;
    if projection(db, input, &decision)?.is_some_and(|value| value["ready"] != true) {
        return Err(crate::journal::StartSuperseded.into());
    }
    crate::request_graph::validate(db)
}

#[cfg(test)]
#[path = "wait_targets_tests.rs"]
mod tests;
