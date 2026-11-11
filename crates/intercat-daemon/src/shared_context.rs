//! Normalize one immutable shared checkpoint without changing its source worker.
//! The queue owns pinning and admission; this layer writes only Git objects.
use crate::{
    Result,
    lineage::{Basis, Frozen, Point, View, Views},
    replacement_reparent::{self, Inputs},
};
use intercat_git::Repository;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "camelCase",
    deny_unknown_fields
)]
pub enum Outcome {
    View(Box<View>),
    Conflict(replacement_reparent::Conflict),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Projection {
    pub source: Point,
    pub previous: Option<View>,
    pub from: Point,
    pub onto: Point,
    pub views: Views,
}
impl Projection {
    /// A solution belongs to these exact source and ancestor choices, across
    /// descendant reviews. Queue revisions and descendant files are not inputs.
    pub fn identity(&self, namespace: uuid::Uuid) -> Result<uuid::Uuid> {
        Ok(uuid::Uuid::new_v5(
            &namespace,
            &Sha256::digest(serde_json::to_vec(self)?),
        ))
    }
    /// Freeze only checkpoints through the exact source/ancestor cuts. The
    /// retired worker's newer checkpoints are not object roots of this step.
    pub fn freeze(&self, graph: &Frozen) -> Result<Frozen> {
        use std::collections::BTreeMap;
        fn origin(basis: &Basis, pending: &mut Vec<(String, u64)>) {
            pending.extend(
                basis
                    .parents
                    .iter()
                    .chain(&basis.inherits)
                    .chain(&basis.base_inherits)
                    .map(|(track, mark)| (track.clone(), mark.number)),
            );
            for view in &basis.views {
                pending.push((view.track.clone(), view.source.number));
                origin(&view.origin, pending);
            }
        }
        let mut pending = vec![];
        for point in [&self.source, &self.from, &self.onto] {
            if graph.by_sha(&point.track, &point.sha)? != point {
                return Err("The shared context's frozen checkpoint changed.".into());
            }
            pending.push((point.track.clone(), point.number));
        }
        for view in self.views.iter().chain(self.previous.iter()) {
            pending.push((view.track.clone(), view.source.number));
            origin(&view.origin, &mut pending);
        }
        let mut limits = BTreeMap::<String, u64>::new();
        while let Some((track, through)) = pending.pop() {
            graph.point(&track, through)?;
            if limits.get(&track).is_some_and(|old| *old >= through) {
                continue;
            }
            limits.insert(track.clone(), through);
            for point in graph
                .points
                .iter()
                .filter(|point| point.track == track && point.number <= through)
            {
                origin(&point.basis(), &mut pending);
            }
        }
        Ok(Frozen {
            points: graph
                .points
                .iter()
                .filter(|point| {
                    limits
                        .get(&point.track)
                        .is_some_and(|through| point.number <= *through)
                })
                .cloned()
                .collect(),
            integrated: Default::default(),
        })
    }
}
pub fn project(
    repo: &Repository,
    graph: &Frozen,
    input: &Projection,
    resolutions: &[crate::merge_choices::Solved],
) -> Result<Outcome> {
    let Projection {
        source,
        previous,
        from,
        onto,
        views,
    } = input;
    if graph.by_sha(&source.track, &source.sha)? != source {
        return Err("The shared context's source checkpoint changed.".into());
    }
    let mut calculation = graph.clone();
    let mut checkpoint = source.clone();
    let head = if let Some(previous) = previous {
        if previous.track != source.track || previous.source != source.inherited() {
            return Err("The previous shared context belongs to another checkpoint.".into());
        }
        crate::lineage::view::validate(graph, repo, &vec![previous.clone()])?;
        // Reapply another changed parent to the already normalized snapshot.
        // The original checkpoint identity and graph remain immutable. The
        // computational point excludes its own view, which would recurse into
        // itself when calculating its pre-own baseline.
        checkpoint.views = previous.origin.views.clone();
        checkpoint.parents = previous.origin.parents.clone();
        checkpoint.base_at = previous.origin.base.clone();
        checkpoint.base_inherits_at = previous.origin.base_inherits.clone();
        checkpoint.inherits = previous.origin.inherits.clone();
        *calculation
            .points
            .iter_mut()
            .find(|point| **point == *source)
            .ok_or("The shared source has no frozen ancestry.")? = checkpoint.clone();
        &previous.head
    } else {
        &source.sha
    };
    let prepared = match replacement_reparent::prepare(
        repo,
        &calculation,
        &Inputs {
            destination: replacement_reparent::Destination::Snapshot,
            views: views.clone(),
            checkpoint,
            inherited: from.clone(),
            replacement: onto.clone(),
            worktree: repo.tree_id(head.parse()?)?.to_string(),
            integrated: None,
            resolutions: resolutions.to_vec(),
        },
    )? {
        replacement_reparent::Outcome::Prepared(prepared) => prepared,
        replacement_reparent::Outcome::Conflict(conflict) => {
            return Ok(Outcome::Conflict(conflict));
        }
    };
    let base = repo.context_commit(
        prepared.base.parse()?,
        crate::lineage::view::head(onto, &prepared.views)?.parse()?,
        &format!(
            "Intercat shared context origin\n\n{}\n",
            serde_json::to_string(&(&source.track, source.inherited()))?
        ),
    )?;
    let origin = Basis {
        views: prepared.views,
        parents: prepared.parents,
        base: base.to_string(),
        base_inherits: prepared.base_inherits,
        inherits: prepared.inherits,
    };
    Ok(Outcome::View(Box::new(View::create(
        repo,
        graph,
        source,
        origin,
        prepared.worktree.parse()?,
    )?)))
}
