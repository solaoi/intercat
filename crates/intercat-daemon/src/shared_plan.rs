//! Discover immutable shared-context steps without opening Git or changing files.
//! Callers compute and protect each missing step outside the journal writer.
use crate::{
    Result,
    lineage::{Frozen, Point, View, Views, view},
    shared_context::{self, Projection},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rewrite {
    pub track: String,
    pub shared_through: u64,
    pub target: Point,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Saved {
    pub input: Projection,
    pub view: View,
}
impl Saved {
    pub fn verify(
        &self,
        repo: &intercat_git::Repository,
        graph: &Frozen,
        solutions: &[crate::merge_choices::Solved],
    ) -> Result<()> {
        if !matches!(
            shared_context::project(repo, graph, &self.input, solutions)?,
            shared_context::Outcome::View(actual) if *actual == self.view
        ) {
            return Err("The saved shared context no longer reproduces its frozen inputs.".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    Project(Box<Projection>),
    Ready(Views),
}

pub fn next(
    graph: &Frozen,
    source: &Point,
    context: &Views,
    rewrites: &[Rewrite],
    saved: &[Saved],
) -> Result<Next> {
    let mut changes = BTreeMap::new();
    for rewrite in rewrites {
        if rewrite.track.is_empty()
            || rewrite.track == rewrite.target.track
            || graph.by_sha(&rewrite.target.track, &rewrite.target.sha)? != &rewrite.target
            || changes.insert(rewrite.track.as_str(), rewrite).is_some()
        {
            return Err("The shared context's chosen ancestor paths changed.".into());
        }
    }
    Planner {
        graph,
        context,
        changes,
        saved,
        active: BTreeSet::new(),
        complete: BTreeMap::new(),
    }
    .walk(source)
}

struct Planner<'a> {
    graph: &'a Frozen,
    context: &'a Views,
    changes: BTreeMap<&'a str, &'a Rewrite>,
    saved: &'a [Saved],
    active: BTreeSet<(String, u64)>,
    complete: BTreeMap<(String, u64), Views>,
}
impl Planner<'_> {
    fn views(&self, source: &Point) -> Result<Views> {
        // A consumer also carries later descendants' views. Those must not
        // become inputs of an earlier source or of an independent new parent.
        let effective = view::point(source, self.context)?;
        let selected = self
            .context
            .iter()
            .filter(|view| {
                let through = if view.track == source.track {
                    source.number
                } else {
                    effective
                        .inherits
                        .get(&view.track)
                        .into_iter()
                        .chain(effective.parents.get(&view.track))
                        .chain(effective.base_inherits_at.get(&view.track))
                        .map(|mark| mark.number)
                        .max()
                        .unwrap_or(0)
                };
                view.source.number <= through
            })
            .cloned()
            .collect();
        let original =
            view::find(source, self.context)?.map_or(&source.views, |view| &view.origin.views);
        view::overlay(original, &selected)
    }
    fn affected(&self, point: &Point) -> bool {
        self.changes.iter().any(|(track, change)| {
            point
                .inherits
                .get(*track)
                .or_else(|| point.parents.get(*track))
                .is_some_and(|mark| mark.number > change.shared_through)
        })
    }
    fn walk(&mut self, source: &Point) -> Result<Next> {
        if self.graph.by_sha(&source.track, &source.sha)? != source {
            return Err("The shared context's source checkpoint changed.".into());
        }
        let key = (source.track.clone(), source.number);
        if let Some(views) = self.complete.get(&key) {
            return Ok(Next::Ready(views.clone()));
        }
        if !self.active.insert(key.clone()) {
            return Err("The shared checkpoint contexts contain a dependency cycle.".into());
        }
        let result = self.walk_inner(source);
        self.active.remove(&key);
        if let Ok(Next::Ready(views)) = &result {
            self.complete.insert(key, views.clone());
        }
        result
    }
    fn walk_inner(&mut self, source: &Point) -> Result<Next> {
        let context = self.views(source)?;
        let effective = view::point(source, &context)?;
        if !self.affected(&effective) {
            return Ok(Next::Ready(context));
        }
        let mut generated = vec![];
        // Later baseline clamps may stop at any earlier own checkpoint. Compute
        // those views in numeric order without importing the retired worker's
        // current files or checkpoints beyond the exact shared cut.
        let mut earlier = self
            .graph
            .points
            .iter()
            .filter(|point| point.track == source.track && point.number < source.number)
            .cloned()
            .collect::<Vec<_>>();
        earlier.sort_by_key(|point| point.number);
        for point in earlier {
            match self.walk(&point)? {
                Next::Ready(views) => generated = view::merge([generated, views])?,
                pending => return Ok(pending),
            }
        }
        let mut previous = view::find(source, &context)?.cloned();
        for (track, mark) in &effective.parents {
            let from = self.graph.by_sha(track, &mark.sha)?.clone();
            if from.inherited() != *mark {
                return Err("The shared source's parent checkpoint changed.".into());
            }
            let onto = self
                .changes
                .get(track.as_str())
                .filter(|change| mark.number > change.shared_through)
                .map_or_else(|| from.clone(), |change| change.target.clone());
            let parent_views = match self.walk(&onto)? {
                Next::Ready(views) => views,
                pending => return Ok(pending),
            };
            generated = view::merge([generated, parent_views])?;
            let before = previous
                .as_ref()
                .map_or(&context, |view| &view.origin.views);
            let views = view::overlay(before, &generated)?;
            if from == onto && view::head(&from, before)? == view::head(&onto, &views)? {
                continue;
            }
            let input = Projection {
                source: source.clone(),
                previous: previous.clone(),
                from,
                onto,
                views,
            };
            let mut matches = self.saved.iter().filter(|saved| saved.input == input);
            let Some(saved) = matches.next() else {
                return Ok(Next::Project(Box::new(input)));
            };
            if matches.any(|other| other.view != saved.view)
                || saved.view.track != source.track
                || saved.view.source != source.inherited()
            {
                return Err(
                    "The saved shared step identifies different normalized contexts.".into(),
                );
            }
            previous = Some(saved.view.clone());
        }
        let Some(view) = previous else {
            return Err(
                "The affected shared source has no recorded dependency to normalize.".into(),
            );
        };
        let mut views = view::overlay(&view.origin.views, &generated)?;
        views = view::overlay(&views, &vec![view])?;
        Ok(Next::Ready(views))
    }
}
