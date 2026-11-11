//! An immutable receipt for the exact per-track delta published to a session.
use crate::{
    Result,
    lineage::{Ancestors, Inherited},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Record {
    pub id: Uuid,
    pub job_id: Uuid,
    pub operation: Uuid,
    pub session_id: String,
    pub folder_id: String,
    pub track_id: String,
    pub checkpoint: Inherited,
    pub ordinal: u64,
    pub position: u64,
    pub reference: String,
    pub sha: String,
    pub parent: String,
    pub tree: String,
    pub coverage_before: Ancestors,
    pub coverage_after: Ancestors,
}
impl Record {
    pub fn validate(&self) -> Result<()> {
        if self.job_id.get_version_num() != 5
            || self.operation.get_version_num() != 5
            || self.id != Uuid::new_v5(&self.job_id, self.sha.as_bytes())
            || self.ordinal == 0
            || self.position == 0
            || self.checkpoint.number == 0
        {
            return Err("Invalid published integration record identity.".into());
        }
        for id in [&self.session_id, &self.folder_id, &self.track_id] {
            if id.is_empty() || id.len() > 128 || id.chars().any(char::is_control) {
                return Err("Invalid published integration scope.".into());
            }
        }
        for sha in [&self.sha, &self.parent, &self.tree, &self.checkpoint.sha] {
            sha.parse::<intercat_git::ObjectId>()?;
        }
        intercat_git::validate_reference(&self.reference)?;
        for mark in self
            .coverage_before
            .values()
            .chain(self.coverage_after.values())
        {
            if mark.number == 0 {
                return Err("Invalid integration coverage number.".into());
            }
            mark.sha.parse::<intercat_git::ObjectId>()?;
        }
        if self
            .coverage_after
            .get(&self.track_id)
            .is_none_or(|mark| mark.number < self.checkpoint.number)
        {
            return Err("The integration record does not include its saved checkpoint.".into());
        }
        for (track, before) in &self.coverage_before {
            if self.coverage_after.get(track).is_none_or(|after| {
                after.number < before.number
                    || after.number == before.number && after.sha != before.sha
            }) {
                return Err("Published integration coverage moved backwards.".into());
            }
        }
        Ok(())
    }
}
