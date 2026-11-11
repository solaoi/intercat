//! Object-only planning for a worker's explicit realignment. File application,
//! process checks and checkpoint publication belong to its durable operation.
use crate::{
    Result,
    lineage::{Ancestors, Basis, Frozen},
};
use intercat_git::{Merge, ObjectId, Repository};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: uuid::Uuid,
    pub track_id: String,
    pub source: Source,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Source {
    Commit {
        job_id: uuid::Uuid,
        revision: u64,
    },
    Reparent {
        job_id: uuid::Uuid,
        revision: u64,
        parent: String,
    },
}
impl Source {
    pub(crate) fn review(&self) -> (uuid::Uuid, u64) {
        match self {
            Self::Commit { job_id, revision }
            | Self::Reparent {
                job_id, revision, ..
            } => (*job_id, *revision),
        }
    }
}
pub(crate) fn file_operation(request: &Request) -> uuid::Uuid {
    uuid::Uuid::new_v5(&request.op_id, b"apply-realignment")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Queued,
    WaitingForWorker,
    Preparing,
    NeedsProcesses,
    WaitingProcesses,
    StoppingProcesses,
    NeedsAccess,
    WaitingAccess,
    NeedsMerge,
    Held,
    Failed,
    Done,
    Discarded,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Job {
    pub request: Request,
    pub session_id: String,
    pub revision: u64,
    pub phase: Phase,
    pub keep_processes: bool,
    pub wait_for_access: bool,
    pub stop_command: Option<uuid::Uuid>,
    pub stop_checkpoint: Option<uuid::Uuid>,
    pub inspection: Option<crate::workspace_access::Inspection>,
    pub error: Option<String>,
    pub merge_input: Option<uuid::Uuid>,
    pub merge_boundary: Option<uuid::Uuid>,
    pub shared_input: Option<crate::shared_merge::Summary>,
    pub resolutions: Vec<crate::merge_choices::Solved>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum ControlAction {
    KeepProcesses {},
    StopProcesses {},
    GoAhead { fingerprint: String },
    Wait {},
    Retry {},
    Discard {},
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Control {
    pub op_id: uuid::Uuid,
    pub id: uuid::Uuid,
    pub revision: u64,
    pub action: ControlAction,
}
#[derive(Debug)]
pub struct Waiting;
impl std::fmt::Display for Waiting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Waiting for the worker's idle connection and checkpoint."
        )
    }
}
impl std::error::Error for Waiting {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConflictStage {
    Parents,
    Baseline,
    Overlay,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Resolution {
    Ready {
        tree: String,
    },
    Conflict {
        stage: ConflictStage,
        base: String,
        target: String,
        incoming: String,
        paths: Vec<String>,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Plan {
    pub track: String,
    pub before: String,
    pub target: String,
    pub before_basis: Basis,
    pub basis: Basis,
    pub resolution: Resolution,
    pub resolutions: Vec<crate::merge_choices::Solved>,
    pub reparent: Option<crate::replacement_reparent::Inputs>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reparent {
    pub destination: crate::replacement_reparent::Destination,
    pub views: crate::lineage::Views,
    pub checkpoint: crate::lineage::Point,
    pub inherited: crate::lineage::Point,
    pub replacement: crate::lineage::Point,
    pub integrated: Option<crate::lineage::Point>,
}
impl From<&crate::replacement_reparent::Inputs> for Reparent {
    fn from(input: &crate::replacement_reparent::Inputs) -> Self {
        Self {
            destination: input.destination.clone(),
            views: input.views.clone(),
            checkpoint: input.checkpoint.clone(),
            inherited: input.inherited.clone(),
            replacement: input.replacement.clone(),
            integrated: input.integrated.clone(),
        }
    }
}
pub struct Input<'a> {
    pub track: &'a str,
    pub basis: &'a Basis,
    pub before: ObjectId,
    pub target: ObjectId,
    pub coverage: &'a Ancestors,
    pub resolutions: &'a [crate::merge_choices::Solved],
}
impl Plan {
    pub fn prepare(repo: &Repository, graph: &Frozen, input: Input<'_>) -> Result<Self> {
        let Input {
            track,
            basis,
            before,
            target,
            coverage,
            resolutions,
        } = input;
        for (owner, mark) in coverage {
            if graph.point(owner, mark.number)?.sha != mark.sha {
                return Err("The published checkpoint coverage changed.".into());
            }
        }
        for (owner, mark) in &basis.base_inherits {
            if coverage
                .get(owner)
                .is_none_or(|next| next.number < mark.number)
            {
                return Err("The realignment target precedes the worker's existing base.".into());
            }
        }
        let mut merges = crate::merge_choices::Merges::new(repo, resolutions)?;
        let target_tree = repo.tree_id(target)?;
        let incoming = repo.tree_id(before)?;
        let (stage, merged) = match graph.workspace_baseline(&mut merges, track, basis, coverage)? {
            Merge::Clean(baseline) => (
                ConflictStage::Overlay,
                merges.merge_trees(baseline, target_tree, incoming)?,
            ),
            conflict => (ConflictStage::Baseline, conflict),
        };
        let resolution = match merged {
            Merge::Clean(tree) => Resolution::Ready {
                tree: tree.to_string(),
            },
            Merge::Conflicts {
                base,
                target,
                incoming,
                paths,
            } => Resolution::Conflict {
                stage,
                base: base.to_string(),
                target: target.to_string(),
                incoming: incoming.to_string(),
                paths: paths.into_iter().map(|path| path.to_string()).collect(),
            },
        };
        merges.finish()?;
        Ok(Self {
            reparent: None,
            resolutions: resolutions.to_vec(),
            track: track.into(),
            before: before.to_string(),
            target: target.to_string(),
            before_basis: basis.clone(),
            basis: Basis {
                views: basis.views.clone(),
                parents: basis.parents.clone(),
                base: target.to_string(),
                base_inherits: coverage.clone(),
                inherits: basis.inherits.clone(),
            },
            resolution,
        })
    }
    pub(crate) fn prepare_reparent(
        repo: &Repository,
        graph: &Frozen,
        basis: &Basis,
        before: ObjectId,
        source: Reparent,
        resolutions: &[crate::merge_choices::Solved],
    ) -> Result<Self> {
        if basis.parents != source.checkpoint.parents
            || basis.inherits != source.checkpoint.inherits
        {
            return Err(
                "The worker's current dependencies changed. Review its saved checkpoint again."
                    .into(),
            );
        }
        let inputs = crate::replacement_reparent::Inputs {
            destination: source.destination,
            views: source.views,
            checkpoint: source.checkpoint,
            inherited: source.inherited,
            replacement: source.replacement,
            integrated: source.integrated,
            worktree: repo.tree_id(before)?.to_string(),
            resolutions: resolutions.to_vec(),
        };
        let ancestry = crate::replacement_reparent::parentage(
            graph,
            &inputs.checkpoint,
            &inputs.inherited,
            &inputs.replacement,
            inputs.integrated.as_ref(),
            &inputs.views,
            &inputs.destination,
        )?;
        let (base, resolution) = match crate::replacement_reparent::prepare(repo, graph, &inputs)? {
            crate::replacement_reparent::Outcome::Prepared(prepared) => {
                let base = repo.checkpoint_commit(
                    prepared.base.parse()?,
                    inputs.replacement.sha.parse()?,
                    "Reparented worker baseline",
                )?;
                repo.sync_objects(&[base])?;
                (
                    base.to_string(),
                    Resolution::Ready {
                        tree: prepared.worktree.clone(),
                    },
                )
            }
            crate::replacement_reparent::Outcome::Conflict(conflict) => (
                basis.base.clone(),
                Resolution::Conflict {
                    stage: match conflict.stage {
                        crate::replacement_reparent::Stage::Parents => ConflictStage::Parents,
                        crate::replacement_reparent::Stage::Worktree => ConflictStage::Overlay,
                        crate::replacement_reparent::Stage::Base => ConflictStage::Baseline,
                    },
                    base: conflict.base,
                    target: conflict.target,
                    incoming: conflict.incoming,
                    paths: conflict.paths,
                },
            ),
        };
        Ok(Self {
            track: inputs.checkpoint.track.clone(),
            before: before.to_string(),
            target: inputs.replacement.sha.clone(),
            before_basis: basis.clone(),
            basis: Basis {
                views: ancestry.views,
                base,
                parents: ancestry.parents,
                inherits: ancestry.inherits,
                base_inherits: ancestry.base_inherits,
            },
            resolution,
            resolutions: resolutions.to_vec(),
            reparent: Some(inputs),
        })
    }
}
