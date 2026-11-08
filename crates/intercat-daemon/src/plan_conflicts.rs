//! Planner findings belong to the accepted placement, not later file scans.
use crate::{
    Result,
    plans::{Context, DraftConflict, GenerationState, ItemState, Plan, State},
    proposals::Action,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS plan_conflicts(
        id TEXT PRIMARY KEY, plan TEXT NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
        track_a TEXT NOT NULL, track_b TEXT NOT NULL, detail TEXT NOT NULL,
        created_at INTEGER NOT NULL);",
    )?;
    Ok(())
}

pub(crate) fn replace(db: &Connection, plan: &Plan, conflicts: &[DraftConflict]) -> Result<()> {
    db.execute(
        "DELETE FROM plan_conflicts WHERE plan=?1",
        [plan.id.to_string()],
    )?;
    for conflict in conflicts {
        let id = Uuid::new_v5(
            &plan.generation.id,
            &serde_json::to_vec(&(&conflict.track_a, &conflict.track_b, &conflict.what))?,
        );
        db.execute(
            "INSERT INTO plan_conflicts VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                id.to_string(),
                plan.id.to_string(),
                conflict.track_a,
                conflict.track_b,
                conflict.what.trim(),
                crate::journal::timestamp()
            ],
        )?;
    }
    Ok(())
}

pub(crate) fn assign_tracks(
    db: &Connection,
    plan: Uuid,
    assigned: &BTreeMap<String, Uuid>,
) -> Result<()> {
    for (reference, track) in assigned {
        db.execute(
            "UPDATE plan_conflicts SET
            track_a=CASE WHEN track_a=?2 THEN ?3 ELSE track_a END,
            track_b=CASE WHEN track_b=?2 THEN ?3 ELSE track_b END WHERE plan=?1",
            params![plan.to_string(), reference, track.to_string()],
        )?;
    }
    Ok(())
}

fn pair_link(child: &str, parent: &str, a: &str, b: &str) -> bool {
    child == a && parent == b || child == b && parent == a
}

fn parallel_blocker(plan: &Plan, a: &str, b: &str, context: &Context) -> Option<&'static str> {
    if !matches!(plan.state, State::Draft | State::Held) {
        return Some("Hold this plan before changing its order.");
    }
    for item in plan
        .items
        .iter()
        .filter(|item| item.state != ItemState::Cut && (item.track_ref == a || item.track_ref == b))
    {
        if item.state != ItemState::Planned {
            return Some("A step has already started. Its dependencies cannot change.");
        }
        if item.start_requested {
            return Some("Hold these steps before changing their order.");
        }
        for dependency in &item.after {
            let parent = plan
                .items
                .iter()
                .find(|parent| parent.plan_item_id.to_string() == dependency.id)
                .map(|parent| parent.track_ref.as_str())
                .or_else(|| {
                    context
                        .external
                        .get(&dependency.id)
                        .map(|parent| parent.track_id.as_str())
                });
            if dependency.inherit
                && parent.is_some_and(|parent| pair_link(&item.track_ref, parent, a, b))
            {
                return Some(
                    "These steps depend on inherited files. Review their dependencies before running together.",
                );
            }
        }
    }
    None
}

pub(crate) fn candidates(db: &Connection) -> Result<Vec<Value>> {
    let rows = db.prepare("SELECT id,plan,track_a,track_b,detail,created_at FROM plan_conflicts ORDER BY created_at,rowid")?
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
            row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, i64>(5)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut values = Vec::new();
    for (id, plan_id, a, b, detail, at) in rows {
        let answered: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM proposal_answers
            WHERE json_extract(record,'$.source.conflictId')=?1)",
            [&id],
            |row| row.get(0),
        )?;
        if answered {
            continue;
        }
        let plan = crate::plan_store::read(db, plan_id.parse()?)?;
        if plan.state == State::SingleJob
            || plan.generation.state != GenerationState::Ready
            || plan.rewrite.is_some()
            || !plan.items.iter().any(|item| {
                item.state == ItemState::Planned && (item.track_ref == a || item.track_ref == b)
            })
        {
            continue;
        }
        let context = crate::plan_store::context(db, plan.id)?;
        let folder = |track: &str| {
            plan.items
                .iter()
                .find(|item| item.track_ref == track && item.state != ItemState::Cut)
                .map(|item| item.folder_id.as_str())
                .or_else(|| context.tracks.get(track).map(String::as_str))
        };
        if folder(&a).is_none() || folder(&a) != folder(&b) {
            continue;
        }
        let label = |track: &str| -> Result<String> {
            Ok(if let Some(number) = track.strip_prefix("new:") {
                format!("New track {number}")
            } else {
                db.query_row("SELECT label FROM tracks WHERE id=?1", [track], |row| {
                    row.get(0)
                })?
            })
        };
        let (label_a, label_b) = (label(&a)?, label(&b)?);
        let proposal_id = Uuid::new_v5(&id.parse()?, &plan.rev.to_le_bytes());
        let session = crate::timeline_history::session(db)?;
        values.push(json!({"id":proposal_id,"kind":"plan-conflict","planId":plan.id,"planRevision":plan.rev,"replyBlockedReason":crate::plan_feedback::unavailable(db,&plan),
            "label":format!("{label_a} + {label_b}"), "createdAt":at,
            "text":format!("{label_a} and {label_b} may conflict"),
            "detail":format!("Planned overlap: {detail}\nFine keeps the current plan. Run together removes only ordering links between these tracks in this plan; other start conditions still apply."),
            "source":{"sessionId":session,"planId":plan.id,"planRevision":plan.rev,
                "conflictId":id,"trackA":a,"trackB":b},
            "options":[{"action":"keep-plan","text":"Fine"},
                {"action":"parallel-plan","text":"Run together","disabledReason":parallel_blocker(&plan,&a,&b,&context)}]}));
    }
    Ok(values)
}

pub(crate) fn respond(db: &Connection, proposal: &Value, action: &Action) -> Result<Value> {
    let source = &proposal["source"];
    let mut plan = crate::plan_store::read(
        db,
        source["planId"].as_str().ok_or("Missing plan.")?.parse()?,
    )?;
    plan.require_revision(
        source["planRevision"]
            .as_u64()
            .ok_or("Missing plan revision.")?,
    )?;
    match action {
        Action::KeepPlan => {}
        Action::ParallelPlan => {
            let a = source["trackA"].as_str().ok_or("Missing first track.")?;
            let b = source["trackB"].as_str().ok_or("Missing second track.")?;
            let context = crate::plan_store::context(db, plan.id)?;
            if let Some(reason) = parallel_blocker(&plan, a, b, &context) {
                return Err(reason.into());
            }
            let before = plan.clone();
            let tracks: BTreeMap<_, _> = plan
                .items
                .iter()
                .map(|item| (item.plan_item_id.to_string(), item.track_ref.clone()))
                .collect();
            for item in plan
                .items
                .iter_mut()
                .filter(|item| item.state == ItemState::Planned)
            {
                item.after.retain(|dependency| {
                    let parent = tracks.get(&dependency.id).map(String::as_str).or_else(|| {
                        context
                            .external
                            .get(&dependency.id)
                            .map(|parent| parent.track_id.as_str())
                    });
                    dependency.inherit
                        || !parent.is_some_and(|parent| pair_link(&item.track_ref, parent, a, b))
                });
            }
            crate::plan_edit::finish(db, &before, &mut plan)?;
            crate::plan_history::record(db, &before, &plan, "Run planned tracks together")?;
            crate::plan_store::save(db, &plan, "plan.conflict-resolved")?;
        }
        _ => return Err("This action is not a plan conflict choice.".into()),
    }
    Ok(
        json!({"proposalId":proposal["id"],"action":action,"planId":plan.id,
        "planRevision":plan.rev,"source":source}),
    )
}
