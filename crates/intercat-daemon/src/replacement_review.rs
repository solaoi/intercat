//! A saved path choice determines undo receipts and affected dependencies.
//! Evaluation is read-only: stopping, holding words and changing files belong
//! to the confirmed operation, not to a preview or a radio-button selection.
use crate::{
    Result,
    branch_choices::{Choice, Family},
    integration_record::Record,
    lineage::{Frozen, Inherited, Point},
    replacement_delta::{self, Boundary, Mixed, Selection},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Review {
    pub families: Vec<Family>,
    pub choices: BTreeMap<String, Choice>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Parent {
    pub track: String,
    pub checkpoint: Inherited,
    /// A retired root is replaced by its chosen path. A descendant follows
    /// the new checkpoint of its own parent after that parent is realigned.
    pub onto: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Descendant {
    pub track: String,
    pub included: bool,
    pub parents: Vec<Parent>,
}
#[derive(Clone, Debug)]
pub struct Evaluation {
    pub pending: Vec<String>,
    pub retired: BTreeMap<String, Boundary>,
    pub records: Vec<Record>,
    pub mixed: Vec<Mixed>,
    pub descendants: Vec<Descendant>,
}
impl Evaluation {
    pub fn ready(&self) -> bool {
        self.pending.is_empty()
            && self.mixed.is_empty()
            && self.descendants.iter().all(|child| !child.included)
    }
}

fn later(point: &Point, retired: &BTreeMap<String, Boundary>) -> bool {
    retired.iter().any(|(source, boundary)| {
        point
            .inherits
            .get(source)
            .is_some_and(|mark| mark.number > boundary.shared_through())
    })
}
impl Review {
    /// Frozen source context retained by the selected paths. A continuation
    /// retains its closed source just as a real fork retains its exact prefix.
    pub(crate) fn shared_prefixes(&self) -> Result<crate::lineage::Ancestors> {
        self.validate()?;
        let mut prefixes = crate::lineage::Ancestors::new();
        for family in &self.families {
            if let Some(choice) = self.choices.get(&family.root) {
                for (track, mark) in family.prefix(&choice.kept)? {
                    crate::lineage::include(&mut prefixes, &track, &mark)?;
                }
            }
        }
        Ok(prefixes)
    }
    fn validate(&self) -> Result<()> {
        let mut members = BTreeSet::new();
        let mut roots = BTreeSet::new();
        for family in &self.families {
            if !roots.insert(family.root.as_str())
                || family
                    .members
                    .iter()
                    .any(|member| !members.insert(member.id.as_str()))
            {
                return Err("A path belongs to more than one saved branch family.".into());
            }
            // Validate even an unanswered family without requiring an available
            // source; the only available path might still be preparing.
            family.validate()?;
        }
        for (root, choice) in &self.choices {
            let family = self
                .families
                .iter()
                .find(|family| family.root == *root)
                .ok_or("The saved path choice has no branch family.")?;
            if family.choose(&choice.kept)? != *choice {
                return Err("The saved path choice no longer matches its fork points.".into());
            }
        }
        Ok(())
    }

    /// The transport supplies only a member ID. Boundaries always come from
    /// this saved family, never from renderer-supplied checkpoint numbers.
    pub fn choose(&mut self, kept: &str) -> Result<()> {
        self.validate()?;
        let family = self
            .families
            .iter()
            .find(|family| family.members.iter().any(|member| member.id == kept))
            .ok_or("Choose a path from this saved review.")?;
        self.choices
            .insert(family.root.clone(), family.choose(kept)?);
        Ok(())
    }

    /// `current` contains one saved point per track in the folder, including
    /// excluded descendants. `included` is the current Commit selection.
    pub fn evaluate(
        &self,
        graph: &Frozen,
        current: &[Point],
        included: &BTreeSet<String>,
        records: &[Record],
        cancelled: &BTreeSet<Uuid>,
    ) -> Result<Evaluation> {
        self.validate()?;
        let mut points = BTreeMap::new();
        for point in current {
            if points.insert(point.track.as_str(), point).is_some()
                || graph.by_sha(&point.track, &point.sha)? != point
            {
                return Err("The branch review's saved track checkpoint changed.".into());
            }
        }
        if included
            .iter()
            .any(|track| !points.contains_key(track.as_str()))
        {
            return Err("An included path has no saved checkpoint in this review.".into());
        }
        let mut pending = Vec::new();
        let mut retired = BTreeMap::new();
        let mut replacements = BTreeMap::new();
        for family in &self.families {
            let active = self.choices.contains_key(&family.root)
                || family.members.iter().any(|member| {
                    included.contains(&member.id)
                        || current.iter().any(|point| {
                            included.contains(&point.track)
                                && point.inherits.contains_key(&member.id)
                        })
                });
            if !active {
                continue;
            }
            let Some(choice) = self.choices.get(&family.root) else {
                pending.push(family.root.clone());
                continue;
            };
            for (track, boundary) in &choice.retired {
                retired.insert(track.clone(), *boundary);
                replacements.insert(track.clone(), choice.kept.clone());
            }
        }
        for choice in self.choices.values() {
            if let Some(point) = points.get(choice.kept.as_str())
                && later(point, &choice.retired)
            {
                return Err("The chosen path also inherits changes that would be retired. Keep their source path or review a different saved branch.".into());
            }
        }
        if included.iter().any(|id| retired.contains_key(id)) {
            return Err("A retired path cannot be included in the same Commit.".into());
        }
        let (records, mixed) = if retired.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            match replacement_delta::select(records, &retired, cancelled)? {
                Selection::Ready(records) => (records, Vec::new()),
                Selection::Mixed(mixed) => (Vec::new(), mixed),
            }
        };
        let mut descendants = Vec::new();
        let closed_sources = self
            .families
            .iter()
            .flat_map(|family| &family.forks)
            .filter(|fork| fork.kind == crate::branch_choices::ForkKind::Continuation)
            .map(|fork| fork.parent.as_str())
            .collect::<BTreeSet<_>>();
        for point in current {
            if retired.contains_key(&point.track)
                || closed_sources.contains(point.track.as_str())
                || !later(point, &retired)
            {
                continue;
            }
            let mut parents = Vec::new();
            for (track, mark) in &point.parents {
                let raw = graph.by_sha(track, &mark.sha)?;
                if raw.inherited() != *mark {
                    return Err("A descendant's saved parent checkpoint changed.".into());
                }
                let parent = crate::lineage::view::point(raw, &point.views)?;
                let changed_root = retired
                    .get(track)
                    .is_some_and(|boundary| mark.number > boundary.shared_through());
                if changed_root || later(&parent, &retired) {
                    parents.push(Parent {
                        track: track.clone(),
                        checkpoint: mark.clone(),
                        onto: if changed_root {
                            replacements[track].clone()
                        } else {
                            track.clone()
                        },
                    });
                }
            }
            if parents.is_empty() {
                return Err(
                    "The affected descendant has no recorded direct parent to realign.".into(),
                );
            }
            descendants.push(Descendant {
                track: point.track.clone(),
                included: included.contains(&point.track),
                parents,
            });
        }
        // Keep the caller's stable order among independent tracks, but always
        // update B before a C that still inherits B's old saved checkpoint.
        let mut ordered = Vec::new();
        while !descendants.is_empty() {
            let next = descendants.iter().position(|child| {
                !child
                    .parents
                    .iter()
                    .any(|parent| descendants.iter().any(|other| other.track == parent.onto))
            });
            ordered.push(
                descendants.remove(next.ok_or("The affected dependency chain contains a cycle.")?),
            );
        }
        Ok(Evaluation {
            pending,
            retired,
            records,
            mixed,
            descendants: ordered,
        })
    }
}
