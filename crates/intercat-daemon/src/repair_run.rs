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
    RemoveSecret { finding: String },
    Edit { selection: Selection, text: String },
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub job_id: Uuid,
    pub revision: u64,
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
    pub job_id: Uuid,
    pub job_revision: u64,
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
    pub(crate) fn validate_request(job: &integration::Job, request: &Request) -> Result<()> {
        let candidate = job
            .candidate
            .as_ref()
            .ok_or(Rejected("No candidate to repair."))?;
        if job.id != request.job_id
            || job.revision != request.revision
            || !matches!(
                job.status,
                integration::Status::Failed | integration::Status::Checked
            )
            || candidate.revision != request.revision
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
            Purpose::Check
                if matches!(candidate.state, State::CheckFailed | State::CheckMutated)
                    && candidate.commits.iter().any(|c| c.track == request.track) =>
            {
                Ok(())
            }
            Purpose::RemoveSecret { finding }
                if candidate.state == State::SecretFound
                    && candidate
                        .blocked_commit
                        .as_ref()
                        .is_some_and(|c| c.track == request.track)
                    && candidate
                        .secret_scans
                        .last()
                        .is_some_and(|s| s.findings.iter().any(|f| &f.id == finding))
                    && !job.kept_secrets.contains(finding) =>
            {
                Ok(())
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
                _ => return Err("This repair Run cannot be stopped.".into()),
            },
            Control::Resume if self.phase == Phase::Held && self.host.is_none() => {
                self.secret_input = None;
                self.phase = Phase::Queued;
                self.start_command = None;
                self.turn = None;
                self.failure = None;
                self.session_ready = false;
            }
            Control::Discard
                if matches!(
                    self.phase,
                    Phase::Queued | Phase::Held | Phase::Captured | Phase::Failed
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
            Event::Prepare if self.phase == Phase::Queued => self.phase = Phase::Preparing,
            Event::WorkspaceReady
                if matches!(self.phase, Phase::Preparing | Phase::Stopping)
                    && !self.workspace_ready
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
                self.host = None;
                self.host_nonce = None;
                self.secret_revision = None;
                self.session_ready = false;
                self.approvals.clear();
                self.prompts.clear();
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
