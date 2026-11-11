//! A Start fresh request holds future dispatch without interrupting the source.
//! Its history is frozen at the actual saved turn boundary, never at the click.
use crate::{checkpoint::Checkpoint, preparation::Workspace};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub track_id: String,
    pub session_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Command {
    pub op_id: Uuid,
    pub id: Uuid,
    pub revision: u64,
    pub action: Action,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Action {
    Cancel,
    Retry,
    AllowAccess { fingerprint: String },
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Read {
    pub id: Uuid,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    WaitingForTurn,
    Captured,
    Releasing,
    Inspecting,
    NeedsAccess,
    CapturingFiles,
    PublishingFiles,
    SnapshotReady,
    Summarizing,
    SummaryReady,
    SummaryHeld,
    PublishingOwner,
    OwnerHeld,
    Transferred,
    FilesHeld,
    Cancelling,
    Cancelled,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Boundary {
    pub checkpoint: Checkpoint,
    pub context: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Files {
    pub checkpoint: Checkpoint,
    pub lfs_directory: std::path::PathBuf,
    pub summary_id: Uuid,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Job {
    pub request: Request,
    pub revision: u64,
    pub phase: Phase,
    pub successor_track: String,
    pub successor_session: String,
    pub workspace: Workspace,
    pub folder: crate::project::Folder,
    pub session: String,
    pub slug: String,
    pub boundary: Option<Boundary>,
    pub inspection: Option<crate::workspace_access::Inspection>,
    pub access_consent: Option<String>,
    pub files: Option<Files>,
    pub summary_id: Option<Uuid>,
    pub summary_input: Option<Value>,
    pub summary: Option<crate::handoff::Document>,
    pub error: Option<String>,
}
impl Job {
    pub(crate) fn status(&self) -> Value {
        serde_json::json!({"request":self.request,"revision":self.revision,"phase":self.phase,"successorTrack":self.successor_track,"successorSession":self.successor_session,"checkpoint":self.boundary.as_ref().map(|value| &value.checkpoint),"inspection":self.inspection,"files":self.files.as_ref().map(|files|&files.checkpoint),"summaryReady":self.summary.is_some(),"error":self.error})
    }
}
/// Lifecycle changes never create a second writer. The released source stays
/// held while final files are captured and protected for the later handoff.
pub enum Change {
    Request(Request),
    Capture {
        id: Uuid,
        revision: u64,
    },
    Release {
        id: Uuid,
        revision: u64,
    },
    Released {
        id: Uuid,
        revision: u64,
    },
    Inspect {
        id: Uuid,
        revision: u64,
        inspection: crate::workspace_access::Inspection,
    },
    ApproveAccess {
        op_id: Uuid,
        id: Uuid,
        revision: u64,
        fingerprint: String,
    },
    Files {
        id: Uuid,
        revision: u64,
        files: Files,
    },
    PrepareSummary {
        id: Uuid,
        revision: u64,
        changes: Value,
    },
    Summarized {
        id: Uuid,
        revision: u64,
        document: crate::handoff::Document,
    },
    Transfer {
        id: Uuid,
        revision: u64,
    },
    OwnerPublished {
        id: Uuid,
        revision: u64,
    },
    RefreshFiles {
        id: Uuid,
        revision: u64,
        inspection: crate::workspace_access::Inspection,
    },
    Published {
        id: Uuid,
        revision: u64,
    },
    FilesChanged {
        id: Uuid,
        revision: u64,
        inspection: crate::workspace_access::Inspection,
    },
    Retry {
        op_id: Uuid,
        id: Uuid,
        revision: u64,
    },
    Failed {
        id: Uuid,
        revision: u64,
        error: String,
    },
    Cancel {
        op_id: Uuid,
        id: Uuid,
        revision: u64,
    },
    Cancelled {
        id: Uuid,
        revision: u64,
    },
}

#[derive(Debug)]
pub(crate) struct Waiting(pub &'static str);
impl std::fmt::Display for Waiting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Waiting {}
