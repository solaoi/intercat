//! Omit the unintegrated work carried by a conflicting checkpoint.
//! All decisions use the Commit job's frozen ancestry, never live track state.
use crate::{
    Result,
    integration::{Job, RowState},
};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Exclusion {
    track: String,
    dependents: Vec<String>,
    omitted: bool,
}

fn carries(job: &Job, child: &str, parent: &str) -> Result<bool> {
    if child == parent {
        return Ok(false);
    }
    let track = job
        .tracks
        .iter()
        .find(|track| track.id == child)
        .ok_or("Unknown integration track.")?;
    let Some(checkpoint) = &track.checkpoint else {
        return Ok(false);
    };
    let point = job.lineage.by_sha(child, &checkpoint.sha)?;
    let Some(inherited) = point.inherits.get(parent) else {
        return Ok(false);
    };
    Ok(job
        .lineage
        .integrated
        .get(parent)
        .map_or(0, |mark| mark.number)
        < inherited.number)
}

fn dependents(job: &Job, parent: &str) -> Result<Vec<String>> {
    job.tracks
        .iter()
        .filter_map(|track| match carries(job, &track.id, parent) {
            Ok(true) => Some(Ok(track.id.clone())),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

pub(crate) fn project(job: &Job) -> Result<Vec<Exclusion>> {
    let conflicts: BTreeSet<_> = job
        .plan
        .iter()
        .flat_map(|plan| &plan.rows)
        .filter(|row| row.state == RowState::Conflict)
        .map(|row| row.track.as_str())
        .chain(job.omitted_conflicts.iter().map(String::as_str))
        .collect();
    job.tracks
        .iter()
        .filter(|track| conflicts.contains(track.id.as_str()))
        .map(|track| {
            let omitted = job.omitted_conflicts.contains(&track.id);
            let dependents = dependents(job, &track.id)?
                .into_iter()
                .filter(|id| {
                    omitted
                        || job
                            .tracks
                            .iter()
                            .any(|track| track.id == *id && track.included)
                })
                .collect();
            Ok(Exclusion {
                track: track.id.clone(),
                dependents,
                omitted,
            })
        })
        .collect()
}

pub(crate) fn select(job: &mut Job, id: &str, included: bool) -> Result<()> {
    let track = job
        .tracks
        .iter()
        .find(|track| track.id == id)
        .ok_or("Unknown integration track.")?;
    if included && track.checkpoint.is_none() {
        return Err("This track has no saved checkpoint.".into());
    }
    if included {
        for parent in &job.omitted_conflicts {
            if carries(job, id, parent)? {
                let label = &job
                    .tracks
                    .iter()
                    .find(|track| track.id == *parent)
                    .ok_or("The omitted conflict track is unavailable.")?
                    .label;
                return Err(format!(
                    "Include {label} before including a track that carries its unintegrated work."
                )
                .into());
            }
        }
        job.omitted_conflicts.remove(id);
    } else if job
        .plan
        .iter()
        .flat_map(|plan| &plan.rows)
        .any(|row| row.track == id && row.state == RowState::Conflict)
    {
        job.omitted_conflicts.insert(id.to_owned());
    }
    job.tracks
        .iter_mut()
        .find(|track| track.id == id)
        .unwrap()
        .included = included;
    enforce(job)?;
    order_sources_first(job)
}

/// Refresh keeps the user's omitted conflicts, while testing new saved ancestry.
pub(crate) fn enforce(job: &mut Job) -> Result<()> {
    if job
        .omitted_conflicts
        .iter()
        .any(|id| !job.tracks.iter().any(|track| track.id == *id))
    {
        return Err("An omitted conflict track is unavailable. Keep this review or discard it before opening another.".into());
    }
    let mut excluded = job.omitted_conflicts.clone();
    for parent in &job.omitted_conflicts {
        excluded.extend(dependents(job, parent)?);
    }
    for track in &mut job.tracks {
        if excluded.contains(&track.id) {
            track.included = false;
        }
    }
    Ok(())
}

pub(crate) fn validate(job: &Job) -> Result<()> {
    for track in job.tracks.iter().filter(|track| track.included) {
        for parent in &job.omitted_conflicts {
            if track.id == *parent || carries(job, &track.id, parent)? {
                return Err("The Commit selection includes work from an omitted conflict.".into());
            }
        }
    }
    validate_order(
        job,
        &job.tracks
            .iter()
            .filter(|track| track.included)
            .map(|track| track.id.clone())
            .collect::<Vec<_>>(),
    )
}

pub(crate) fn validate_order(job: &Job, order: &[String]) -> Result<()> {
    for (index, child) in order.iter().enumerate() {
        for parent in &order[index + 1..] {
            if carries(job, child, parent)? {
                return Err("Commit inherited sources before their dependent tracks.".into());
            }
        }
    }
    Ok(())
}

pub(crate) fn order_sources_first(job: &mut Job) -> Result<()> {
    let mut pending: Vec<_> = job
        .tracks
        .iter()
        .filter(|track| track.included)
        .map(|track| track.id.clone())
        .collect();
    let mut ordered = Vec::new();
    while !pending.is_empty() {
        let mut next = None;
        for (index, child) in pending.iter().enumerate() {
            let mut waits = false;
            for parent in &pending {
                waits |= carries(job, child, parent)?;
            }
            if !waits {
                next = Some(index);
                break;
            }
        }
        ordered.push(pending.remove(next.ok_or("The inherited Commit order contains a cycle.")?));
    }
    let slots: Vec<_> = job
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| track.included)
        .map(|(index, _)| index)
        .collect();
    // Keep excluded rows in place so a checkbox action does not move its row.
    for (slot, id) in slots.into_iter().zip(ordered) {
        let source = job.tracks.iter().position(|track| track.id == id).unwrap();
        job.tracks.swap(slot, source);
    }
    Ok(())
}
