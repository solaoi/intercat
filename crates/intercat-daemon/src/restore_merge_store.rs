//! Imported solutions advance only their Restore, atomically with the Run.
//! Immutable conflict inputs remain readable after the next stage is admitted.
use crate::{
    Result,
    path_restore_store::{Application, Plan},
    repair_run::{Owner, Purpose, Run},
    restore_merge::Frozen,
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
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Saved {
    pub result: Applied,
    pub next: Option<Plan>,
}
pub(crate) enum Change {
    Freeze(Box<Frozen>),
    Begin(Box<Applied>),
    Plan { id: Uuid, next: Box<Plan> },
    Complete(Uuid),
}
pub(crate) fn result(db: &Connection, id: Uuid) -> Result<(Saved, String)> {
    let (raw, state): (String, String) = db.query_row(
        "SELECT payload,state FROM operations WHERE id=?1 AND kind='restore.merge.result'",
        [id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok((serde_json::from_str(&raw)?, state))
}
pub(crate) fn for_run(db: &Connection, run: &Run) -> Result<Frozen> {
    let Owner::Restore { id, revision } = run.owner else {
        return Err("This Run does not own a Restore merge.".into());
    };
    let key = Frozen::identity(id, revision);
    let raw:String=db.query_row("SELECT payload FROM operations WHERE id=?1 AND kind='restore.merge.inputs' AND state='complete'",[key.to_string()],|row|row.get(0))?;
    let input: Frozen = serde_json::from_str(&raw)?;
    if input.id != key
        || input.request.op_id != id
        || input.revision != revision
        || input.source.entry.track != run.track
        || run.purpose != Purpose::Merge
    {
        return Err("The Restore merge source belongs to another Run.".into());
    }
    input.files(run.id)?;
    Ok(input)
}
pub(crate) fn validate_start(db: &Connection, run: &Run) -> Result<Frozen> {
    let input = for_run(db, run)?;
    let (app, state) = crate::path_restore_store::application(db, &input.request)?
        .ok_or("Missing Restore file intent.")?;
    let job = crate::path_restore_store::read(db, &input.request)?;
    if state != "prepared"
        || !input.matches(&app)?
        || job.phase != crate::path_restore::Phase::NeedsMerge
        || job.merge_input.as_ref() != Some(&input.summary())
    {
        return Err("The Restore conflict changed before this merge Run.".into());
    }
    Ok(input)
}
fn validate_next(input: &Frozen, choice: &crate::merge_choices::Solved, next: &Plan) -> Result<()> {
    let mut expected = input.inputs.clone();
    expected.resolutions.push(choice.clone());
    let (inputs, anchor) = match next {
        Plan::Files(files) => (&files.prepared.inputs, &files.anchor),
        Plan::Conflict { inputs, anchor, .. } => (inputs.as_ref(), anchor),
        Plan::Reopen => return Err("A cancelled Restore cannot become a plain reopen.".into()),
    };
    if *inputs != expected || *anchor != input.anchor {
        return Err("The next Restore plan has different frozen inputs.".into());
    }
    Ok(())
}
pub(crate) fn change(db: &mut Connection, change: Change) -> Result<()> {
    let tx = db.transaction()?;
    match change {
        Change::Freeze(input) => {
            input.files(input.id)?;
            let (app, state) = crate::path_restore_store::application(&tx, &input.request)?
                .ok_or("Missing Restore file intent.")?;
            let mut job = crate::path_restore_store::read(&tx, &input.request)?;
            if state != "prepared"
                || !input.matches(&app)?
                || matches!(
                    job.phase,
                    crate::path_restore::Phase::Held
                        | crate::path_restore::Phase::Done
                        | crate::path_restore::Phase::Failed
                )
            {
                return Err("The Restore conflict changed before freezing its merge.".into());
            }
            let old: Option<(String, String, String)> = tx
                .query_row(
                    "SELECT kind,payload,state FROM operations WHERE id=?1",
                    [input.id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((kind, raw, state)) = old {
                if kind != "restore.merge.inputs"
                    || state != "complete"
                    || serde_json::from_str::<serde_json::Value>(&raw)?
                        != serde_json::to_value(&input)?
                {
                    return Err("Restore merge input identity reused.".into());
                }
            } else {
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'restore.merge.inputs',?2,'complete')",
                    params![input.id.to_string(), serde_json::to_string(&input)?],
                )?;
            }
            if job.merge_input.as_ref() != Some(&input.summary())
                || job.phase != crate::path_restore::Phase::NeedsMerge
            {
                job.merge_input = Some(input.summary());
                job.phase = crate::path_restore::Phase::NeedsMerge;
                job.error = None;
                crate::path_restore_store::save_job(&tx, &mut job)?;
            }
        }
        Change::Begin(applied) => {
            let old: Option<(String, String)> = tx
                .query_row(
                    "SELECT kind,payload FROM operations WHERE id=?1",
                    [applied.id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((kind, raw)) = old {
                if kind != "restore.merge.result"
                    || serde_json::to_value(serde_json::from_str::<Saved>(&raw)?.result)?
                        != serde_json::to_value(&applied)?
                {
                    return Err("Restore merge result identity reused.".into());
                }
            } else {
                let mut run = crate::repair_run_store::read(&tx, applied.run)?;
                let input = validate_start(&tx, &run)?;
                let files = input.files(run.id)?;
                if applied.id != Uuid::new_v5(&run.id, b"apply-restore-merge")
                    || applied.revision != run.revision
                    || run.phase != crate::repair_run::Phase::Captured
                    || run.host.is_some()
                    || run.refresh.is_some()
                    || run.restoring.is_some()
                    || serde_json::to_value(&input)? != serde_json::to_value(&applied.input)?
                    || (
                        &applied.choice.base,
                        &applied.choice.target,
                        &applied.choice.incoming,
                    ) != (&files.base, &files.target, &files.incoming)
                {
                    return Err("The captured Restore merge changed.".into());
                }
                applied.choice.tree.parse::<intercat_git::ObjectId>()?;
                run.phase = crate::repair_run::Phase::Applying;
                run.revision = run
                    .revision
                    .checked_add(1)
                    .ok_or("Run revision overflow.")?;
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'restore.merge.result',?2,'prepared')",
                    params![
                        applied.id.to_string(),
                        serde_json::to_string(&Saved {
                            result: *applied,
                            next: None
                        })?
                    ],
                )?;
                crate::repair_run_store::save(&tx, &run)?;
                crate::repair_run_store::record(&tx, "restore.merge.applying", &run)?;
            }
        }
        Change::Plan { id, next } => {
            let (mut saved, state) = result(&tx, id)?;
            if let Some(old) = &saved.next {
                if serde_json::to_value(old)? != serde_json::to_value(&next)? {
                    return Err("The saved Restore merge continuation changed.".into());
                }
            } else {
                if state != "prepared" {
                    return Err("The Restore merge result is not ready for a plan.".into());
                }
                let run = crate::repair_run_store::read(&tx, saved.result.run)?;
                if !matches!(
                    run.phase,
                    crate::repair_run::Phase::Applying | crate::repair_run::Phase::NeedsRecovery
                ) || run.host.is_some()
                {
                    return Err("Close the Restore merge host before planning its result.".into());
                }
                let input = validate_start(&tx, &run)?;
                if serde_json::to_value(&input)? != serde_json::to_value(&saved.result.input)? {
                    return Err("The Restore result belongs to another conflict.".into());
                }
                validate_next(&input, &saved.result.choice, &next)?;
                saved.next = Some(*next);
                tx.execute(
                    "UPDATE operations SET payload=?2,state='dispatched' WHERE id=?1",
                    params![id.to_string(), serde_json::to_string(&saved)?],
                )?;
            }
        }
        Change::Complete(id) => {
            let (saved, state) = result(&tx, id)?;
            if state != "complete" {
                if state != "dispatched" {
                    return Err(
                        "Protect the Restore merge continuation before admitting it.".into(),
                    );
                }
                let mut run = crate::repair_run_store::read(&tx, saved.result.run)?;
                if !matches!(
                    run.phase,
                    crate::repair_run::Phase::Applying | crate::repair_run::Phase::NeedsRecovery
                ) || run.host.is_some()
                {
                    return Err("The Restore merge Run cannot release its result yet.".into());
                }
                let input = validate_start(&tx, &run)?;
                if serde_json::to_value(&input)? != serde_json::to_value(&saved.result.input)? {
                    return Err("The saved Restore result was superseded.".into());
                }
                let next = saved
                    .next
                    .ok_or("Missing protected Restore continuation.")?;
                validate_next(&input, &saved.result.choice, &next)?;
                let mut job = crate::path_restore_store::read(&tx, &input.request)?;
                job.phase = match next {
                    Plan::Files(_) => crate::path_restore::Phase::Queued,
                    Plan::Conflict { .. } => crate::path_restore::Phase::NeedsMerge,
                    Plan::Reopen => unreachable!(),
                };
                let app = Application {
                    request: input.request.clone(),
                    source: input.source.clone(),
                    plan: next,
                };
                tx.execute("UPDATE operations SET payload=?2,state='prepared' WHERE id=?1 AND kind='workspace.restorePath'",params![crate::path_restore::file_operation(&input.request).to_string(),serde_json::to_string(&app)?])?;
                job.merge_input = None;
                job.error = None;
                job.inspection = None;
                crate::path_restore_store::save_job(&tx, &mut job)?;
                run.phase = crate::repair_run::Phase::Done;
                run.failure = None;
                run.revision = run
                    .revision
                    .checked_add(1)
                    .ok_or("Run revision overflow.")?;
                crate::repair_run_store::save(&tx, &run)?;
                crate::repair_run_store::record(&tx, "restore.merge.applied", &run)?;
                tx.execute(
                    "UPDATE operations SET state='complete' WHERE id=?1",
                    [id.to_string()],
                )?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}
