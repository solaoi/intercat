//! Freeze branch families from actual saved fork points. Selection describes
//! retired paths and exact undo boundaries; it never stops or changes a worker.
use crate::{
    Result,
    branch::{Branch, Phase},
    branch_points::Origin,
    lineage::{Frozen, Inherited},
    replacement_delta::Boundary,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Member {
    pub id: String,
    pub label: String,
    pub checkpoint: Option<Inherited>,
    pub available: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Fork {
    pub kind: ForkKind,
    pub parent: String,
    pub child: String,
    pub checkpoint: Inherited,
    pub origin: Origin,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ForkKind {
    Branch,
    Continuation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Family {
    pub root: String,
    pub members: Vec<Member>,
    pub forks: Vec<Fork>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Choice {
    pub root: String,
    pub kept: String,
    pub retired: BTreeMap<String, Boundary>,
}
fn root<'a>(track: &'a str, forks: &BTreeMap<&str, &'a Fork>) -> Result<&'a str> {
    let mut current = track;
    let mut seen = BTreeSet::new();
    while let Some(fork) = forks.get(current) {
        if !seen.insert(current) {
            return Err("The saved branch family contains a cycle.".into());
        }
        current = &fork.parent;
    }
    Ok(current)
}
fn index(family: &Family) -> Result<BTreeMap<&str, &Fork>> {
    let members = family
        .members
        .iter()
        .map(|member| member.id.as_str())
        .collect::<BTreeSet<_>>();
    if members.len() != family.members.len()
        || members.len() < 2
        || !members.contains(family.root.as_str())
        || family.forks.len() != members.len() - 1
    {
        return Err("The saved branch family changed. Review its paths again.".into());
    }
    let mut forks = BTreeMap::new();
    for fork in &family.forks {
        if fork.parent == fork.child
            || fork.child == family.root
            || !members.contains(fork.parent.as_str())
            || !members.contains(fork.child.as_str())
            || fork.checkpoint.number == 0
            || forks.insert(fork.child.as_str(), fork).is_some()
        {
            return Err("The saved branch family has an invalid source checkpoint.".into());
        }
        fork.checkpoint.sha.parse::<intercat_git::ObjectId>()?;
    }
    for member in &family.members {
        if member.id.is_empty()
            || member.label.is_empty()
            || member.available && member.checkpoint.is_none()
            || root(&member.id, &forks)? != family.root
        {
            return Err("A saved branch path no longer belongs to this family.".into());
        }
        if let Some(point) = &member.checkpoint {
            if point.number == 0 {
                return Err("A branch checkpoint number is missing.".into());
            }
            point.sha.parse::<intercat_git::ObjectId>()?;
        }
    }
    Ok(forks)
}
impl Family {
    pub fn has_alternatives(&self) -> bool {
        self.forks.iter().any(|fork| fork.kind == ForkKind::Branch)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        index(self).map(|_| ())
    }
    /// Exact saved context shared by this path. Continuations keep their whole
    /// closed source; a real branch keeps only its recorded fork prefix.
    pub(crate) fn prefix(&self, kept: &str) -> Result<crate::lineage::Ancestors> {
        let forks = index(self)?;
        if !self.members.iter().any(|member| member.id == kept) {
            return Err("Choose a path from this saved branch family.".into());
        }
        let mut prefix = crate::lineage::Ancestors::new();
        let mut current = kept;
        while let Some(fork) = forks.get(current) {
            crate::lineage::include(&mut prefix, &fork.parent, &fork.checkpoint)?;
            current = &fork.parent;
        }
        Ok(prefix)
    }
    pub fn choose(&self, kept: &str) -> Result<Choice> {
        let forks = index(self)?;
        let chosen = self
            .members
            .iter()
            .find(|member| member.id == kept)
            .ok_or("Choose a path from this saved branch family.")?;
        if !chosen.available || chosen.checkpoint.is_none() {
            return Err("Wait for this path's saved checkpoint before choosing it.".into());
        }
        let mut retired = self
            .members
            .iter()
            .filter(|member| member.id != kept)
            .map(|member| (member.id.clone(), Boundary::EntireTrack))
            .collect::<BTreeMap<_, _>>();
        let mut current = kept;
        while let Some(fork) = forks.get(current) {
            match fork.kind {
                ForkKind::Branch => {
                    retired.insert(fork.parent.clone(), Boundary::Fork(fork.checkpoint.number));
                }
                ForkKind::Continuation => {
                    // This source permanently stopped at the saved boundary.
                    // Continuing it preserves all of its published receipts;
                    // only a genuine alternate cuts or replaces this prefix.
                    retired.remove(&fork.parent);
                }
            }
            current = &fork.parent;
        }
        Ok(Choice {
            root: self.root.clone(),
            kept: kept.into(),
            retired,
        })
    }
}

/// Members are already restricted to the review's folder and Intercat session.
/// Unavailable sources may remain visible so choosing another path can retire them.
pub fn capture(
    members: &[Member],
    graph: &Frozen,
    slug: &str,
    branches: &[Branch],
    continuations: &[crate::continuation::Continuation],
) -> Result<Vec<Family>> {
    let by_id = members
        .iter()
        .map(|member| (member.id.as_str(), member))
        .collect::<BTreeMap<_, _>>();
    if by_id.len() != members.len() {
        return Err("Repeated branch review member.".into());
    }
    let mut all_forks = BTreeMap::new();
    let mut ready = BTreeMap::new();
    for branch in branches {
        let Some(child) = by_id.get(branch.track_id.as_str()) else {
            continue;
        };
        let parent = by_id
            .get(branch.request.parent_track_id.as_str())
            .ok_or("The source of this branch is missing from the review.")?;
        let reference = format!("refs/intercat/{slug}/tracks/{}", child.label);
        intercat_git::validate_reference(&reference)?;
        if branch.workspace.track_id != child.id
            || branch.workspace.reference != reference
            || !branch.point.matches(&branch.request.point)
            || parent.label.trim_end_matches(|c: char| c.is_ascii_digit())
                != child.label.trim_end_matches(|c: char| c.is_ascii_digit())
        {
            return Err("This saved branch belongs to another review or source track.".into());
        }
        let point = graph.by_sha(&parent.id, branch.point.sha())?;
        if all_forks
            .insert(
                child.id.as_str(),
                Fork {
                    kind: ForkKind::Branch,
                    parent: parent.id.clone(),
                    child: child.id.clone(),
                    checkpoint: point.inherited(),
                    origin: branch.origin.clone(),
                },
            )
            .is_some()
        {
            return Err("A branch path has more than one recorded source.".into());
        }
        ready.insert(child.id.as_str(), branch.phase == Phase::Ready);
    }
    for continuation in continuations {
        let Some(child) = by_id.get(continuation.successor.as_str()) else {
            continue;
        };
        let parent = by_id
            .get(continuation.source.as_str())
            .ok_or("The continuation source is missing from this review.")?;
        let point = graph.by_sha(&parent.id, &continuation.checkpoint.sha)?;
        if parent.available
            || parent.label.trim_end_matches(|c: char| c.is_ascii_digit())
                != child.label.trim_end_matches(|c: char| c.is_ascii_digit())
        {
            return Err("This continuation has an open or unrelated source writer.".into());
        }
        if all_forks
            .insert(
                child.id.as_str(),
                Fork {
                    kind: ForkKind::Continuation,
                    parent: parent.id.clone(),
                    child: child.id.clone(),
                    checkpoint: point.inherited(),
                    origin: continuation.origin.clone(),
                },
            )
            .is_some()
        {
            return Err("This continuation has more than one source.".into());
        }
        ready.insert(child.id.as_str(), continuation.ready);
    }
    let refs = all_forks
        .iter()
        .map(|(id, fork)| (*id, fork))
        .collect::<BTreeMap<_, _>>();
    let mut groups = BTreeMap::<String, Family>::new();
    for member in members {
        let owner = root(&member.id, &refs)?;
        let family = groups.entry(owner.into()).or_insert_with(|| Family {
            root: owner.into(),
            members: vec![],
            forks: vec![],
        });
        if let Some(checkpoint) = &member.checkpoint
            && graph.by_sha(&member.id, &checkpoint.sha)?.inherited() != *checkpoint
        {
            return Err("The branch review's saved checkpoint changed.".into());
        }
        let mut member = member.clone();
        member.available &= ready.get(member.id.as_str()).copied().unwrap_or(true);
        family.members.push(member);
    }
    let mut result = vec![];
    for (_, mut family) in groups {
        if family.members.len() < 2 {
            continue;
        }
        family.forks = all_forks
            .values()
            .filter(|fork| family.members.iter().any(|member| member.id == fork.child))
            .cloned()
            .collect();
        index(&family)?;
        result.push(family);
    }
    result.sort_by_key(|family| members.iter().position(|member| member.id == family.root));
    Ok(result)
}
