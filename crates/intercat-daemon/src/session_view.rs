//! Durable session facts reconstructed from the applied notification journal.
//! Streaming text remains in the bounded, secret-checked presentation path.
use crate::Result;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnState {
    Running,
    Queued,
    Historical,
    Settled,
    Retracted,
    Unqueued,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    #[serde(rename = "turnId")]
    pub id: Uuid,
    pub state: TurnState,
    #[serde(rename = "commandId", skip_serializing_if = "Option::is_none")]
    pub command: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_scheduled: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_retry_scheduled: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub foreground_completed: Option<Value>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingApproval {
    pub approval_id: String,
    pub requested: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_update: Option<Value>,
}

#[derive(Clone, Debug, Default)]
pub struct SessionView {
    pub items: Vec<Value>,
    pub turns: BTreeMap<Uuid, Turn>,
    pub active_turn: Option<Uuid>,
    pub session_state: BTreeMap<String, Value>,
    pub pending_approvals: Vec<PendingApproval>,
    pub resolved_approvals: Vec<Value>,
    pub pending_user_inputs: Vec<Value>,
    pub settled_user_inputs: Vec<Value>,
    // Preserve the latest view while keeping ambiguous ownership fail-closed.
    pub(crate) conflicting_starts: BTreeSet<Uuid>,
    turn_order: Vec<Uuid>,
    state_cursors: BTreeMap<String, Value>,
    snapshot_state: Option<Value>,
}

impl SessionView {
    pub(crate) fn snapshot_metadata(&self) -> Option<&Value> {
        self.snapshot_state.as_ref()
    }

    /// Complete current snapshot state, including additive fields from its seed.
    pub fn snapshot_state(&self) -> Option<Value> {
        self.snapshot_state.as_ref().map(|state| {
            let mut state = state.clone();
            state["items"] = serde_json::json!(self.items);
            state
        })
    }

    pub(crate) fn seed(
        &mut self,
        session: &str,
        snapshot: &crate::session_snapshot::Snapshot,
    ) -> Result<()> {
        let state = &snapshot.state;
        // User-message identities are execution evidence, but do not invent
        // a terminal outcome for a turn the retained event log never named.
        for item in state["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["kind"] == "userMessage")
        {
            let (Some(id), Some(command)) = (item["turnId"].as_str(), item["commandId"].as_str())
            else {
                continue;
            };
            let id: Uuid = id.parse()?;
            let command: Uuid = command.parse()?;
            if let Some(old) = self.turns.get_mut(&id) {
                if old.command.is_some_and(|old| old != command) {
                    return Err("Snapshot contains conflicting submit identities.".into());
                }
                old.command = Some(command);
            } else {
                self.turn_order.push(id);
                self.turns.insert(
                    id,
                    Turn {
                        id,
                        state: TurnState::Historical,
                        command: Some(command),
                        terminal: None,
                        error: None,
                        retry_scheduled: None,
                        model_retry_scheduled: None,
                        foreground_completed: None,
                    },
                );
            }
        }
        for queued in state["queuedTurns"].as_array().unwrap() {
            let id: Uuid = string(queued, "turnId")?.parse()?;
            let command: Uuid = string(queued, "commandId")?.parse()?;
            if let Some(old) = self.turns.get_mut(&id) {
                if !matches!(old.state, TurnState::Historical | TurnState::Queued)
                    || old.command.is_some_and(|old| old != command)
                {
                    return Err("Snapshot queue contradicts an observed turn.".into());
                }
                old.state = TurnState::Queued;
                old.command = Some(command);
            } else {
                self.turn_order.push(id);
                self.turns.insert(
                    id,
                    Turn {
                        id,
                        state: TurnState::Queued,
                        command: Some(command),
                        terminal: None,
                        error: None,
                        retry_scheduled: None,
                        model_retry_scheduled: None,
                        foreground_completed: None,
                    },
                );
            }
        }
        let active = &state["activeTurn"];
        if !active.is_null() {
            let id: Uuid = string(active, "turnId")?.parse()?;
            let command: Uuid = string(active, "commandId")?.parse()?;
            if let Some(old) = self.turns.get(&id) {
                if !matches!(
                    old.state,
                    TurnState::Running | TurnState::Queued | TurnState::Historical
                ) || old.command.is_some_and(|old| old != command)
                {
                    return Err(
                        "The snapshot contradicts an observed turn outcome or identity.".into(),
                    );
                }
            } else {
                self.turn_order.push(id);
                self.turns.insert(
                    id,
                    Turn {
                        id,
                        command: Some(command),
                        state: TurnState::Running,
                        terminal: None,
                        error: None,
                        retry_scheduled: None,
                        model_retry_scheduled: None,
                        foreground_completed: None,
                    },
                );
            }
            self.turns.get_mut(&id).unwrap().command = Some(command);
            self.turns.get_mut(&id).unwrap().state = TurnState::Running;
            self.active_turn = Some(id);
        } else {
            self.active_turn = None;
        }
        let approvals = state["pendingApprovals"].as_array().unwrap();
        let questions = state["pendingUserInputs"].as_array().unwrap();
        if approvals.iter().any(|p| {
            p["approvalId"]
                .as_str()
                .is_some_and(|id| self.approval_resolved(id))
        }) || questions.iter().any(|p| {
            p["userInputId"]
                .as_str()
                .is_some_and(|id| self.user_input_settled(id))
        }) {
            return Err("The snapshot revives a settled request.".into());
        }
        self.pending_approvals.retain(|held| {
            approvals
                .iter()
                .any(|p| p["approvalId"] == held.approval_id)
        });
        self.pending_user_inputs.retain(|held| {
            questions
                .iter()
                .any(|p| p["userInputId"] == held["userInputId"])
        });
        self.items.clear();
        for item in state["items"].as_array().unwrap() {
            if let Some(old) = self
                .items
                .iter_mut()
                .find(|old| old["itemId"] == item["itemId"])
            {
                *old = item.clone();
            } else {
                self.items.push(item.clone());
            }
        }
        self.session_state.clear();
        self.state_cursors.clear();
        for (family, value) in snapshot.families(session) {
            self.state_cursors
                .insert(family.clone(), snapshot.cursor.clone().into());
            self.session_state.insert(family, value);
        }
        let mut state = state.clone();
        state.as_object_mut().unwrap().remove("items");
        self.snapshot_state = Some(state);
        Ok(())
    }
    pub fn ordered_turns(&self) -> Vec<&Turn> {
        self.turn_order.iter().map(|id| &self.turns[id]).collect()
    }

    pub fn approval_resolved(&self, id: &str) -> bool {
        self.resolved_approvals
            .iter()
            .any(|p| p["approvalId"] == id)
    }

    pub fn user_input_settled(&self, id: &str) -> bool {
        self.settled_user_inputs
            .iter()
            .any(|p| p["userInputId"] == id)
    }

    pub fn apply(&mut self, session: &str, frame: &Value) -> Result<()> {
        let p = &frame["params"];
        if !p.is_object() || p["sessionId"] != session {
            return Ok(());
        }
        let method = frame["method"].as_str().unwrap_or_default();
        let previously_ended = p["turnId"]
            .as_str()
            .and_then(|id| id.parse::<Uuid>().ok())
            .and_then(|id| self.turns.get(&id))
            .is_some_and(|t| {
                matches!(
                    t.state,
                    TurnState::Settled | TurnState::Retracted | TurnState::Unqueued
                )
            });
        match method {
            "item/started" | "item/updated" | "item/completed" => {
                let item = &p["item"];
                let id = string(item, "itemId")?;
                let revision = item["revision"].as_u64().ok_or("Invalid item revision.")?;
                if let Some(held) = self.items.iter_mut().find(|held| held["itemId"] == id) {
                    if revision > held["revision"].as_u64().ok_or("Invalid held revision.")? {
                        *held = item.clone();
                    }
                } else {
                    self.items.push(item.clone());
                }
            }
            "turn/started"
            | "turn/completed"
            | "turn/retracted"
            | "turn/unqueued"
            | "turn/retryScheduled"
            | "turn/modelRetryScheduled"
            | "turn/foregroundCompleted" => {
                self.turn(method, p)?;
            }
            "approval/requested" => {
                let id = string(p, "approvalId")?;
                if !self.approval_resolved(id) {
                    let fresh = PendingApproval {
                        approval_id: id.into(),
                        requested: p.clone(),
                        latest_update: None,
                    };
                    if let Some(held) = self
                        .pending_approvals
                        .iter_mut()
                        .find(|a| a.approval_id == id)
                    {
                        *held = fresh;
                    } else {
                        self.pending_approvals.push(fresh);
                    }
                }
            }
            "approval/updated" => {
                let id = string(p, "approvalId")?;
                if let Some(held) = self
                    .pending_approvals
                    .iter_mut()
                    .find(|a| a.approval_id == id)
                {
                    held.latest_update = Some(p.clone());
                }
            }
            "approval/resolved" => {
                let id = string(p, "approvalId")?;
                self.pending_approvals.retain(|a| a.approval_id != id);
                if !self.approval_resolved(id) {
                    self.resolved_approvals.push(p.clone());
                }
            }
            "userInput/requested" => {
                let id = string(p, "userInputId")?;
                if !self.user_input_settled(id) {
                    if let Some(held) = self
                        .pending_user_inputs
                        .iter_mut()
                        .find(|a| a["userInputId"] == id)
                    {
                        *held = p.clone();
                    } else {
                        self.pending_user_inputs.push(p.clone());
                    }
                }
            }
            "userInput/settled" => {
                let id = string(p, "userInputId")?;
                self.pending_user_inputs.retain(|a| a["userInputId"] != id);
                if !self.user_input_settled(id) {
                    self.settled_user_inputs.push(p.clone());
                }
            }
            "session/modelChanged"
            | "session/reasoningEffortChanged"
            | "session/goalChanged"
            | "session/todoListChanged"
            | "session/branchChanged"
            | "session/tokenUsage"
            | "session/contextUsage"
            | "session/approvalModeChanged"
            | "session/nameChanged" => {
                if let Some(cursor) = p.get("viewCursor") {
                    if self.state_cursors.get(method) == Some(cursor) {
                        return Ok(());
                    }
                    self.state_cursors.insert(method.into(), cursor.clone());
                }
                self.session_state.insert(method.into(), p.clone());
            }
            _ => {}
        }
        // A lastTurn restatement is already included in the read snapshot's
        // count. It supplies execution evidence without inventing a new turn.
        let already_counted = previously_ended
            || frame["_intercatRecovered"] == true && p.get("viewCursor").is_none();
        self.update_snapshot(method, p, already_counted);
        Ok(())
    }

    fn update_snapshot(&mut self, method: &str, p: &Value, previously_ended: bool) {
        if method == "turn/retracted" && p["outcome"] == "declined"
            || method == "approval/requested"
                && p["approvalId"]
                    .as_str()
                    .is_some_and(|id| self.approval_resolved(id))
            || method == "userInput/requested"
                && p["userInputId"]
                    .as_str()
                    .is_some_and(|id| self.user_input_settled(id))
        {
            return;
        }
        let Some(state) = &mut self.snapshot_state else {
            return;
        };
        let fields = |keys: &[&str]| -> Value {
            Value::Object(
                keys.iter()
                    .filter_map(|key| p.get(*key).map(|v| ((*key).into(), v.clone())))
                    .collect(),
            )
        };
        match method {
            "session/modelChanged" => {
                state["effectiveModel"] = fields(&["modelId", "providerId", "source"])
            }
            "session/reasoningEffortChanged" => {
                state["reasoningEffort"] = fields(&["reasoningEffort", "source"])
            }
            "session/goalChanged" => state["goal"] = p["goal"].clone(),
            "session/todoListChanged" => {
                state["todoList"] = fields(&["revision", "sourceTool", "items"])
            }
            "session/branchChanged" => {
                state["branch"] = fields(&["workspaceRoot", "branch", "vcs"])
            }
            "session/nameChanged" => state["name"] = p["name"].clone(),
            "session/tokenUsage" => state["tokenUsage"] = p["cumulative"].clone(),
            "session/contextUsage" => {
                state["contextUsage"] = fields(&["windowTokens", "usedTokens", "pressure"])
            }
            "session/approvalModeChanged" => {
                state["approvalMode"] = fields(&["mode", "source"]);
                state["approvalMode"]["lastCommandId"] = p["commandId"].clone();
            }
            "turn/started" => {
                if self.active_turn.map(|id| id.to_string()).as_deref() == p["turnId"].as_str() {
                    state["activeTurn"] = fields(&["turnId", "commandId"]);
                }
                state["queuedTurns"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|t| t["turnId"] != p["turnId"]);
            }
            "turn/completed" | "turn/retracted" | "turn/unqueued" => {
                if state["activeTurn"]["turnId"] == p["turnId"] {
                    state["activeTurn"] = Value::Null;
                }
                state["queuedTurns"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|t| t["turnId"] != p["turnId"]);
                if method == "turn/completed" && !previously_ended {
                    state["turnCount"] =
                        serde_json::json!(state["turnCount"].as_u64().unwrap().saturating_add(1));
                }
            }
            "approval/requested" | "userInput/requested" => {
                let (list, id) = if method == "approval/requested" {
                    ("pendingApprovals", "approvalId")
                } else {
                    ("pendingUserInputs", "userInputId")
                };
                let entries = state[list].as_array_mut().unwrap();
                if !entries.iter().any(|entry| entry[id] == p[id]) {
                    entries.push(fields(&[id, "itemId", "viewCursor"]));
                }
            }
            "approval/resolved" => state["pendingApprovals"]
                .as_array_mut()
                .unwrap()
                .retain(|a| a["approvalId"] != p["approvalId"]),
            "userInput/settled" => state["pendingUserInputs"]
                .as_array_mut()
                .unwrap()
                .retain(|q| q["userInputId"] != p["userInputId"]),
            _ => {}
        }
    }

    fn turn(&mut self, method: &str, p: &Value) -> Result<()> {
        let id = string(p, "turnId")?.parse()?;
        if method == "turn/retracted" && p["outcome"] == "declined" {
            if let Some(turn) = self.turns.get_mut(&id) {
                turn.command = Some(string(p, "commandId")?.parse()?);
            }
            return Ok(());
        }
        if !self.turns.contains_key(&id) {
            self.turn_order.push(id);
        }
        let turn = self.turns.entry(id).or_insert(Turn {
            id,
            state: TurnState::Running,
            command: None,
            terminal: None,
            error: None,
            retry_scheduled: None,
            model_retry_scheduled: None,
            foreground_completed: None,
        });
        match method {
            "turn/started" if matches!(turn.state, TurnState::Running | TurnState::Queued) => {
                turn.state = TurnState::Running;
                let command = string(p, "commandId")?.parse()?;
                if turn.command.is_some_and(|previous| previous != command) {
                    self.conflicting_starts.insert(id);
                }
                turn.command = Some(command);
                self.active_turn = Some(id);
            }
            "turn/completed"
                if !matches!(turn.state, TurnState::Retracted | TurnState::Unqueued) =>
            {
                turn.state = TurnState::Settled;
                turn.terminal = Some(string(p, "terminal")?.into());
                if let Some(error) = p.get("error") {
                    turn.error = Some(error.clone());
                }
                turn.retry_scheduled = None;
                turn.model_retry_scheduled = None;
                turn.foreground_completed = None;
                if self.active_turn == Some(id) {
                    self.active_turn = None;
                }
            }
            "turn/retracted" | "turn/unqueued" => {
                turn.command = Some(string(p, "commandId")?.parse()?);
                turn.state = if method == "turn/retracted" {
                    TurnState::Retracted
                } else {
                    TurnState::Unqueued
                };
                if self.active_turn == Some(id) {
                    self.active_turn = None;
                }
            }
            "turn/retryScheduled" if turn.state == TurnState::Running => {
                turn.retry_scheduled = Some(p.clone())
            }
            "turn/modelRetryScheduled" if turn.state == TurnState::Running => {
                turn.model_retry_scheduled = Some(p.clone())
            }
            "turn/foregroundCompleted" if turn.state == TurnState::Running => {
                turn.foreground_completed = Some(p.clone())
            }
            _ => {}
        }
        Ok(())
    }
}

fn string<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value[field]
        .as_str()
        .ok_or_else(|| format!("Session view is missing {field}.").into())
}
