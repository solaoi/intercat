//! Automatic corrections are worker inputs with a durable review origin.
use serde::{Deserialize, Serialize};
use uuid::Uuid;
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Planned,
    Held,
    Reserved,
    Running,
    AwaitingReview,
    Resolved,
    Unresolved,
    Superseded,
    Outdated,
    Cut,
    Blocked,
    Kept,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fix {
    pub id: Uuid,
    pub revision: u64,
    pub note_id: Uuid,
    pub track_id: String,
    pub input_id: Option<Uuid>,
    pub state: State,
    pub checked_sha: Option<String>,
    pub charge_epoch: Option<i64>,
    pub extra: bool,
    pub reason: Option<String>,
    pub completed_sha: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub enum Action {
    FixOnceMore,
    KeepIt,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Command {
    pub op_id: Uuid,
    pub fix_id: Uuid,
    pub revision: u64,
    pub action: Action,
}
#[derive(Clone, Debug)]
pub struct Check {
    pub fix: Fix,
    pub note: crate::reviews::Note,
    pub checkpoint: String,
    pub workspace: std::path::PathBuf,
}
pub enum Checked {
    Ready,
    Retry,
    Outdated,
}
