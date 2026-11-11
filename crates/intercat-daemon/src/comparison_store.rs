//! Durable Check receipts. An interrupted command is held, never replayed on startup.
use crate::{
    Result,
    candidate::Settings,
    comparison::{Action, Control, Job, Outcome, Phase, Side},
    comparison_source::Capture,
};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) enum Change {
    Install {
        capture: Box<Capture>,
        settings: Box<Settings>,
        trees: [String; 2],
        changes: [Vec<crate::integration::Change>; 2],
    },
    Begin {
        id: Uuid,
        revision: u64,
    },
    Finish {
        id: Uuid,
        attempt: u64,
        side: usize,
        outcome: Outcome,
    },
    Interrupted {
        id: Uuid,
        attempt: u64,
    },
}
pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch("CREATE TABLE comparison_jobs(id TEXT PRIMARY KEY,session TEXT NOT NULL,record TEXT NOT NULL,settings TEXT NOT NULL);
        CREATE TABLE comparison_cache(folder TEXT NOT NULL,tree TEXT NOT NULL,settings_hash TEXT NOT NULL,record TEXT NOT NULL,PRIMARY KEY(folder,tree,settings_hash));")?;
    Ok(())
}
pub(crate) fn read(db: &Connection, id: Uuid) -> Result<Option<Job>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM comparison_jobs WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| -> Result<Job> {
        let job: Job = serde_json::from_str(&raw)?;
        job.validate()?;
        if job.id != id {
            return Err("Comparison identity changed.".into());
        }
        Ok(job)
    })
    .transpose()
}
fn execution_hash(hash: &str, history: bool, sha: &str) -> Result<String> {
    Ok(if history {
        Sha256::digest(serde_json::to_vec(&(hash, sha))?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    } else {
        hash.to_owned()
    })
}
pub(crate) fn settings(db: &Connection, id: Uuid) -> Result<Settings> {
    let raw: String = db.query_row(
        "SELECT settings FROM comparison_jobs WHERE id=?1",
        [id.to_string()],
        |row| row.get(0),
    )?;
    let settings: Settings = serde_json::from_str(&raw)?;
    let job = read(db, id)?.ok_or("Unknown comparison.")?;
    if job.settings_hash != settings.hash()? || job.reuse_checks == settings.history {
        return Err("Saved comparison settings changed.".into());
    }
    for side in &job.sides {
        if side.settings_hash
            != execution_hash(&job.settings_hash, settings.history, &side.source.point.sha)?
        {
            return Err("Saved comparison execution inputs changed.".into());
        }
    }
    Ok(settings)
}
fn save(db: &Connection, job: &mut Job) -> Result<()> {
    job.revision = job
        .revision
        .checked_add(1)
        .ok_or("Comparison revision overflow.")?;
    job.validate()?;
    db.execute(
        "UPDATE comparison_jobs SET record=?2 WHERE id=?1",
        params![job.id.to_string(), serde_json::to_string(job)?],
    )?;
    db.execute(
        "INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'comparison.changed',?3)",
        params![
            crate::journal::timestamp(),
            job.session,
            serde_json::json!({"id":job.id,"revision":job.revision}).to_string()
        ],
    )?;
    Ok(())
}
fn cached(db: &Connection, folder: &str, tree: &str, hash: &str) -> Result<Option<Outcome>> {
    let raw: Option<String> = db
        .query_row(
            "SELECT record FROM comparison_cache WHERE folder=?1 AND tree=?2 AND settings_hash=?3",
            params![folder, tree, hash],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| -> Result<Outcome> {
        let outcome: Outcome = serde_json::from_str(&raw)?;
        outcome.validate(tree, hash)?;
        if !outcome.cacheable() {
            return Err("Invalid cached Check outcome.".into());
        }
        Ok(outcome)
    })
    .transpose()
}
pub(crate) fn change(db: &mut Connection, change: Change) -> Result<Job> {
    let tx = db.transaction()?;
    let mut job = match change {
        Change::Install {
            capture,
            settings,
            trees,
            changes,
        } => {
            let capture = *capture;
            let settings = *settings;
            if let Some(saved) = read(&tx, capture.request.op_id)? {
                if saved.request != capture.request {
                    return Err("Comparison operation ID was reused.".into());
                }
                return Ok(saved);
            }
            let current = crate::comparison_source::capture(&tx, capture.request.clone())?;
            if serde_json::to_value(&current)? != serde_json::to_value(&capture)? {
                return Err("The session or Check settings changed. Open a new comparison.".into());
            }
            if tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE id=?1)",
                [capture.request.op_id.to_string()],
                |row| row.get::<_, bool>(0),
            )? {
                return Err("The comparison operation ID belongs to another request.".into());
            }
            let hash = settings.hash()?;
            let [left, right] = capture.sources;
            let [left_changes, right_changes] = changes;
            let mut sides = [
                Side {
                    source: left,
                    tree: trees[0].clone(),
                    settings_hash: String::new(),
                    changes: left_changes,
                    outcome: None,
                    cached: false,
                },
                Side {
                    source: right,
                    tree: trees[1].clone(),
                    settings_hash: String::new(),
                    changes: right_changes,
                    outcome: None,
                    cached: false,
                },
            ];
            for side in &mut sides {
                // Git history makes equal trees with different heads different
                // execution inputs. Include that head in this side's settings.
                side.settings_hash =
                    execution_hash(&hash, settings.history, &side.source.point.sha)?;
                // History includes mutable tag refs, not just the chosen tree.
                // Such Checks always run independently; never reuse a receipt.
                side.outcome = if settings.history {
                    None
                } else {
                    cached(&tx, &capture.folder.id, &side.tree, &side.settings_hash)?
                };
                side.cached = side.outcome.is_some();
            }
            let phase = if sides.iter().all(|side| side.outcome.is_some()) {
                Phase::Done
            } else {
                Phase::Ready
            };
            let job = Job {
                id: capture.request.op_id,
                revision: 0,
                attempt: u64::from(phase != Phase::Done),
                request: capture.request,
                session: capture.session,
                folder: capture.folder,
                slug: capture.slug,
                base: capture.base,
                settings_hash: hash,
                reuse_checks: !settings.history,
                phase,
                sides,
            };
            job.validate()?;
            tx.execute(
                "INSERT INTO comparison_jobs VALUES(?1,?2,?3,?4)",
                params![
                    job.id.to_string(),
                    job.session,
                    serde_json::to_string(&job)?,
                    serde_json::to_string(&settings)?
                ],
            )?;
            tx.execute(
                "INSERT INTO operations VALUES(?1,'comparison.open',?2,'complete')",
                params![job.id.to_string(), serde_json::to_string(&job.request)?],
            )?;
            job
        }
        Change::Begin { id, revision } => {
            let mut job = read(&tx, id)?.ok_or("Unknown comparison.")?;
            if job.revision != revision || job.phase != Phase::Ready {
                return Err("The comparison changed. Reopen its saved result.".into());
            }
            job.phase = Phase::Running;
            job
        }
        Change::Finish {
            id,
            attempt,
            side,
            outcome,
        } => {
            let mut job = read(&tx, id)?.ok_or("Unknown comparison.")?;
            if job.phase != Phase::Running || job.attempt != attempt {
                return Ok(job);
            }
            let member = job.sides.get_mut(side).ok_or("Unknown comparison side.")?;
            if member.outcome.is_some() {
                return Err("The saved Check already has a result.".into());
            }
            outcome.validate(&member.tree, &member.settings_hash)?;
            if job.reuse_checks && outcome.cacheable() {
                tx.execute("INSERT INTO comparison_cache VALUES(?1,?2,?3,?4) ON CONFLICT(folder,tree,settings_hash) DO UPDATE SET record=excluded.record",params![job.folder.id,member.tree,member.settings_hash,serde_json::to_string(&outcome)?])?;
            }
            member.outcome = Some(outcome.clone());
            if job.reuse_checks && outcome.cacheable() {
                let tree = member.tree.clone();
                let hash = member.settings_hash.clone();
                for other in &mut job.sides {
                    if other.outcome.is_none() && other.tree == tree && other.settings_hash == hash
                    {
                        other.outcome = Some(outcome.clone());
                        other.cached = true;
                    }
                }
            }
            if job.sides.iter().all(|side| side.outcome.is_some()) {
                job.phase = if job
                    .sides
                    .iter()
                    .all(|side| side.outcome.as_ref().is_some_and(Outcome::cacheable))
                {
                    Phase::Done
                } else {
                    Phase::Held
                };
            }
            job
        }
        Change::Interrupted { id, attempt } => {
            let mut job = read(&tx, id)?.ok_or("Unknown comparison.")?;
            if job.phase != Phase::Running || job.attempt != attempt {
                return Ok(job);
            }
            job.phase = Phase::Held;
            job
        }
    };
    save(&tx, &mut job)?;
    tx.commit()?;
    Ok(job)
}
pub(crate) fn control(db: &mut Connection, request: Control) -> Result<Job> {
    if request.op_id.get_version_num() != 7 || request.attempt == 0 || request.revision == 0 {
        return Err("Invalid comparison control ID.".into());
    }
    let tx = db.transaction()?;
    let prior: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((kind, payload)) = prior {
        if kind != "comparison.control"
            || serde_json::from_str::<serde_json::Value>(&payload)?
                != serde_json::to_value(&request)?
        {
            return Err("Comparison control ID was reused.".into());
        }
        return read(&tx, request.id)?.ok_or_else(|| "Unknown comparison.".into());
    }
    let mut job = read(&tx, request.id)?.ok_or("Unknown comparison.")?;
    // Progress may advance while the Stop button is being pressed. Hold names
    // one reserved attempt; it must never stop a later explicit Retry.
    if job.attempt != request.attempt
        || request.revision > job.revision
        || request.action == Action::Retry && job.revision != request.revision
    {
        return Err("The comparison changed. Reopen its saved result.".into());
    }
    match request.action {
        Action::Hold if matches!(job.phase, Phase::Running | Phase::Ready) => {
            job.phase = Phase::Held
        }
        Action::Retry if job.phase == Phase::Held => {
            for side in &mut job.sides {
                if !side.outcome.as_ref().is_some_and(Outcome::cacheable) {
                    side.outcome = None;
                    side.cached = false;
                }
            }
            job.attempt = job
                .attempt
                .checked_add(1)
                .ok_or("Comparison attempt overflow.")?;
            job.phase = Phase::Ready;
        }
        _ => return Err("This comparison cannot perform that action.".into()),
    }
    save(&tx, &mut job)?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'comparison.control',?2,'complete')",
        params![request.op_id.to_string(), serde_json::to_string(&request)?],
    )?;
    tx.commit()?;
    Ok(job)
}
pub(crate) fn recover(db: &mut Connection) -> Result<()> {
    let tx = db.transaction()?;
    let mut query=tx.prepare("SELECT id FROM comparison_jobs WHERE json_extract(record,'$.phase') IN ('running','ready')")?;
    let ids = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(query);
    for id in ids {
        let mut job = read(&tx, Uuid::parse_str(&id)?)?.ok_or("Unknown comparison.")?;
        job.phase = Phase::Held;
        save(&tx, &mut job)?;
    }
    tx.commit()?;
    Ok(())
}
pub(crate) fn snapshot(db: &Connection) -> Result<Vec<serde_json::Value>> {
    let mut query=db.prepare("SELECT j.record FROM comparison_jobs j JOIN settings s ON s.key='session' AND j.session=json_extract(s.value,'$.id') ORDER BY j.rowid")?;
    query
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|raw| serde_json::from_str::<Job>(&raw?)?.project())
        .collect()
}
