//! Explicit resolutions are keyed by all three frozen trees. Replaying a
//! lineage synthesis may use earlier choices, but never choices for other inputs.
use crate::Result;
use intercat_git::{Merge, ObjectId, Repository};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Solved {
    pub base: String,
    pub target: String,
    pub incoming: String,
    pub tree: String,
}

pub struct Merges<'a> {
    repository: &'a Repository,
    choices: Vec<((ObjectId, ObjectId, ObjectId), ObjectId)>,
    used: BTreeSet<usize>,
}
impl<'a> Merges<'a> {
    pub fn new(repository: &'a Repository, saved: &[Solved]) -> Result<Self> {
        let mut keys = BTreeSet::new();
        let mut choices = vec![];
        for choice in saved {
            let key = (
                choice.base.parse()?,
                choice.target.parse()?,
                choice.incoming.parse()?,
            );
            if !keys.insert(key) {
                return Err("A frozen merge was resolved more than once.".into());
            }
            choices.push((key, choice.tree.parse()?));
        }
        Ok(Self {
            repository,
            choices,
            used: BTreeSet::new(),
        })
    }
    pub fn repository(&self) -> &'a Repository {
        self.repository
    }
    pub fn merge_trees(
        &mut self,
        base: ObjectId,
        target: ObjectId,
        incoming: ObjectId,
    ) -> Result<Merge> {
        let result = self.repository.merge_trees(base, target, incoming)?;
        if matches!(result, Merge::Conflicts { .. })
            && let Some((index, (_, tree))) = self
                .choices
                .iter()
                .enumerate()
                .find(|(_, (key, _))| *key == (base, target, incoming))
        {
            // Validate the resolved object as a tree before using it in another merge.
            self.repository.tree_files(*tree)?;
            self.used.insert(index);
            return Ok(Merge::Clean(*tree));
        }
        Ok(result)
    }
    pub fn finish(&self) -> Result<()> {
        if self.used.len() != self.choices.len() {
            return Err("A saved resolution belongs to different merge inputs.".into());
        }
        Ok(())
    }
}
