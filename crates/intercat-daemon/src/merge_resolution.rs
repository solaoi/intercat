//! A frozen conflict is edited in a shallow private checkout. Only its saved
//! result may become the later track's contribution; work Runs stay untouched.
use crate::{
    Result,
    candidate::{Settings, State},
    checkout::Checkout,
    integration::{Job, Status},
    repair::Original,
};
use intercat_git::Repository;
use serde::{Deserialize, Serialize};
use std::{ffi::OsString, path::Path};
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Conflict {
    Overlay {
        base: String,
        incoming: String,
        paths: Vec<String>,
    },
    Baseline {
        paths: Vec<String>,
    },
    SavedIndex {
        paths: Vec<String>,
    },
}

/// The trigger identifies frozen build inputs, so a changed review revision
/// cannot restart a merge that the user stopped or discarded.
pub fn automatic_request(job: &Job) -> Result<Option<crate::repair_run::Request>> {
    let Some(candidate) = &job.candidate else {
        return Ok(None);
    };
    if job.status != Status::Failed
        || candidate.state != State::Conflict
        || !matches!(candidate.conflict, Some(Conflict::Overlay { .. }))
    {
        return Ok(None);
    }
    let track = candidate
        .failed_track
        .as_ref()
        .ok_or("The conflict has no owner.")?;
    let trigger = serde_json::to_vec(&(
        candidate.build_revision,
        &candidate.head,
        track,
        &candidate.conflict,
    ))?;
    let request = crate::repair_run::Request {
        op_id: Uuid::new_v5(&job.id, &trigger),
        owner: crate::repair_run::Owner::Commit {
            id: job.id,
            revision: job.revision,
        },
        track: track.clone(),
        purpose: crate::repair_run::Purpose::Merge,
    };
    crate::repair_run::Run::validate_request(job, &request)?;
    Ok(Some(request))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Basis {
    pub job_id: Uuid,
    pub revision: u64,
    pub track: String,
    pub target: String,
    pub head: String,
    pub tree: String,
    pub base: String,
    pub checkpoint: String,
    pub incoming: String,
    pub settings_hash: String,
    pub prefix: Vec<Original>,
    pub conflicts: Vec<String>,
    pub marker: Uuid,
}
impl Basis {
    pub fn capture(job: &Job, owner: &str, settings: &Settings, marker: Uuid) -> Result<Self> {
        let candidate = job
            .candidate
            .as_ref()
            .ok_or("Build the conflicted candidate first.")?;
        if job.status != Status::Failed
            || candidate.state != State::Conflict
            || candidate.job_id != job.id
            || candidate.revision != job.revision
            || candidate.settings_hash != settings.hash()?
            || candidate.failed_track.as_deref() != Some(owner)
            || candidate.blocked_commit.is_some()
        {
            return Err("The integration conflict changed. Review it again.".into());
        }
        let plan = job.plan.as_ref().ok_or("Missing frozen conflict plan.")?;
        let owner_position = job
            .tracks
            .iter()
            .position(|track| track.id == owner && track.included)
            .ok_or("The conflict owner is not included.")?;
        let checkpoint = job.tracks[owner_position]
            .checkpoint
            .as_ref()
            .ok_or("The conflict has no checkpoint.")?;
        let (base, incoming, conflicts) = match candidate
            .conflict
            .as_ref()
            .ok_or("Missing frozen conflict details.")?
        {
            Conflict::Overlay {
                base,
                incoming,
                paths,
            } if !paths.is_empty() => (base.clone(), incoming.clone(), paths.clone()),
            Conflict::Baseline { .. } => {
                return Err(
                    "Resolve the inherited baseline before this integration conflict.".into(),
                );
            }
            Conflict::SavedIndex { .. } => {
                return Err(
                    "Resolve the saved checkpoint's own conflicts before integrating it.".into(),
                );
            }
            _ => return Err("The frozen conflict has no unresolved paths.".into()),
        };
        let mut previous = None;
        let target = candidate
            .undo
            .as_ref()
            .map(|undo| undo.sha.clone())
            .unwrap_or_else(|| plan.target_head.clone());
        let mut parent = target.clone();
        let mut prefix = Vec::new();
        for commit in &candidate.commits {
            let position = job
                .tracks
                .iter()
                .position(|track| track.id == commit.track && track.included)
                .ok_or("Invalid conflict prefix owner.")?;
            if position >= owner_position
                || previous.is_some_and(|old| old >= position)
                || commit.parent != parent
            {
                return Err(
                    "The candidate prefix is not an ordered chain before this conflict.".into(),
                );
            }
            parent = commit.sha.clone();
            previous = Some(position);
            prefix.push(Original {
                track: commit.track.clone(),
                sha: commit.sha.clone(),
                parent: commit.parent.clone(),
                tree: commit.tree.clone(),
            });
        }
        if candidate.head != parent
            || prefix
                .last()
                .is_some_and(|commit| commit.tree != candidate.tree)
        {
            return Err("The candidate prefix changed before resolving the conflict.".into());
        }
        let repository = Repository::open(&job.project.root)?;
        if prefix.is_empty()
            && repository.tree_id(target.parse()?)?
                != candidate.tree.parse::<intercat_git::ObjectId>()?
        {
            return Err("The empty conflict prefix no longer matches its target.".into());
        }
        Ok(Self {
            job_id: job.id,
            revision: job.revision,
            track: owner.into(),
            target,
            head: candidate.head.clone(),
            tree: candidate.tree.clone(),
            base,
            checkpoint: checkpoint.sha.clone(),
            incoming,
            settings_hash: candidate.settings_hash.clone(),
            prefix,
            conflicts,
            marker,
        })
    }
    fn verify_prefix(&self, repository: &Repository) -> Result<()> {
        repository.commit_details(self.target.parse()?)?;
        for commit in &self.prefix {
            let actual = repository.commit_details(commit.sha.parse()?)?;
            if actual.parents != vec![commit.parent.parse::<intercat_git::ObjectId>()?]
                || actual.tree != commit.tree
            {
                return Err("The private conflict prefix changed.".into());
            }
        }
        if repository.tree_id(self.head.parse()?)? != self.tree.parse::<intercat_git::ObjectId>()? {
            return Err("The conflict prefix tree changed.".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Plan {
    pub basis: Basis,
    pub tree: String,
    pub left_out: Vec<String>,
}
impl Plan {
    pub fn validate(&self, job: &Job, settings: &Settings) -> Result<()> {
        if self.basis != Basis::capture(job, &self.basis.track, settings, self.basis.marker)? {
            return Err("The merge result belongs to another conflict.".into());
        }
        let _: intercat_git::ObjectId = self.tree.parse()?;
        Ok(())
    }
}

pub struct Workspace {
    pub files: crate::merge_workspace::Workspace,
    pub(crate) basis: Box<Basis>,
}
impl Basis {
    fn files(&self) -> Result<crate::merge_workspace::Input> {
        use sha2::{Digest, Sha256};
        let binding = Sha256::digest(serde_json::to_vec(self)?)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(crate::merge_workspace::Input {
            binding,
            track: self.track.clone(),
            head: self.head.clone(),
            base: self.base.clone(),
            target: self.tree.clone(),
            incoming: self.incoming.clone(),
            conflicts: self.conflicts.clone(),
            marker: self.marker,
        })
    }
}
impl Workspace {
    pub async fn rebuild(
        &self,
        job: &Job,
        plan: &Plan,
        root: &Path,
        cache_root: &Path,
        settings: &Settings,
        cancel: watch::Receiver<bool>,
    ) -> Result<crate::candidate::Candidate> {
        plan.validate(job, settings)?;
        if plan.basis != *self.basis {
            return Err("The merge plan belongs to another private workspace.".into());
        }
        self.files.checkout.verify_storage()?;
        crate::candidate::build_resolved(
            job,
            root,
            cache_root,
            settings,
            crate::candidate::MergeInput {
                plan,
                repository: &self.files.checkout.workdir,
                original: &self.files.original,
            },
            cancel,
        )
        .await
    }
    pub(crate) fn validate(&self, job: &Job, owner: &str, settings: &Settings) -> Result<()> {
        if *self.basis != Basis::capture(job, owner, settings, self.basis.marker)?
            || self.files.input != self.basis.files()?
        {
            return Err("The merge workspace belongs to another conflict.".into());
        }
        self.files.checkout.verify_storage()
    }
    pub async fn prepare(
        job: &Job,
        candidate_root: &Path,
        root: &Path,
        owner: &str,
        settings: &Settings,
        marker: Uuid,
        cancel: watch::Receiver<bool>,
    ) -> Result<Self> {
        let basis = Basis::capture(job, owner, settings, marker)?;
        let original = Checkout::resume(
            candidate_root,
            job.project.source().await?,
            settings.checkout.clone(),
            OsString::from(&settings.path),
            settings.toolchain.clone(),
        )
        .await?;
        basis.verify_prefix(&Repository::open(&original.workdir)?)?;
        let files = crate::merge_workspace::Workspace::prepare(
            &original,
            root,
            basis.files()?,
            &settings.path,
            &settings.toolchain,
            cancel,
        )
        .await?;
        Ok(Self {
            basis: Box::new(basis),
            files,
        })
    }
    pub async fn resume(
        job: &Job,
        candidate_root: &Path,
        root: &Path,
        owner: &str,
        settings: &Settings,
        marker: Uuid,
    ) -> Result<Self> {
        let basis = Basis::capture(job, owner, settings, marker)?;
        let original = Checkout::resume(
            candidate_root,
            job.project.source().await?,
            settings.checkout.clone(),
            OsString::from(&settings.path),
            settings.toolchain.clone(),
        )
        .await?;
        basis.verify_prefix(&Repository::open(&original.workdir)?)?;
        let files = crate::merge_workspace::Workspace::resume(
            &original,
            root,
            basis.files()?,
            &settings.path,
            &settings.toolchain,
        )
        .await?;
        Ok(Self {
            basis: Box::new(basis),
            files,
        })
    }
    pub async fn finish(
        &self,
        job: &Job,
        settings: &Settings,
        cancel: watch::Receiver<bool>,
    ) -> Result<Plan> {
        self.validate(job, &self.basis.track, settings)?;
        self.basis
            .verify_prefix(&Repository::open(&self.files.original)?)?;
        let result = self.files.finish(cancel).await?;
        Ok(Plan {
            basis: self.basis.as_ref().clone(),
            tree: result.tree,
            left_out: result.left_out,
        })
    }
    pub async fn resolve(
        &self,
        snapshot: &crate::merge_manual::Snapshot,
        job: &Job,
        settings: &Settings,
        cancel: watch::Receiver<bool>,
    ) -> Result<Plan> {
        self.validate(job, &self.basis.track, settings)?;
        self.basis
            .verify_prefix(&Repository::open(&self.files.original)?)?;
        let result = crate::merge_editor::resolve(&self.files, snapshot, cancel).await?;
        Ok(Plan {
            basis: self.basis.as_ref().clone(),
            tree: result.tree,
            left_out: result.left_out,
        })
    }
}
