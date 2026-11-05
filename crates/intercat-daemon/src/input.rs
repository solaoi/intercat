//! Read staged words in the background. RPC admission and cancellation never
//! wait for GPU work; a revision check guards every persisted model result.
use crate::{
    Result,
    decision::{Outcome, Request},
    editor::Manager,
    journal::Journal,
};
use serde::Serialize;
use std::sync::Arc;
use tokio::{
    sync::{Notify, watch},
    task::JoinHandle,
};

pub struct Processor {
    stop: watch::Sender<bool>,
    pub status: watch::Receiver<Status>,
    task: Option<JoinHandle<Result<()>>>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    running: bool,
    detail: Option<String>,
}
impl Processor {
    pub async fn start(journal: Journal, editor: Manager, wake: Arc<Notify>) -> Result<Self> {
        journal.recover_inputs().await?;
        let (stop, stopping) = watch::channel(false);
        let (state, status) = watch::channel(Status {
            running: true,
            detail: None,
        });
        let task = tokio::spawn(async move {
            let result = process_inputs(journal, editor, wake, stopping).await;
            state.send_replace(Status {
                running: false,
                detail: result.as_ref().err().map(|_| {
                    "Input processing stopped. Restart the daemon before placing more requests."
                        .into()
                }),
            });
            result
        });
        Ok(Self {
            stop,
            status,
            task: Some(task),
        })
    }
    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.take() {
            task.await??;
        }
        Ok(())
    }
}
impl Drop for Processor {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

async fn process_inputs(
    journal: Journal,
    editor: Manager,
    wake: Arc<Notify>,
    mut stopping: watch::Receiver<bool>,
) -> Result<()> {
    let mut status = editor.subscribe();
    loop {
        if *stopping.borrow() {
            return Ok(());
        }
        if editor.status().model_loaded
            && let Some(input) = journal.claim_input().await?
        {
            let request = Request::new(
                &input.text,
                input.tracks,
                input.target,
                input.selected.as_deref(),
            );
            let outcome = match request {
                Err(_) => Outcome::Asked("The selected track is no longer available.".into()),
                Ok(request) => {
                    let result = tokio::select! {
                        _ = stopping.changed() => return Ok(()),
                        result = editor.decide(&request.wire) => result,
                    };
                    match result {
                        Ok(value) => match request.resolve(value) {
                            Ok(decision) => Outcome::Classified(decision),
                            Err(_) => Outcome::Asked("The Editor returned an invalid decision. Please try again.".into()),
                        },
                        Err(_) if !editor.status().model_loaded => Outcome::Defer,
                        Err(error) if error.is::<crate::editor::InferenceTimeout>() => Outcome::Asked("The Editor took too long. Choose where this should go.".into()),
                        Err(_) => Outcome::Asked("The Editor could not read this request. Please shorten it or try again.".into()),
                    }
                }
            };
            let deferred = matches!(outcome, Outcome::Defer);
            journal
                .apply_input(input.id, input.revision, outcome)
                .await?;
            if !deferred {
                continue;
            }
        }
        tokio::select! {
            _ = stopping.changed() => return Ok(()),
            _ = wake.notified() => {},
            changed = status.changed() => { changed?; },
        }
    }
}
