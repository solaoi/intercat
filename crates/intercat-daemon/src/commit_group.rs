//! A review of every project folder. Selecting a displayed folder never changes
//! the members whose frozen candidates the Commit button confirms.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Open,
    Committing,
    Partial,
    Done,
    Discarded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Member {
    pub folder_id: String,
    pub name: String,
    /// None freezes the absence of saved work when this review was opened.
    pub job_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Group {
    pub id: Uuid,
    pub revision: u64,
    pub session_id: String,
    pub project_id: Uuid,
    pub status: Status,
    pub members: Vec<Member>,
    pub tracks: Option<Vec<String>>,
    pub last_commit: Option<Uuid>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Open {
    pub op_id: Uuid,
    pub tracks: Option<Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    pub job_id: Uuid,
    pub revision: u64,
    pub head: String,
    pub tree: String,
    pub settings_hash: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Commit {
    pub op_id: Uuid,
    pub id: Uuid,
    pub revision: u64,
    pub targets: Vec<Target>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Discard {
    pub op_id: Uuid,
    pub id: Uuid,
    pub revision: u64,
}

pub(crate) enum Change {
    Open(Open),
    Commit(Commit),
    Discard(Discard),
    Reconcile(Uuid),
}
pub(crate) struct Changed {
    pub group: Group,
    pub jobs: Vec<crate::integration::Job>,
}
