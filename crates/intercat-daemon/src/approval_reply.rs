//! Explicit approval replies that must wait for the original turn to stop.
use crate::{Result, decision::Target, secret_input::Owner};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub op_id: Uuid,
    pub owner: Owner,
    pub track_id: String,
    pub session_id: String,
    pub turn_id: Uuid,
    pub approval_id: String,
    pub requirement: Value,
    pub text: String,
}

impl Request {
    pub fn stop_id(&self) -> Uuid {
        Uuid::new_v5(&self.op_id, b"stop-approval-turn")
    }
}

pub(crate) async fn confirm(journal: &crate::journal::Journal, id: Uuid) -> Result<()> {
    let mut changes = journal.subscribe();
    loop {
        changes.borrow_and_update();
        let saved = journal
            .operation(id)
            .await?
            .ok_or("The approval response was not saved.")?;
        if saved["state"] == "complete" {
            return Ok(());
        }
        if saved["kind"] != "approval.reply" || saved["state"] != "prepared" {
            return Err("The approval reply was held because its original repair changed. Your text has been kept.".into());
        }
        changes.changed().await?;
    }
}

pub(crate) fn queue(db: &mut Connection, request: Request) -> Result<()> {
    let payload = serde_json::to_string(&request)?;
    let tx = db.transaction()?;
    if let Some((kind, previous)) = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if kind != "approval.reply" || previous != payload {
            return Err("Approval reply operation ID reused.".into());
        }
        return Ok(());
    }
    if request.owner == Owner::Work {
        let input = Uuid::new_v5(&request.op_id, b"approval-reply").to_string();
        crate::journal::insert_utterance(
            &tx,
            &input,
            &request.text,
            &Target::Lock(request.track_id.clone()),
            None,
            &[],
            None,
        )?;
        let decision = json!({"kind":"trim","trackId":request.track_id,"boundary":"now","command":null,"confidence":1.0,"inferenceMs":0.0,"timedOut":false,"scores":{"approvalReply":request.op_id}});
        tx.execute(
            "UPDATE utterances SET state='redirecting',decision=?2 WHERE id=?1",
            params![input, decision.to_string()],
        )?;
        crate::redirect::queue_reply(
            &tx,
            crate::redirect::Redirect {
                input_id: input,
                revision: 1,
                track_id: request.track_id.clone(),
                session_id: request.session_id.clone(),
                turn_id: request.turn_id,
                phase: crate::redirect::Phase::Stopping,
            },
        )?;
    }
    tx.execute(
        "INSERT INTO operations VALUES(?1,'approval.reply',?2,?3)",
        params![
            request.op_id.to_string(),
            payload,
            if request.owner == Owner::Work {
                "complete"
            } else {
                "prepared"
            }
        ],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'approval.reply-queued',?2)",
        params![crate::journal::timestamp(), payload],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn advance(db: &mut Connection) -> Result<()> {
    let requests: Vec<Request> = db
        .prepare("SELECT payload FROM operations WHERE kind='approval.reply' AND state='prepared'")?
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|value| Ok(serde_json::from_str(&value?)?))
        .collect::<Result<_>>()?;
    for request in requests {
        let Owner::Repair { run_id } = request.owner else {
            continue;
        };
        let tx = db.transaction()?;
        let run = crate::repair_run_store::read(&tx, run_id)?;
        let moved = run.session.as_deref() != Some(&request.session_id)
            || run.turn != Some(request.turn_id)
            || matches!(
                run.phase,
                crate::repair_run::Phase::Discarded
                    | crate::repair_run::Phase::Done
                    | crate::repair_run::Phase::Failed
            );
        let stopped_again: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM operations later JOIN operations original ON original.id=?1
             WHERE later.rowid>original.rowid AND later.id<>?2 AND later.kind='repair.control'
             AND json_extract(later.payload,'$.request.id')=?3 AND json_extract(later.payload,'$.request.action')='stop')",
            params![request.op_id.to_string(),request.stop_id().to_string(),run_id.to_string()], |row| row.get(0),
        )?;
        if moved || stopped_again {
            tx.execute(
                "UPDATE operations SET state='superseded' WHERE id=?1",
                [request.op_id.to_string()],
            )?;
            tx.execute(
                "INSERT INTO events(ts,kind,payload) VALUES(?1,'approval.reply-held',?2)",
                params![
                    crate::journal::timestamp(),
                    serde_json::to_string(&request)?
                ],
            )?;
        } else if run.phase == crate::repair_run::Phase::Held
            && run.host.is_none()
            && run.restoring.is_none()
            && !crate::secret_input_store::repair_pending(&tx, run.id)?
            && !run
                .refresh
                .as_ref()
                .is_some_and(|refresh| refresh.error.is_none())
        {
            let result = crate::repair_run_store::followup_in(
                &tx,
                crate::repair_run::FollowUp {
                    op_id: Uuid::new_v5(&request.op_id, b"approval-reply"),
                    id: run_id,
                    revision: run.revision,
                    text: request.text.clone(),
                    selection: None,
                },
            );
            match result {
                Ok(_) => {
                    tx.execute(
                        "UPDATE operations SET state='complete' WHERE id=?1",
                        [request.op_id.to_string()],
                    )?;
                }
                Err(error) if error.is::<crate::repair_run::Rejected>() => {
                    tx.execute(
                        "UPDATE operations SET state='superseded' WHERE id=?1",
                        [request.op_id.to_string()],
                    )?;
                    tx.execute(
                        "INSERT INTO events(ts,kind,payload) VALUES(?1,'approval.reply-held',?2)",
                        params![
                            crate::journal::timestamp(),
                            serde_json::to_string(&request)?
                        ],
                    )?;
                }
                Err(error) => return Err(error),
            }
        }
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repair_reply_confirmation_waits_for_completion_and_preserves_held_text() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        let journal = crate::journal::Journal::open(
            root.path().join("db"),
            crate::secrets::Key::new([7; 32]),
        )
        .await?;
        for state in ["complete", "superseded"] {
            let id = Uuid::now_v7();
            journal
                .intent(
                    &id.to_string(),
                    "approval.reply",
                    json!({"text":"Keep this reply"}),
                )
                .await?;
            let waiting = journal.clone();
            let mut confirmation = tokio::spawn(async move { confirm(&waiting, id).await });
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(50), &mut confirmation)
                    .await
                    .is_err()
            );
            if state == "complete" {
                journal.finish(&id.to_string(), "dispatched").await?;
            }
            journal.finish(&id.to_string(), state).await?;
            assert_eq!(confirmation.await?.is_ok(), state == "complete");
            assert_eq!(
                journal.operation(id).await?.unwrap()["payload"]["text"],
                "Keep this reply"
            );
        }
        Ok(())
    }
}
