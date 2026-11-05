//! A validated snapshot seed. Raw values retain additive protocol members.
use crate::Result;
use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub cursor: String,
    pub state: Value,
}

impl Snapshot {
    pub fn from_read(read: &Value) -> Result<Option<Self>> {
        if !matches!(
            read["history"]["mode"].as_str(),
            Some("snapshot" | "anchoredSnapshot")
        ) {
            return Ok(None);
        }
        let raw = &read["history"]["snapshot"];
        if raw.is_null() {
            return Ok(None);
        }
        let version = raw["schemaVersion"]
            .as_u64()
            .ok_or("Snapshot has no schema version.")?;
        if version > 1 {
            return Ok(None);
        }
        if version != 1 {
            return Err("Unsupported snapshot schema version.".into());
        }
        let typed: muse_code_sdk::msp::ViewSnapshot = serde_json::from_value(raw.clone())?;
        if typed.view_cursor.is_empty() {
            return Err("Snapshot has no cursor.".into());
        }
        if (read["history"]["mode"] == "anchoredSnapshot") != typed.anchor.is_some() {
            return Err("Snapshot anchor does not match the served mode.".into());
        }
        let state = &raw["state"];
        for key in [
            "items",
            "activeTurn",
            "queuedTurns",
            "pendingApprovals",
            "pendingUserInputs",
            "approvalMode",
            "effectiveModel",
            "goal",
            "todoList",
            "branch",
            "name",
            "tokenUsage",
            "turnCount",
        ] {
            if state.get(key).is_none() {
                return Err(format!("Snapshot omitted {key}.").into());
            }
        }
        Ok(Some(Self {
            cursor: typed.view_cursor,
            state: state.clone(),
        }))
    }

    /// State-store values have no invented sourceRange or command identity.
    pub(crate) fn families(&self, session: &str) -> Vec<(String, Value)> {
        let mut result = Vec::new();
        for (field, method, wrapper) in [
            ("effectiveModel", "session/modelChanged", None),
            ("reasoningEffort", "session/reasoningEffortChanged", None),
            ("goal", "session/goalChanged", Some("goal")),
            ("todoList", "session/todoListChanged", None),
            ("branch", "session/branchChanged", None),
            ("name", "session/nameChanged", Some("name")),
            ("tokenUsage", "session/tokenUsage", Some("cumulative")),
            ("contextUsage", "session/contextUsage", None),
            ("approvalMode", "session/approvalModeChanged", None),
        ] {
            let Some(value) = self.state.get(field) else {
                continue;
            };
            if value.is_null() && field != "goal" {
                continue;
            }
            let mut params = json!({"sessionId":session, "viewCursor":self.cursor});
            if let Some(wrapper) = wrapper {
                params[wrapper] = value.clone();
            } else if let Some(fields) = value.as_object() {
                params.as_object_mut().unwrap().extend(fields.clone());
            }
            result.push((method.into(), params));
        }
        result
    }
}
