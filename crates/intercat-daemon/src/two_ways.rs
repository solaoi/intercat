//! One instruction, one saved boundary, and two independently owned paths.
use crate::{
    Result,
    branch_start::{Point, Selection as BranchSelection},
};
use muse_code_sdk::msp::{
    ModelListResult, ModelReasoningEffortVariants, ReasoningEffort, UsageReadResult,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const PRIMARY_EFFORT: ReasoningEffort = ReasoningEffort::Minimal;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub op_id: Uuid,
    pub input_id: String,
    pub revision: i64,
    pub selection: Selection,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Selection {
    pub track_id: String,
    pub session_id: String,
    pub variation: Variation,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OptionsQuery {
    pub input_id: String,
    pub revision: i64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionsSource {
    pub query: OptionsQuery,
    pub track: String,
    pub session: String,
    pub released: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Options {
    pub input_id: String,
    pub revision: i64,
    pub selection: Selection,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Instruction {
    pub input_id: String,
    pub effort: ReasoningEffort,
    pub phase: InstructionPhase,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InstructionPhase {
    Pending,
    Active,
    Finished,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Waiting,
    CapturingInitial,
    Inspecting,
    AwaitingUsageConfirmation,
    InspectingConfirmedUsage,
    Forking,
    AwaitingReceipt,
    Queued,
    Held,
    Cancelling,
    Cancelled,
    Declined,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Variation {
    pub model_id: String,
    pub provider_id: String,
    pub primary: ReasoningEffort,
    pub alternate: ReasoningEffort,
}
impl Variation {
    pub fn from_catalog(catalog: &ModelListResult) -> Result<Self> {
        let active = catalog
            .models
            .iter()
            .filter(|model| model.is_active)
            .collect::<Vec<_>>();
        let [model] = active.as_slice() else {
            return Err(
                "The current model is unavailable. Reconnect before trying two ways.".into(),
            );
        };
        let Some(ModelReasoningEffortVariants::Known(efforts)) = &model.variants else {
            return Err("This model's effort choices are unavailable.".into());
        };
        if !efforts.contains(&PRIMARY_EFFORT) {
            return Err("The current model does not support the original turn's effort.".into());
        }
        let alternate = [
            ReasoningEffort::Xhigh,
            ReasoningEffort::High,
            ReasoningEffort::Medium,
            ReasoningEffort::Low,
            ReasoningEffort::Max,
            ReasoningEffort::Ultra,
            ReasoningEffort::None,
        ]
        .into_iter()
        .find(|effort| efforts.contains(effort))
        .ok_or("The current model has no second effort choice.")?;
        if model.model_id.is_empty()
            || model.provider_id.is_empty()
            || model.provider_id != catalog.provider_id
        {
            return Err("The current model's provider changed.".into());
        }
        Ok(Self {
            model_id: model.model_id.clone(),
            provider_id: model.provider_id.clone(),
            primary: PRIMARY_EFFORT,
            alternate,
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Budget {
    Unknown,
    Available { remaining: i64, observed_at: i64 },
    Insufficient { remaining: i64, observed_at: i64 },
}
pub fn budget(read: &UsageReadResult, now: i64) -> Result<Budget> {
    let Some(usage) = &read.usage else {
        return Ok(Budget::Unknown);
    };
    if usage.window.used_percent < 0
        || usage.weekly.used_percent < 0
        || usage.window.window_duration_mins <= 0
        || usage.observed_at_ms < 0
        || usage.observed_at_ms > now
        || usage.window.resets_at_ms <= now
        || usage.weekly.resets_at_ms <= now
    {
        return Err("The plan usage observation is invalid or has expired.".into());
    }
    let remaining = 100i64
        .saturating_sub(usage.window.used_percent.max(usage.weekly.used_percent))
        .max(0);
    Ok(if remaining < 20 {
        Budget::Insufficient {
            remaining,
            observed_at: usage.observed_at_ms,
        }
    } else {
        Budget::Available {
            remaining,
            observed_at: usage.observed_at_ms,
        }
    })
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Job {
    pub request: Request,
    pub revision: u64,
    pub phase: Phase,
    pub source_track: String,
    pub source_session: String,
    pub branch_id: Uuid,
    pub child_input: Uuid,
    pub point: Option<Point>,
    pub variation: Option<Variation>,
    pub usage_observed_at: Option<i64>,
    pub error: Option<String>,
    pub resume: Option<Phase>,
}
pub enum Change {
    Prepare(Request),
    Capture {
        id: Uuid,
        revision: u64,
    },
    InitialCaptured {
        id: Uuid,
        revision: u64,
        snapshot: crate::branch_start::Initial,
    },
    Inspect {
        id: Uuid,
        revision: u64,
        usage: UsageReadResult,
        models: ModelListResult,
    },
    BranchReady {
        id: Uuid,
        revision: u64,
    },
    Publish {
        id: Uuid,
        revision: u64,
    },
    Hold {
        id: Uuid,
        revision: u64,
        error: String,
    },
    Command(Command),
    Cancelled {
        id: Uuid,
        revision: u64,
    },
}
impl Job {
    pub(crate) fn branch_request(&self) -> Result<crate::branch::Request> {
        Ok(crate::branch::Request {
            op_id: self.branch_id,
            parent_track_id: self.source_track.clone(),
            point: match self.point.as_ref().ok_or("Missing paired boundary.")? {
                Point::Turn { checkpoint } => BranchSelection::Turn {
                    turn_id: checkpoint.turn_id,
                },
                Point::Initial { .. } => BranchSelection::Initial {
                    two_ways_id: self.request.op_id,
                },
            },
            instructions: crate::branch::InstructionMode::Checkpoint,
        })
    }
}

#[derive(Debug)]
pub(crate) struct Waiting(pub &'static str);
impl std::fmt::Display for Waiting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Waiting {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Cancel,
    Retry,
    ConfirmUsage,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Command {
    pub op_id: Uuid,
    pub id: Uuid,
    pub revision: u64,
    pub action: Action,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Read {
    pub id: Uuid,
}
