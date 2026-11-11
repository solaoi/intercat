//! A restored branch may borrow a later publication in its base. Undo removes
//! that publication from the target, while the branch still owns its shared
//! fork context. Transport that missing shared context with the branch delta;
//! the later borrowed edits stay in both sides of its delta and are not revived.
//! A continued writer also retains a closed source's exact saved prefix, even
//! if an earlier alternative removed that prefix from the publication target.
use crate::{
    Result,
    integration::{Job, Track},
    lineage::{Ancestors, include, view},
    merge_choices::Merges,
    replacement_delta::Boundary,
};
use intercat_git::Merge;

pub(crate) fn calculate(
    job: &Job,
    track: &Track,
    integrated: &Ancestors,
    merges: &mut Merges<'_>,
) -> Result<Merge> {
    let point = job.lineage.by_sha(
        &track.id,
        &track
            .checkpoint
            .as_ref()
            .ok_or("Missing saved branch.")?
            .sha,
    )?;
    let mut missing = job.lineage.missing_base_parents(&track.basis, integrated)?;
    let before = job.plan.as_ref().and_then(|plan| plan.coverage.as_ref());
    let prefix = job
        .path_review
        .families
        .iter()
        .find(|family| {
            job.path_review
                .choices
                .get(&family.root)
                .is_some_and(|choice| choice.kept == track.id)
        })
        .map(|family| family.prefix(&track.id))
        .transpose()?
        .unwrap_or_default();
    if job.undo.is_some() || !prefix.is_empty() {
        // Retain only declared context, including an earlier own publication.
        // Other work borrowed during Restore must not become this path's delta.
        for (source, base) in &track.basis.base_inherits {
            let required = if source == &point.track {
                Some(point.inherited())
            } else {
                point.inherits.get(source).cloned()
            };
            let Some(required) = required else { continue };
            let needed = if base.number < required.number {
                base
            } else {
                &required
            };
            if integrated
                .get(source)
                .is_none_or(|mark| mark.number < needed.number)
                && ((job.undo.is_some()
                    && before
                        .and_then(|marks| marks.get(source))
                        .is_some_and(|mark| mark.number >= needed.number))
                    || prefix
                        .get(source)
                        .is_some_and(|mark| mark.number >= needed.number))
            {
                include(&mut missing, source, needed)?;
            }
        }
    }
    if missing.iter().any(|(source, needed)| {
        (prefix
            .get(source)
            .is_none_or(|mark| mark.number < needed.number)
            && (job.undo.is_none()
                || before
                    .and_then(|marks| marks.get(source))
                    .is_none_or(|mark| mark.number < needed.number)))
            || match job
                .path_state
                .as_ref()
                .and_then(|state| state.retired.get(source))
            {
                Some(Boundary::EntireTrack) => true,
                Some(Boundary::Fork(through)) => needed.number > *through,
                None => false,
            }
    }) {
        return Err("Include the saved dependency before preparing this path.".into());
    }
    let mut tree = match job
        .lineage
        .track_baseline(merges, point, &track.basis, integrated)?
    {
        Merge::Clean(tree) => tree,
        conflict => return Ok(conflict),
    };
    let mut covered = integrated.clone();
    let mut deltas = vec![];
    // Calculate ancestors first, then subtract the resulting own deltas in
    // reverse order. Two direct roots can share an earlier saved ancestor.
    while !missing.is_empty() {
        let mut next = None;
        for (parent, needed) in &missing {
            let source = job.lineage.by_sha(parent, &needed.sha)?;
            if !view::point(source, &track.basis.views)?
                .inherits
                .keys()
                .any(|ancestor| missing.contains_key(ancestor))
            {
                next = Some(parent.clone());
                break;
            }
        }
        let parent = next.ok_or("The shared branch context contains a cycle.")?;
        let needed = missing.remove(&parent).unwrap();
        let source = job.lineage.by_sha(&parent, &needed.sha)?;
        let base = match crate::replacement_reparent::shared_baseline(
            merges,
            &job.lineage,
            source,
            &track.basis.views,
            &covered,
        )? {
            Merge::Clean(tree) => tree,
            conflict => return Ok(conflict),
        };
        let head = merges
            .repository()
            .tree_id(view::head(source, &track.basis.views)?.parse()?)?;
        deltas.push((head, base));
        job.lineage.incorporate(
            &mut covered,
            view::point(source, &track.basis.views)?.as_ref(),
        )?;
    }
    for (head, base) in deltas.into_iter().rev() {
        tree = match merges.merge_trees(head, tree, base)? {
            Merge::Clean(tree) => tree,
            conflict => return Ok(conflict),
        };
    }
    Ok(Merge::Clean(tree))
}
