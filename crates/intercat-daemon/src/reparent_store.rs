//! Resolve an affected direct parent at the worker's actual idle boundary.
//! The chosen replacement stays bound to the saved Commit review checkpoint.
#[cfg(test)]
#[path = "reparent_acceptance.rs"]
mod tests;
use crate::{
    Result,
    integration::{Job, Status},
    lineage::Frozen,
    realign::Reparent,
    realign_store::{Application, Origin},
    runtime::WorkerState,
};
use rusqlite::{Connection, params};
pub(crate) struct Target {
    pub reference: String,
    pub sha: String,
    pub origin: Origin,
}
/// Already queued siblings keep their original request identity while another
/// descendant completes. Only recorded sibling completions may advance their
/// review cursor; a stale new request or any user edit still needs a new review.
pub(crate) fn validate_review(
    db: &Connection,
    job: &Job,
    request: &crate::realign::Request,
) -> Result<()> {
    let crate::realign::Source::Reparent {
        job_id, revision, ..
    } = &request.source
    else {
        return Err("This request does not name a replacement review.".into());
    };
    if job.id == *job_id && job.revision == *revision {
        return Ok(());
    }
    let queued = crate::realign_queue::read(db, request.op_id)?;
    if job.id != *job_id
        || job.revision < *revision
        || queued.is_none_or(|queued| {
            queued.request != *request
                || matches!(
                    queued.phase,
                    crate::realign::Phase::Done | crate::realign::Phase::Discarded
                )
        })
    {
        return Err("The replacement review changed. Review this realignment again.".into());
    }
    let mut query = db.prepare("SELECT json_extract(payload,'$.revision'),json_extract(payload,'$.track') FROM events WHERE kind='replacement.realigned' AND json_extract(payload,'$.job')=?1 AND json_extract(payload,'$.revision')>?2 AND json_extract(payload,'$.revision')<=?3 ORDER BY json_extract(payload,'$.revision')")?;
    let mut cursor = *revision;
    for step in query.query_map(
        params![
            job.id.to_string(),
            i64::try_from(*revision)?,
            i64::try_from(job.revision)?
        ],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    )? {
        let (next, track) = step?;
        let next = u64::try_from(next)?;
        if cursor.checked_add(1) != Some(next) || track == request.track_id {
            return Err("The replacement review changed outside this queued realignment.".into());
        }
        cursor = next;
    }
    if cursor != job.revision {
        return Err("The replacement review changed outside this queued realignment.".into());
    }
    Ok(())
}
pub(crate) fn capture(
    db: &Connection,
    job: &Job,
    parent: &str,
    worker: &WorkerState,
    graph: &Frozen,
    saved: Option<&Application>,
) -> Result<Target> {
    if matches!(
        job.status,
        Status::Discarded | Status::Done | Status::Committing | Status::CommitBlocked
    ) || parent.is_empty()
        || parent.len() > 128
        || !job.contributions.is_empty()
    {
        return Err("Review the replacement before realigning its dependent worker.".into());
    }
    if let Some(saved) = saved {
        let inputs = saved
            .plan
            .reparent
            .as_ref()
            .ok_or("The saved file operation has a different realignment source.")?;
        return Ok(Target {
            reference: saved.target_ref.clone(),
            sha: saved.plan.target.clone(),
            origin: Origin::Reparent {
                coverage: saved.plan.basis.base_inherits.clone(),
                input: Box::new(Reparent::from(inputs)),
            },
        });
    }
    let state = job
        .path_state
        .as_ref()
        .ok_or("Missing replacement dependency review.")?;
    if !state.pending.is_empty() || !state.mixed.is_empty() {
        return Err(
            "Choose a path without mixed published changes before realigning a dependent worker."
                .into(),
        );
    }
    let change = state
        .descendants
        .iter()
        .find(|child| child.track == worker.track_id)
        .and_then(|child| child.parents.iter().find(|source| source.track == parent))
        .ok_or("This worker no longer has the affected dependency. Refresh its review.")?;
    if state
        .descendants
        .iter()
        .any(|child| child.track == change.onto)
    {
        return Err("Realign the affected parent before its dependent worker.".into());
    }
    let checkpoint = worker
        .checkpoint
        .as_ref()
        .ok_or("Wait for this worker's saved checkpoint.")?;
    let point = graph.by_sha(&worker.track_id, &checkpoint.sha)?.clone();
    let inherited = point
        .parents
        .get(parent)
        .ok_or("The worker changed its direct dependency. Refresh the review.")?;
    let inherited_point = graph.by_sha(parent, &inherited.sha)?.clone();
    if inherited_point.inherited() != *inherited {
        return Err("The saved parent checkpoint changed.".into());
    }
    let shared = change.onto == parent
        && job
            .path_review
            .shared_prefixes()?
            .get(parent)
            .is_some_and(|through| inherited.number <= through.number);
    let (reference, sha, replacement_point, views) = if shared {
        if inherited_point.inherited() != change.checkpoint {
            return Err("The worker changed its shared dependency. Refresh the review.".into());
        }
        let request = crate::shared_context_store::Request {
            job: job.id,
            revision: job.revision,
            descendant: worker.track_id.clone(),
            parent: parent.into(),
        };
        let reference = crate::shared_context_service::source_ref(&job.slug, &inherited_point.sha);
        let views = match crate::shared_context_store::plan(db, job, &request)? {
            crate::shared_plan::Next::Ready(views) => views,
            crate::shared_plan::Next::Project(_) => {
                return Ok(Target {
                    reference,
                    sha: inherited_point.sha.clone(),
                    origin: Origin::Shared(request),
                });
            }
        };
        (
            reference,
            inherited_point.sha.clone(),
            inherited_point.clone(),
            views,
        )
    } else {
        let replacement = job
            .tracks
            .iter()
            .find(|track| track.id == change.onto)
            .and_then(|track| track.checkpoint.as_ref())
            .ok_or("The chosen parent has no frozen checkpoint.")?;
        let point = graph.by_sha(&change.onto, &replacement.sha)?.clone();
        (
            replacement.reference.clone(),
            replacement.sha.clone(),
            point.clone(),
            crate::lineage::view::merge([point.views.clone()])?,
        )
    };
    let integrated = graph
        .integrated
        .get(&worker.track_id)
        .map(|mark| -> Result<crate::lineage::Point> {
            let point = graph.by_sha(&worker.track_id, &mark.sha)?;
            if point.inherited() != *mark {
                return Err("The worker's integrated checkpoint changed.".into());
            }
            Ok(point.clone())
        })
        .transpose()?;
    let reparent = Reparent {
        destination: {
            let mut coverage = job
                .plan
                .as_ref()
                .and_then(|plan| plan.coverage.clone())
                .ok_or("The replacement has no frozen destination coverage.")?;
            if let Some(undo) = &job.undo {
                crate::replacement_delta::rewind(&undo.records, &mut coverage);
            }
            crate::replacement_reparent::Destination::Publication { coverage }
        },
        views: crate::lineage::view::overlay(&point.views, &views)?,
        checkpoint: point,
        inherited: inherited_point,
        replacement: replacement_point,
        integrated,
    };
    let ancestry = crate::replacement_reparent::parentage(
        graph,
        &reparent.checkpoint,
        &reparent.inherited,
        &reparent.replacement,
        reparent.integrated.as_ref(),
        &reparent.views,
        &reparent.destination,
    )?;
    Ok(Target {
        reference,
        sha,
        origin: Origin::Reparent {
            coverage: ancestry.base_inherits,
            input: Box::new(reparent),
        },
    })
}

/// The worker checkpoint, new ancestry and saved review advance together.
/// The published head and other tracks' frozen checkpoints stay unchanged.
pub(crate) fn complete(
    db: &Connection,
    app: &Application,
    checkpoint: &crate::checkpoint::Checkpoint,
) -> Result<()> {
    let crate::realign::Source::Reparent { job_id, .. } = &app.request.source else {
        return Ok(());
    };
    let mut job = crate::integration_store::read(db, *job_id)?;
    validate_review(db, &job, &app.request)?;
    if matches!(
        job.status,
        Status::Committing
            | Status::CommitBlocked
            | Status::Done
            | Status::Discarded
            | Status::Building
            | Status::Staging
    ) {
        return Err("The replacement review changed during its dependency realignment.".into());
    }
    let track = job
        .tracks
        .iter_mut()
        .find(|track| track.id == app.request.track_id)
        .ok_or("The realigned track is missing from its replacement review.")?;
    track.basis = app.plan.basis.clone();
    track.checkpoint = Some(checkpoint.clone());
    let count:i64=db.query_row("SELECT count(*) FROM operations WHERE kind='checkpoint.save' AND state='complete' AND json_extract(payload,'$.trackId')=?1",[&track.id],|row|row.get(0))?;
    track.checkpoint_count = count.try_into()?;
    if let Some(previous) = &app.previous {
        let clip:Option<i64>=db.query_row("SELECT max(ordinal) FROM timeline_clips WHERE track=?1 AND json_extract(record,'$.turnId')=?2 AND json_extract(record,'$.category')='step' AND json_extract(record,'$.repairRunId') IS NULL",params![track.id,previous.turn_id.to_string()],|row|row.get(0))?;
        if clip.is_some() {
            track.last_clip = clip;
        }
    }
    job.lineage = crate::lineage_store::freeze(db)?.for_tracks(
        job.tracks
            .iter()
            .flat_map(|track| {
                std::iter::once(track.id.clone())
                    .chain(track.basis.parents.keys().cloned())
                    .chain(track.basis.base_inherits.keys().cloned())
                    .chain(track.basis.inherits.keys().cloned())
            })
            .collect(),
    );
    let point = job.lineage.by_sha(&app.request.track_id, &checkpoint.sha)?;
    for family in &mut job.path_review.families {
        if let Some(member) = family
            .members
            .iter_mut()
            .find(|member| member.id == app.request.track_id)
        {
            member.checkpoint = Some(point.inherited());
        }
    }
    let plan = job
        .plan
        .as_mut()
        .ok_or("Missing frozen replacement target.")?;
    let row = plan
        .rows
        .iter_mut()
        .find(|row| row.track == app.request.track_id)
        .ok_or("Missing realigned review row.")?;
    row.base = app.plan.basis.base.clone();
    row.expected_base = Some(row.base.clone());
    for row in &mut plan.rows {
        row.state = crate::integration::RowState::Waiting;
        row.tree = None;
        row.delta_base = None;
        row.changes.clear();
        row.conflicts.clear();
    }
    plan.tree = None;
    plan.undo_conflict = None;
    let previous_revision = job.revision;
    job.revision = job
        .revision
        .checked_add(1)
        .ok_or("Replacement review revision overflow.")?;
    job.order_review = None;
    job.candidate = None;
    job.kept_secrets.clear();
    job.error = None;
    crate::integration_store::copy_settings(db, job.id, previous_revision, job.revision, None)?;
    crate::integration_assumptions::capture(db, &mut job)?;
    crate::replacement_review_store::refresh(db, &mut job)?;
    job.status = if job.path_state.as_ref().is_some_and(|state| !state.ready()) {
        Status::Choosing
    } else {
        Status::Preparing
    };
    crate::integration_exclusions::order_sources_first(&mut job)?;
    crate::integration_store::save(db, &job)?;
    db.execute("INSERT INTO events(ts,session_id,kind,payload) VALUES(?1,?2,'replacement.realigned',?3)",params![crate::journal::timestamp(),job.session_id,serde_json::json!({"operation":app.operation,"job":job.id,"revision":job.revision,"track":app.request.track_id,"checkpoint":checkpoint.sha}).to_string()])?;
    Ok(())
}
