//! Planner defaults become ordinary, checkpoint-scoped assumptions only when
//! an owning item actually runs. A retry never resets an earlier answer.
use crate::{Result, assumptions::Assumption, plans::Plan};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use uuid::Uuid;

pub(crate) fn candidates(db: &Connection) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    for plan in crate::plan_store::list(db)? {
        if crate::plan_feedback::unavailable(db, &plan).is_some() {
            continue;
        }
        for assumption in plan
            .assumptions
            .iter()
            .filter(|value| value.kept_at.is_none())
        {
            values.push(json!({"id":assumption.id,"assumptionId":assumption.id,
                "planId":plan.id,"planRevision":plan.rev,"label":plan.summary,
                "text":assumption.what,"detail":format!("Assumed: {}",assumption.default)}));
        }
    }
    Ok(values)
}

pub(crate) fn keep(db: &Connection, plan: &mut Plan, id: Uuid) -> Result<()> {
    if let Some(reason) = crate::plan_feedback::unavailable(db, plan) {
        return Err(reason.into());
    }
    let value = plan
        .assumptions
        .iter_mut()
        .find(|value| value.id == id && value.kept_at.is_none())
        .ok_or("The plan assumption changed or was already answered.")?;
    value.kept_at = Some(crate::journal::timestamp());
    // This is an answer, not an edit to the scheduled work. The plan command's
    // operation ID is the replay guard; each template ID survives only while
    // its exact question and default remain in the accepted plan.
    Ok(())
}

pub(crate) fn adopt(db: &Connection, session: &str) -> Result<()> {
    let turns = db
        .prepare(
            "SELECT DISTINCT json_extract(record,'$.turnId') FROM timeline_clips
        WHERE json_extract(record,'$.sessionId')=?1
        AND json_extract(record,'$.turnId') IS NOT NULL
        AND json_extract(record,'$.repairRunId') IS NULL",
        )?
        .query_map([session], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for turn in turns {
        adopt_turn(db, session, &turn)?;
    }
    Ok(())
}

pub(crate) fn adopt_turn(db: &Connection, session: &str, turn: &str) -> Result<()> {
    let first: Option<(String, String)> = db
        .query_row(
            "SELECT id,record FROM timeline_clips
        WHERE json_extract(record,'$.sessionId')=?1 AND json_extract(record,'$.turnId')=?2
        AND json_extract(record,'$.repairRunId') IS NULL ORDER BY ordinal LIMIT 1",
            params![session, turn],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((clip, raw)) = first {
        let record: Value = serde_json::from_str(&raw)?;
        let origin = if let Some((plan_id, item_id)) =
            crate::plan_execution::turn_owner(db, session, turn)?
        {
            Some((crate::plan_store::read(db, plan_id)?, item_id))
        } else {
            let raw: Option<String> = db.query_row(
                "SELECT p.record FROM utterances u JOIN attachment_turns a ON a.command=u.dispatch_command
                 JOIN plans p ON p.source=u.id WHERE a.session=?1 AND a.turn=?2 AND a.inherited=0
                 AND u.revision=json_extract(p.record,'$.sourceRevision')",
                params![session,turn], |row| row.get(0)).optional()?;
            raw.map(|raw| serde_json::from_str::<Plan>(&raw))
                .transpose()?
                .filter(|plan| plan.state == crate::plans::State::SingleJob)
                .map(|plan| {
                    let id = plan.id;
                    (plan, id)
                })
        };
        let Some((plan, item_id)) = origin else {
            return Ok(());
        };
        for template in &plan.assumptions {
            let id = Uuid::new_v5(&template.id, item_id.as_bytes()).to_string();
            let source = format!("plan/{}/{}", template.id, item_id);
            let evidence = format!("Planner assumed: {} — {}", template.what, template.default);
            let value = Assumption {
                id: id.clone(),
                source: "planning".into(),
                source_id: source.clone(),
                track_id: record["trackId"]
                    .as_str()
                    .ok_or("The plan clip has no track.")?
                    .into(),
                clip_id: clip.clone(),
                question_id: None,
                question: template.what.clone(),
                options: vec![template.default.clone()],
                text: template.default.clone(),
                evidence: evidence.clone(),
                revision: 1,
                observed_at: Some(serde_json::from_value(record["start"].clone())?),
                state: if template.kept_at.is_some() {
                    "kept"
                } else {
                    "open"
                }
                .into(),
                kept_at: template.kept_at,
                changed_at: None,
                change_input_id: None,
            };
            let inserted = db.execute(
                "INSERT INTO assumptions VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO NOTHING",
                params![
                    id,
                    source,
                    clip,
                    value.state,
                    serde_json::to_string(&value)?
                ],
            )?;
            if inserted == 0 {
                continue;
            }
            db.execute(
                "INSERT INTO assumption_sources VALUES(?1,?2,?3,'complete',NULL,0)",
                params![source, clip, evidence],
            )?;
            crate::editor_log::assumed(db, &value)?;
            db.execute(
                "UPDATE timeline_clips SET record=record WHERE id=?1",
                [&clip],
            )?;
        }
    }
    Ok(())
}
