//! Immutable checkpoint ancestry and the baseline used to integrate its delta.
//! Checkpoint numbers, rather than Git reachability, identify incorporated work.
use crate::Result;
use intercat_git::{Merge, ObjectId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[path = "lineage_view.rs"]
pub mod view;
pub use view::{View, Views};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Inherited {
    pub number: u64,
    pub sha: String,
}
pub type Ancestors = BTreeMap<String, Inherited>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Basis {
    pub views: Views,
    /// Explicit dependency roots, distinct from their transitive coverage.
    pub parents: Ancestors,
    pub base: String,
    pub base_inherits: Ancestors,
    pub inherits: Ancestors,
}
impl Basis {
    pub fn root(base: String) -> Self {
        Self {
            views: Vec::new(),
            parents: Ancestors::new(),
            base,
            base_inherits: Ancestors::new(),
            inherits: Ancestors::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Point {
    pub views: Views,
    pub parents: Ancestors,
    pub track: String,
    pub number: u64,
    pub sha: String,
    pub base_at: String,
    pub base_inherits_at: Ancestors,
    pub inherits: Ancestors,
}
impl Point {
    pub fn inherited(&self) -> Inherited {
        Inherited {
            number: self.number,
            sha: self.sha.clone(),
        }
    }
    pub fn basis(&self) -> Basis {
        Basis {
            views: self.views.clone(),
            parents: self.parents.clone(),
            base: self.base_at.clone(),
            base_inherits: self.base_inherits_at.clone(),
            inherits: self.inherits.clone(),
        }
    }
}

pub fn include(into: &mut Ancestors, track: &str, next: &Inherited) -> Result<()> {
    if let Some(old) = into.get(track) {
        if old.number == next.number && old.sha != next.sha {
            return Err("One checkpoint number identifies different saved trees.".into());
        }
        if old.number >= next.number {
            return Ok(());
        }
    }
    if next.number == 0 {
        return Err("Checkpoint numbers start at one.".into());
    }
    into.insert(track.to_owned(), next.clone());
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Frozen {
    pub points: Vec<Point>,
    pub integrated: Ancestors,
}
impl Frozen {
    /// An integration can only protect objects from its own repositories and
    /// inherited file history. Unrelated folders are not Git object sources.
    pub fn for_tracks(mut self, mut tracks: BTreeSet<String>) -> Self {
        let mut pending = tracks.iter().cloned().collect::<Vec<_>>();
        while let Some(owner) = pending.pop() {
            for point in self.points.iter().filter(|point| point.track == owner) {
                let mut related: BTreeSet<String> = point
                    .base_inherits_at
                    .keys()
                    .chain(point.inherits.keys())
                    .chain(point.parents.keys())
                    .cloned()
                    .collect();
                view::tracks(&point.views, &mut related);
                for ancestor in related {
                    if tracks.insert(ancestor.clone()) {
                        pending.push(ancestor);
                    }
                }
            }
        }
        self.points.retain(|point| tracks.contains(&point.track));
        self.integrated.retain(|track, _| tracks.contains(track));
        self
    }
    pub fn point(&self, track: &str, number: u64) -> Result<&Point> {
        self.points
            .iter()
            .find(|p| p.track == track && p.number == number)
            .ok_or_else(|| "The inherited checkpoint is unavailable.".into())
    }
    pub fn by_sha(&self, track: &str, sha: &str) -> Result<&Point> {
        self.points
            .iter()
            .find(|p| p.track == track && p.sha == sha)
            .ok_or_else(|| "The saved checkpoint has no ancestry record.".into())
    }
    fn resolve(&self, track: &str, mark: &Inherited) -> Result<&Point> {
        let point = self.point(track, mark.number)?;
        if point.sha != mark.sha {
            return Err("The inherited checkpoint identity changed.".into());
        }
        Ok(point)
    }

    /// Compose L from the effective base and min(integrated, inherited) for
    /// each ancestor. The caller applies the checkpoint delta exactly once.
    pub fn baseline(
        &self,
        merges: &mut crate::merge_choices::Merges<'_>,
        basis: &Basis,
        integrated: &Ancestors,
    ) -> Result<Merge> {
        view::validate(self, merges.repository(), &basis.views)?;
        self.baseline_inner(
            merges,
            basis,
            integrated,
            &mut BTreeSet::new(),
            &Point::basis,
        )
    }

    /// Compose complete direct-parent checkpoints. Unlike an incremental
    /// Commit baseline, each parent's earlier own publications still belong
    /// in this tree. Use their common Git history for every recursive source,
    /// then the same ancestry-aware three-way composition for shared changes.
    pub(crate) fn parent_tree(
        &self,
        merges: &mut crate::merge_choices::Merges<'_>,
        parents: &Ancestors,
        views: &Views,
    ) -> Result<Merge> {
        view::validate(self, merges.repository(), views)?;
        let inherits = self.inherited_from(parents, views)?;
        let mut roots = parents.iter();
        let (track, mark) = roots
            .next()
            .ok_or("A parent tree needs saved dependency roots.")?;
        let mut common = view::head(self.resolve(track, mark)?, views)?.parse()?;
        for (track, mark) in roots {
            common = merges
                .repository()
                .merge_base(
                    common,
                    view::head(self.resolve(track, mark)?, views)?.parse()?,
                )?
                .ok_or("The dependency roots have no common repository history.")?;
        }
        let common = common.to_string();
        let mut covered = Ancestors::new();
        for point in &self.points {
            let effective = view::point(point, views)?;
            if view::head(point, views)? == common {
                self.incorporate(&mut covered, &effective)?;
            }
            if effective.base_at == common {
                for (track, mark) in &effective.base_inherits_at {
                    self.resolve(track, mark)?;
                    include(&mut covered, track, mark)?;
                }
            }
        }
        let origin = |point: &Point| Basis {
            views: point.views.clone(),
            parents: point.parents.clone(),
            base: common.clone(),
            base_inherits: covered.clone(),
            inherits: point.inherits.clone(),
        };
        let recipe = Basis {
            views: views.clone(),
            parents: parents.clone(),
            base: common.clone(),
            base_inherits: covered.clone(),
            inherits: inherits.clone(),
        };
        self.baseline_inner(merges, &recipe, &inherits, &mut BTreeSet::new(), &origin)
    }
    pub fn track_baseline(
        &self,
        merges: &mut crate::merge_choices::Merges<'_>,
        point: &Point,
        basis: &Basis,
        integrated: &Ancestors,
    ) -> Result<Merge> {
        self.baseline_for_track(
            merges,
            &point.track,
            point.number,
            &point.inherits,
            basis,
            integrated,
        )
    }
    /// Current working files may contain edits after the last checkpoint. All
    /// published checkpoints of this worker are eligible for their baseline.
    pub fn workspace_baseline(
        &self,
        merges: &mut crate::merge_choices::Merges<'_>,
        track: &str,
        basis: &Basis,
        integrated: &Ancestors,
    ) -> Result<Merge> {
        self.baseline_for_track(merges, track, u64::MAX, &basis.inherits, basis, integrated)
    }
    fn baseline_for_track(
        &self,
        merges: &mut crate::merge_choices::Merges<'_>,
        track: &str,
        through: u64,
        inherits: &Ancestors,
        basis: &Basis,
        integrated: &Ancestors,
    ) -> Result<Merge> {
        let mut effective = basis.clone();
        effective.inherits = inherits.clone();
        if let Some(mark) = integrated.get(track) {
            self.resolve(track, mark)?;
            // A realigned base is a session commit, not one of this worker's
            // checkpoints. Its explicit coverage must not be inferred from
            // that commit's identity or from the current file contents.
            let already = if let Some(mark) = effective.base_inherits.get(track) {
                self.resolve(track, mark)?.number
            } else {
                0
            };
            let number = mark.number.min(through);
            if number > already {
                let raw = self.point(track, number)?;
                let saved = view::point(raw, &effective.views)?;
                effective.base = view::head(raw, &effective.views)?.into();
                effective.base_inherits = saved.inherits.clone();
                include(&mut effective.base_inherits, track, &saved.inherited())?;
            }
        }
        self.baseline(merges, &effective, integrated)
    }
    fn baseline_inner(
        &self,
        merges: &mut crate::merge_choices::Merges<'_>,
        basis: &Basis,
        integrated: &Ancestors,
        stack: &mut BTreeSet<(String, u64)>,
        origin: &impl Fn(&Point) -> Basis,
    ) -> Result<Merge> {
        let repo = merges.repository();
        let mut pending = Ancestors::new();
        for (track, inherited) in &basis.inherits {
            let Some(done) = integrated.get(track) else {
                continue;
            };
            let target = if done.number < inherited.number {
                done
            } else {
                inherited
            };
            self.resolve(track, target)?;
            pending.insert(track.clone(), target.clone());
        }
        let mut tree = repo.tree_id(basis.base.parse()?)?;
        let mut covered = basis.base_inherits.clone();
        while !pending.is_empty() {
            let mut next = None;
            for (track, mark) in &pending {
                let point = view::point(self.resolve(track, mark)?, &basis.views)?;
                if !point
                    .inherits
                    .keys()
                    .any(|parent| pending.contains_key(parent))
                {
                    next = Some((track.clone(), mark.clone()));
                    break;
                }
            }
            let next = next.ok_or("The checkpoint ancestry contains a cycle.")?;
            let (track, mark) = next;
            pending.remove(&track);
            let raw = self.resolve(&track, &mark)?;
            let point = view::point(raw, &basis.views)?;
            if covered
                .get(&track)
                .is_some_and(|old| old.number >= mark.number)
            {
                continue;
            }
            if !stack.insert((track.clone(), mark.number)) {
                return Err("The checkpoint ancestry contains a cycle.".into());
            }
            let base = if let Some(old) = covered.get(&track) {
                repo.tree_id(view::head(self.resolve(&track, old)?, &basis.views)?.parse()?)?
            } else {
                match self.baseline_inner(merges, &origin(&point), &covered, stack, origin)? {
                    Merge::Clean(base) => base,
                    conflict => {
                        stack.remove(&(track.clone(), mark.number));
                        return Ok(conflict);
                    }
                }
            };
            stack.remove(&(track.clone(), mark.number));
            tree = match merges.merge_trees(
                base,
                tree,
                repo.tree_id(view::head(raw, &basis.views)?.parse()?)?,
            )? {
                Merge::Clean(tree) => tree,
                conflict => return Ok(conflict),
            };
            for (parent, inherited) in &point.inherits {
                include(&mut covered, parent, inherited)?;
            }
            include(&mut covered, &track, &mark)?;
        }
        Ok(Merge::Clean(tree))
    }

    pub fn inherited_from(&self, parents: &Ancestors, views: &Views) -> Result<Ancestors> {
        let mut inherited = Ancestors::new();
        for (track, mark) in parents {
            self.incorporate(
                &mut inherited,
                view::point(self.resolve(track, mark)?, views)?.as_ref(),
            )?;
        }
        Ok(inherited)
    }

    /// A dependency already in the worker's base cannot arrive through that
    /// worker's delta. Its required part must exist in the target first.
    pub fn missing_base_parents(&self, basis: &Basis, coverage: &Ancestors) -> Result<Ancestors> {
        let mut missing = Ancestors::new();
        for (track, required) in &basis.parents {
            self.resolve(track, required)?;
            let Some(base) = basis.base_inherits.get(track) else {
                continue;
            };
            self.resolve(track, base)?;
            let needed = if base.number < required.number {
                base
            } else {
                required
            };
            if let Some(actual) = coverage.get(track) {
                self.resolve(track, actual)?;
                if actual.number >= needed.number {
                    continue;
                }
            }
            missing.insert(track.clone(), needed.clone());
        }
        Ok(missing)
    }

    pub fn incorporate(&self, into: &mut Ancestors, point: &Point) -> Result<()> {
        for (track, mark) in &point.inherits {
            self.resolve(track, mark)?;
            include(into, track, mark)?;
        }
        include(into, &point.track, &point.inherited())
    }

    pub fn roots(&self) -> Result<Vec<ObjectId>> {
        let mut roots: Vec<ObjectId> = self
            .points
            .iter()
            .flat_map(|point| [&point.sha, &point.base_at])
            .map(|sha| sha.parse().map_err(Into::into))
            .collect::<Result<_>>()?;
        for point in &self.points {
            view::roots(&point.views, &mut roots)?;
        }
        Ok(roots)
    }
}
