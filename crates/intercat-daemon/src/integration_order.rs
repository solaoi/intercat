//! Inspect frozen, saved changes before the Commit review starts merging.
use crate::{
    Result,
    integration::{Job, Status},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Overlap {
    pub path: String,
    pub tracks: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    pub overlaps: Vec<Overlap>,
    pub order: Vec<String>,
    pub confirmed: bool,
}

pub(crate) fn install(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS integration_order_reviews(
        job TEXT NOT NULL REFERENCES integration_jobs(id) ON DELETE CASCADE, revision INTEGER NOT NULL, record TEXT NOT NULL,
        PRIMARY KEY(job,revision));",
    )?;
    Ok(())
}

pub(crate) fn read(db: &Connection, id: Uuid, revision: u64) -> Result<Option<Review>> {
    db.query_row(
        "SELECT record FROM integration_order_reviews WHERE job=?1 AND revision=?2",
        params![id.to_string(), i64::try_from(revision)?],
        |row| row.get::<_, String>(0),
    )
    .optional()?
    .map(|record| Ok(serde_json::from_str(&record)?))
    .transpose()
}

fn save(db: &Connection, job: &Job, review: &Review) -> Result<()> {
    db.execute(
        "INSERT INTO integration_order_reviews VALUES(?1,?2,?3)",
        params![
            job.id.to_string(),
            i64::try_from(job.revision)?,
            serde_json::to_string(review)?
        ],
    )?;
    Ok(())
}

pub fn inspect(job: &Job) -> Result<Review> {
    let repo = crate::integration::repository(job)?;
    let plan = job
        .plan
        .as_ref()
        .ok_or("Freeze the Commit inputs before comparing changes.")?;
    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut order = Vec::new();
    for track in job.tracks.iter().filter(|track| track.included) {
        let Some(checkpoint) = &track.checkpoint else {
            continue;
        };
        let checkpoint_id = checkpoint.sha.parse()?;
        if repo.reference_target(&checkpoint.reference)? != Some(checkpoint_id) {
            return Err(format!("{}'s frozen checkpoint reference changed.", track.label).into());
        }
        let row = plan
            .rows
            .iter()
            .find(|row| row.track == track.id)
            .ok_or("Missing frozen track base.")?;
        let baseline = row
            .expected_integrated
            .as_deref()
            .unwrap_or(&row.base)
            .parse()?;
        for change in crate::integration::changes(
            &repo,
            repo.tree_id(baseline)?,
            repo.tree_id(checkpoint_id)?,
        )? {
            owners
                .entry(change.path)
                .or_default()
                .push(track.id.clone());
        }
        order.push(track.id.clone());
    }
    let overlaps: Vec<_> = owners
        .into_iter()
        .filter(|(_, tracks)| tracks.len() > 1)
        .map(|(path, tracks)| Overlap { path, tracks })
        .collect();
    Ok(Review {
        confirmed: overlaps.is_empty(),
        overlaps,
        order,
    })
}

pub(crate) fn prepared(
    db: &mut Connection,
    id: Uuid,
    revision: u64,
    review: Review,
) -> Result<Job> {
    let tx = db.transaction()?;
    let mut job = crate::integration_store::read(&tx, id)?;
    if job.revision != revision || job.status != Status::Preparing || job.plan.is_none() {
        return Err("The Commit inputs changed before order review.".into());
    }
    if job.order_review.is_some() {
        return Ok(job);
    }
    save(&tx, &job, &review)?;
    if !review.confirmed {
        job.status = Status::Ordering;
    }
    job.order_review = Some(review);
    crate::integration_store::save(&tx, &job)?;
    tx.execute("INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'integration.order-reviewed',?3)",
        params![crate::journal::timestamp(),job.session_id,serde_json::json!({"jobId":id,"revision":revision,"review":job.order_review}).to_string()])?;
    tx.commit()?;
    Ok(job)
}

pub(crate) fn confirm(db: &Connection, job: &mut Job, order: &[String]) -> Result<()> {
    if job.status != Status::Ordering {
        return Err("There is no pending Commit order proposal.".into());
    }
    let mut review = job
        .order_review
        .clone()
        .ok_or("The Commit order proposal is missing.")?;
    let expected: BTreeSet<_> = review.order.iter().collect();
    if order.len() != expected.len() || order.iter().collect::<BTreeSet<_>>() != expected {
        return Err("Include each selected track exactly once in the Commit order.".into());
    }
    crate::integration_exclusions::validate_order(job, order)?;
    // Excluded tracks stay visible after the selected, ordered tracks.
    let mut old = std::mem::take(&mut job.tracks);
    for id in order {
        let index = old
            .iter()
            .position(|track| track.id == *id)
            .ok_or("Unknown Commit track.")?;
        job.tracks.push(old.remove(index));
    }
    job.tracks.extend(old);
    review.order = order.to_vec();
    review.confirmed = true;
    job.revision += 1;
    job.status = Status::Preparing;
    save(db, job, &review)?;
    job.order_review = Some(review);
    Ok(())
}
