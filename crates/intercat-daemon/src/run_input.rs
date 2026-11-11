//! Integration Runs have distinct domain inputs and shared host metadata.
//! A realignment never manufactures a failed Commit candidate.
use crate::{Result, candidate::Settings, integration::Job, project::Folder};

pub(crate) enum Source {
    Commit {
        job: Box<Job>,
        settings: Box<Settings>,
    },
    Realign(Box<crate::realign_merge::Frozen>),
    Undo(Box<crate::undo_merge::Frozen>),
    Shared(Box<crate::shared_merge::Frozen>),
    Restore(Box<crate::restore_merge::Frozen>),
}
impl Source {
    pub(crate) fn settings(&self) -> &Settings {
        match self {
            Self::Commit { settings, .. } => settings,
            Self::Realign(input) => &input.settings,
            Self::Undo(input) => &input.settings,
            Self::Shared(input) => &input.settings,
            Self::Restore(input) => &input.settings,
        }
    }
    pub(crate) fn folder(&self) -> &Folder {
        match self {
            Self::Commit { job, .. } => &job.project,
            Self::Realign(input) => &input.project,
            Self::Undo(input) => &input.project,
            Self::Shared(input) => &input.record.input.project,
            Self::Restore(input) => &input.project,
        }
    }
    pub(crate) fn label(&self, track: &str) -> Result<&str> {
        match self {
            Self::Commit { job, .. } => job
                .tracks
                .iter()
                .find(|item| item.id == track)
                .map(|item| item.label.as_str())
                .ok_or_else(|| "Missing Run owner.".into()),
            Self::Realign(input) if input.request.track_id == track => Ok(&input.label),
            Self::Undo(input) if input.track == track => Ok(&input.label),
            Self::Restore(input) if input.source.entry.track == track => {
                Ok(&input.source.entry.label)
            }
            Self::Shared(input) if input.record.input.projection.source.track == track => {
                Ok(&input.label)
            }
            _ => Err("The Run belongs to another worker.".into()),
        }
    }
    pub(crate) fn session_name(&self) -> &str {
        match self {
            Self::Commit { job, .. } => &job.session_name,
            Self::Realign(input) => &input.session_name,
            Self::Undo(input) => &input.session_name,
            Self::Shared(input) => &input.session_name,
            Self::Restore(input) => &input.session_name,
        }
    }
}
