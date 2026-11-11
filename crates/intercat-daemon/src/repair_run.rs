//! Durable ownership and dispatch reservation for an integration repair Run.
//! A repair has its own host/session/turn; it never borrows a work Run's identity.
use crate::{Result, candidate::State, integration, process::Identity};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Purpose {
    Check,
    Merge,
    RemoveSecret {
        finding: String,
    },
    Assumption {
        id: Uuid,
        revision: i64,
        text: String,
    },
    Edit {
        selection: Selection,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Side {
    Before,
    After,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Selection {
    pub tree: String,
    pub path: String,
    pub side: Side,
    pub object: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Owner {
    Commit { id: Uuid, revision: u64 },
    Realign { id: Uuid, revision: u64 },
    Undo { id: Uuid, revision: u64 },
    Shared { id: Uuid, revision: u64 },
    Restore { id: Uuid, revision: u64 },
}
impl Owner {
    pub fn id(self) -> Uuid {
        match self {
            Self::Commit { id, .. }
            | Self::Realign { id, .. }
            | Self::Undo { id, .. }
            | Self::Shared { id, .. }
            | Self::Restore { id, .. } => id,
        }
    }
    pub fn revision(self) -> u64 {
        match self {
            Self::Commit { revision, .. }
            | Self::Realign { revision, .. }
            | Self::Undo { revision, .. }
            | Self::Shared { revision, .. }
            | Self::Restore { revision, .. } => revision,
        }
    }
    pub(crate) fn commit(self) -> Result<(Uuid, u64)> {
        match self {
            Self::Commit { id, revision } => Ok((id, revision)),
            Self::Realign { .. }
            | Self::Undo { .. }
            | Self::Shared { .. }
            | Self::Restore { .. } => {
                Err(Rejected("This operation requires a Commit review.").into())
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub owner: Owner,
    pub track: String,
    pub purpose: Purpose,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RefreshRequest {
    pub op_id: Uuid,
    pub job_id: Uuid,
    pub revision: u64,
    pub repair_id: Uuid,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Refresh {
    pub operation: Uuid,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Queued,
    Preparing,
    Running,
    Stopping,
    Held,
    Manual,
    Captured,
    Applying,
    Done,
    Failed,
    NeedsRecovery,
    Discarded,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: Uuid,
    pub revision: u64,
    pub owner: Owner,
    pub track: String,
    pub purpose: Purpose,
    pub phase: Phase,
    pub created_at: i64,
    pub host: Option<Identity>,
    pub host_nonce: Option<Uuid>,
    pub secret_revision: Option<u64>,
    pub secret_input: Option<Uuid>,
    pub checkpoint: Option<crate::checkpoint::Checkpoint>,
    pub session: Option<String>,
    pub session_ready: bool,
    pub workspace_ready: bool,
    pub start_command: Option<Uuid>,
    pub turn: Option<Uuid>,
    pub failure: Option<Failure>,
    pub approvals: Vec<crate::execution::Approval>,
    pub prompts: Vec<crate::questions::Prompt>,
    pub output: Option<String>,
    pub instruction: Option<String>,
    pub selection: Option<Selection>,
    pub refresh: Option<Refresh>,
    pub restoring: Option<Uuid>,
    pub remaining: Option<crate::run_processes::Remaining>,
    pub manual: Option<crate::merge_manual::Snapshot>,
    pub external_editor: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FollowUp {
    pub op_id: Uuid,
    pub id: Uuid,
    pub revision: u64,
    pub text: String,
    pub selection: Option<Selection>,
}

#[derive(Debug)]
pub struct Rejected(pub &'static str);
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Rejected {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Failure {
    Preparation,
    Host,
    Protocol,
    Capture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Control {
    Stop,
    Resume,
    Discard,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Command {
    pub op_id: Uuid,
    pub id: Uuid,
    pub revision: u64,
    pub action: Control,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "event",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Event {
    BeginManual,
    ManualFailed,
    ExternalOpened {
        path: String,
    },
    ExternalReturned,
    CaptureManual {
        snapshot: crate::merge_manual::Snapshot,
        cleanup: Box<crate::run_processes::Remaining>,
    },
    Prepare,
    WorkspaceReady,
    HostStarted {
        host: Identity,
        nonce: Uuid,
        secret_revision: Option<u64>,
    },
    SessionReady {
        session: String,
    },
    ReserveStart {
        command: Uuid,
    },
    Started {
        command: Uuid,
        turn: Uuid,
    },
    Presentation {
        approvals: Vec<crate::execution::Approval>,
        prompts: Vec<crate::questions::Prompt>,
        output: Option<String>,
    },
    HostClosed {
        outcome: Outcome,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(
    tag = "outcome",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Outcome {
    Completed,
    Stopped,
    Reconfigure,
    Failed { failure: Failure },
}

impl Run {
    pub(crate) fn new(request: Request, owner: Owner, manual: bool) -> Self {
        Self {
            id: Uuid::new_v5(&request.op_id, b"repair"),
            revision: 1,
            owner,
            track: request.track,
            selection: match &request.purpose {
                crate::repair_run::Purpose::Edit { selection, .. } => Some(selection.clone()),
                _ => None,
            },
            purpose: request.purpose,
            phase: if manual { Phase::Manual } else { Phase::Queued },
            created_at: crate::journal::timestamp(),
            host: None,
            host_nonce: None,
            secret_revision: None,
            secret_input: None,
            checkpoint: None,
            session: None,
            session_ready: false,
            workspace_ready: false,
            start_command: None,
            turn: None,
            failure: None,
            approvals: vec![],
            prompts: vec![],
            output: None,
            instruction: None,
            refresh: None,
            restoring: None,
            remaining: None,
            manual: None,
            external_editor: false,
        }
    }

    pub(crate) fn retired(&mut self, mut remaining: crate::run_processes::Remaining) -> Result<()> {
        if !matches!(self.phase, Phase::Running | Phase::Stopping)
            || remaining.host.is_none()
            || self.host != remaining.host
            || self.host_nonce != Some(remaining.nonce)
            || self.turn.is_none()
            || self.remaining.is_some()
        {
            return Err("The retired host does not own the current repair Run.".into());
        }
        if self.phase == Phase::Stopping && remaining.state != crate::run_processes::State::Cleaning
        {
            remaining.state = crate::run_processes::State::Stopping;
        }
        if self.phase != Phase::Stopping {
            self.phase = Phase::Captured;
        }
        self.remaining = Some(remaining);
        self.transport_closed();
        Ok(())
    }
    fn transport_closed(&mut self) {
        self.host = None;
        self.host_nonce = None;
        self.secret_revision = None;
        self.session_ready = false;
        self.approvals.clear();
        self.prompts.clear();
    }
    pub(crate) fn validate_request(job: &integration::Job, request: &Request) -> Result<()> {
        let (job_id, revision) = request.owner.commit()?;
        let candidate = job
            .candidate
            .as_ref()
            .ok_or(Rejected("No candidate to repair."))?;
        if job.id != job_id
            || job.revision != revision
            || !matches!(
                job.status,
                integration::Status::Failed | integration::Status::Checked
            )
            || candidate.revision != revision
            || candidate.job_id != job.id
        {
            return Err(Rejected("The candidate changed. Review it again.").into());
        }
        if !job
            .tracks
            .iter()
            .any(|t| t.id == request.track && t.included)
        {
            return Err(Rejected("Choose a track included in this candidate.").into());
        }
        match &request.purpose {
            Purpose::Merge
                if candidate.state == State::Conflict
                    && candidate.failed_track.as_deref() == Some(request.track.as_str())
                    && matches!(
                        candidate.conflict,
                        Some(crate::merge_resolution::Conflict::Overlay { .. })
                    ) =>
            {
                Ok(())
            }
            Purpose::Check
                if matches!(candidate.state, State::CheckFailed | State::CheckMutated)
                    && candidate.commits.iter().any(|c| c.track == request.track) =>
            {
                Ok(())
            }
            Purpose::RemoveSecret { finding }
                if candidate.state == State::SecretFound
                    && candidate.secret_scans.last().is_some_and(|scan| {
                        scan.findings.iter().any(|item| {
                            &item.id == finding
                                && item.rule != crate::secret_scan::BINARY_REVIEW_RULE
                        }) && candidate
                            .commits
                            .iter()
                            .chain(&candidate.blocked_commit)
                            .any(|commit| {
                                commit.track == request.track && commit.sha == scan.binding.commit
                            })
                    })
                    && !job.kept_secrets.contains(finding) =>
            {
                Ok(())
            }
            Purpose::Assumption { id, revision, text }
                if candidate
                    .commits
                    .iter()
                    .any(|commit| commit.track == request.track)
                    && job.assumptions.iter().any(|review| {
                        crate::integration_assumptions::included(job, review)
                            && review.assumption.id == id.to_string()
                            && review.assumption.revision == *revision
                            && review.assumption.track_id == request.track
                            && review.assumption.state == "open"
                    }) =>
            {
                validate_instruction(text)
            }
            Purpose::Edit { selection, text }
                if matches!(
                    candidate.state,
                    State::Checked | State::CheckFailed | State::CheckMutated | State::SecretFound
                ) =>
            {
                validate_instruction(text)?;
                Self::validate_selection(job, &request.track, selection)
            }
            _ => Err(Rejected("The repair request does not match the current candidate.").into()),
        }
    }

    pub(crate) fn validate_selection(
        job: &integration::Job,
        track: &str,
        selection: &Selection,
    ) -> Result<()> {
        let candidate = job
            .candidate
            .as_ref()
            .ok_or(Rejected("No candidate to repair."))?;
        let commit = candidate
            .blocked_commit
            .as_ref()
            .filter(|c| c.track == track)
            .or_else(|| candidate.commits.iter().find(|c| c.track == track));
        let file = commit
            .and_then(|c| c.changes.iter().find(|c| c.path == selection.path))
            .and_then(|c| match selection.side {
                Side::Before => c.before.as_ref(),
                Side::After => c.after.as_ref(),
            });
        if selection.tree
            != candidate
                .blocked_commit
                .as_ref()
                .map_or(&candidate.tree, |c| &c.tree)
                .as_str()
            || file.is_none_or(|file| {
                file.object != selection.object || !matches!(file.mode, 0o100644 | 0o100755)
            })
            || selection.start_line == 0
            || selection.end_line < selection.start_line
        {
            return Err(Rejected("The selected diff changed. Select its lines again.").into());
        }
        Ok(())
    }

    pub(crate) fn control(&mut self, control: Control) -> Result<()> {
        if self.external_editor && control != Control::Stop {
            return Err("Files may still be open in your editor. Confirm Go ahead before changing this merge.".into());
        }
        if self.remaining.is_some() && control == Control::Resume {
            return Err("Stop this Run's remaining processes before restarting it.".into());
        }
        if self.restoring.is_some() && control != Control::Stop {
            return Err(
                Rejected("Finish the saved-file restore before changing this repair.").into(),
            );
        }
        if control != Control::Stop
            && self
                .refresh
                .as_ref()
                .is_some_and(|refresh| refresh.error.is_none())
        {
            return Err("This repair is stopping for the confirmed Refresh.".into());
        }
        if control != Control::Stop {
            self.refresh = None;
        }
        match control {
            Control::Stop => match self.phase {
                Phase::Queued => self.phase = Phase::Held,
                Phase::Preparing | Phase::Running => self.phase = Phase::Stopping,
                Phase::Stopping | Phase::Held => {}
                Phase::Manual => self.phase = Phase::Held,
                _ => return Err("This repair Run cannot be stopped.".into()),
            },
            Control::Resume
                if self.host.is_none()
                    && (matches!(self.phase, Phase::Held | Phase::Manual)
                        || self.phase == Phase::Failed
                            && matches!(self.purpose, Purpose::Merge)
                            && self.failure == Some(Failure::Capture)) =>
            {
                self.secret_input = None;
                self.phase = Phase::Queued;
                self.start_command = None;
                self.turn = None;
                self.failure = None;
                self.session_ready = false;
                self.manual = None;
            }
            Control::Discard
                if matches!(
                    self.phase,
                    Phase::Queued | Phase::Held | Phase::Manual | Phase::Captured | Phase::Failed
                ) && self.host.is_none() =>
            {
                self.phase = Phase::Discarded
            }
            _ => return Err("Stop the repair host before changing this Run.".into()),
        }
        Ok(())
    }

    pub(crate) fn transition(&mut self, event: &Event) -> Result<()> {
        match event {
            Event::ExternalOpened { .. } if self.phase == Phase::Manual && self.host.is_none() => {
                self.external_editor = true
            }
            Event::ExternalReturned if self.phase == Phase::Manual && self.external_editor => {
                self.external_editor = false
            }
            Event::ManualFailed if self.phase == Phase::Captured && self.manual.is_some() => {
                self.phase = Phase::Failed;
                self.failure = Some(Failure::Capture);
            }
            Event::BeginManual
                if matches!(self.purpose, Purpose::Merge)
                    && matches!(self.phase, Phase::Held | Phase::Manual | Phase::Failed)
                    && self.host.is_none()
                    && self.remaining.is_none()
                    && self.refresh.is_none()
                    && self.restoring.is_none() =>
            {
                self.phase = Phase::Manual;
                self.failure = None;
                self.manual = None;
            }
            Event::CaptureManual { snapshot, cleanup }
                if self.phase == Phase::Manual
                    && self.workspace_ready
                    && self.host.is_none()
                    && self.remaining.is_none()
                    && self.refresh.is_none()
                    && self.restoring.is_none()
                    && !self.external_editor =>
            {
                if snapshot.reference
                    != format!("refs/intercat/manual/{}/{}", self.id, snapshot.operation)
                {
                    return Err("The manual resolution belongs to another Run.".into());
                }
                let _: intercat_git::ObjectId = snapshot.sha.parse()?;
                cleanup.validate()?;
                if cleanup.host.is_some() || cleanup.state != crate::run_processes::State::Cleaning
                {
                    return Err("Manual resolution cleanup cannot own a model host.".into());
                }
                self.manual = Some(snapshot.clone());
                self.remaining = Some((**cleanup).clone());
                self.phase = Phase::Captured;
            }
            Event::Prepare if self.phase == Phase::Queued => self.phase = Phase::Preparing,
            Event::WorkspaceReady
                if matches!(
                    self.phase,
                    Phase::Preparing | Phase::Stopping | Phase::Manual
                ) && !self.workspace_ready
                    && self.host.is_none()
                    && self.session.is_none() =>
            {
                self.workspace_ready = true;
            }
            Event::HostStarted {
                host,
                nonce,
                secret_revision,
            } if matches!(self.phase, Phase::Preparing | Phase::Stopping)
                && self.workspace_ready
                && self.host.is_none()
                && self.start_command.is_none()
                && host.pid > 1 =>
            {
                self.host = Some(*host);
                self.host_nonce = Some(*nonce);
                self.secret_revision = *secret_revision;
                self.session_ready = false;
            }
            Event::SessionReady { session }
                if matches!(self.phase, Phase::Preparing | Phase::Stopping)
                    && self.host.is_some()
                    && self.start_command.is_none()
                    && !session.is_empty()
                    && session.len() <= 256 =>
            {
                self.session = Some(session.clone());
                self.session_ready = true;
            }
            Event::ReserveStart { command }
                if self.phase == Phase::Preparing
                    && self.restoring.is_none()
                    && self.refresh.is_none()
                    && self.host.is_some()
                    && self.session.is_some()
                    && self.session_ready
                    && self.start_command.is_none() =>
            {
                self.start_command = Some(*command);
                self.phase = Phase::Running;
                self.output = None;
            }
            // A Stop may arrive while turn/start is in flight. Preserve that
            // Stop while retaining the returned turn ID for the interrupt.
            Event::Started { command, turn }
                if matches!(self.phase, Phase::Running | Phase::Stopping)
                    && self.start_command == Some(*command)
                    && self.turn.is_none() =>
            {
                self.turn = Some(*turn)
            }
            Event::Presentation {
                approvals,
                prompts,
                output,
            } if matches!(self.phase, Phase::Running | Phase::Stopping) => {
                self.approvals = approvals.clone();
                self.prompts = prompts.clone();
                self.output = output.clone();
            }
            Event::HostClosed { outcome }
                if matches!(
                    self.phase,
                    Phase::Preparing | Phase::Running | Phase::Stopping | Phase::NeedsRecovery
                ) =>
            {
                self.phase = match outcome {
                    Outcome::Reconfigure
                        if self.phase == Phase::Preparing && self.start_command.is_none() =>
                    {
                        Phase::Queued
                    }
                    Outcome::Completed if self.phase == Phase::Running && self.turn.is_some() => {
                        Phase::Captured
                    }
                    Outcome::Stopped => Phase::Held,
                    Outcome::Failed { failure } => {
                        self.failure = Some(*failure);
                        Phase::Failed
                    }
                    _ => return Err("The repair has no completed turn to capture.".into()),
                };
                self.transport_closed();
            }
            _ => return Err("The repair event does not match the current Run state.".into()),
        }
        Ok(())
    }
}

pub(crate) fn validate_instruction(text: &str) -> Result<()> {
    if text.trim().is_empty() || text.len() > 16 * 1024 || text.contains('\0') {
        return Err(Rejected("A repair instruction must be nonempty and at most 16 KiB.").into());
    }
    Ok(())
}
