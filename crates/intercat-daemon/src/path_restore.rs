//! Restore a retired track's cancelled contributions without reviving borrowed
//! files removed by later publications. Files and journal admission are separate.
use crate::{
    Result,
    integration_record::Record,
    lineage::{Ancestors, Basis, Frozen, Inherited},
    merge_choices::{Merges, Solved},
};
use intercat_git::{Merge, ObjectId, Repository};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub retirement_id: Uuid,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccessConsent {
    pub op_id: Uuid,
    pub request: Request,
    pub fingerprint: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ControlAction {
    Retry,
    Hold,
    KeepProcesses,
    DismissRealign { head: String },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Control {
    pub op_id: Uuid,
    pub id: Uuid,
    pub revision: u64,
    pub action: ControlAction,
}
pub fn file_operation(request: &Request) -> Uuid {
    Uuid::new_v5(&request.op_id, b"restore-path-files")
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Queued,
    WaitingForWorker,
    WaitingForProcesses,
    WaitingForAccess,
    NeedsMerge,
    Applying,
    Held,
    Done,
    Failed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Job {
    pub request: Request,
    pub track_id: String,
    pub session_id: String,
    pub folder_id: String,
    pub revision: u64,
    pub phase: Phase,
    pub keep_processes: bool,
    pub error: Option<String>,
    pub inspection: Option<crate::workspace_access::Inspection>,
    pub restored: Option<Restored>,
    pub merge_input: Option<crate::restore_merge::Summary>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Restored {
    pub track_id: String,
    pub session_id: Option<String>,
    pub previous_base: String,
    pub base: String,
    pub checkpoint: Option<crate::checkpoint::Checkpoint>,
    pub suggest_realign: bool,
}
#[derive(Debug)]
pub(crate) struct MergeRequired;
impl std::fmt::Display for MergeRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Resolve the saved Restore merge before applying files.")
    }
}
impl std::error::Error for MergeRequired {}
#[derive(Debug)]
pub(crate) struct Waiting(pub &'static str);
impl std::fmt::Display for Waiting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Waiting {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Inputs {
    pub track: String,
    pub before: String,
    pub basis: Basis,
    pub target: String,
    pub coverage: Ancestors,
    /// Exact saved source context on the restored path, without later edits.
    pub prefix: Ancestors,
    /// This retirement's cancelled receipts for this writer/prefix, newest first.
    pub cancelled: Vec<Record>,
    pub resolutions: Vec<Solved>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Stage {
    Baseline,
    Undo,
    Files,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Conflict {
    pub stage: Stage,
    pub base: String,
    pub target: String,
    pub incoming: String,
    pub paths: Vec<String>,
    pub reversed: Vec<Uuid>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Prepared {
    pub inputs: Inputs,
    pub tree: String,
    pub basis: Basis,
    /// The surviving own publication before the first cancelled checkpoint.
    pub integrated: Option<Inherited>,
    pub reversed: Vec<Uuid>,
}
impl Prepared {
    pub fn verify(&self, repo: &Repository, graph: &Frozen) -> Result<()> {
        if !matches!(prepare(repo, graph, &self.inputs)?, Outcome::Prepared(actual) if actual.as_ref() == self)
        {
            return Err("The saved Restore does not reproduce its files and baseline.".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Prepared(Box<Prepared>),
    Conflict(Box<Conflict>),
}

impl Inputs {
    fn validate(&self, repo: &Repository, graph: &Frozen) -> Result<Option<Inherited>> {
        if self.track.is_empty() || self.cancelled.is_empty() {
            return Err("A realigned Restore needs this track's cancelled publications.".into());
        }
        for (track, mark) in self
            .coverage
            .iter()
            .chain(&self.basis.parents)
            .chain(&self.basis.inherits)
            .chain(&self.basis.base_inherits)
            .chain(&self.prefix)
        {
            if graph.point(track, mark.number)?.inherited() != *mark {
                return Err("The Restore checkpoint coverage changed.".into());
            }
        }
        for record in &self.cancelled {
            record.validate()?;
            if (record.track_id != self.track
                && self
                    .prefix
                    .get(&record.track_id)
                    .is_none_or(|through| record.checkpoint.number > through.number))
                || graph
                    .point(&record.track_id, record.checkpoint.number)?
                    .inherited()
                    != record.checkpoint
            {
                return Err("The cancelled publication belongs to another Restore source.".into());
            }
        }
        for id in [&self.before, &self.target, &self.basis.base] {
            repo.commit_details(id.parse()?)?;
        }
        let integrated = self
            .cancelled
            .iter()
            .rev()
            .find(|record| record.track_id == self.track)
            .and_then(|first| first.coverage_before.get(&self.track))
            .cloned();
        if let Some(mark) = &integrated
            && (mark.number
                >= self
                    .cancelled
                    .iter()
                    .filter(|record| record.track_id == self.track)
                    .map(|record| record.checkpoint.number)
                    .min()
                    .unwrap()
                || graph.point(&self.track, mark.number)?.inherited() != *mark)
        {
            return Err("The Restore's surviving own checkpoint changed.".into());
        }
        Ok(integrated)
    }
}

fn tree(
    result: Merge,
    stage: Stage,
    reversed: &[Uuid],
) -> std::result::Result<ObjectId, Box<Conflict>> {
    match result {
        Merge::Clean(tree) => Ok(tree),
        Merge::Conflicts {
            base,
            target,
            incoming,
            paths,
        } => Err(Box::new(Conflict {
            stage,
            base: base.to_string(),
            target: target.to_string(),
            incoming: incoming.to_string(),
            paths: paths.into_iter().map(|path| path.to_string()).collect(),
            reversed: reversed.into(),
        })),
    }
}

/// A cancelled receipt can span both the shared fork and its retired suffix.
/// The target may have retained the fork, then deliberately changed its files.
/// Keep that covered context in the baseline so it is not a new restored edit.
fn retained_baseline(
    merges: &mut Merges<'_>,
    graph: &Frozen,
    inputs: &Inputs,
    mut baseline: ObjectId,
) -> Result<Merge> {
    let mut covered = inputs.coverage.clone();
    crate::replacement_delta::rewind(&inputs.cancelled, &mut covered);
    let mut required = inputs.prefix.clone();
    if let Some(mark) = inputs.coverage.get(&inputs.track) {
        crate::lineage::include(&mut required, &inputs.track, mark)?;
    }
    let mut pending = Ancestors::new();
    for (track, through) in required {
        let Some(actual) = inputs.coverage.get(&track) else {
            continue;
        };
        let needed = if actual.number < through.number {
            actual
        } else {
            &through
        };
        if covered
            .get(&track)
            .is_none_or(|before| before.number < needed.number)
        {
            pending.insert(track, needed.clone());
        }
    }
    while !pending.is_empty() {
        let mut next = None;
        for (track, mark) in &pending {
            let point = graph.by_sha(track, &mark.sha)?;
            if !crate::lineage::view::point(point, &inputs.basis.views)?
                .inherits
                .keys()
                .any(|parent| pending.contains_key(parent))
            {
                next = Some(track.clone());
                break;
            }
        }
        let track = next.ok_or("The retained Restore context contains a cycle.")?;
        let mark = pending.remove(&track).unwrap();
        let point = graph.by_sha(&track, &mark.sha)?;
        let base = match crate::replacement_reparent::shared_baseline(
            merges,
            graph,
            point,
            &inputs.basis.views,
            &covered,
        )? {
            Merge::Clean(tree) => tree,
            conflict => return Ok(conflict),
        };
        baseline = match merges.merge_trees(
            base,
            baseline,
            merges
                .repository()
                .tree_id(crate::lineage::view::head(point, &inputs.basis.views)?.parse()?)?,
        )? {
            Merge::Clean(tree) => tree,
            conflict => return Ok(conflict),
        };
        graph.incorporate(
            &mut covered,
            crate::lineage::view::point(point, &inputs.basis.views)?.as_ref(),
        )?;
    }
    Ok(Merge::Clean(baseline))
}

pub fn prepare(repo: &Repository, graph: &Frozen, inputs: &Inputs) -> Result<Outcome> {
    let integrated = inputs.validate(repo, graph)?;
    let mut merges = Merges::new(repo, &inputs.resolutions)?;
    let baseline = match tree(
        graph.workspace_baseline(&mut merges, &inputs.track, &inputs.basis, &graph.integrated)?,
        Stage::Baseline,
        &[],
    ) {
        Ok(tree) => tree,
        Err(conflict) => {
            merges.finish()?;
            return Ok(Outcome::Conflict(conflict));
        }
    };
    let undone = crate::replacement_delta::reverse(&mut merges, baseline, &inputs.cancelled)?;
    let baseline = match tree(undone.result, Stage::Undo, &undone.applied) {
        Ok(tree) => tree,
        Err(conflict) => {
            merges.finish()?;
            return Ok(Outcome::Conflict(conflict));
        }
    };
    let baseline = match tree(
        retained_baseline(&mut merges, graph, inputs, baseline)?,
        Stage::Undo,
        &undone.applied,
    ) {
        Ok(tree) => tree,
        Err(conflict) => {
            merges.finish()?;
            return Ok(Outcome::Conflict(conflict));
        }
    };
    let restored = tree(
        merges.merge_trees(
            baseline,
            repo.tree_id(inputs.target.parse()?)?,
            repo.tree_id(inputs.before.parse()?)?,
        )?,
        Stage::Files,
        &undone.applied,
    );
    merges.finish()?;
    Ok(match restored {
        Ok(tree) => Outcome::Prepared(Box::new(Prepared {
            inputs: inputs.clone(),
            tree: tree.to_string(),
            integrated,
            reversed: undone.applied,
            basis: Basis {
                base: inputs.target.clone(),
                base_inherits: inputs.coverage.clone(),
                ..inputs.basis.clone()
            },
        })),
        Err(conflict) => Outcome::Conflict(conflict),
    })
}
