//! Post-decision text is display-only and is bound to the accepted reading.
use crate::{
    Result,
    decision::PendingInput,
    editor::{Manager, Priority, ReadingCancellation},
    journal::Journal,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::{sync::watch, task::JoinHandle};

const PREFIX: &str = include_str!("prompts/decision_description.txt");

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Description {
    pub detail: String,
    pub line: String,
    pub title: String,
}
impl Description {
    pub fn parse(text: &str) -> Result<Self> {
        let value: Self = serde_json::from_str(text)?;
        if [&value.detail, &value.line, &value.title]
            .iter()
            .any(|text| text.trim().is_empty() || text.len() > 16 * 1024)
        {
            return Err("The Editor description has invalid text.".into());
        }
        Ok(value)
    }
}

#[derive(Clone, Debug)]
pub struct Ticket {
    pub input: PendingInput,
    pub reading: i64,
    pub decision: String,
    // A separate transport identity keeps display cancellation independent of input inference.
    pub request: crate::input_readings::Ticket,
    pub track_label: Option<String>,
}

#[derive(Clone)]
pub enum Finish {
    Complete(Description),
    Interrupted,
    Failed,
}

async fn generate(journal: &Journal, editor: &Manager, ticket: &Ticket) -> Result<Finish> {
    let input = journal.prepare_input(ticket.input.clone()).await?;
    let decision: Value = serde_json::from_str(&ticket.decision)?;
    let request = journal
        .screen_editor(
            json!({
                "prefix": PREFIX,
                "tail": json!({"request": input.text, "destination": {"worker": ticket.track_label},
                    "decision": {"operation": decision["kind"], "boundary": decision["boundary"],
                        "command": decision["command"]}}).to_string(),
                "maxTokens": 512,
            }),
            Priority::Background,
        )
        .await?;
    if editor
        .count_tokens(&request, Priority::Background, Some(&ticket.request))
        .await?
        > 2000
    {
        return Ok(Finish::Failed);
    }
    let response = editor.generate(request, Some(&ticket.request)).await?;
    if response["preempted"] == true {
        return Ok(Finish::Interrupted);
    }
    if response["preempted"] != false {
        return Err("The Editor did not complete the description.".into());
    }
    Ok(Finish::Complete(Description::parse(
        response["text"]
            .as_str()
            .ok_or("The Editor did not return description text.")?,
    )?))
}

async fn save(journal: &Journal, ticket: &Ticket, outcome: &Finish) -> Result<bool> {
    let Finish::Complete(description) = outcome else {
        return journal
            .finish_description(ticket.clone(), outcome.clone())
            .await;
    };
    let screened = journal
        .screen_editor(
            json!({
                "tail": serde_json::to_string(description)?,
            }),
            Priority::Background,
        )
        .await;
    let Ok(screened) = screened else {
        return journal
            .finish_description(ticket.clone(), Finish::Failed)
            .await;
    };
    let outcome = Description::parse(screened.params()["tail"].as_str().unwrap_or_default())
        .map(Finish::Complete)
        .unwrap_or(Finish::Failed);
    let result = journal.finish_description(ticket.clone(), outcome).await;
    drop(screened);
    result
}

pub struct Service {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}
impl Service {
    pub fn start(journal: Journal, editor: Manager) -> Self {
        let (stop, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut changes = journal.subscribe();
            let mut status = editor.subscribe();
            let mut pending: Option<(Ticket, Finish)> = None;
            loop {
                if *stopping.borrow() {
                    return;
                }
                tokio::select! {
                    _ = stopping.changed() => return,
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {},
                }
                if let Some((ticket, outcome)) = &pending {
                    let saved = tokio::select! {
                        _ = stopping.changed() => return,
                        saved = save(&journal, ticket, outcome) => saved,
                    };
                    if saved.is_ok_and(|saved| saved) {
                        pending = None;
                    }
                    continue;
                }
                if !editor.status().model_loaded {
                    continue;
                }
                if crate::rephrases::process_next(&journal, &editor, &mut stopping)
                    .await
                    .unwrap_or(false)
                {
                    continue;
                }
                let Ok(Some(ticket)) = journal.claim_description().await else {
                    let _ =
                        crate::clip_summaries::process_next(&journal, &editor, &mut stopping).await;
                    continue;
                };
                let generation = editor.status().generation;
                let _cancellation = ReadingCancellation {
                    editor: editor.clone(),
                    request: ticket.request.clone(),
                };
                let work = generate(&journal, &editor, &ticket);
                tokio::pin!(work);
                let outcome = loop {
                    tokio::select! {
                        _ = stopping.changed() => return,
                        result = &mut work => break result.unwrap_or(Finish::Failed),
                        changed = changes.changed() => {
                            if changed.is_err() { return; }
                            if !journal.description_current(ticket.clone()).await.unwrap_or(false) {
                                break Finish::Interrupted;
                            }
                        },
                        changed = status.changed() => {
                            if changed.is_err() { return; }
                            if editor.status().generation != generation || !editor.status().model_loaded {
                                break Finish::Interrupted;
                            }
                        },
                    }
                };
                pending = Some((ticket.clone(), outcome));
            }
        });
        Self { stop, task }
    }
    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}
