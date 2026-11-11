//! Explicit Push confirmation and durable per-destination outcomes.
use crate::{
    Result, push_review::Review, push_target::Destination, push_wire::Receipt, secret_scan::Scan,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Action {
    Confirm { review: Uuid },
    Retry { id: Uuid, revision: u64 },
    Cancel { id: Uuid, revision: u64 },
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Command {
    pub op_id: Uuid,
    pub action: Action,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Queued,
    Preparing,
    Scanning,
    Hooks,
    Lfs,
    Sending,
    Uncertain,
    Done,
    Failed,
    Cancelled,
    NeedsReview,
}
impl Phase {
    pub fn active(self) -> bool {
        matches!(
            self,
            Self::Queued
                | Self::Preparing
                | Self::Scanning
                | Self::Hooks
                | Self::Lfs
                | Self::Sending
        )
    }
    pub(crate) fn interrupted(self) -> Self {
        if self == Self::Sending {
            Self::Uncertain
        } else if self.active() {
            Self::Failed
        } else {
            self
        }
    }
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Recipient {
    pub folder_id: String,
    pub destination: Destination,
    pub phase: Phase,
    pub attempt: u64,
    pub scans: Vec<Scan>,
    pub receipt: Option<Receipt>,
    pub reason: Option<String>,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Job {
    pub id: Uuid,
    pub revision: u64,
    pub review: Review,
    pub stopping: bool,
    pub recipients: Vec<Recipient>,
}
impl Job {
    pub fn active(&self) -> bool {
        self.recipients
            .iter()
            .any(|recipient| recipient.phase.active())
    }
    pub fn done(&self) -> bool {
        self.recipients
            .iter()
            .all(|recipient| recipient.phase == Phase::Done)
    }
    pub(crate) fn validate(&self) -> Result<()> {
        self.review.validate()?;
        let expected = self
            .review
            .members
            .iter()
            .flat_map(|member| {
                member
                    .target
                    .destinations
                    .iter()
                    .map(move |destination| (&member.folder.id, destination))
            })
            .collect::<Vec<_>>();
        if self.id.get_version_num() != 7 || expected.len() != self.recipients.len() {
            return Err("The Push job has different destinations from its review.".into());
        }
        for ((folder, destination), recipient) in expected.into_iter().zip(&self.recipients) {
            if folder != &recipient.folder_id || destination != &recipient.destination {
                return Err("The Push recipient changed.".into());
            }
            if let Some(receipt) = &recipient.receipt {
                let member = self
                    .review
                    .members
                    .iter()
                    .find(|member| member.folder.id == recipient.folder_id)
                    .ok_or("Missing Push folder.")?;
                if receipt.sha != member.target.sha
                    || receipt.reference != member.target.target_ref
                    || receipt.destination != recipient.destination
                    || recipient.phase != Phase::Done
                {
                    return Err("The Push receipt does not match its reviewed ref.".into());
                }
            } else if recipient.phase == Phase::Done {
                return Err("The Push result has no verified receipt.".into());
            }
        }
        Ok(())
    }
}
/// Only the driver can advance an attempt. A late result cannot alter a retry.
#[derive(Clone, Debug)]
pub(crate) struct Progress {
    pub id: Uuid,
    pub recipient: usize,
    pub attempt: u64,
    pub phase: Phase,
    pub scans: Option<Vec<Scan>>,
    pub receipt: Option<Receipt>,
    pub reason: Option<&'static str>,
}

/// Commit decisions are reusable only when the complete scan binding matches.
#[derive(Clone, Debug)]
pub(crate) struct Approval {
    pub scan: Scan,
    pub kept: std::collections::BTreeSet<String>,
}
impl Approval {
    pub(crate) fn accepts(&self, scan: &Scan) -> bool {
        self.scan.binding == scan.binding
            && self.scan.findings == scan.findings
            && self.scan.unscanned_lfs == scan.unscanned_lfs
            && scan.accepted(&self.kept)
    }
}
