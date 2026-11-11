//! Durable application actions and the requests captured by a go-ahead.
use crate::{
    Result,
    decision::{Decision, Kind},
    input_start::{self, Permission},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum WaitingRequest {
    Input { input_id: Uuid, revision: i64 },
    Plan { plan_id: Uuid, revision: u64 },
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS input_command_candidates(
        input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE, record TEXT NOT NULL);
        CREATE INDEX IF NOT EXISTS waiting_for_go ON utterances(created_at,id)
        WHERE state='placed' AND json_extract(decision,'$.boundary')='when-told';",
    )?;
    Ok(())
}

pub(crate) fn capture(db: &Connection, input: &str) -> Result<()> {
    let mut query = db.prepare(
        "SELECT id,revision FROM utterances WHERE state='placed'
        AND json_extract(decision,'$.boundary')='when-told'
        AND json_extract(decision,'$.kind') IN ('fork','extend','schedule')
        AND NOT EXISTS(SELECT 1 FROM plan_inputs WHERE plan_inputs.input=utterances.id)
        AND NOT EXISTS(SELECT 1 FROM plans WHERE plans.source=utterances.id AND json_extract(plans.record,'$.state')<>'single-job')
        ORDER BY created_at,rowid",
    )?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut candidates = Vec::new();
    for (id, revision) in rows {
        if input_start::unavailable(db, &id, revision, Permission::GoAhead)?.is_none() {
            candidates.push(WaitingRequest::Input {
                input_id: id.parse()?,
                revision,
            });
        }
    }
    for plan in crate::plan_store::list(db)? {
        if plan.state == crate::plans::State::Draft
            && plan.generation.state == crate::plans::GenerationState::Ready
            && plan.rewrite.is_none()
            && plan
                .items
                .iter()
                .any(|item| item.state == crate::plans::ItemState::Planned)
        {
            candidates.push(WaitingRequest::Plan {
                plan_id: plan.id,
                revision: plan.rev,
            });
        }
    }
    db.execute("INSERT INTO input_command_candidates VALUES(?1,?2) ON CONFLICT(input) DO UPDATE SET record=excluded.record", params![input, serde_json::to_string(&candidates)?])?;
    Ok(())
}

fn captured(db: &Connection, input: &str) -> Result<Vec<WaitingRequest>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM input_command_candidates WHERE input=?1",
            [input],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?
        .unwrap_or_default())
}
fn is_go_ahead(decision: &Decision) -> bool {
    decision.kind == Kind::Command && decision.command.as_deref() == Some("start-waiting")
}
fn scoped(db: &Connection, input: &str, decision: &Decision) -> Result<Vec<WaitingRequest>> {
    let mut result = Vec::new();
    for candidate in captured(db, input)? {
        let matches = if let Some(track) = &decision.track_id {
            match &candidate {
                WaitingRequest::Input { input_id, .. } => {
                    db.query_row(
                        "SELECT json_extract(decision,'$.trackId') FROM utterances WHERE id=?1",
                        [input_id.to_string()],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()?
                    .flatten()
                    .as_ref()
                        == Some(track)
                }
                WaitingRequest::Plan { plan_id, .. } => crate::plan_store::read(db, *plan_id)
                    .is_ok_and(|plan| {
                        plan.items
                            .iter()
                            .filter(|item| item.state != crate::plans::ItemState::Cut)
                            .all(|item| &item.track_ref == track)
                    }),
            }
        } else {
            true
        };
        if matches {
            result.push(candidate);
        }
    }
    Ok(result)
}
fn unavailable(db: &Connection, candidate: &WaitingRequest) -> Result<Option<String>> {
    match candidate {
        WaitingRequest::Input { input_id, revision } => Ok(input_start::unavailable(
            db,
            &input_id.to_string(),
            *revision,
            Permission::GoAhead,
        )?
        .map(str::to_owned)),
        WaitingRequest::Plan { plan_id, revision } => {
            let Ok(plan) = crate::plan_store::read(db, *plan_id) else {
                return Ok(Some("This plan is unavailable.".into()));
            };
            if plan.rev != *revision {
                return Ok(Some("The plan changed. Read this request again.".into()));
            }
            if plan.state != crate::plans::State::Draft {
                return Ok(Some("This plan is no longer waiting for your go.".into()));
            }
            Ok(crate::plan_store::start_unavailable(db, &plan)?.map(str::to_owned))
        }
    }
}

pub(crate) fn projection(
    db: &Connection,
    input: &str,
    decision: &Decision,
) -> Result<Option<Value>> {
    if !is_go_ahead(decision) {
        return Ok(None);
    }
    let mut candidates = Vec::new();
    for candidate in scoped(db, input, decision)? {
        let (text, label) = match &candidate {
            WaitingRequest::Input { input_id, .. } => db.query_row(
                "SELECT coalesce(nullif(u.text,''),json_extract(s.record,'$.preview'),''),t.label FROM utterances u LEFT JOIN utterance_sources s ON s.input=u.id LEFT JOIN tracks t ON t.id=json_extract(u.decision,'$.trackId') WHERE u.id=?1",
                [input_id.to_string()], |row| Ok((row.get::<_,String>(0)?, row.get::<_,Option<String>>(1)?)),
            ).optional()?.unwrap_or_else(|| ("Request unavailable".into(), None)),
            WaitingRequest::Plan { plan_id, .. } => (crate::plan_store::read(db, *plan_id).map(|plan| plan.summary).unwrap_or_else(|_| "Plan unavailable".into()), None),
        };
        let mut characters = text.chars();
        let mut preview: String = characters.by_ref().take(200).collect();
        if characters.next().is_some() {
            preview.push('…');
        }
        let reason = unavailable(db, &candidate)?;
        let mut value = serde_json::to_value(&candidate)?;
        value["text"] = preview.into();
        value["label"] = json!(label);
        value["available"] = reason.is_none().into();
        value["reason"] = json!(reason);
        candidates.push(value);
    }
    Ok(Some(
        json!({"candidates":candidates,"started":decision.scores.get("commandResult")}),
    ))
}
pub(crate) fn begin(
    db: &Connection,
    input: &str,
    decision: &mut Decision,
) -> Result<Option<String>> {
    let candidates = scoped(db, input, decision)?;
    if candidates.len() != 1 {
        return Ok(Some(
            if candidates.is_empty() {
                "Nothing is waiting for your go."
            } else {
                "Which waiting request or plan should start?"
            }
            .into(),
        ));
    }
    if let Some(reason) = unavailable(db, &candidates[0])? {
        return Ok(Some(reason));
    }
    select(db, input, decision, &candidates[0])?;
    Ok(None)
}

pub(crate) fn select(
    db: &Connection,
    input: &str,
    decision: &mut Decision,
    choice: &WaitingRequest,
) -> Result<()> {
    if !is_go_ahead(decision) || !scoped(db, input, decision)?.contains(choice) {
        return Err("Choose a waiting request or plan shown with this command.".into());
    }
    if let Some(reason) = unavailable(db, choice)? {
        return Err(reason.into());
    }
    let source: Uuid = input.parse()?;
    let operation = Uuid::new_v5(&source, b"start-waiting");
    let started = match choice {
        WaitingRequest::Input { input_id, revision } => {
            input_start::start(
                db,
                &operation.to_string(),
                &input_id.to_string(),
                *revision,
                Permission::GoAhead,
            )?;
            WaitingRequest::Input {
                input_id: *input_id,
                revision: revision + 1,
            }
        }
        WaitingRequest::Plan { plan_id, .. } => {
            let mut plan = crate::plan_store::read(db, *plan_id)?;
            let before = plan.rev;
            crate::plan_store::start(db, &mut plan)?;
            crate::plan_history::advance_revision(db, plan.id, before, plan.rev)?;
            crate::plan_store::save(db, &plan, "plan.started-by-words")?;
            db.execute(
                "INSERT INTO operations VALUES(?1,'plan.go',?2,'complete')",
                params![operation.to_string(), serde_json::to_string(choice)?],
            )?;
            WaitingRequest::Plan {
                plan_id: *plan_id,
                revision: plan.rev,
            }
        }
    };
    decision.scores["commandResult"] = serde_json::to_value(started)?;
    Ok(())
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Presentation {
    pub command: String,
    pub operation_id: Uuid,
    pub receipt_id: Uuid,
    pub track_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_tracks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publication: Option<crate::push_review::Selection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restore: Option<Vec<crate::restore_review::Candidate>>,
}

pub(crate) fn prepare_presentation(db: &Connection, decision: &mut Decision) -> Result<bool> {
    if decision.kind != Kind::Command
        || !matches!(
            decision.command.as_deref(),
            Some(
                "commit"
                    | "push"
                    | "open-pr"
                    | "settings"
                    | "stage-mode"
                    | "branch-from"
                    | "restore"
            )
        )
    {
        return Ok(false);
    }
    let presentation = Presentation {
        command: decision.command.clone().unwrap(),
        operation_id: Uuid::now_v7(),
        receipt_id: Uuid::now_v7(),
        track_id: decision.track_id.clone(),
        commit_tracks: None,
        publication: None,
        restore: if decision.command.as_deref() == Some("restore") {
            Some(crate::restore_review::capture(
                db,
                decision.track_id.as_deref(),
            )?)
        } else {
            None
        },
    };
    decision.scores["presentation"] = serde_json::to_value(presentation)?;
    Ok(true)
}

pub(crate) fn presentation(decision: &Decision) -> Result<Option<Presentation>> {
    if decision.kind != Kind::Command
        || !matches!(
            decision.command.as_deref(),
            Some(
                "commit"
                    | "push"
                    | "open-pr"
                    | "settings"
                    | "stage-mode"
                    | "branch-from"
                    | "restore"
            )
        )
    {
        return Ok(None);
    }
    let Some(record) = decision.scores.get("presentation") else {
        return Ok(None);
    };
    let presentation: Presentation = serde_json::from_value(record.clone())?;
    if decision.command.as_deref() != Some(&presentation.command)
        || decision.track_id != presentation.track_id
    {
        return Ok(None);
    }
    if let Some(tracks) = &presentation.commit_tracks
        && (presentation.command != "commit"
            || tracks.is_empty()
            || tracks.len() > 256
            || tracks
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != tracks.len()
            || tracks.iter().any(|id| Uuid::parse_str(id).is_err()))
    {
        return Err("Invalid Commit review selection.".into());
    }
    if let Some(selection) = &presentation.publication {
        if !matches!(presentation.command.as_str(), "push" | "open-pr")
            || presentation.track_id.is_some()
        {
            return Err("Invalid publication review command.".into());
        }
        selection.validate()?;
    }
    match (presentation.command.as_str(), &presentation.restore) {
        ("restore", Some(candidates)) => {
            crate::restore_review::validate(candidates, presentation.track_id.as_deref())?;
        }
        ("restore", None) | (_, Some(_)) => return Err("Invalid Restore review command.".into()),
        _ => {}
    }
    Ok(Some(presentation))
}

pub(crate) fn presented(db: &Connection, input: &str, receipt: &str) -> Result<()> {
    let raw: String = db.query_row(
        "SELECT decision FROM utterances WHERE id=?1",
        [input],
        |row| row.get(0),
    )?;
    let decision: Decision = serde_json::from_str(&raw)?;
    let presentation = presentation(&decision)?.ok_or("This application action changed.")?;
    if presentation.receipt_id.to_string() != receipt {
        return Err("This application action has a different receipt.".into());
    }
    db.execute(
        "UPDATE utterances SET state='landed',revision=revision+1,reason=NULL WHERE id=?1",
        [input],
    )?;
    Ok(())
}

pub(crate) fn resume_presentation(
    db: &Connection,
    operation: &str,
    input: &str,
    revision: i64,
) -> Result<bool> {
    let (state, current, raw): (String, i64, Option<String>) = db.query_row(
        "SELECT state,revision,decision FROM utterances WHERE id=?1",
        [input],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let Some(raw) = raw else {
        return Ok(false);
    };
    let decision: Decision = serde_json::from_str(&raw)?;
    if decision.kind != Kind::Command {
        return Ok(false);
    }
    if state != "held"
        || current != revision
        || presentation(&decision)?.is_none()
        || crate::edit_hold::active(db, input)?
        || crate::confirmation::pending(db, input)?.is_some()
        || crate::decision_review::read(&decision)?.is_some()
    {
        return Err("Confirm or reread this application action before opening it.".into());
    }
    db.execute(
        "UPDATE utterances SET state='placed',revision=revision+1,reason=NULL WHERE id=?1",
        [input],
    )?;
    let payload = json!({"utteranceId":input,"revision":revision}).to_string();
    db.execute(
        "INSERT INTO operations VALUES(?1,'utterance.start',?2,'complete')",
        params![operation, payload],
    )?;
    db.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.started',?2)",
        params![crate::journal::timestamp(), payload],
    )?;
    Ok(true)
}
