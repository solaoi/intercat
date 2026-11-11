//! Journal scope for an app-owned undo conflict Run.
use crate::{
    Result,
    repair_run::{Owner, Purpose, Run},
    undo_merge::Frozen,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Applied {
    pub id: Uuid,
    pub run: Uuid,
    pub revision: u64,
    pub input: Frozen,
    pub choice: crate::merge_choices::Solved,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Saved {
    result: Applied,
    captured_revision: Option<u64>,
}
pub(crate) enum Change {
    Begin(Box<Applied>),
    Admit(Uuid),
    Dispatch(Uuid),
    Complete(Uuid),
}

fn saved(db: &Connection, id: Uuid) -> Result<(Saved, String)> {
    let (raw, state): (String, String) = db.query_row(
        "SELECT payload,state FROM operations WHERE id=?1 AND kind='undo.merge.result'",
        [id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((serde_json::from_str(&raw)?, state))
}
pub(crate) fn change(
    db: &mut Connection,
    change: Change,
) -> Result<Option<crate::integration::Job>> {
    let tx = db.transaction()?;
    let result = match change {
        Change::Begin(result) => {
            let old: Option<(String, String)> = tx
                .query_row(
                    "SELECT kind,payload FROM operations WHERE id=?1",
                    [result.id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((kind, raw)) = old {
                if kind != "undo.merge.result"
                    || serde_json::to_value(serde_json::from_str::<Saved>(&raw)?.result)?
                        != serde_json::to_value(&result)?
                {
                    return Err("Undo merge result identity reused.".into());
                }
            } else {
                let mut run = crate::repair_run_store::read(&tx, result.run)?;
                let input = for_run(&tx, &run)?;
                let files = input.files(run.id)?;
                if result.id != Uuid::new_v5(&run.id, b"apply-undo-merge")
                    || result.revision != run.revision
                    || run.phase != crate::repair_run::Phase::Captured
                    || run.host.is_some()
                    || run.refresh.is_some()
                    || run.restoring.is_some()
                    || serde_json::to_value(&input)? != serde_json::to_value(&result.input)?
                    || (
                        &result.choice.base,
                        &result.choice.target,
                        &result.choice.incoming,
                    ) != (&files.base, &files.target, &files.incoming)
                {
                    return Err("The captured undo merge changed.".into());
                }
                result.choice.tree.parse::<intercat_git::ObjectId>()?;
                run.phase = crate::repair_run::Phase::Applying;
                run.revision = run
                    .revision
                    .checked_add(1)
                    .ok_or("Run revision overflow.")?;
                let id = result.id;
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'undo.merge.result',?2,'prepared')",
                    params![
                        id.to_string(),
                        serde_json::to_string(&Saved {
                            result: *result,
                            captured_revision: None
                        })?
                    ],
                )?;
                crate::repair_run_store::save(&tx, &run)?;
                crate::repair_run_store::record(&tx, "undo.merge.applying", &run)?;
            }
            None
        }
        Change::Admit(id) => {
            let (mut saved, state) = saved(&tx, id)?;
            if state == "prepared" {
                let result = &saved.result;
                let mut run = crate::repair_run_store::read(&tx, result.run)?;
                let input = for_run(&tx, &run)?;
                if !matches!(
                    run.phase,
                    crate::repair_run::Phase::Applying | crate::repair_run::Phase::NeedsRecovery
                ) || run.host.is_some()
                    || serde_json::to_value(&input)? != serde_json::to_value(&result.input)?
                {
                    return Err("The undo merge result was superseded.".into());
                }
                let mut job = crate::integration_store::read(&tx, input.job)?;
                let undo = job.undo.as_mut().ok_or("Missing frozen undo plan.")?;
                undo.resolutions.push(result.choice.clone());
                job.revision = job
                    .revision
                    .checked_add(1)
                    .ok_or("Review revision overflow.")?;
                job.candidate = None;
                job.kept_secrets.clear();
                job.error = None;
                let plan = job.plan.as_mut().ok_or("Missing frozen target.")?;
                plan.tree = None;
                plan.undo_conflict = None;
                for row in &mut plan.rows {
                    row.state = crate::integration::RowState::Waiting;
                    row.tree = None;
                    row.delta_base = None;
                    row.changes.clear();
                    row.conflicts.clear();
                }
                crate::integration_store::copy_settings(
                    &tx,
                    job.id,
                    input.revision,
                    job.revision,
                    None,
                )?;
                crate::replacement_review_store::refresh(&tx, &mut job)?;
                job.status = crate::integration::Status::Preparing;
                crate::integration_store::save(&tx, &job)?;
                run.phase = crate::repair_run::Phase::Done;
                run.failure = None;
                run.revision = run
                    .revision
                    .checked_add(1)
                    .ok_or("Run revision overflow.")?;
                crate::repair_run_store::save(&tx, &run)?;
                crate::repair_run_store::record(&tx, "undo.merge.applied", &run)?;
                saved.captured_revision = Some(job.revision);
                tx.execute(
                    "UPDATE operations SET payload=?2,state='dispatched' WHERE id=?1",
                    params![id.to_string(), serde_json::to_string(&saved)?],
                )?;
            } else if !matches!(state.as_str(), "dispatched" | "complete") {
                return Err("The undo result has an invalid state.".into());
            }
            None
        }
        Change::Dispatch(id) => {
            let (saved, state) = saved(&tx, id)?;
            if state == "dispatched" {
                let job = crate::integration_store::read(&tx, saved.result.input.job)?;
                (Some(job.revision) == saved.captured_revision).then_some(job)
            } else if state == "complete" {
                None
            } else {
                return Err("The undo result is not admitted yet.".into());
            }
        }
        Change::Complete(id) => {
            let (_, state) = saved(&tx, id)?;
            if !matches!(state.as_str(), "dispatched" | "complete") {
                return Err("The undo result has not been admitted.".into());
            }
            tx.execute(
                "UPDATE operations SET state='complete' WHERE id=?1",
                [id.to_string()],
            )?;
            None
        }
    };
    tx.commit()?;
    Ok(result)
}

pub(crate) fn result(db: &Connection, id: Uuid) -> Result<(Applied, String)> {
    let (saved, state) = saved(db, id)?;
    Ok((saved.result, state))
}

pub(crate) fn for_run(db: &Connection, run: &Run) -> Result<Frozen> {
    let Owner::Undo { id, revision } = run.owner else {
        return Err("This Run does not own an undo conflict.".into());
    };
    let job = crate::integration_store::read(db, id)?;
    if job.revision != revision || run.purpose != Purpose::Merge {
        return Err("The undo review changed while its merge was active.".into());
    }
    let settings = crate::integration_store::settings(db, id, revision)?
        .ok_or("Missing frozen undo settings.")?;
    let input = Frozen::capture(&job, settings)?;
    if input.track != run.track {
        return Err("The undo merge source track changed.".into());
    }
    Ok(input)
}
