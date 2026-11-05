//! User-input prompts retain the host's question IDs and exact option labels.
use crate::Result;
use muse_code_sdk::msp::{UserInputAnswer, UserInputRequestParams, UserInputSelectionMode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prompt {
    pub request: UserInputRequestParams,
    pub revision: String,
    pub phase: Phase,
    pub operation: Option<Uuid>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    Open,
    Sending,
    AwaitingSettlement,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Response {
    Answer { answers: Vec<UserInputAnswer> },
    Explain { content: String },
    Stop { answers: Vec<UserInputAnswer> },
}
fn invalid() -> Box<dyn std::error::Error + Send + Sync> {
    std::io::Error::other("The answer does not match the current questions.").into()
}
fn bounded(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.chars().count() <= max
}
impl Prompt {
    pub fn parse(value: Value) -> Result<Self> {
        let request: UserInputRequestParams = serde_json::from_value(value)?;
        if !bounded(&request.user_input_id, 128)
            || !bounded(&request.session_id, 128)
            || !bounded(&request.turn_id, 128)
            || !(1..=3).contains(&request.questions.len())
        {
            return Err(invalid());
        }
        let mut ids = HashSet::new();
        for question in &request.questions {
            if !bounded(&question.id, 64)
                || !ids.insert(&question.id)
                || !bounded(&question.header, 12)
                || !bounded(&question.question, 16384)
                || !(2..=3).contains(&question.options.len())
            {
                return Err(invalid());
            }
            let mut labels = HashSet::new();
            for option in &question.options {
                if !bounded(&option.label, 80)
                    || !labels.insert(&option.label)
                    || option
                        .description
                        .as_ref()
                        .is_some_and(|d| d.chars().count() > 240)
                    || option
                        .preview
                        .as_ref()
                        .is_some_and(|p| p.content.chars().count() > 2000)
                {
                    return Err(invalid());
                }
            }
            if question.selection.mode == UserInputSelectionMode::Multiple {
                let min = question.selection.min_selections.unwrap_or(1);
                let max = question
                    .selection
                    .max_selections
                    .unwrap_or(question.options.len());
                if min == 0
                    || max < min
                    || max > question.options.len()
                    || question.options.iter().any(|o| o.preview.is_some())
                {
                    return Err(invalid());
                }
            }
        }
        // Transport cursors may change when the same request is presented again.
        let content = json!({"sessionId":request.session_id,"userInputId":request.user_input_id,"turnId":request.turn_id,"questions":request.questions});
        let revision = Sha256::digest(serde_json::to_vec(&content)?)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(Self {
            request,
            revision,
            phase: Phase::Open,
            operation: None,
        })
    }
    pub fn key(&self) -> Uuid {
        Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            format!("{}:{}", self.request.session_id, self.request.user_input_id).as_bytes(),
        )
    }
    pub fn validate(&self, response: &Response) -> Result<()> {
        let (answers, complete) = match response {
            Response::Explain { content } => {
                return if bounded(content, 500) {
                    Ok(())
                } else {
                    Err(invalid())
                };
            }
            Response::Answer { answers } => (answers, true),
            Response::Stop { answers } => (answers, false),
        };
        if (complete && answers.len() != self.request.questions.len())
            || answers.len() > self.request.questions.len()
        {
            return Err(invalid());
        }
        let mut seen = HashSet::new();
        for answer in answers {
            if !seen.insert(&answer.question_id) {
                return Err(invalid());
            }
            let question = self
                .request
                .questions
                .iter()
                .find(|q| q.id == answer.question_id)
                .ok_or_else(invalid)?;
            if answer
                .note
                .as_ref()
                .is_some_and(|note| note.chars().count() > 500)
            {
                return Err(invalid());
            }
            let kinds = usize::from(answer.selected_label.is_some())
                + usize::from(!answer.selected_labels.is_empty())
                + usize::from(answer.free_text.is_some());
            if kinds != 1 {
                return Err(invalid());
            }
            if let Some(text) = &answer.free_text {
                if !bounded(text, 500) {
                    return Err(invalid());
                }
                continue;
            }
            let options: HashSet<_> = question.options.iter().map(|o| &o.label).collect();
            match question.selection.mode {
                UserInputSelectionMode::Single => {
                    if !answer.selected_labels.is_empty()
                        || !answer
                            .selected_label
                            .as_ref()
                            .is_some_and(|label| options.contains(label))
                    {
                        return Err(invalid());
                    }
                }
                UserInputSelectionMode::Multiple => {
                    let labels: HashSet<_> = answer.selected_labels.iter().collect();
                    if answer.selected_label.is_some()
                        || labels.len() != answer.selected_labels.len()
                        || labels.len() < question.selection.min_selections.unwrap_or(1)
                        || labels.len() > question.selection.max_selections.unwrap_or(options.len())
                        || !labels.is_subset(&options)
                    {
                        return Err(invalid());
                    }
                }
            }
        }
        Ok(())
    }
    pub fn command(&self, id: Uuid, response: &Response) -> Result<(&'static str, Value)> {
        self.validate(response)?;
        let mut params = json!({"commandId":id,"sessionId":self.request.session_id,"userInputId":self.request.user_input_id});
        let method = match response {
            Response::Answer { answers } => {
                params["answers"] = serde_json::to_value(answers)?;
                "userInput/answer"
            }
            Response::Explain { content } => {
                params["clarification"] = json!({"format":"text","content":content});
                "userInput/clarify"
            }
            Response::Stop { answers } => {
                params["answers"] = serde_json::to_value(answers)?;
                "userInput/interrupt"
            }
        };
        Ok((method, params))
    }
}
