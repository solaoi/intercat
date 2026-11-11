//! Saved branch results. Comparing never chooses, retires, or commits a path.
use crate::{
    Result,
    check_run::{Record, State},
    checkpoint::Checkpoint,
    integration::Change,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Point {
    pub track_id: String,
    pub sha: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Open {
    pub op_id: Uuid,
    pub parent: Point,
    pub child: Point,
}
impl Open {
    pub fn validate(&self) -> Result<()> {
        if self.op_id.get_version_num() != 7 || self.parent.track_id == self.child.track_id {
            return Err("Choose two saved paths from the same branch.".into());
        }
        for point in [&self.parent, &self.child] {
            Uuid::parse_str(&point.track_id)?;
            point.sha.parse::<intercat_git::ObjectId>()?;
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Source {
    pub point: Point,
    pub label: String,
    pub checkpoint: Checkpoint,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Outcome {
    pub state: State,
    pub check: Option<Record>,
    pub reason: Option<String>,
    pub dependency_runs: Vec<crate::dependencies::Run>,
    pub dependency_error: Option<crate::dependencies::Failure>,
}
impl Outcome {
    pub fn cacheable(&self) -> bool {
        self.check.as_ref().is_some_and(|check| {
            match (self.state, check.command.as_ref(), check.code) {
                (State::Passed, None, None) | (State::Passed, Some(_), Some(0)) => true,
                (State::Failed, Some(_), Some(code)) => code != 0,
                _ => false,
            }
        })
    }
    pub fn validate(&self, tree: &str, settings: &str) -> Result<()> {
        if self
            .check
            .as_ref()
            .is_some_and(|check| check.tree != tree || check.settings_hash != settings)
            || matches!(self.state, State::Passed | State::Failed) && self.check.is_none()
        {
            return Err("The Check result belongs to a different saved tree or settings.".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Side {
    pub source: Source,
    pub tree: String,
    pub settings_hash: String,
    pub changes: Vec<Change>,
    pub outcome: Option<Outcome>,
    pub cached: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Ready,
    Running,
    Held,
    Done,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Job {
    pub id: Uuid,
    pub revision: u64,
    pub attempt: u64,
    pub request: Open,
    pub session: String,
    pub folder: crate::project::Folder,
    pub slug: String,
    pub base: crate::branch_start::Point,
    pub settings_hash: String,
    pub reuse_checks: bool,
    pub phase: Phase,
    pub sides: [Side; 2],
}
impl Job {
    pub fn validate(&self) -> Result<()> {
        self.request.validate()?;
        if self.id != self.request.op_id
            || self.sides[0].source.point != self.request.parent
            || self.sides[1].source.point != self.request.child
            || self.settings_hash.len() != 64
            || self.phase != Phase::Done && self.attempt == 0
        {
            return Err("Invalid saved path comparison.".into());
        }
        self.base.sha().parse::<intercat_git::ObjectId>()?;
        for side in &self.sides {
            side.tree.parse::<intercat_git::ObjectId>()?;
            if side.settings_hash.len() != 64
                || !side
                    .settings_hash
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err("Invalid Check settings identity.".into());
            }
            if side.source.checkpoint.sha != side.source.point.sha
                || !side.source.checkpoint.unmerged_paths.is_empty()
            {
                return Err("The comparison needs completed, resolved checkpoints.".into());
            }
            if let Some(result) = &side.outcome {
                result.validate(&side.tree, &side.settings_hash)?;
            }
            if side.cached
                && (!self.reuse_checks || !side.outcome.as_ref().is_some_and(Outcome::cacheable))
            {
                return Err("Invalid saved Check cache receipt.".into());
            }
        }
        if self.phase == Phase::Done
            && self
                .sides
                .iter()
                .any(|side| !side.outcome.as_ref().is_some_and(Outcome::cacheable))
        {
            return Err("The comparison has not checked both saved paths.".into());
        }
        Ok(())
    }
    pub fn project(&self) -> Result<serde_json::Value> {
        self.validate()?;
        let mut value = serde_json::to_value(self)?;
        value["folder"] = serde_json::json!({"id":self.folder.id,"name":self.folder.name});
        value["summary"] = serde_json::to_value(crate::comparison_summary::summarize(self))?;
        Ok(value)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Control {
    pub op_id: Uuid,
    pub id: Uuid,
    pub revision: u64,
    pub attempt: u64,
    pub action: Action,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Hold,
    Retry,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiffRequest {
    pub id: Uuid,
    pub basis: DiffBasis,
    pub path: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiffBasis {
    Parent,
    Child,
    Paths,
}
