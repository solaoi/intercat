//! A plan may start automatically only while a window keeps observing it.
use crate::{
    Result,
    clock::Stamp,
    plans::{GenerationState, Plan, State},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[cfg(test)]
#[path = "plan_start_tests.rs"]
mod tests;

const DELAY_MS: i64 = 5_000;
const MAX_GAP_MS: i64 = 1_000;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Mode {
    #[default]
    Manual,
    AfterDelay,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Setting {
    pub revision: u64,
    pub mode: Mode,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Change {
    pub op_id: Uuid,
    pub revision: u64,
    pub mode: Mode,
}

pub(crate) fn read(db: &Connection) -> Result<Setting> {
    let raw: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key='plans_start'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw
        .map(|raw| serde_json::from_str(&raw))
        .transpose()?
        .unwrap_or_default())
}

pub(crate) fn change(db: &mut Connection, request: Change) -> Result<Setting> {
    if request.op_id.get_version_num() != 7 {
        return Err("Invalid Plans start operation.".into());
    }
    let tx = db.transaction()?;
    let payload = serde_json::to_string(&request)?;
    if let Some((kind, old)) = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if kind != "plans-start.change" || old != payload {
            return Err("Operation ID reused.".into());
        }
        return read(&tx);
    }
    let current = read(&tx)?;
    if current.revision != request.revision {
        return Err("Plans start changed in another window. Reload before saving.".into());
    }
    let next = Setting {
        revision: current
            .revision
            .checked_add(1)
            .ok_or("Plans start revision overflow.")?,
        mode: request.mode,
    };
    let value = serde_json::to_string(&next)?;
    tx.execute("INSERT INTO settings VALUES('plans_start',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [&value])?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'plans-start.change',?2,'complete')",
        params![request.op_id.to_string(), payload],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'plans-start.changed',?2)",
        params![crate::journal::timestamp(), value],
    )?;
    tx.commit()?;
    Ok(next)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Seen {
    pub plan_id: Uuid,
    pub revision: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Observation {
    pub owner: Uuid,
    pub sequence: u64,
    pub idle: bool,
    pub plans: Vec<Seen>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Countdown {
    pub plan_id: Uuid,
    pub revision: u64,
    pub remaining_ms: i64,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Update {
    pub countdowns: Vec<Countdown>,
    pub started: Vec<Plan>,
}

#[derive(Deserialize, Serialize)]
struct Watching {
    plan: Seen,
    since: Stamp,
}
#[derive(Deserialize, Serialize)]
struct Frame {
    at: Stamp,
    watching: Vec<Watching>,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    // A daemon restart cannot preserve evidence that a window is still visible.
    db.execute_batch("CREATE TEMP TABLE plan_observers(owner TEXT PRIMARY KEY,sequence INTEGER NOT NULL,at INTEGER NOT NULL,record TEXT NOT NULL);")?;
    Ok(())
}

pub(crate) fn observe(db: &mut Connection, request: Observation, now: &Stamp) -> Result<Update> {
    if request.owner.get_version_num() != 7
        || request.sequence == 0
        || request.sequence > i64::MAX as u64
        || request.plans.len() > 1_000
        || request.plans.iter().any(|plan| plan.revision == 0)
        || request
            .plans
            .iter()
            .map(|plan| plan.plan_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != request.plans.len()
    {
        return Err("Invalid plan observation.".into());
    }
    let tx = db.transaction()?;
    let previous: Option<(i64, String)> = tx
        .query_row(
            "SELECT sequence,record FROM plan_observers WHERE owner=?1",
            [request.owner.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if previous
        .as_ref()
        .is_some_and(|(sequence, _)| *sequence >= request.sequence as i64)
    {
        return Err("Stale plan observation.".into());
    }
    let previous = previous
        .map(|(_, raw)| serde_json::from_str::<Frame>(&raw))
        .transpose()?;
    let uninterrupted = previous.as_ref().filter(|frame| {
        let (gap, reboot) = now.elapsed(&frame.at);
        !reboot && now.mono_ms >= frame.at.mono_ms && gap <= MAX_GAP_MS
    });
    let mut frame = Frame {
        at: now.clone(),
        watching: vec![],
    };
    let mut result = Update::default();
    if request.idle
        && !crate::dispatch_control::paused(&tx)?
        && !crate::confirmation::status(&tx)?.state.active
    {
        let plans = crate::plan_store::list(&tx)?;
        for seen in request.plans {
            let Ok(mut plan) = crate::plan_store::read(&tx, seen.plan_id) else {
                continue;
            };
            if plan.rev != seen.revision
                || plan.start_mode != Mode::AfterDelay
                || !plan.execute_allowed
                || plan.state != State::Draft
                || plan.generation.state != GenerationState::Ready
                || plan.rewrite.is_some()
            {
                continue;
            }
            // Seeing this exact version authorizes the delay, but does not count
            // as a manual review until an automatic start is actually committed.
            plan.seen_revision = Some(plan.rev);
            if crate::plan_store::start_unavailable(&tx, &plan)?.is_some() {
                continue;
            }
            let mut runnable = false;
            for item in plan
                .items
                .iter()
                .filter(|item| item.state == crate::plans::ItemState::Planned)
            {
                runnable |=
                    crate::plan_execution::waiting_for_dependencies(&tx, item, &plan, &plans)?
                        .is_none();
            }
            if !runnable {
                continue;
            }
            let since = uninterrupted
                .and_then(|frame| frame.watching.iter().find(|old| old.plan == seen))
                .map(|old| old.since.clone())
                .unwrap_or_else(|| now.clone());
            let (elapsed, reboot) = now.elapsed(&since);
            if !reboot && elapsed >= DELAY_MS {
                let before = plan.rev;
                crate::plan_store::start(&tx, &mut plan)?;
                crate::plan_history::advance_revision(&tx, plan.id, before, plan.rev)?;
                crate::plan_store::save(&tx, &plan, "plan.started-after-delay")?;
                result.started.push(plan);
            } else {
                result.countdowns.push(Countdown {
                    plan_id: plan.id,
                    revision: plan.rev,
                    remaining_ms: DELAY_MS - elapsed,
                });
                frame.watching.push(Watching { plan: seen, since });
            }
        }
    }
    tx.execute(
        "DELETE FROM plan_observers WHERE at<?1",
        [now.mono_ms - 30_000],
    )?;
    tx.execute("INSERT INTO plan_observers VALUES(?1,?2,?3,?4) ON CONFLICT(owner) DO UPDATE SET sequence=excluded.sequence,at=excluded.at,record=excluded.record", params![request.owner.to_string(), request.sequence as i64, now.mono_ms, serde_json::to_string(&frame)?])?;
    tx.commit()?;
    Ok(result)
}
