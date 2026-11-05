//! Isolated edits to a failed candidate and attribution back into its commit chain.
//! No refs or objects in the project repository are written here. A returned plan
//! still needs new commits, hooks, a secret scan, and Check before publication.
use crate::{
    Result,
    candidate::{Settings, State},
    checkout::Checkout,
    integration::{Job, Status},
};
use intercat_git::{Merge, ObjectId, Repository};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    ffi::OsString,
    path::{Path, PathBuf},
};
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Original {
    pub track: String,
    pub sha: String,
    pub parent: String,
    pub tree: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Basis {
    pub job_id: Uuid,
    pub revision: u64,
    pub target: String,
    pub head: String,
    pub tree: String,
    pub settings_hash: String,
    pub commits: Vec<Original>,
}

impl Basis {
    fn capture(job: &Job, settings: &Settings) -> Result<Self> {
        let candidate = job.candidate.as_ref().ok_or("No candidate to repair.")?;
        if !matches!(job.status, Status::Failed | Status::Checked)
            || candidate.job_id != job.id
            || candidate.revision != job.revision
            || candidate.settings_hash != settings.hash()?
            || !matches!(
                candidate.state,
                State::Checked | State::CheckFailed | State::CheckMutated | State::SecretFound
            )
        {
            return Err("The failed candidate changed. Review it again.".into());
        }
        let plan = job.plan.as_ref().ok_or("No frozen integration plan.")?;
        let mut parent = plan.target_head.clone();
        let mut tracks = HashSet::new();
        let mut position = None;
        let mut commits = Vec::new();
        for commit in candidate.commits.iter().chain(&candidate.blocked_commit) {
            let index = job
                .tracks
                .iter()
                .position(|track| track.id == commit.track && track.included)
                .ok_or("The repair owner is not included in this job.")?;
            if commit.parent != parent
                || !tracks.insert(&commit.track)
                || position.is_some_and(|previous| index <= previous)
            {
                return Err("The failed candidate is not an ordered per-track chain.".into());
            }
            position = Some(index);
            parent = commit.sha.clone();
            commits.push(Original {
                track: commit.track.clone(),
                sha: commit.sha.clone(),
                parent: commit.parent.clone(),
                tree: commit.tree.clone(),
            });
        }
        let tip = commits
            .last()
            .ok_or("The candidate has no commit to repair.")?;
        // A secret-blocked commit has not been imported or made the candidate
        // head. All other failures must identify the end of the complete chain.
        if let Some(blocked) = &candidate.blocked_commit {
            if candidate.state != State::SecretFound || blocked.parent != candidate.head {
                return Err("Invalid secret-blocked candidate.".into());
            }
        } else if tip.sha != candidate.head || tip.tree != candidate.tree {
            return Err("The failed candidate tip does not match its commits.".into());
        }
        Ok(Self {
            job_id: job.id,
            revision: job.revision,
            target: plan.target_head.clone(),
            head: tip.sha.clone(),
            tree: tip.tree.clone(),
            settings_hash: candidate.settings_hash.clone(),
            commits,
        })
    }

    fn verify_objects(&self, repository: &Repository) -> Result<()> {
        repository.commit_details(self.target.parse()?)?;
        for commit in &self.commits {
            let actual = repository.commit_details(commit.sha.parse()?)?;
            if actual.parents != vec![commit.parent.parse::<ObjectId>()?]
                || actual.tree != commit.tree
            {
                return Err("The repair source no longer matches the failed candidate.".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Step {
    pub original: Original,
    pub tree: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Plan {
    pub basis: Basis,
    pub requested_track: String,
    pub applied_track: String,
    pub tree: String,
    pub steps: Vec<Step>,
    pub left_out: Vec<String>,
}

impl Plan {
    pub fn validate(&self, job: &Job, settings: &Settings) -> Result<()> {
        if self.basis != Basis::capture(job, settings)? {
            return Err("The repair result belongs to an older candidate.".into());
        }
        Ok(())
    }

    pub(crate) fn verify(&self, repo: &Repository, cancel: &watch::Receiver<bool>) -> Result<()> {
        if self.tree == self.basis.tree {
            return Err("The repair did not change the candidate tree.".into());
        }
        let (index, trees) = attribute(
            repo,
            &self.basis,
            &self.requested_track,
            self.tree.parse()?,
            cancel,
        )?;
        if self.applied_track != self.basis.commits[index].track
            || self.steps.len() != self.basis.commits.len()
            || self
                .steps
                .iter()
                .zip(&self.basis.commits)
                .zip(trees)
                .any(|((step, original), tree)| step.original != *original || step.tree != tree)
        {
            return Err("The repair plan does not reproduce the captured tree.".into());
        }
        Ok(())
    }
}

pub struct Workspace {
    pub checkout: Checkout,
    basis: Basis,
    owner: String,
    pub(crate) original: PathBuf,
}

impl Workspace {
    pub fn snapshot_workspace(&self, run: Uuid) -> crate::preparation::Workspace {
        crate::preparation::Workspace {
            attachment_roots: self.checkout.attachment_roots.clone(),
            track_id: self.owner.clone(),
            path: self.checkout.workdir.clone(),
            reference: format!("refs/intercat/repairs/{run}"),
            base: self.basis.head.clone(),
            state: "ready".into(),
            error: None,
        }
    }
    pub(crate) fn validate(&self, job: &Job, owner: &str, settings: &Settings) -> Result<()> {
        if self.basis != Basis::capture(job, settings)? || self.owner != owner {
            return Err("The repair workspace belongs to another candidate or owner.".into());
        }
        self.checkout.verify_storage()
    }

    /// Reopen a held repair without redeploying the failed tree over its edits.
    pub async fn resume(
        job: &Job,
        candidate_root: &Path,
        root: &Path,
        owner: &str,
        settings: &Settings,
    ) -> Result<Self> {
        let basis = Basis::capture(job, settings)?;
        if !basis.commits.iter().any(|commit| commit.track == owner) {
            return Err("Choose a track with a commit in the failed candidate.".into());
        }
        let original = Checkout::resume(
            candidate_root,
            job.project.source().await?,
            settings.checkout.clone(),
            OsString::from(&settings.path),
            settings.toolchain.clone(),
        )
        .await?;
        basis.verify_objects(&Repository::open(&original.workdir)?)?;
        let source = Repository::worktree_source(&original.workdir, Some(basis.head.parse()?))?;
        let checkout = Checkout::resume(
            root,
            source,
            settings.checkout.clone(),
            OsString::from(&settings.path),
            settings.toolchain.clone(),
        )
        .await?;
        if Repository::worktree_source(&checkout.workdir, None)?.head != basis.head {
            return Err("The held repair changed its Git head.".into());
        }
        Ok(Self {
            checkout,
            basis,
            owner: owner.into(),
            original: original.workdir,
        })
    }

    /// Clone only the failed tip. Earlier trees are copied after the editing
    /// process has ended, when they are needed for attribution, not exposed to it.
    pub async fn prepare(
        job: &Job,
        candidate_root: &Path,
        root: &Path,
        owner: &str,
        settings: &Settings,
        cancel: watch::Receiver<bool>,
    ) -> Result<Self> {
        let basis = Basis::capture(job, settings)?;
        if !basis.commits.iter().any(|commit| commit.track == owner) {
            return Err("Choose a track with a commit in the failed candidate.".into());
        }
        let candidate = Checkout::resume(
            candidate_root,
            job.project.source().await?,
            settings.checkout.clone(),
            OsString::from(&settings.path),
            settings.toolchain.clone(),
        )
        .await?;
        basis.verify_objects(&Repository::open(&candidate.workdir)?)?;
        let source = Repository::worktree_source(&candidate.workdir, Some(basis.head.parse()?))?;
        let checkout = Checkout::create(
            root,
            source,
            basis.head.parse()?,
            settings.checkout.clone(),
            OsString::from(&settings.path),
            settings.toolchain.clone(),
            cancel.clone(),
        )
        .await?;
        checkout.deploy(basis.tree.parse()?, cancel).await?;
        Ok(Self {
            checkout,
            basis,
            owner: owner.into(),
            original: candidate.workdir,
        })
    }

    /// The editing host and all its children must have stopped before capture.
    /// This returns object IDs in this workspace, never imports into the project.
    pub async fn finish(
        &self,
        job: &Job,
        candidate_root: &Path,
        settings: &Settings,
        cancel: watch::Receiver<bool>,
    ) -> Result<Plan> {
        if self.basis != Basis::capture(job, settings)? {
            return Err("The repair result belongs to an older candidate.".into());
        }
        self.checkout.verify_storage()?;
        let head = self
            .checkout
            .git(&["rev-parse".into(), "HEAD".into()], false, cancel.clone())
            .await?
            .require_success("Read repair head")?;
        if String::from_utf8(head.stdout)?.trim() != self.basis.head {
            return Err("The repair changed its Git head. Keep edits in the working tree.".into());
        }
        let left_out = self
            .checkout
            .stage(self.basis.head.parse()?, cancel.clone())
            .await?;
        let repository = Repository::open(&self.checkout.workdir)?;
        let repaired = repository.tree_from_index(&self.checkout.workdir.join(".git/index"))?;
        if repaired == self.basis.tree {
            return Err("The repair did not change the candidate tree.".into());
        }
        let source = Checkout::resume(
            candidate_root,
            job.project.source().await?,
            settings.checkout.clone(),
            OsString::from(&settings.path),
            settings.toolchain.clone(),
        )
        .await?;
        let originals = Repository::open(&source.workdir)?;
        self.basis.verify_objects(&originals)?;
        repository.import_tree(
            &source.workdir,
            originals.tree_id(self.basis.target.parse()?)?,
        )?;
        for commit in &self.basis.commits {
            repository.import_tree(&source.workdir, commit.tree.parse()?)?;
        }
        let (applied, trees) = attribute(&repository, &self.basis, &self.owner, repaired, &cancel)?;
        repository.sync_objects(&trees)?;
        Ok(Plan {
            basis: self.basis.clone(),
            requested_track: self.owner.clone(),
            applied_track: self.basis.commits[applied].track.clone(),
            tree: repaired.to_string(),
            steps: self
                .basis
                .commits
                .iter()
                .cloned()
                .zip(trees)
                .map(|(original, tree)| Step {
                    original,
                    tree: tree.to_string(),
                })
                .collect(),
            left_out,
        })
    }

    /// Rebuild and validate real commits using the existing candidate pipeline.
    /// This does not admit the result to a job or move any publication refs.
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
        if plan.basis != self.basis {
            return Err("The repair plan belongs to another workspace.".into());
        }
        self.checkout.verify_storage()?;
        plan.verify(&Repository::open(&self.checkout.workdir)?, &cancel)?;
        crate::candidate::build_repaired(
            job,
            root,
            cache_root,
            settings,
            crate::candidate::RepairInput {
                plan,
                repository: &self.checkout.workdir,
                original: &self.original,
            },
            cancel,
        )
        .await
    }
}

fn attribute(
    repo: &Repository,
    basis: &Basis,
    owner: &str,
    repaired: ObjectId,
    cancel: &watch::Receiver<bool>,
) -> Result<(usize, Vec<ObjectId>)> {
    let owner = basis
        .commits
        .iter()
        .position(|commit| commit.track == owner)
        .ok_or("Missing repair owner.")?;
    let original = basis
        .commits
        .iter()
        .map(|commit| commit.tree.parse())
        .collect::<std::result::Result<Vec<ObjectId>, _>>()?;
    let failed: ObjectId = basis.tree.parse()?;
    for index in owner..original.len() {
        if *cancel.borrow() {
            return Err("Repair attribution cancelled.".into());
        }
        // Transplant only the repair delta (failed -> repaired), then replay
        // each later contribution against its own original parent tree.
        let Merge::Clean(mut current) = repo.merge_trees(failed, original[index], repaired)? else {
            continue;
        };
        let mut trees = original[..index].to_vec();
        trees.push(current);
        for next in index + 1..original.len() {
            if *cancel.borrow() {
                return Err("Repair attribution cancelled.".into());
            }
            match repo.merge_trees(original[next - 1], current, original[next])? {
                Merge::Clean(tree) => {
                    trees.push(tree);
                    current = tree;
                }
                Merge::Conflicts(_) => break,
            }
        }
        if trees.len() == original.len() && current == repaired {
            return Ok((index, trees));
        }
    }
    // Applying at the last commit has failed as its base and therefore must
    // reproduce repaired exactly. Never publish an approximation if it does not.
    Err("The repair could not reproduce its final tree.".into())
}
