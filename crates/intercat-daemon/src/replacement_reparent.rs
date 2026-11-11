//! Transplant a descendant's own files and its integrated baseline separately.
//! Checkpoint application, process stopping, and ancestry persistence happen later.
use crate::{
    Result,
    lineage::{Ancestors, Frozen, Point, include},
};
use intercat_git::{Merge, ObjectId, Repository};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Destination {
    /// A private shared source has no publication target of its own.
    Snapshot,
    /// Coverage of the frozen session head after its chosen undo prefix.
    Publication { coverage: Ancestors },
}
impl Destination {
    fn shared_coverage(
        &self,
        track: &str,
        covered: Option<&crate::lineage::Inherited>,
    ) -> Option<crate::lineage::Inherited> {
        let covered = covered?;
        match self {
            Self::Snapshot => Some(covered.clone()),
            Self::Publication { coverage } => coverage.get(track).map(|available| {
                if available.number < covered.number {
                    available.clone()
                } else {
                    covered.clone()
                }
            }),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Inputs {
    pub destination: Destination,
    pub views: crate::lineage::Views,
    pub checkpoint: Point,
    pub inherited: Point,
    pub replacement: Point,
    /// A frozen snapshot tree, including edits after the last checkpoint.
    pub worktree: String,
    pub integrated: Option<Point>,
    pub resolutions: Vec<crate::merge_choices::Solved>,
}
pub(crate) struct Parentage {
    pub views: crate::lineage::Views,
    pub parents: Ancestors,
    pub inherits: Ancestors,
    pub base_inherits: Ancestors,
}

/// Metadata can be checked at the journal boundary before any tree or working
/// file changes. A merge conflict does not change which parents were chosen.
pub(crate) fn parentage(
    graph: &Frozen,
    checkpoint: &Point,
    inherited: &Point,
    replacement: &Point,
    integrated: Option<&Point>,
    changes: &crate::lineage::Views,
    destination: &Destination,
) -> Result<Parentage> {
    let views = crate::lineage::view::overlay(
        &crate::lineage::view::merge([checkpoint.views.clone(), replacement.views.clone()])?,
        changes,
    )?;
    let replacement_context = crate::lineage::view::point(replacement, &views)?;
    let inherited_context = crate::lineage::view::point(inherited, &checkpoint.views)?;
    let shared = inherited.track == replacement.track
        && inherited.inherited() == replacement.inherited()
        && crate::lineage::view::head(inherited, &checkpoint.views)?
            != crate::lineage::view::head(replacement, &views)?;
    if checkpoint.track == inherited.track
        || checkpoint.track == replacement.track
        || (inherited.track == replacement.track
            && replacement.number <= inherited.number
            && !shared)
        || inherited_context.inherits.contains_key(&checkpoint.track)
        || replacement_context.inherits.contains_key(&checkpoint.track)
        || checkpoint.parents.get(&inherited.track) != Some(&inherited.inherited())
        || integrated.is_some_and(|point| {
            point.track != checkpoint.track
                || point.number > checkpoint.number
                || point.number == checkpoint.number && point.sha != checkpoint.sha
        })
    {
        return Err("The descendant no longer has these frozen parent checkpoints.".into());
    }
    for point in [checkpoint, inherited, replacement]
        .into_iter()
        .chain(integrated)
    {
        if point.track.is_empty() || point.number == 0 {
            return Err("Reparenting needs identified saved checkpoints.".into());
        }
        if graph.by_sha(&point.track, &point.sha)? != point {
            return Err("The reparenting checkpoint no longer matches its frozen ancestry.".into());
        }
    }
    if let Destination::Publication { coverage } = destination {
        for (track, mark) in coverage {
            if graph.point(track, mark.number)?.inherited() != *mark {
                return Err("The reparenting destination coverage changed.".into());
            }
        }
    }
    let mut parents = checkpoint.parents.clone();
    parents.remove(&inherited.track);
    if let Some(existing) = parents.insert(replacement.track.clone(), replacement.inherited())
        && existing != replacement.inherited()
    {
        return Err(
            "Another direct dependency already names a different replacement checkpoint.".into(),
        );
    }
    let inherits = graph.inherited_from(&parents, &views)?;
    if inherits.contains_key(&checkpoint.track) {
        return Err("The replacement dependency roots contain their own descendant.".into());
    }
    // A saved base can also contain unrelated published work or this child's
    // shared context after Restore resets its integration cursor. Only replace
    // coverage supplied by the old declared roots; preserve those other facts,
    // including a newer borrowed checkpoint of an ancestor the old root shares.
    let previous_parents = graph.inherited_from(&checkpoint.parents, &checkpoint.views)?;
    let actual_base = base_coverage(checkpoint, integrated)?;
    let mut base_inherits = checkpoint.base_inherits_at.clone();
    base_inherits.retain(|track, covered| {
        previous_parents
            .get(track)
            .is_none_or(|supplied| covered.number > supplied.number)
    });
    if shared {
        base_inherits.remove(&replacement.track);
    }
    for (track, mark) in &inherits {
        if shared && track == &replacement.track {
            if let Some(covered) = destination.shared_coverage(track, actual_base.get(track)) {
                include(
                    &mut base_inherits,
                    track,
                    if covered.number < mark.number {
                        &covered
                    } else {
                        mark
                    },
                )?;
            }
        } else {
            include(&mut base_inherits, track, mark)?;
        }
        // Full parent snapshots also carry their explicitly recorded borrowed
        // base. Keep those marks without expanding an older own checkpoint's
        // obsolete parents through its self coverage entry.
        for (source, covered) in
            &crate::lineage::view::point(graph.by_sha(track, &mark.sha)?, &views)?.base_inherits_at
        {
            if graph.by_sha(source, &covered.sha)?.inherited() != *covered {
                return Err("The chosen parent's saved base coverage changed.".into());
            }
            if !shared || source != &replacement.track {
                include(&mut base_inherits, source, covered)?;
            }
        }
    }
    if let Some(integrated) = integrated {
        // Preserve only this child's own integrated boundary, never the
        // transitive ancestors of the retired parent in that old checkpoint.
        include(
            &mut base_inherits,
            &integrated.track,
            &integrated.inherited(),
        )?;
    }
    Ok(Parentage {
        views,
        parents,
        inherits,
        base_inherits,
    })
}
fn base_coverage(checkpoint: &Point, integrated: Option<&Point>) -> Result<Ancestors> {
    let mut coverage = checkpoint.base_inherits_at.clone();
    if let Some(done) = integrated
        && coverage
            .get(&checkpoint.track)
            .is_none_or(|old| old.number < done.number)
    {
        coverage = crate::lineage::view::point(done, &checkpoint.views)?
            .inherits
            .clone();
        include(&mut coverage, &checkpoint.track, &done.inherited())?;
    }
    Ok(coverage)
}
impl Inputs {
    fn validate(&self, repository: &Repository, graph: &Frozen) -> Result<Parentage> {
        let ancestry = parentage(
            graph,
            &self.checkpoint,
            &self.inherited,
            &self.replacement,
            self.integrated.as_ref(),
            &self.views,
            &self.destination,
        )?;
        for point in [&self.checkpoint, &self.inherited, &self.replacement]
            .into_iter()
            .chain(self.integrated.iter())
        {
            repository.commit_details(point.sha.parse()?)?;
        }
        repository.tree_files(self.worktree.parse()?)?;
        crate::lineage::view::validate(graph, repository, &ancestry.views)?;
        Ok(ancestry)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Prepared {
    pub views: crate::lineage::Views,
    pub inputs: Inputs,
    pub worktree: String,
    pub base: String,
    pub parents: Ancestors,
    pub inherits: Ancestors,
    pub base_inherits: Ancestors,
}
impl Prepared {
    pub fn verify(&self, repository: &Repository, graph: &Frozen) -> Result<()> {
        if !matches!(prepare(repository, graph, &self.inputs)?, Outcome::Prepared(actual) if actual.as_ref() == self)
        {
            return Err(
                "The saved reparenting no longer reproduces these files and baseline.".into(),
            );
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Stage {
    Parents,
    Worktree,
    Base,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Conflict {
    pub stage: Stage,
    pub base: String,
    pub target: String,
    pub incoming: String,
    pub paths: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Prepared(Box<Prepared>),
    Conflict(Conflict),
}
fn merge(
    merges: &mut crate::merge_choices::Merges<'_>,
    stage: Stage,
    base: ObjectId,
    target: ObjectId,
    incoming: ObjectId,
) -> Result<std::result::Result<ObjectId, Conflict>> {
    Ok(freeze(merges.merge_trees(base, target, incoming)?, stage))
}
fn freeze(merge: Merge, stage: Stage) -> std::result::Result<ObjectId, Conflict> {
    match merge {
        Merge::Clean(tree) => Ok(tree),
        Merge::Conflicts {
            base,
            target,
            incoming,
            paths,
        } => Err(Conflict {
            stage,
            base: base.to_string(),
            target: target.to_string(),
            incoming: incoming.to_string(),
            paths: paths.into_iter().map(|path| path.to_string()).collect(),
        }),
    }
}

/// A shared source may itself have borrowed an earlier publication of its own.
/// Undo can cancel that prefix too. Remove just its own delta from each saved
/// origin, preserving that origin's other parents and unrelated borrowed files.
pub(crate) fn shared_baseline(
    merges: &mut crate::merge_choices::Merges<'_>,
    graph: &Frozen,
    source: &Point,
    views: &crate::lineage::Views,
    coverage: &Ancestors,
) -> Result<Merge> {
    let mut recipe = crate::lineage::view::point(source, views)?.basis();
    recipe.views = crate::lineage::view::overlay(&recipe.views, views)?;
    include(&mut recipe.inherits, &source.track, &source.inherited())?;
    if let Some(previous) = recipe.base_inherits.get(&source.track)
        && coverage
            .get(&source.track)
            .is_none_or(|available| available.number < previous.number)
    {
        let previous = graph.by_sha(&source.track, &previous.sha)?;
        if previous.number >= source.number {
            return Err("The shared origin does not precede its source checkpoint.".into());
        }
        let kept = match shared_baseline(merges, graph, previous, views, coverage)? {
            Merge::Clean(tree) => tree,
            conflict => return Ok(conflict),
        };
        let repo = merges.repository();
        return merges.merge_trees(
            repo.tree_id(crate::lineage::view::head(previous, views)?.parse()?)?,
            repo.tree_id(recipe.base.parse()?)?,
            kept,
        );
    }
    graph.baseline(merges, &recipe, coverage)
}

/// Without a published own boundary, replace only parent context actually
/// recorded in the old base. Other borrowed work must remain in both the base
/// and working files, so a later target deletion is not this child's addition.
fn unpublished_base(
    merges: &mut crate::merge_choices::Merges<'_>,
    graph: &Frozen,
    inputs: &Inputs,
    target: ObjectId,
    views: &crate::lineage::Views,
) -> Result<Merge> {
    let mut covered = Ancestors::new();
    for (track, inherited) in
        graph.inherited_from(&inputs.checkpoint.parents, &inputs.checkpoint.views)?
    {
        if let Some(base) = inputs.checkpoint.base_inherits_at.get(&track) {
            let mark = if base.number < inherited.number {
                base
            } else {
                &inherited
            };
            if graph.by_sha(&track, &mark.sha)?.inherited() != *mark {
                return Err("The saved parent coverage changed.".into());
            }
            covered.insert(track, mark.clone());
        }
    }
    let base = inputs.checkpoint.base_at.parse()?;
    let context = if !covered.is_empty() {
        match graph.parent_tree(merges, &covered, &inputs.checkpoint.views)? {
            Merge::Clean(tree) => tree,
            conflict => return Ok(conflict),
        }
    } else {
        let repo = merges.repository();
        let common = repo
            .merge_base(base, inputs.replacement.sha.parse()?)?
            .ok_or("The borrowed base and chosen parent have no common repository history.")?;
        repo.tree_id(common)?
    };
    let target = if inputs.inherited.track == inputs.replacement.track
        && inputs.inherited.inherited() == inputs.replacement.inherited()
    {
        let view = crate::lineage::view::find(&inputs.replacement, views)?
            .ok_or("The shared source has no normalized context.")?;
        let mut coverage = inputs.checkpoint.base_inherits_at.clone();
        match inputs
            .destination
            .shared_coverage(&view.track, coverage.get(&view.track))
        {
            Some(available) => {
                coverage.insert(view.track.clone(), available);
            }
            None => {
                coverage.remove(&view.track);
            }
        }
        match shared_baseline(merges, graph, &inputs.replacement, views, &coverage)? {
            Merge::Clean(tree) => tree,
            conflict => return Ok(conflict),
        }
    } else {
        target
    };
    let incoming = merges.repository().tree_id(base)?;
    merges.merge_trees(context, target, incoming)
}

pub fn prepare(repository: &Repository, graph: &Frozen, inputs: &Inputs) -> Result<Outcome> {
    let Parentage {
        views,
        parents,
        inherits,
        base_inherits,
    } = inputs.validate(repository, graph)?;
    let mut merges = crate::merge_choices::Merges::new(repository, &inputs.resolutions)?;
    let target = match freeze(
        graph.parent_tree(&mut merges, &parents, &views)?,
        Stage::Parents,
    ) {
        Ok(tree) => tree,
        Err(conflict) => return Ok(Outcome::Conflict(conflict)),
    };
    let old = repository.tree_id(
        crate::lineage::view::head(&inputs.inherited, &inputs.checkpoint.views)?.parse()?,
    )?;
    // The worker's base can contain later borrowed changes. Starting at that
    // base would carry the retired parent's files back into the replacement.
    let worktree = match merge(
        &mut merges,
        Stage::Worktree,
        old,
        target,
        inputs.worktree.parse()?,
    )? {
        Ok(tree) => tree,
        Err(conflict) => return Ok(Outcome::Conflict(conflict)),
    };
    // A prior reparenting may already have normalized this worker's own
    // integrated checkpoint onto different parents. Use that explicit saved
    // base coverage before transporting the base again; the original own
    // checkpoint's tree still contains its retired parents.
    let base = if inputs.integrated.is_some()
        || inputs
            .checkpoint
            .base_inherits_at
            .contains_key(&inputs.checkpoint.track)
    {
        let mut previous_coverage = inputs.checkpoint.inherits.clone();
        if let Some(integrated) = &inputs.integrated {
            include(
                &mut previous_coverage,
                &integrated.track,
                &integrated.inherited(),
            )?;
        }
        let previous_base = match freeze(
            graph.workspace_baseline(
                &mut merges,
                &inputs.checkpoint.track,
                &inputs.checkpoint.basis(),
                &previous_coverage,
            )?,
            Stage::Base,
        ) {
            Ok(tree) => tree,
            Err(conflict) => return Ok(Outcome::Conflict(conflict)),
        };
        match merge(&mut merges, Stage::Base, old, target, previous_base)? {
            Ok(tree) => tree,
            Err(conflict) => return Ok(Outcome::Conflict(conflict)),
        }
    } else {
        match freeze(
            unpublished_base(&mut merges, graph, inputs, target, &views)?,
            Stage::Base,
        ) {
            Ok(tree) => tree,
            Err(conflict) => return Ok(Outcome::Conflict(conflict)),
        }
    };
    merges.finish()?;
    repository.sync_objects(&[worktree, base])?;
    Ok(Outcome::Prepared(Box::new(Prepared {
        views,
        inputs: inputs.clone(),
        worktree: worktree.to_string(),
        base: base.to_string(),
        parents,
        inherits,
        base_inherits,
    })))
}
