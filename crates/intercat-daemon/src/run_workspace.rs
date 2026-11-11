//! Integration Run orchestration chooses a domain workspace from its purpose.
//! Fix attribution and conflict resolution retain their distinct saved inputs.
use crate::{
    Result,
    candidate::{Candidate, Settings},
    checkout::Checkout,
    integration::Job,
    repair_run::{Purpose, Run},
};
use std::path::Path;
use tokio::sync::watch;
use uuid::Uuid;

pub enum Workspace {
    Fix(crate::repair::Workspace),
    Merge(crate::merge_resolution::Workspace),
    Realign(crate::realign_merge::Workspace),
    Undo(crate::undo_merge::Workspace),
    Shared(crate::shared_merge::Workspace),
    Restore(crate::restore_merge::Workspace),
}
pub(crate) enum Saved {
    Fix(crate::repair::Plan),
    Merge(crate::merge_resolution::Plan),
}
impl Workspace {
    pub fn checkout(&self) -> &Checkout {
        match self {
            Self::Fix(workspace) => &workspace.checkout,
            Self::Merge(workspace) => &workspace.files.checkout,
            Self::Realign(workspace) => &workspace.files.checkout,
            Self::Undo(workspace) => &workspace.files.checkout,
            Self::Shared(workspace) => &workspace.files.checkout,
            Self::Restore(workspace) => &workspace.files.checkout,
        }
    }
    pub(crate) fn original(&self) -> &Path {
        match self {
            Self::Fix(workspace) => &workspace.original,
            Self::Merge(workspace) => &workspace.files.original,
            Self::Realign(workspace) => &workspace.files.original,
            Self::Undo(workspace) => &workspace.files.original,
            Self::Shared(workspace) => &workspace.files.original,
            Self::Restore(workspace) => &workspace.files.original,
        }
    }
    pub fn snapshot_workspace(&self, run: Uuid) -> crate::preparation::Workspace {
        match self {
            Self::Fix(workspace) => workspace.snapshot_workspace(run),
            Self::Merge(workspace) => workspace.files.snapshot_workspace(run),
            Self::Realign(workspace) => workspace.files.snapshot_workspace(run),
            Self::Undo(workspace) => workspace.files.snapshot_workspace(run),
            Self::Shared(workspace) => workspace.files.snapshot_workspace(run),
            Self::Restore(workspace) => workspace.files.snapshot_workspace(run),
        }
    }
    pub(crate) fn validate(&self, job: &Job, owner: &str, settings: &Settings) -> Result<()> {
        match self {
            Self::Fix(workspace) => workspace.validate(job, owner, settings),
            Self::Merge(workspace) => workspace.validate(job, owner, settings),
            Self::Realign(_) | Self::Undo(_) | Self::Shared(_) | Self::Restore(_) => {
                Err("This Run needs its saved merge source.".into())
            }
        }
    }
    pub(crate) fn merge_files(&self) -> Result<&crate::merge_workspace::Workspace> {
        match self {
            Self::Merge(workspace) => Ok(&workspace.files),
            Self::Realign(workspace) => Ok(&workspace.files),
            Self::Undo(workspace) => Ok(&workspace.files),
            Self::Shared(workspace) => Ok(&workspace.files),
            Self::Restore(workspace) => Ok(&workspace.files),
            _ => Err("This Run has no merge editor.".into()),
        }
    }
    pub(crate) fn validate_source(
        &self,
        source: &crate::run_input::Source,
        track: &str,
    ) -> Result<()> {
        match (self, source) {
            (Self::Realign(workspace), crate::run_input::Source::Realign(input)) => {
                workspace.validate(input)
            }
            (Self::Undo(workspace), crate::run_input::Source::Undo(input)) => {
                workspace.validate(input)
            }
            (Self::Shared(workspace), crate::run_input::Source::Shared(input)) => {
                workspace.validate(input)
            }
            (Self::Restore(workspace), crate::run_input::Source::Restore(input)) => {
                workspace.validate(input)
            }
            (_, crate::run_input::Source::Commit { job, settings }) => {
                self.validate(job, track, settings)
            }
            _ => Err("The Run workspace belongs to another kind of work.".into()),
        }
    }
    pub(crate) async fn open_source(
        source: crate::run_input::Source,
        root: &Path,
        run: &Run,
        cancel: watch::Receiver<bool>,
    ) -> Result<Self> {
        match source {
            crate::run_input::Source::Realign(input) => Ok(Self::Realign(
                crate::realign_merge::Workspace::open(*input, root, run, cancel).await?,
            )),
            crate::run_input::Source::Undo(input) => Ok(Self::Undo(
                crate::undo_merge::Workspace::open(*input, root, run, cancel).await?,
            )),
            crate::run_input::Source::Shared(input) => Ok(Self::Shared(
                crate::shared_merge::Workspace::open(*input, root, run, cancel).await?,
            )),
            crate::run_input::Source::Restore(input) => Ok(Self::Restore(
                crate::restore_merge::Workspace::open(*input, root, run, cancel).await?,
            )),
            crate::run_input::Source::Commit { job, settings } => {
                let candidate = &job
                    .candidate
                    .as_ref()
                    .ok_or("Missing candidate.")?
                    .workspace;
                if run.workspace_ready {
                    Self::resume(&job, candidate, root, run, &settings).await
                } else {
                    Self::prepare(&job, candidate, root, run, &settings, cancel).await
                }
            }
        }
    }
    pub async fn prepare(
        job: &Job,
        candidate_root: &Path,
        root: &Path,
        run: &Run,
        settings: &Settings,
        cancel: watch::Receiver<bool>,
    ) -> Result<Self> {
        if run.owner
            != (crate::repair_run::Owner::Commit {
                id: job.id,
                revision: job.revision,
            })
        {
            return Err("The Run belongs to another integration revision.".into());
        }
        match run.purpose {
            Purpose::Merge => Ok(Self::Merge(
                crate::merge_resolution::Workspace::prepare(
                    job,
                    candidate_root,
                    root,
                    &run.track,
                    settings,
                    run.id,
                    cancel,
                )
                .await?,
            )),
            _ => Ok(Self::Fix(
                crate::repair::Workspace::prepare(
                    job,
                    candidate_root,
                    root,
                    &run.track,
                    settings,
                    cancel,
                )
                .await?,
            )),
        }
    }
    pub async fn resume(
        job: &Job,
        candidate_root: &Path,
        root: &Path,
        run: &Run,
        settings: &Settings,
    ) -> Result<Self> {
        if run.owner
            != (crate::repair_run::Owner::Commit {
                id: job.id,
                revision: job.revision,
            })
        {
            return Err("The Run belongs to another integration revision.".into());
        }
        match run.purpose {
            Purpose::Merge => Ok(Self::Merge(
                crate::merge_resolution::Workspace::resume(
                    job,
                    candidate_root,
                    root,
                    &run.track,
                    settings,
                    run.id,
                )
                .await?,
            )),
            _ => Ok(Self::Fix(
                crate::repair::Workspace::resume(job, candidate_root, root, &run.track, settings)
                    .await?,
            )),
        }
    }
    pub(crate) async fn finish(
        &self,
        job: &Job,
        candidate_root: &Path,
        settings: &Settings,
        cancel: watch::Receiver<bool>,
    ) -> Result<Saved> {
        match self {
            Self::Fix(workspace) => Ok(Saved::Fix(
                workspace
                    .finish(job, candidate_root, settings, cancel)
                    .await?,
            )),
            Self::Merge(workspace) => {
                Ok(Saved::Merge(workspace.finish(job, settings, cancel).await?))
            }
            Self::Realign(_) | Self::Undo(_) | Self::Shared(_) | Self::Restore(_) => {
                Err("Apply this result to its saved merge source.".into())
            }
        }
    }
    pub(crate) async fn rebuild(
        &self,
        job: &Job,
        saved: &Saved,
        root: &Path,
        cache_root: &Path,
        settings: &Settings,
        cancel: watch::Receiver<bool>,
    ) -> Result<Candidate> {
        match (self, saved) {
            (Self::Fix(workspace), Saved::Fix(plan)) => {
                workspace
                    .rebuild(job, plan, root, cache_root, settings, cancel)
                    .await
            }
            (Self::Merge(workspace), Saved::Merge(plan)) => {
                workspace
                    .rebuild(job, plan, root, cache_root, settings, cancel)
                    .await
            }
            _ => Err("The saved integration result belongs to another workspace purpose.".into()),
        }
    }
}
