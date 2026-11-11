//! Shared conflict input/result ownership and consumer wakeups share one writer.
//! Frozen inputs remain available after the mutable context advances.
use crate::{
    Result,
    repair_run::{Owner, Purpose, Run},
    shared_context_store::Phase,
    shared_merge::Frozen,
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
pub(crate) enum Change {
    Freeze {
        request: crate::realign::Request,
        input: Box<Frozen>,
    },
    Begin(Box<Applied>),
    Complete(Uuid),
}
pub(crate) fn for_run(db: &Connection, run: &Run) -> Result<Frozen> {
    let Owner::Shared { id, revision } = run.owner else {
        return Err("This Run does not own a shared conflict.".into());
    };
    let key = Frozen::identity(id, revision);
    let raw:String=db.query_row("SELECT payload FROM operations WHERE id=?1 AND kind='shared.merge.inputs' AND state='complete'",[key.to_string()],|row|row.get(0))?;
    let input: Frozen = serde_json::from_str(&raw)?;
    if input.id != key
        || input.record.id != id
        || input.record.revision != revision
        || run.track != input.record.input.projection.source.track
        || run.purpose != Purpose::Merge
    {
        return Err("The shared merge source changed.".into());
    }
    input.files(run.id)?;
    Ok(input)
}
pub(crate) fn validate_start(db: &Connection, run: &Run) -> Result<Frozen> {
    let input = for_run(db, run)?;
    let current = crate::shared_context_store::read(db, input.record.id)?;
    if current.phase != Phase::NeedsMerge
        || serde_json::to_value(&current)? != serde_json::to_value(&input.record)?
    {
        return Err("The shared conflict changed before its Run started.".into());
    }
    let waiting:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM realign_jobs WHERE phase='needs-merge' AND json_extract(record,'$.sharedInput.id')=?1 AND json_extract(record,'$.sharedInput.revision')=?2)",params![current.id.to_string(),i64::try_from(current.revision)?],|row|row.get(0))?;
    if !waiting {
        return Err("No realignment is waiting for this shared conflict.".into());
    }
    Ok(input)
}
pub(crate) fn change(db: &mut Connection, change: Change) -> Result<()> {
    let tx = db.transaction()?;
    match change {
        Change::Freeze { request, input } => {
            input.files(input.id)?;
            let current = crate::shared_context_store::read(&tx, input.record.id)?;
            if serde_json::to_value(current)? != serde_json::to_value(&input.record)? {
                return Err("The shared source changed before freezing its merge.".into());
            }
            let mut root = crate::realign_queue::read(&tx, request.op_id)?
                .ok_or("Queue the realignment before resolving shared changes.")?;
            let context = crate::realign_store::context(&tx, &request)?;
            let crate::realign_store::Origin::Shared(shared) = &context.origin else {
                return Err("This realignment no longer needs a shared merge.".into());
            };
            let crate::shared_plan::Next::Project(next) =
                crate::shared_context_store::plan(&tx, &context.source, shared)?
            else {
                return Err("The shared source is already ready.".into());
            };
            if root.request != request
                || root.phase != crate::realign::Phase::Preparing
                || root.merge_input.is_some()
                || *next != input.record.input.projection
                || context.source.session_id != input.record.input.session
                || context.source.project.id != input.record.input.project.id
            {
                return Err("The shared merge belongs to another realignment.".into());
            }
            let old: Option<(String, String, String)> = tx
                .query_row(
                    "SELECT kind,payload,state FROM operations WHERE id=?1",
                    [input.id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((kind, raw, state)) = old {
                if kind != "shared.merge.inputs"
                    || state != "complete"
                    || serde_json::from_str::<serde_json::Value>(&raw)?
                        != serde_json::to_value(&input)?
                {
                    return Err("Shared merge input identity reused.".into());
                }
            } else {
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'shared.merge.inputs',?2,'complete')",
                    params![input.id.to_string(), serde_json::to_string(&input)?],
                )?;
            }
            root.shared_input = Some(input.summary());
            crate::realign_queue::save(&tx, &root)?;
        }
        Change::Begin(result) => {
            let old: Option<(String, String)> = tx
                .query_row(
                    "SELECT kind,payload FROM operations WHERE id=?1",
                    [result.id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((kind, raw)) = old {
                if kind != "shared.merge.result"
                    || serde_json::from_str::<serde_json::Value>(&raw)?
                        != serde_json::to_value(&result)?
                {
                    return Err("Shared merge result identity reused.".into());
                }
            } else {
                let mut run = crate::repair_run_store::read(&tx, result.run)?;
                let input = for_run(&tx, &run)?;
                let files = input.files(run.id)?;
                if result.id != Uuid::new_v5(&run.id, b"apply-shared-merge")
                    || result.revision != run.revision
                    || run.phase != crate::repair_run::Phase::Captured
                    || run.host.is_some()
                    || run.refresh.is_some()
                    || run.restoring.is_some()
                    || serde_json::to_value(&result.input)? != serde_json::to_value(&input)?
                    || (
                        &result.choice.base,
                        &result.choice.target,
                        &result.choice.incoming,
                    ) != (&files.base, &files.target, &files.incoming)
                {
                    return Err("The captured shared merge changed.".into());
                }
                validate_start(&tx, &run)?;
                result.choice.tree.parse::<intercat_git::ObjectId>()?;
                run.phase = crate::repair_run::Phase::Applying;
                run.revision = run
                    .revision
                    .checked_add(1)
                    .ok_or("Run revision overflow.")?;
                tx.execute(
                    "INSERT INTO operations VALUES(?1,'shared.merge.result',?2,'prepared')",
                    params![result.id.to_string(), serde_json::to_string(&result)?],
                )?;
                crate::repair_run_store::save(&tx, &run)?;
                crate::repair_run_store::record(&tx, "shared.merge.applying", &run)?;
            }
        }
        Change::Complete(id) => {
            let (raw, state): (String, String) = tx.query_row(
                "SELECT payload,state FROM operations WHERE id=?1 AND kind='shared.merge.result'",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            if state != "complete" {
                let result: Applied = serde_json::from_str(&raw)?;
                let mut run = crate::repair_run_store::read(&tx, result.run)?;
                let input = for_run(&tx, &run)?;
                let mut record = crate::shared_context_store::read(&tx, input.record.id)?;
                if !matches!(
                    run.phase,
                    crate::repair_run::Phase::Applying | crate::repair_run::Phase::NeedsRecovery
                ) || run.host.is_some()
                    || serde_json::to_value(&input)? != serde_json::to_value(&result.input)?
                    || serde_json::to_value(&record)? != serde_json::to_value(&input.record)?
                {
                    return Err("The shared merge result was superseded.".into());
                }
                record.solutions.push(result.choice);
                record.computed = None;
                record.phase = Phase::Pending;
                record.revision = record
                    .revision
                    .checked_add(1)
                    .ok_or("Shared revision overflow.")?;
                crate::shared_context_store::save(&tx, &record)?;
                let mut query=tx.prepare("SELECT id FROM realign_jobs WHERE phase NOT IN ('done','discarded') AND json_extract(record,'$.sharedInput.id')=?1 AND json_extract(record,'$.sharedInput.revision')=?2")?;
                let roots = query
                    .query_map(
                        params![
                            input.record.id.to_string(),
                            i64::try_from(input.record.revision)?
                        ],
                        |row| row.get::<_, String>(0),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(query);
                for root in roots {
                    let mut root = crate::realign_queue::read(&tx, Uuid::parse_str(&root)?)?
                        .ok_or("Missing shared consumer.")?;
                    root.shared_input = None;
                    root.phase = crate::realign::Phase::Queued;
                    root.error = None;
                    root.revision = root
                        .revision
                        .checked_add(1)
                        .ok_or("Realignment revision overflow.")?;
                    crate::realign_queue::save(&tx, &root)?;
                }
                run.phase = crate::repair_run::Phase::Done;
                run.failure = None;
                run.revision = run
                    .revision
                    .checked_add(1)
                    .ok_or("Run revision overflow.")?;
                crate::repair_run_store::save(&tx, &run)?;
                crate::repair_run_store::record(&tx, "shared.merge.applied", &run)?;
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
