//! Journal-owned branch review capture and immutable receipt selection.
use crate::{
    Result,
    branch_choices::Member,
    integration::Job,
    integration_record::Record,
    replacement_review::{Evaluation, Review},
};
use rusqlite::{Connection, params};
use std::collections::BTreeSet;
use uuid::Uuid;

pub(crate) fn capture(db: &Connection, job: &Job) -> Result<Review> {
    let mut query = db.prepare("SELECT t.id,t.label,w.record FROM tracks t JOIN workspaces w ON w.track_id=t.id ORDER BY t.rowid")?;
    let mut members = Vec::new();
    for row in query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })? {
        let (id, label, raw) = row?;
        let workspace: crate::preparation::Workspace = serde_json::from_str(&raw)?;
        if crate::folders::track(db, &id)?.id() != job.project.id()
            || workspace.reference != format!("refs/intercat/{}/tracks/{label}", job.slug)
        {
            continue;
        }
        let checkpoint = job
            .lineage
            .points
            .iter()
            .filter(|point| point.track == id)
            .max_by_key(|point| point.number)
            .map(|point| point.inherited());
        members.push(Member {
            available: workspace.state == "ready"
                && !crate::continuation::closed(db, &id)?
                && !crate::path_retirement::holds(db, &id)?
                && job
                    .tracks
                    .iter()
                    .any(|track| track.id == id && track.checkpoint.is_some()),
            id,
            label,
            checkpoint,
        });
    }
    let mut review = Review {
        families: crate::branch_choices::capture(
            &members,
            &job.lineage,
            &job.slug,
            &crate::branch::snapshot(db)?,
            &crate::continuation::list(db)?,
        )?,
        choices: Default::default(),
    };
    for family in &review.families {
        if !family.has_alternatives() {
            if let Some(member) = family.members.iter().find(|member| member.available) {
                review
                    .choices
                    .insert(family.root.clone(), family.choose(&member.id)?);
            }
            continue;
        }
        for member in family.members.iter().filter(|member| member.available) {
            let choice = family.choose(&member.id)?;
            let mut all = true;
            for retired in choice.retired.keys() {
                all &= crate::continuation::closed(db, retired)?
                    || crate::path_retirement::already_aside(db, retired, &choice)?;
            }
            if all {
                review.choices.insert(family.root.clone(), choice);
                break;
            }
        }
    }
    Ok(review)
}

pub(crate) fn evaluate(db: &Connection, job: &Job) -> Result<Evaluation> {
    let current = job
        .tracks
        .iter()
        .filter_map(|track| {
            track
                .checkpoint
                .as_ref()
                .map(|checkpoint| job.lineage.by_sha(&track.id, &checkpoint.sha).cloned())
        })
        .collect::<Result<Vec<_>>>()?;
    let included = job
        .tracks
        .iter()
        .filter(|track| track.included && track.checkpoint.is_some())
        .map(|track| track.id.clone())
        .collect();
    let mut query = db.prepare(
        "SELECT record FROM integration_records WHERE session=?1 AND folder=?2 ORDER BY position",
    )?;
    let records = query
        .query_map(params![job.session_id, job.project.id], |row| {
            row.get::<_, String>(0)
        })?
        .map(|raw| Ok(serde_json::from_str::<Record>(&raw?)?))
        .collect::<Result<Vec<_>>>()?;
    let mut query = db.prepare("SELECT c.record FROM integration_cancelled c JOIN integration_records r ON r.id=c.record WHERE r.session=?1 AND r.folder=?2")?;
    let cancelled = query
        .query_map(params![job.session_id, job.project.id], |row| {
            row.get::<_, String>(0)
        })?
        .map(|raw| Ok(Uuid::parse_str(&raw?)?))
        .collect::<Result<BTreeSet<_>>>()?;
    job.path_review
        .evaluate(&job.lineage, &current, &included, &records, &cancelled)
}

pub(crate) fn choose(db: &Connection, job: &mut Job, track: &str) -> Result<()> {
    if !job.contributions.is_empty() {
        return Err("Refresh before changing a path choice after a repair.".into());
    }
    job.path_review.choose(track)?;
    let choice = job
        .path_review
        .choices
        .values()
        .find(|choice| choice.kept == track)
        .ok_or("Missing saved path choice.")?;
    for item in &mut job.tracks {
        if item.id == choice.kept {
            item.included = true;
            job.omitted_conflicts.remove(&item.id);
        } else if choice.retired.contains_key(&item.id) {
            item.included = false;
            job.omitted_conflicts.remove(&item.id);
        }
    }
    refresh(db, job)
}

/// Updating a preview never writes cancellation receipts or stops a worker.
pub(crate) fn refresh(db: &Connection, job: &mut Job) -> Result<()> {
    let state = evaluate(db, job)?;
    job.undo = if state.pending.is_empty() && state.mixed.is_empty() && !state.records.is_empty() {
        let parent = job
            .plan
            .as_ref()
            .ok_or("Freeze the Commit target before choosing a path.")?
            .target_head
            .clone();
        let mut replacements = Vec::new();
        for family in &job.path_review.families {
            let Some(choice) = job.path_review.choices.get(&family.root) else {
                continue;
            };
            let kept = family
                .members
                .iter()
                .find(|member| member.id == choice.kept)
                .ok_or("Missing chosen path.")?;
            for member in &family.members {
                if state
                    .records
                    .iter()
                    .any(|record| record.track_id == member.id)
                {
                    let fork = family.forks.iter().find(|fork| {
                        fork.parent == member.id
                            && choice.retired.get(&member.id)
                                == Some(&crate::replacement_delta::Boundary::Fork(
                                    fork.checkpoint.number,
                                ))
                    });
                    let since = match fork {
                        Some(fork) => match fork.origin.ended_at {
                            Some(ended_at) => {
                                let time: String = db.query_row(
                                    "SELECT strftime('%H:%M',?1 / 1000.0,'unixepoch','localtime')",
                                    [ended_at],
                                    |row| row.get(0),
                                )?;
                                format!(" since {time}")
                            }
                            None => format!(" since checkpoint {}", fork.checkpoint.number),
                        },
                        None => String::new(),
                    };
                    replacements.push(format!(
                        "Undo {}’s changes{since} (replaced by {})",
                        member.label, kept.label
                    ));
                }
            }
        }
        let resolutions = job
            .undo
            .as_ref()
            .filter(|previous| previous.parent == parent && previous.records == state.records)
            .map(|previous| previous.resolutions.clone())
            .unwrap_or_default();
        Some(crate::replacement_undo::Plan {
            parent,
            message: replacements.join("\n"),
            records: state.records.clone(),
            resolutions,
        })
    } else {
        None
    };
    job.path_state = Some(state);
    Ok(())
}
