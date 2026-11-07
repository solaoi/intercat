//! Stage-specific uncertainty and explicit choices made by the user.
use crate::{
    Result,
    decision::{Decision, Kind},
    runtime::WorkerState,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    Operation,
    Lane,
    Trim,
    Boundary,
    Command,
    StaleTurn,
    StartTurn,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub session_id: String,
    pub turn_id: Uuid,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Review {
    pub stage: Stage,
    pub turn: Option<Turn>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Choice {
    Instruction,
    StopAndSend,
    AfterTurn,
    AfterWorkerTurn {
        source: crate::start_condition::Turn,
    },
    WhenTold,
    Now,
    Command {
        command: String,
    },
}

pub const COMMANDS: [&str; 10] = [
    "commit",
    "push",
    "open-pr",
    "export",
    "branch-from",
    "restore",
    "stage-mode",
    "settings",
    "plan-only",
    "start-waiting",
];

pub fn read(decision: &Decision) -> Result<Option<Review>> {
    decision
        .scores
        .get("review")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(Into::into)
}

pub fn write(decision: &mut Decision, review: Review) -> Result<()> {
    decision.scores["review"] = serde_json::to_value(review)?;
    Ok(())
}

fn has_timing(text: &str) -> bool {
    text.to_lowercase()
        .split(|c: char| !c.is_ascii_alphabetic())
        .any(|word| ["after", "when", "until", "before", "wait"].contains(&word))
        || [
            "待って",
            "まで",
            "たら",
            "終わった後",
            "確認してから",
            "合図",
        ]
        .iter()
        .any(|word| text.contains(word))
}

pub fn gate(
    decision: &mut Decision,
    probabilities: &BTreeMap<String, f64>,
    text: &str,
) -> Result<()> {
    let operation = probabilities.get("decision").copied();
    let lane = probabilities.get("lane").copied();
    let boundary = probabilities.get("boundary").copied();
    let command = probabilities.get("command").copied();
    // Every present stage is checked independently. Explicit New/Only choices
    // omit stages rather than assigning them a fabricated confidence.
    let stage = if operation.is_some_and(|p| p < 0.3) {
        decision.kind = Kind::Park;
        return Ok(());
    } else if operation.is_some_and(|p| p < 0.55) {
        Some(if decision.kind == Kind::Command {
            Stage::Command
        } else {
            Stage::Operation
        })
    } else if decision.track_id.is_none()
        && matches!(decision.kind, Kind::Extend | Kind::Schedule | Kind::Trim)
    {
        None
    } else if decision.track_id.is_some() && lane.is_some_and(|p| p < 0.3) {
        decision.kind = Kind::Park;
        return Ok(());
    } else if decision.track_id.is_some() && lane.is_some_and(|p| p < 0.55) {
        Some(Stage::Lane)
    } else if decision.kind == Kind::Trim && operation.is_some_and(|p| p < 0.8) {
        Some(Stage::Trim)
    } else if boundary.is_some_and(|p| p < 0.55) {
        Some(Stage::Boundary)
    } else if command.is_some_and(|p| p < 0.55) {
        Some(Stage::Command)
    } else if decision.kind == Kind::Extend && has_timing(text) {
        Some(Stage::Operation)
    } else {
        None
    };
    if let Some(stage) = stage {
        write(decision, Review { stage, turn: None })?;
    }
    Ok(())
}

pub fn bind(review: &mut Review, worker: Option<&WorkerState>) {
    if !matches!(review.stage, Stage::Trim | Stage::Boundary) {
        return;
    }
    review.turn = worker.and_then(|worker| {
        Some(Turn {
            session_id: worker.session_id.clone()?,
            turn_id: worker.active_turn?,
        })
    });
}

pub fn reason(review: &Review, label: &str) -> String {
    match review.stage {
        Stage::Operation => {
            if label.is_empty() {
                "What should this request do?".into()
            } else {
                format!("Send to {label} as an instruction?")
            }
        }
        Stage::Lane => "Where should this request go?".into(),
        Stage::Trim => format!("Stop {label} now?"),
        Stage::Boundary => "When should this run?".into(),
        Stage::Command => "Which action should I prepare?".into(),
        Stage::StaleTurn => {
            "The turn it was for has ended · Send as instruction or take back".into()
        }
        Stage::StartTurn => "Which running turn should the new track wait for?".into(),
    }
}

pub fn confirm(
    mut decision: Decision,
    review: &Review,
    choice: &Choice,
    worker: Option<&WorkerState>,
) -> Result<Decision> {
    let permitted = match (&review.stage, choice) {
        (Stage::Operation | Stage::StaleTurn, Choice::Instruction) => decision.track_id.is_some(),
        (Stage::Trim, Choice::StopAndSend | Choice::AfterTurn) => decision.track_id.is_some(),
        (Stage::Boundary, Choice::Now | Choice::AfterTurn | Choice::WhenTold) => true,
        (Stage::StartTurn, Choice::Now | Choice::WhenTold | Choice::AfterWorkerTurn { .. }) => {
            decision.kind == Kind::Fork
        }
        (Stage::Command, Choice::Command { command }) => COMMANDS.contains(&command.as_str()),
        _ => false,
    };
    if !permitted {
        return Err("This choice does not answer the current question.".into());
    }
    if let Some(scores) = decision.scores.as_object_mut() {
        scores.remove("review");
    }
    decision.scores["confirmation"] = json!({"choice":choice,"by":"user"});
    match choice {
        Choice::Instruction => {
            decision.kind = Kind::Extend;
            decision.boundary = Some("after-turn".into());
        }
        Choice::AfterTurn | Choice::AfterWorkerTurn { .. } => {
            if decision.kind != Kind::Fork {
                decision.kind = Kind::Schedule;
            }
            decision.boundary = Some("after-turn".into());
        }
        Choice::WhenTold => {
            if decision.kind != Kind::Fork {
                decision.kind = Kind::Schedule;
            }
            decision.boundary = Some("when-told".into());
        }
        Choice::Now | Choice::StopAndSend => {
            let current = worker.and_then(|worker| {
                Some(Turn {
                    session_id: worker.session_id.clone()?,
                    turn_id: worker.active_turn?,
                })
            });
            if current.is_some() && current != review.turn {
                write(
                    &mut decision,
                    Review {
                        stage: Stage::StaleTurn,
                        turn: review.turn.clone(),
                    },
                )?;
                return Ok(decision);
            }
            if decision.kind != Kind::Fork {
                decision.kind = if current.is_some() {
                    Kind::Trim
                } else {
                    Kind::Extend
                };
            }
            decision.boundary = Some("now".into());
        }
        Choice::Command { command } => {
            decision.kind = Kind::Command;
            decision.command = Some(command.clone());
        }
    }
    Ok(decision)
}
