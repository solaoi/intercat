//! Processes surviving an integration Run's completed model turn. Resource
//! decisions have their own revision so they cannot invalidate its saved work.
use crate::{Result, process::Identity};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Waiting,
    Kept,
    Stopping,
    Cleaning,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Remaining {
    pub nonce: Uuid,
    pub host: Option<Identity>,
    pub directory: PathBuf,
    pub workspace: PathBuf,
    pub label: String,
    pub session_name: String,
    pub created_at: i64,
    pub revision: u64,
    pub state: State,
    pub error: Option<String>,
}
impl Remaining {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.nonce.get_version_num() != 7
            || self
                .host
                .is_some_and(|host| host.pid <= 1 || host.started_micros >= 1_000_000)
            || self.host.is_none() && self.state != State::Cleaning
            || !self.directory.is_absolute()
            || self.directory.parent().is_none()
            || !self.workspace.is_absolute()
            || self.workspace.parent().is_none()
            || self.label.is_empty()
            || self.label.len() > 256
            || self.session_name.len() > 512
            || self.created_at < 0
            || self.revision != 0
            || !matches!(self.state, State::Waiting | State::Cleaning)
            || self.error.is_some()
        {
            return Err("Invalid completed Run process ownership.".into());
        }
        Ok(())
    }
    pub(crate) fn decide(&mut self, command: &Command) -> Result<()> {
        if self.nonce != command.nonce || self.revision != command.revision {
            return Err("The remaining processes changed. Review them again.".into());
        }
        if self.state == State::Cleaning && command.action != Action::RetryCleanup
            || self.state != State::Cleaning && command.action == Action::RetryCleanup
            || self.state == State::Stopping && command.action == Action::Keep
        {
            return Err("These Run processes are already stopping or being cleaned up.".into());
        }
        self.error = None;
        self.state = match command.action {
            Action::Keep => State::Kept,
            Action::Stop => State::Stopping,
            Action::RetryCleanup => State::Cleaning,
        };
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or("Process decision revision overflow.")?;
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Keep,
    Stop,
    RetryCleanup,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Command {
    pub op_id: Uuid,
    pub id: Uuid,
    pub nonce: Uuid,
    pub revision: u64,
    pub action: Action,
}
