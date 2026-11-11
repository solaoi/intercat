//! A private contextual view keeps the original checkpoint identity and own
//! revision while supplying its normalized tree and pre-own integration origin.
use super::{Ancestors, Basis, Frozen, Inherited, Point};
use crate::Result;
use intercat_git::{ObjectId, Repository};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct View {
    pub track: String,
    pub source: Inherited,
    pub head: String,
    pub origin: Box<Basis>,
}
pub type Views = Vec<View>;

fn message(track: &str, source: &Inherited, origin: &Basis) -> Result<String> {
    Ok(format!(
        "Intercat checkpoint context\n\n{}\n",
        serde_json::to_string(&(track, source, origin))?
    ))
}
impl View {
    pub fn create(
        repo: &Repository,
        graph: &Frozen,
        source: &Point,
        origin: Basis,
        tree: ObjectId,
    ) -> Result<Self> {
        if graph.by_sha(&source.track, &source.sha)? != source {
            return Err("The shared context's source checkpoint changed.".into());
        }
        validate(graph, repo, &origin.views)?;
        repo.tree_files(tree)?;
        let head = repo.context_commit(
            tree,
            origin.base.parse()?,
            &message(&source.track, &source.inherited(), &origin)?,
        )?;
        repo.sync_objects(&[head])?;
        let view = Self {
            track: source.track.clone(),
            source: source.inherited(),
            head: head.to_string(),
            origin: Box::new(origin),
        };
        validate(graph, repo, &vec![view.clone()])?;
        Ok(view)
    }
}

pub fn merge(sets: impl IntoIterator<Item = Views>) -> Result<Views> {
    let mut unique = BTreeMap::new();
    for view in sets.into_iter().flatten() {
        let key = (view.track.clone(), view.source.number);
        if let Some(old) = unique.get(&key) {
            if old != &view {
                return Err("The same shared checkpoint has different normalized contexts. Resolve their context before combining these parents.".into());
            }
        } else {
            unique.insert(key, view);
        }
    }
    Ok(unique.into_values().collect())
}
/// An explicit context change supersedes that source's previous view while
/// keeping its canonical checkpoint identity. Independent parents use merge.
pub(crate) fn overlay(original: &Views, changes: &Views) -> Result<Views> {
    let mut values = BTreeMap::new();
    for view in merge([original.clone()])? {
        values.insert((view.track.clone(), view.source.number), view);
    }
    for view in merge([changes.clone()])? {
        let key = (view.track.clone(), view.source.number);
        if values
            .get(&key)
            .is_some_and(|old| old.source != view.source)
        {
            return Err("A changed context replaced the identity of its shared checkpoint.".into());
        }
        values.insert(key, view);
    }
    Ok(values.into_values().collect())
}
pub(crate) fn find<'a>(point: &Point, views: &'a Views) -> Result<Option<&'a View>> {
    let mut matches = views
        .iter()
        .filter(|view| view.track == point.track && view.source.number == point.number);
    let found = matches.next();
    if matches.next().is_some() || found.is_some_and(|view| view.source.sha != point.sha) {
        return Err("A normalized context no longer identifies one source checkpoint.".into());
    }
    Ok(found)
}
pub(crate) fn head<'a>(point: &'a Point, views: &'a Views) -> Result<&'a str> {
    Ok(find(point, views)?.map_or(point.sha.as_str(), |view| view.head.as_str()))
}
pub(crate) fn point<'a>(source: &'a Point, views: &Views) -> Result<Cow<'a, Point>> {
    if views.is_empty() {
        return Ok(Cow::Borrowed(source));
    }
    let mut point = source.clone();
    if let Some(view) = find(source, views)? {
        point.parents = view.origin.parents.clone();
        point.base_at = view.origin.base.clone();
        point.base_inherits_at = view.origin.base_inherits.clone();
        point.inherits = view.origin.inherits.clone();
        point.views = overlay(&view.origin.views, views)?;
    } else {
        point.views = overlay(&point.views, views)?;
    }
    Ok(Cow::Owned(point))
}
fn marks(graph: &Frozen, values: &Ancestors) -> Result<()> {
    for (track, mark) in values {
        if graph.point(track, mark.number)?.inherited() != *mark {
            return Err("A shared context's checkpoint coverage changed.".into());
        }
    }
    Ok(())
}
fn validate_inner(
    graph: &Frozen,
    repo: &Repository,
    views: &Views,
    stack: &mut BTreeSet<(String, u64)>,
) -> Result<()> {
    let mut unique = BTreeSet::new();
    for view in views {
        let key = (view.track.clone(), view.source.number);
        if !unique.insert(key.clone()) || !stack.insert(key.clone()) {
            return Err("The shared checkpoint contexts contain a duplicate or a cycle.".into());
        }
        let raw = graph.point(&view.track, view.source.number)?;
        if view.track.is_empty()
            || view.source.number == 0
            || raw.inherited() != view.source
            || view
                .origin
                .base_inherits
                .get(&view.track)
                .is_some_and(|mark| mark.number >= view.source.number)
            || view
                .origin
                .inherits
                .get(&view.track)
                .is_some_and(|mark| mark.number >= view.source.number)
        {
            return Err("The shared checkpoint context changed its original identity.".into());
        }
        marks(graph, &view.origin.parents)?;
        marks(graph, &view.origin.inherits)?;
        marks(graph, &view.origin.base_inherits)?;
        let commit = repo.commit_details(view.head.parse()?)?;
        if commit.parents != vec![view.origin.base.parse::<ObjectId>()?]
            || commit.message != message(&view.track, &view.source, &view.origin)?
        {
            return Err("The shared context no longer matches its immutable origin.".into());
        }
        repo.tree_id(view.origin.base.parse()?)?;
        validate_inner(graph, repo, &view.origin.views, stack)?;
        stack.remove(&key);
    }
    Ok(())
}
pub fn validate(graph: &Frozen, repo: &Repository, views: &Views) -> Result<()> {
    validate_inner(graph, repo, views, &mut BTreeSet::new())
}

pub(crate) fn roots(views: &Views, output: &mut Vec<ObjectId>) -> Result<()> {
    for view in views {
        output.extend([
            view.source.sha.parse::<ObjectId>()?,
            view.head.parse::<ObjectId>()?,
            view.origin.base.parse::<ObjectId>()?,
        ]);
        roots(&view.origin.views, output)?;
    }
    Ok(())
}
pub(crate) fn tracks(views: &Views, output: &mut BTreeSet<String>) {
    for view in views {
        output.insert(view.track.clone());
        output.extend(
            view.origin
                .parents
                .keys()
                .chain(view.origin.inherits.keys())
                .chain(view.origin.base_inherits.keys())
                .cloned(),
        );
        tracks(&view.origin.views, output);
    }
}
