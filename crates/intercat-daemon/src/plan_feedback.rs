//! A reply revises an unstarted plan; stale replies remain unsent words.
use crate::{
    Result,
    plans::{Fallback, Feedback, GenerationState, Plan, State},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reply {
    pub op_id: Uuid,
    pub plan_id: Uuid,
    pub revision: u64,
    pub question_id: Uuid,
    pub question: String,
    pub text: String,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS plan_feedback_replies(
        input TEXT PRIMARY KEY REFERENCES utterances(id) ON DELETE CASCADE,
        operation TEXT NOT NULL UNIQUE REFERENCES operations(id) ON DELETE CASCADE,
        record TEXT NOT NULL);",
    )?;
    Ok(())
}

pub(crate) fn unavailable(db: &Connection, plan: &Plan) -> Option<String> {
    if !matches!(plan.state, State::Draft | State::Held)
        || plan.generation.state != GenerationState::Ready
        || plan.rewrite.is_some()
    {
        return Some("Reply can revise a ready plan before it starts.".into());
    }
    plan.items.iter().find_map(|item| {
        crate::plan_edit::editable(db, item)
            .err()
            .map(|error| error.to_string())
    })
}

pub(crate) fn reply(db: &mut Connection, request: &Reply) -> Result<Value> {
    if request.op_id.get_version_num() != 7
        || request.revision == 0
        || request.text.trim().is_empty()
        || request.text.len() > 32 * 1024
        || request.question.len() > 8192
    {
        return Err("A plan reply needs an operation, a version and 1–32768 bytes of text.".into());
    }
    let payload = serde_json::to_string(request)?;
    db.pragma_update(None, "synchronous", "FULL")?;
    let tx = db.transaction()?;
    let prior: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, previous)) = prior {
        if kind != "plan.reply" || previous != payload {
            return Err("Operation ID reused.".into());
        }
        let record: String = tx.query_row(
            "SELECT record FROM plan_feedback_replies WHERE operation=?1",
            [request.op_id.to_string()],
            |row| row.get(0),
        )?;
        return Ok(serde_json::from_str::<Value>(&record)?["result"].clone());
    }
    let mut plan = crate::plan_store::read(&tx, request.plan_id).ok();
    let proposal = crate::plan_conflicts::candidates(&tx)?
        .into_iter()
        .find(|value| {
            value["id"] == request.question_id.to_string()
                && value["planId"] == request.plan_id.to_string()
        });
    let proposal =
        proposal.or(crate::plan_assumptions::candidates(&tx)?
            .into_iter()
            .find(|value| {
                value["id"] == request.question_id.to_string()
                    && value["planId"] == request.plan_id.to_string()
            }));
    let reason = if plan
        .as_ref()
        .is_none_or(|plan| plan.rev != request.revision)
    {
        Some("the question changed meanwhile".to_owned())
    } else if proposal.is_none() {
        Some("the question was answered meanwhile".to_owned())
    } else {
        unavailable(&tx, plan.as_ref().unwrap())
    };
    let question = proposal
        .as_ref()
        .map(|value| {
            format!(
                "{}\n{}",
                value["text"].as_str().unwrap(),
                value["detail"].as_str().unwrap()
            )
        })
        .unwrap_or_else(|| request.question.clone());
    let input = Uuid::new_v5(&request.op_id, b"plan-reply").to_string();
    crate::journal::insert_utterance(
        &tx,
        &input,
        &request.text,
        &crate::decision::Target::None,
        None,
        &[],
        None,
        true,
        None,
    )?;
    let state = if reason.is_some() {
        "orphaned"
    } else {
        "landed"
    };
    tx.execute(
        "UPDATE utterances SET state=?2,reason=?3 WHERE id=?1",
        params![input, state, reason],
    )?;
    if reason.is_none() {
        let plan = plan.as_mut().unwrap();
        crate::plan_store::retry_generation(&tx, plan)?;
        plan.feedback = Some(Feedback {
            instruction: request.text.clone(),
            pending: true,
            fallback: None,
        });
        crate::plan_store::save(&tx, plan, "plan.reply-queued")?;
    }
    let result = json!({"inputId":input,"planId":request.plan_id,"state":state});
    tx.execute(
        "INSERT INTO operations VALUES(?1,'plan.reply',?2,'complete')",
        params![request.op_id.to_string(), payload],
    )?;
    tx.execute(
        "INSERT INTO plan_feedback_replies VALUES(?1,?2,?3)",
        params![
            input,
            request.op_id.to_string(),
            json!({"question":question,"result":result}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(result)
}

pub(crate) fn projection(db: &Connection, input: &str) -> Result<Option<Value>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM plan_feedback_replies WHERE input=?1",
            [input],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw|->Result<Value>{let record:Value=serde_json::from_str(&raw)?;Ok(json!({"planId":record["result"]["planId"],"question":record["question"],"state":record["result"]["state"]}))}).transpose()
}

pub(crate) fn restore_reply(db: &Connection, input: &str) -> Result<()> {
    let reply:Option<String>=db.query_row("SELECT r.record FROM plan_feedback_replies r JOIN utterances u ON u.id=r.input WHERE r.input=?1 AND json_extract(u.target,'$.mode')='none'",[input],|row|row.get(0)).optional()?;
    if let Some(reply) = reply {
        let reply: Value = serde_json::from_str(&reply)?;
        let reason = if reply["result"]["state"] == "landed" {
            "the plan reply was already used"
        } else {
            "the question changed meanwhile"
        };
        db.execute(
            "UPDATE utterances SET state='orphaned',reason=?2 WHERE id=?1",
            params![input, reason],
        )?;
    }
    Ok(())
}

pub(crate) fn revision(plan: &Plan) -> Option<Value> {
    plan.feedback.as_ref().map(|feedback|json!({"kind":"plan",
        "instruction":if feedback.pending {feedback.instruction.as_str()} else {"Regenerate this current plan. Preserve its existing corrections and manual edits."},
        "summary":plan.summary,"assumptions":plan.assumptions,"items":plan.items.iter().map(crate::plan_edit::configuration).collect::<Vec<_>>()}))
}

pub(crate) fn capture_fallback(plan: &mut Plan) {
    let Some(feedback) = plan.feedback.as_mut() else {
        return;
    };
    feedback.fallback = Some(Fallback {
        summary: plan.summary.clone(),
        items: plan
            .items
            .iter()
            .filter(|item| item.state != crate::plans::ItemState::Cut)
            .map(crate::plan_edit::configuration)
            .collect(),
        removed_titles: plan
            .items
            .iter()
            .filter(|item| item.state == crate::plans::ItemState::Cut)
            .map(|item| item.title.clone())
            .collect(),
        pending_correction: feedback.pending.then(|| feedback.instruction.clone()),
    });
}

pub(crate) fn fallback(db: &Connection, input: &str) -> Result<Option<String>> {
    let Some(plan) =
        crate::plan_store::source(db, input)?.filter(|plan| plan.state == State::SingleJob)
    else {
        return Ok(None);
    };
    let revision: i64 = db.query_row(
        "SELECT revision FROM utterances WHERE id=?1",
        [input],
        |row| row.get(0),
    )?;
    if revision != plan.source_revision {
        return Ok(None);
    }
    let context = plan.feedback.and_then(|feedback| feedback.fallback);
    if context.is_none() && plan.assumptions.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!(
        "Carry out this plan as one job. Use its planning defaults unless later corrected. Revised items and pending corrections supersede contradictory original details. Never perform removed items.\n{}",
        serde_json::to_string(&json!({"revision":context,"assumptions":plan.assumptions}))?
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn one_job_without_a_reply_keeps_the_planners_defaults() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE plans(source TEXT,record TEXT); CREATE TABLE utterances(id TEXT,revision INTEGER);")?;
        let mut plan = Plan::new(Uuid::now_v7(), 1, true, None);
        plan.state = State::SingleJob;
        plan.assumptions = vec![crate::plans::PlanAssumption {
            id: Uuid::now_v7(),
            what: "Dummy format".into(),
            default: "JSON".into(),
            kept_at: None,
        }];
        db.execute(
            "INSERT INTO plans VALUES(?1,?2)",
            params![plan.source_id, serde_json::to_string(&plan)?],
        )?;
        db.execute("INSERT INTO utterances VALUES(?1,1)", [&plan.source_id])?;
        let instructions = fallback(&db, &plan.source_id)?.unwrap();
        assert!(instructions.contains("Dummy format") && instructions.contains("JSON"));
        Ok(())
    }

    #[test]
    fn fallback_keeps_pending_correction_and_removed_work_out_of_the_job() -> Result<()> {
        let db = Connection::open_in_memory()?;
        db.execute_batch("CREATE TABLE plans(source TEXT,record TEXT); CREATE TABLE utterances(id TEXT,revision INTEGER);")?;
        let mut plan = Plan::new(Uuid::now_v7(), 1, true, None);
        let item = crate::plans::Item {
            plan_item_id: Uuid::now_v7(),
            track_ref: "new:1".into(),
            folder_id: "dummy".into(),
            order: 0,
            title: "Keep this dummy job".into(),
            instruction: "Write a dummy file.".into(),
            constraints: vec![],
            done_when: "File exists.".into(),
            after: vec![],
            skip_order: vec![],
            start_requested: false,
            state: crate::plans::ItemState::Planned,
            utterance_id: None,
            detail: None,
        };
        plan.items.push(item.clone());
        let mut removed = item;
        removed.plan_item_id = Uuid::now_v7();
        removed.title = "Removed dummy job".into();
        removed.state = crate::plans::ItemState::Cut;
        plan.items.push(removed);
        plan.feedback = Some(Feedback {
            instruction: "Use the revised dummy name.".into(),
            pending: true,
            fallback: None,
        });
        capture_fallback(&mut plan);
        plan.state = State::SingleJob;
        db.execute(
            "INSERT INTO plans VALUES(?1,?2)",
            params![plan.source_id, serde_json::to_string(&plan)?],
        )?;
        db.execute(
            "INSERT INTO utterances VALUES(?1,?2)",
            params![plan.source_id, plan.source_revision],
        )?;
        let text = fallback(&db, &plan.source_id)?.unwrap();
        assert!(text.contains("Keep this dummy job"));
        assert!(text.contains("Use the revised dummy name."));
        db.execute("UPDATE utterances SET revision=revision+1", [])?;
        assert!(fallback(&db, &plan.source_id)?.is_none());
        let context = plan.feedback.unwrap().fallback.unwrap();
        assert_eq!(context.items.len(), 1);
        assert_eq!(context.removed_titles[0], "Removed dummy job");
        Ok(())
    }
}
