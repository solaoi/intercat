//! Immutable checkpoint ancestry and the baseline used to integrate its delta.
//! Checkpoint numbers, rather than Git reachability, identify incorporated work.
use crate::Result;
use intercat_git::{Merge, ObjectId, Repository};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

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
    pub base: String,
    pub base_inherits: Ancestors,
    pub inherits: Ancestors,
}
impl Basis {
    pub fn root(base: String) -> Self {
        Self {
            base,
            base_inherits: Ancestors::new(),
            inherits: Ancestors::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Point {
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
        repo: &Repository,
        basis: &Basis,
        integrated: &Ancestors,
    ) -> Result<Merge> {
        self.baseline_inner(repo, basis, integrated, &mut BTreeSet::new())
    }
    pub fn track_baseline(
        &self,
        repo: &Repository,
        point: &Point,
        basis: &Basis,
        integrated: &Ancestors,
    ) -> Result<Merge> {
        let mut effective = basis.clone();
        effective.inherits = point.inherits.clone();
        if let Some(mark) = integrated.get(&point.track) {
            self.resolve(&point.track, mark)?;
            let already = self
                .by_sha(&point.track, &effective.base)
                .ok()
                .map_or(0, |base| base.number);
            let number = mark.number.min(point.number);
            if number > already {
                let saved = self.point(&point.track, number)?;
                effective.base = saved.sha.clone();
                effective.base_inherits = saved.inherits.clone();
            }
        }
        self.baseline(repo, &effective, integrated)
    }
    fn baseline_inner(
        &self,
        repo: &Repository,
        basis: &Basis,
        integrated: &Ancestors,
        stack: &mut BTreeSet<(String, u64)>,
    ) -> Result<Merge> {
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
            let next = pending
                .iter()
                .find_map(|(track, mark)| {
                    let point = self.point(track, mark.number).ok()?;
                    (!point
                        .inherits
                        .keys()
                        .any(|parent| pending.contains_key(parent)))
                    .then(|| (track.clone(), mark.clone()))
                })
                .ok_or("The checkpoint ancestry contains a cycle.")?;
            let (track, mark) = next;
            pending.remove(&track);
            let point = self.resolve(&track, &mark)?;
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
                repo.tree_id(self.resolve(&track, old)?.sha.parse()?)?
            } else {
                match self.baseline_inner(repo, &point.basis(), &covered, stack)? {
                    Merge::Clean(base) => base,
                    conflict => {
                        stack.remove(&(track.clone(), mark.number));
                        return Ok(conflict);
                    }
                }
            };
            stack.remove(&(track.clone(), mark.number));
            tree = match repo.merge_trees(base, tree, repo.tree_id(point.sha.parse()?)?)? {
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

    pub fn incorporate(&self, into: &mut Ancestors, point: &Point) -> Result<()> {
        for (track, mark) in &point.inherits {
            self.resolve(track, mark)?;
            include(into, track, mark)?;
        }
        include(into, &point.track, &point.inherited())
    }

    pub fn roots(&self) -> Result<Vec<ObjectId>> {
        self.points
            .iter()
            .flat_map(|point| [&point.sha, &point.base_at])
            .map(|sha| sha.parse().map_err(Into::into))
            .collect()
    }
}
