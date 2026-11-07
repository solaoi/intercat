//! Approval-stage identity and refresh ordering, shared by both worker loops.
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Response {
    Allow,
    Deny,
    Reply { text: String, mode: ReplyMode },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prepared {
    Decision {
        choice: String,
        feedback: Option<String>,
    },
    Restart {
        text: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReplyMode {
    Feedback,
    Restart,
}

pub fn reply_mode(request: &Value) -> ReplyMode {
    if accepts_feedback(request) {
        ReplyMode::Feedback
    } else {
        ReplyMode::Restart
    }
}

pub fn wire_params(record: &Value) -> Value {
    let mut params = record.clone();
    params.as_object_mut().unwrap().remove("response");
    params.as_object_mut().unwrap().remove("scope");
    params
}

pub async fn replay(
    journal: &crate::journal::Journal,
    id: uuid::Uuid,
    scope: &str,
    approval: &str,
    requirement: &Value,
    response: &Response,
) -> Result<bool> {
    let Some(saved) = journal.operation(id).await? else {
        return Ok(false);
    };
    if saved["kind"] == "approval.reply" {
        let request: crate::approval_reply::Request =
            serde_json::from_value(saved["payload"].clone())?;
        if request.owner.key(&request.track_id) != scope {
            return Err("This reply belongs to another worker.".into());
        }
        let text = match response {
            Response::Reply {
                text,
                mode: ReplyMode::Restart,
            } => text,
            _ => return Err("Operation ID reused with another response.".into()),
        };
        if saved["payload"]["approvalId"] != approval
            || saved["payload"]["requirement"] != *requirement
            || saved["payload"]["text"] != *text
        {
            return Err("Operation ID reused with a different approval reply.".into());
        }
        if saved["state"] == "superseded" {
            return Err("The approval reply was held because its original repair changed.".into());
        }
        return Ok(true);
    }
    if saved["kind"] != "approval.decide"
        || saved["payload"]["scope"] != scope
        || saved["payload"]["approvalId"] != approval
        || saved["payload"]["requirementId"] != *requirement
        || saved["payload"]["response"] != serde_json::to_value(response)?
    {
        return Err("Operation ID reused with a different approval response.".into());
    }
    if saved["state"] != "complete" {
        return Err("The previous response is awaiting confirmation. Check the worker before answering again.".into());
    }
    Ok(true)
}

pub fn offered(request: &Value, allow: bool) -> Option<String> {
    choices(request, allow)
        .next()
        .and_then(|choice| choice["choiceId"].as_str())
        .map(str::to_owned)
}

fn choices(request: &Value, allow: bool) -> impl Iterator<Item = &Value> {
    request["availableChoices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(move |choice| {
            choice["scope"] == "once"
                && if allow {
                    choice["decision"] == "approved"
                } else {
                    choice["decision"] == "denied" || choice["decision"] == "abort"
                }
        })
}

pub fn accepts_feedback(request: &Value) -> bool {
    choices(request, false).any(|choice| choice["acceptsFeedback"] == true)
}

impl Response {
    pub async fn screen(&self, journal: &crate::journal::Journal) -> Result<()> {
        if let Self::Reply { text, .. } = self {
            if text.trim().is_empty() || text.len() > 16 * 1024 || text.contains('\0') {
                return Err("An approval reply must be nonempty and at most 16 KiB.".into());
            }
            let screened = journal
                .screen_start(serde_json::json!({"text":text}))
                .await?;
            if screened.params()["text"] != *text {
                return Err("Remove the detected secret from this reply before sending it.".into());
            }
        }
        Ok(())
    }
    pub fn prepare(&self, request: &Value) -> Result<Prepared> {
        match self {
            Self::Allow | Self::Deny => Ok(Prepared::Decision {
                choice: offered(request, matches!(self, Self::Allow))
                    .ok_or("Approval choice is unavailable.")?,
                feedback: None,
            }),
            Self::Reply { text, mode } => {
                if *mode != reply_mode(request) {
                    return Err(
                        "The approval reply action changed. Review the current card.".into(),
                    );
                }
                if text.trim().is_empty() || text.len() > 16 * 1024 || text.contains('\0') {
                    return Err("An approval reply must be nonempty and at most 16 KiB.".into());
                }
                match choices(request, false).find(|choice| choice["acceptsFeedback"] == true) {
                    Some(choice) => Ok(Prepared::Decision {
                        choice: choice["choiceId"]
                            .as_str()
                            .ok_or("Invalid approval choice.")?
                            .into(),
                        feedback: Some(text.clone()),
                    }),
                    None => Ok(Prepared::Restart { text: text.clone() }),
                }
            }
        }
    }
}

#[derive(Default)]
pub struct Stages {
    requests: HashMap<String, Value>,
    resolved: BTreeSet<String>,
    decided: BTreeSet<String>,
    acknowledged: BTreeSet<String>,
    in_flight: HashMap<String, String>,
    deferred: HashMap<String, (String, Value)>,
}

fn same_request(left: &Value, right: &Value) -> bool {
    let content = |value: &Value| {
        let mut value = value.clone();
        if let Some(object) = value.as_object_mut() {
            object.remove("sourceRange");
            object.remove("viewCursor");
        }
        value
    };
    content(left) == content(right)
}

/// Identity comes from the retained request; stage facts come from the update.
fn refresh(request: &Value, update: &Value) -> Result<Value> {
    if request["sessionId"] != update["sessionId"] {
        return Err("Approval update belongs to another session".into());
    }
    let mut next = request.clone();
    let object = next.as_object_mut().ok_or("Invalid approval request")?;
    for name in [
        "availableChoices",
        "currentRequirementId",
        "sessionId",
        "sourceRange",
        "subject",
        "viewCursor",
    ] {
        object.insert(
            name.into(),
            update
                .get(name)
                .ok_or("Incomplete approval update")?
                .clone(),
        );
    }
    object.remove("subagentOrigin");
    if let Some(origin) = update.get("subagentOrigin") {
        object.insert("subagentOrigin".into(), origin.clone());
    }
    Ok(next)
}

impl Stages {
    pub fn new(decided: BTreeSet<String>) -> Self {
        Self {
            decided,
            ..Self::default()
        }
    }
    pub fn get(&self, id: &str) -> Option<&Value> {
        self.requests.get(id)
    }
    pub fn len(&self) -> usize {
        self.requests.len()
    }
    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }
    pub fn is_resolved(&self, id: &str) -> bool {
        self.resolved.contains(id)
    }
    pub fn is_decided(&self, id: &str, requirement: &Value) -> bool {
        self.decided
            .contains(&crate::reconcile::approval_key(id, requirement))
    }
    pub fn current(&self, request: &Value) -> bool {
        request["approvalId"]
            .as_str()
            .and_then(|id| self.get(id))
            .is_some_and(|held| same_request(held, request))
    }
    pub fn clear_pending(&mut self) {
        self.requests.clear();
        self.in_flight.clear();
        self.deferred.clear();
    }
    pub fn observe(&mut self, method: &str, params: &Value) -> Result<Option<Value>> {
        let id = params["approvalId"].as_str().ok_or("Missing approval ID")?;
        if method == "approval/resolved" {
            self.close(id);
            return Ok(None);
        }
        if self.resolved.contains(id) {
            return Ok(None);
        }
        if method == "approval/updated" && params["change"]["kind"] == "alreadyTerminal" {
            self.close(id);
            return Ok(None);
        }
        // The ack may close the whole approval. Do not decide its trailing
        // stage until that ack proves the approval is still open.
        if let Some(current) = self.in_flight.get(id) {
            if *current == crate::reconcile::approval_key(id, &params["currentRequirementId"]) {
                return Ok(None);
            }
            self.deferred
                .insert(id.into(), (method.into(), params.clone()));
            return Ok(None);
        }
        let request = match method {
            "approval/request" | "approval/requested" => params.clone(),
            "approval/updated" => match self.requests.get(id) {
                Some(request) => refresh(request, params)?,
                None => return Ok(None),
            },
            _ => return Err("Not an approval event".into()),
        };
        let key = crate::reconcile::approval_key(id, &request["currentRequirementId"]);
        if self.acknowledged.contains(&key) {
            return Ok(None);
        }
        let unchanged = self
            .requests
            .get(id)
            .is_some_and(|old| same_request(old, &request));
        self.requests.insert(id.into(), request.clone());
        Ok((!unchanged).then_some(request))
    }
    pub fn begin(&mut self, id: &str, requirement: &Value) -> bool {
        if self.in_flight.contains_key(id)
            || self.resolved.contains(id)
            || !self
                .requests
                .get(id)
                .is_some_and(|r| r["currentRequirementId"] == *requirement)
        {
            return false;
        }
        let key = crate::reconcile::approval_key(id, requirement);
        if !self.decided.insert(key.clone()) {
            return false;
        }
        self.in_flight.insert(id.into(), key);
        true
    }
    pub fn cancel(&mut self, id: &str) -> Result<Option<Value>> {
        if let Some(key) = self.in_flight.remove(id) {
            self.decided.remove(&key);
        }
        self.release(id)
    }
    pub fn acknowledged(&mut self, id: &str, terminal: bool) -> Result<Option<Value>> {
        let key = self
            .in_flight
            .remove(id)
            .ok_or("Approval ack has no in-flight decision")?;
        self.acknowledged.insert(key);
        if terminal {
            self.close(id);
            return Ok(None);
        }
        self.release(id)
    }
    fn release(&mut self, id: &str) -> Result<Option<Value>> {
        match self.deferred.remove(id) {
            Some((method, params)) => self.observe(&method, &params),
            None => Ok(None),
        }
    }
    fn close(&mut self, id: &str) {
        self.resolved.insert(id.into());
        self.requests.remove(id);
        self.deferred.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reply_uses_a_feedback_capable_denial_even_when_another_choice_is_first() {
        let request = json!({"availableChoices":[
            {"choiceId":"allow","scope":"once","decision":"approved"},
            {"choiceId":"deny","scope":"once","decision":"denied"},
            {"choiceId":"abort","scope":"once","decision":"abort","acceptsFeedback":true}
        ]});
        assert_eq!(
            Response::Reply {
                text: "Yes, run it".into(),
                mode: ReplyMode::Feedback
            }
            .prepare(&request)
            .unwrap(),
            Prepared::Decision {
                choice: "abort".into(),
                feedback: Some("Yes, run it".into())
            }
        );
        assert_eq!(
            Response::Allow.prepare(&request).unwrap(),
            Prepared::Decision {
                choice: "allow".into(),
                feedback: None
            }
        );
        assert_eq!(
            Response::Deny.prepare(&request).unwrap(),
            Prepared::Decision {
                choice: "deny".into(),
                feedback: None
            }
        );
    }

    #[test]
    fn reply_without_feedback_requires_stopping_before_sending_a_new_turn() {
        let request = json!({"availableChoices":[
            {"choiceId":"allow","scope":"once","decision":"approved","acceptsFeedback":true},
            {"choiceId":"deny","scope":"once","decision":"denied"}
        ]});
        assert_eq!(
            Response::Reply {
                text: "Use another folder.".into(),
                mode: ReplyMode::Restart
            }
            .prepare(&request)
            .unwrap(),
            Prepared::Restart {
                text: "Use another folder.".into()
            }
        );
    }

    #[test]
    fn reply_rejects_empty_oversized_and_nul_text() {
        for text in [" ".into(), "x".repeat(16 * 1024 + 1), "x\0y".into()] {
            assert!(
                Response::Reply {
                    text,
                    mode: ReplyMode::Restart
                }
                .prepare(&json!({}))
                .is_err()
            );
        }
    }

    #[test]
    fn a_changed_feedback_capability_cannot_silently_change_the_selected_action() {
        let request =
            json!({"availableChoices":[{"choiceId":"deny","decision":"denied","scope":"once"}]});
        assert!(
            Response::Reply {
                text: "Keep the folder.".into(),
                mode: ReplyMode::Feedback
            }
            .prepare(&request)
            .is_err()
        );
    }

    fn request() -> Value {
        json!({"sessionId":"session","approvalId":"approval","turnId":"turn",
            "itemId":"item","toolCallId":"call","toolName":"bash","taskId":null,
            "rawArgs":"original args","protectedWrite":false,"judgeEscalated":false,
            "currentRequirementId":{"approvalId":"approval","sourceIndex":0},
            "availableChoices":[{"choiceId":"allow"}],"subject":{"command":"first"},
            "viewCursor":"first","sourceRange":{},"subagentOrigin":{"sessionId":"child"}})
    }
    fn update() -> Value {
        json!({"sessionId":"session","approvalId":"approval",
            "currentRequirementId":{"approvalId":"approval","sourceIndex":1},
            "availableChoices":[{"choiceId":"abort"}],"subject":{"command":"second"},
            "viewCursor":"second","sourceRange":{"new":true},"change":{"kind":"stageResolved"}})
    }

    #[test]
    fn stage_update_retains_identity_and_replaces_only_current_stage_facts() -> Result<()> {
        let mut stages = Stages::default();
        let initial = request();
        stages.observe("approval/requested", &initial)?;
        let next = stages.observe("approval/updated", &update())?.unwrap();
        for key in [
            "approvalId",
            "turnId",
            "itemId",
            "toolCallId",
            "toolName",
            "rawArgs",
        ] {
            assert_eq!(next[key], initial[key]);
        }
        assert_eq!(next["currentRequirementId"]["sourceIndex"], 1);
        assert_eq!(next["subject"]["command"], "second");
        assert_eq!(next["availableChoices"][0]["choiceId"], "abort");
        assert!(next.get("subagentOrigin").is_none());
        let mut foreign = update();
        foreign["sessionId"] = json!("other");
        assert!(stages.observe("approval/updated", &foreign).is_err());
        Ok(())
    }

    #[test]
    fn a_terminal_ack_discards_the_update_waiting_behind_it() -> Result<()> {
        let mut stages = Stages::default();
        let initial = request();
        stages.observe("approval/request", &initial)?;
        assert!(stages.begin("approval", &initial["currentRequirementId"]));
        assert!(stages.observe("approval/updated", &update())?.is_none());
        assert!(stages.acknowledged("approval", true)?.is_none());
        assert!(stages.is_empty());
        assert!(stages.observe("approval/request", &initial)?.is_none());
        assert!(stages.observe("approval/updated", &update())?.is_none());
        Ok(())
    }

    #[test]
    fn a_nonterminal_ack_releases_the_updated_stage_once() -> Result<()> {
        let mut stages = Stages::default();
        let initial = request();
        stages.observe("approval/request", &initial)?;
        assert!(stages.begin("approval", &initial["currentRequirementId"]));
        stages.observe("approval/updated", &update())?;
        let next = stages.acknowledged("approval", false)?.unwrap();
        assert!(stages.observe("approval/request", &next)?.is_none());
        assert!(stages.begin("approval", &next["currentRequirementId"]));
        assert!(!stages.begin("approval", &next["currentRequirementId"]));
        Ok(())
    }

    #[test]
    fn a_resolve_before_the_ack_cannot_be_reopened() -> Result<()> {
        let mut stages = Stages::default();
        let initial = request();
        stages.observe("approval/requested", &initial)?;
        stages.begin("approval", &initial["currentRequirementId"]);
        stages.observe("approval/resolved", &json!({"approvalId":"approval"}))?;
        assert!(stages.acknowledged("approval", true)?.is_none());
        assert!(stages.observe("approval/request", &initial)?.is_none());
        Ok(())
    }

    #[test]
    fn failed_preparation_can_be_retried_before_any_command_was_sent() -> Result<()> {
        let mut stages = Stages::default();
        let initial = request();
        stages.observe("approval/requested", &initial)?;
        stages.begin("approval", &initial["currentRequirementId"]);
        stages.cancel("approval")?;
        assert!(stages.begin("approval", &initial["currentRequirementId"]));
        Ok(())
    }

    #[test]
    fn an_unknown_update_or_recovered_decision_never_creates_a_new_command() -> Result<()> {
        let initial = request();
        let mut stages = Stages::new(BTreeSet::from([crate::reconcile::approval_key(
            "approval",
            &initial["currentRequirementId"],
        )]));
        assert!(stages.observe("approval/updated", &update())?.is_none());
        assert!(stages.observe("approval/requested", &initial)?.is_some());
        assert!(!stages.begin("approval", &initial["currentRequirementId"]));
        Ok(())
    }

    #[test]
    fn an_acknowledged_stage_cannot_replace_a_newer_stage() -> Result<()> {
        let mut stages = Stages::default();
        let initial = request();
        stages.observe("approval/requested", &initial)?;
        stages.begin("approval", &initial["currentRequirementId"]);
        stages.acknowledged("approval", false)?;
        let next = stages.observe("approval/updated", &update())?.unwrap();
        assert!(stages.observe("approval/request", &initial)?.is_none());
        assert!(stages.current(&next));
        Ok(())
    }

    #[test]
    fn clearing_finished_turns_preserves_terminal_decisions() -> Result<()> {
        let mut stages = Stages::default();
        let initial = request();
        stages.observe("approval/requested", &initial)?;
        stages.begin("approval", &initial["currentRequirementId"]);
        stages.acknowledged("approval", true)?;
        stages.clear_pending();
        assert!(stages.observe("approval/request", &initial)?.is_none());
        Ok(())
    }

    #[test]
    fn duplicate_of_the_inflight_stage_does_not_erase_a_waiting_update() -> Result<()> {
        let mut stages = Stages::default();
        let initial = request();
        stages.observe("approval/requested", &initial)?;
        stages.begin("approval", &initial["currentRequirementId"]);
        stages.observe("approval/updated", &update())?;
        stages.observe("approval/request", &initial)?;
        let next = stages.acknowledged("approval", false)?.unwrap();
        assert_eq!(next["currentRequirementId"]["sourceIndex"], 1);
        Ok(())
    }
}
