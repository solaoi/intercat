//! Resolve a dependency to its own turn, never the track's latest result.
use crate::{
    Result,
    plans::{ItemState, Plan},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub(crate) struct Turn {
    pub session: String,
    pub id: Uuid,
    pub checkpoint: Option<String>,
}

pub(crate) struct Parent {
    pub track: String,
    pub folder: String,
    pub title: String,
    pub state: ItemState,
    pub unclear: bool,
    pub turn: Option<Turn>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Location {
    pub id: String,
    pub plan_id: Option<Uuid>,
    pub item_id: Option<Uuid>,
    pub retryable: bool,
    pub unavailable: bool,
    pub track_id: String,
    pub title: String,
    pub state: ItemState,
    pub unclear: bool,
    pub clip_id: Option<String>,
    pub at_ms: Option<i64>,
}

/// Resolve the dependencies displayed with pending items even when their
/// source clip is outside the timeline's current fetch window.
pub(crate) fn projection(db: &Connection, plans: &[Plan]) -> Result<BTreeMap<String, Location>> {
    let referenced: BTreeSet<String> = plans
        .iter()
        .flat_map(|plan| &plan.items)
        .flat_map(|item| {
            std::iter::once(item.plan_item_id.to_string())
                .chain(item.after.iter().map(|after| after.id.clone()))
        })
        .collect();
    let mut result = BTreeMap::new();
    for id in referenced {
        let Some(parent) = read(db, plans, &id)? else {
            result.insert(
                id.clone(),
                Location {
                    id,
                    plan_id: None,
                    item_id: None,
                    retryable: false,
                    unavailable: true,
                    track_id: String::new(),
                    title: "Unavailable dependency".into(),
                    state: ItemState::Failed,
                    unclear: false,
                    clip_id: None,
                    at_ms: None,
                },
            );
            continue;
        };
        let owner = plans.iter().find_map(|plan| {
            plan.items
                .iter()
                .find(|item| item.plan_item_id.to_string() == id)
                .map(|item| (plan, item))
        });
        let track_id = owner
            .filter(|(_, item)| item.track_ref.starts_with("new:"))
            .map(|(plan, item)| format!("{}/{}", plan.id, item.track_ref))
            .unwrap_or_else(|| parent.track.clone());
        let point: Option<(String, Option<i64>)> = if let Some(turn) = &parent.turn {
            if owner.is_some() || crate::dependency_retry::latest(db, &id)?.is_some() {
                db.query_row("SELECT c.id,coalesce(json_extract(c.record,'$.end.timelineMs'),json_extract(c.record,'$.start.timelineMs')) FROM timeline_turns t JOIN timeline_clips c ON c.id=json_extract(t.record,'$.current') WHERE t.session=?1 AND t.turn=?2", params![turn.session,turn.id.to_string()], |row| Ok((row.get(0)?,row.get(1)?))).optional()?
            } else {
                db.query_row("SELECT id,coalesce(json_extract(record,'$.end.timelineMs'),json_extract(record,'$.start.timelineMs')) FROM timeline_clips WHERE id=?1",[&id],|row|Ok((row.get(0)?,row.get(1)?))).optional()?
            }
        } else {
            None
        };
        result.insert(
            id.clone(),
            Location {
                plan_id: owner.map(|(plan, _)| plan.id),
                item_id: owner.map(|(_, item)| item.plan_item_id),
                retryable: owner
                    .map(|(plan, item)| crate::plan_continuation::retryable(db, plan, item))
                    .transpose()?
                    .unwrap_or(crate::dependency_retry::retryable(db, plans, &id)?),
                unavailable: false,
                id,
                track_id,
                title: parent.title,
                state: parent.state,
                unclear: parent.unclear,
                clip_id: point.as_ref().map(|(id, _)| id.clone()),
                at_ms: point.and_then(|(_, at)| at),
            },
        );
    }
    Ok(result)
}

pub(crate) fn read(db: &Connection, plans: &[Plan], id: &str) -> Result<Option<Parent>> {
    if let Some(item) = plans
        .iter()
        .flat_map(|plan| &plan.items)
        .find(|item| item.plan_item_id.to_string() == id)
    {
        let origin = crate::plan_execution::item_origin(db, item)?;
        let outcome = if matches!(
            item.state,
            ItemState::Running | ItemState::Succeeded | ItemState::Failed
        ) {
            origin
                .as_ref()
                .map(|origin| crate::plan_execution::settled_outcome(db, origin))
                .transpose()?
                .flatten()
        } else {
            None
        };
        return Ok(Some(Parent {
            track: item.track_ref.clone(),
            folder: item.folder_id.clone(),
            title: item.title.clone(),
            state: outcome
                .as_ref()
                .map(|(state, _)| state.clone())
                .unwrap_or_else(|| item.state.clone()),
            unclear: outcome
                .as_ref()
                .map(|(_, detail)| detail.as_deref())
                .unwrap_or(item.detail.as_deref())
                == Some("Result unclear"),
            turn: origin.map(|origin| Turn {
                session: origin.identity.session_id,
                id: origin.identity.turn_id,
                checkpoint: origin.checkpoint,
            }),
        }));
    }
    let row: Option<(String, String, String)> = db
        .query_row(
            "SELECT c.record,t.label,f.folder FROM timeline_clips c JOIN tracks t ON t.id=c.track
         JOIN track_folders f ON f.track=c.track WHERE c.id=?1
         AND json_extract(c.record,'$.repairRunId') IS NULL",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((raw, label, folder)) = row else {
        return Ok(None);
    };
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Clip {
        track_id: String,
        session_id: String,
        turn_id: Option<Uuid>,
    }
    let clip: Clip = serde_json::from_str(&raw)?;
    let Some(mut turn) = clip.turn_id else {
        return Ok(None);
    };
    let mut session = clip.session_id;
    if let Some(attempt) = crate::dependency_retry::latest(db, id)? {
        let Some(origin) = crate::input_origin::read(db, attempt.input)? else {
            return Ok(Some(Parent {
                track: clip.track_id,
                folder,
                title: label,
                state: ItemState::Planned,
                unclear: false,
                turn: None,
            }));
        };
        if origin.track_id != clip.track_id {
            return Ok(None);
        }
        session = origin.identity.session_id;
        turn = origin.identity.turn_id;
    }
    let terminal: bool = db.query_row(
        "SELECT coalesce(json_extract(record,'$.terminal'),0) FROM timeline_turns WHERE session=?1 AND turn=?2",
        params![session, turn.to_string()], |row| row.get(0),
    ).optional()?.unwrap_or(false);
    let result = crate::report_result::final_report(db, &session, &turn.to_string())?;
    let completed = crate::report_result::terminal_status(db, &session, &turn.to_string())?
        .as_deref()
        == Some("completed");
    let state = if !terminal {
        ItemState::Running
    } else if !completed {
        ItemState::Failed
    } else if result
        .as_ref()
        .is_some_and(|result| result["kind"] == "done")
    {
        ItemState::Succeeded
    } else {
        ItemState::Failed
    };
    let checkpoint = crate::input_origin::turn_checkpoint(db, &clip.track_id, turn)?;
    Ok(Some(Parent {
        track: clip.track_id,
        folder,
        title: label,
        state,
        unclear: terminal && completed && result.is_none(),
        turn: Some(Turn {
            session,
            id: turn,
            checkpoint: checkpoint.map(|point| point.sha),
        }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_dependencies_keep_their_own_plan_track_and_cut_state() -> Result<()> {
        let db = Connection::open_in_memory()?;
        let mut parent = Plan::new(Uuid::now_v7(), 1, true, None);
        let mut child = Plan::new(Uuid::now_v7(), 1, true, None);
        let item = crate::plans::Item {
            plan_item_id: Uuid::now_v7(),
            track_ref: "new:1".into(),
            folder_id: "dummy".into(),
            order: 0,
            title: "Dummy parent".into(),
            instruction: "Dummy".into(),
            constraints: vec![],
            done_when: "Done".into(),
            after: vec![],
            skip_order: vec![],
            start_requested: false,
            state: ItemState::Cut,
            utterance_id: None,
            detail: None,
        };
        let mut dependent = item.clone();
        dependent.plan_item_id = Uuid::now_v7();
        dependent.state = ItemState::Planned;
        dependent.after = vec![crate::plans::Dependency {
            id: item.plan_item_id.to_string(),
            inherit: false,
        }];
        parent.items.push(item.clone());
        child.items.push(dependent);
        let result = projection(&db, &[parent.clone(), child])?;
        assert_eq!(result.len(), 2);
        let source = &result[&item.plan_item_id.to_string()];
        assert_eq!(source.track_id, format!("{}/new:1", parent.id));
        assert_eq!(source.state, ItemState::Cut);
        assert!(source.clip_id.is_none());
        assert!(source.at_ms.is_none());
        Ok(())
    }
}
