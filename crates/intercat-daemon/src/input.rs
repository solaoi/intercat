//! Read staged words in the background. RPC admission and cancellation never
//! wait for GPU work; a revision check guards every persisted model result.
use crate::{
    Result,
    decision::{Candidate, Decision, Kind, Outcome, PendingInput, Request},
    editor::{Manager, Priority},
    journal::Journal,
};
use serde::Serialize;
use std::sync::Arc;
use tokio::{
    sync::{Notify, mpsc, watch},
    task::{JoinHandle, JoinSet},
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

struct Rereading {
    ticket: crate::input_readings::Ticket,
    generation: u64,
    editor: Manager,
    task: JoinHandle<Result<Outcome>>,
}
impl Drop for Rereading {
    fn drop(&mut self) {
        self.task.abort();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let editor = self.editor.clone();
            let ticket = self.ticket.clone();
            runtime.spawn(async move {
                let _ = editor.cancel_reading(&ticket).await;
            });
        }
    }
}

#[derive(Clone)]
enum RereadWrite {
    Apply(crate::input_readings::Ticket, Outcome),
    Release(crate::input_readings::Ticket),
}

#[derive(Debug)]
struct Preempted;
impl std::fmt::Display for Preempted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("The reading was preempted.")
    }
}
impl std::error::Error for Preempted {}

async fn process_inputs(
    journal: Journal,
    editor: Manager,
    wake: Arc<Notify>,
    mut stopping: watch::Receiver<bool>,
) -> Result<()> {
    let mut status = editor.subscribe();
    let mut changes = journal.subscribe();
    let mut rereading: Option<Rereading> = None;
    let mut pending_write: Option<RereadWrite> = None;
    let mut readings = JoinSet::new();
    let mut foreground = None;
    let (deadlines, mut expired) = mpsc::unbounded_channel();
    loop {
        changes.borrow_and_update();
        if *stopping.borrow() {
            return Ok(());
        }
        if foreground.is_none()
            && editor.status().model_loaded
            && let Some(input) = journal.claim_input().await?
        {
            if let Some(active) = rereading.take() {
                active.task.abort();
                pending_write = Some(RereadWrite::Release(active.ticket.clone()));
            }
            let ticket = ticket(&input);
            foreground = Some(ticket.clone());
            let task_journal = journal.clone();
            let task_editor = editor.clone();
            let task_deadlines = deadlines.clone();
            let generation = editor.status().generation;
            readings.spawn(async move {
                let result = async {
                    let prepared = task_journal.prepare_input(input.clone()).await?;
                    if prepared.text.trim().is_empty() {
                        return Ok(Outcome::Rephrase { generate: false });
                    }
                    classify(
                        &task_journal,
                        &task_editor,
                        &prepared,
                        Priority::Input,
                        Some(&task_deadlines),
                    )
                    .await
                }
                .await;
                (ticket, input, generation, result)
            });
        }
        if let Some(write) = pending_write.take() {
            let saved = match write.clone() {
                RereadWrite::Apply(ticket, outcome) => {
                    journal.apply_reread(ticket, outcome).await?
                }
                RereadWrite::Release(ticket) => journal.release_reread(ticket).await?,
            };
            if !saved {
                pending_write = Some(write);
            }
        }
        if let Some(active) = &rereading
            && (!editor.status().model_loaded
                || editor.status().generation != active.generation
                || !journal.reread_current(active.ticket.clone()).await?)
        {
            let active = rereading.take().unwrap();
            active.task.abort();
            pending_write = Some(RereadWrite::Release(active.ticket.clone()));
            continue;
        }
        if foreground.is_none()
            && rereading.is_none()
            && pending_write.is_none()
            && editor.status().model_loaded
            && let Some(pending) = journal.claim_reread().await?
        {
            let task_journal = journal.clone();
            let task_editor = editor.clone();
            let ticket = pending.ticket;
            let generation = editor.status().generation;
            let task = tokio::spawn(async move {
                let mut input = pending.input;
                if let Some(context) = pending.context {
                    input.context = Some(task_journal.prepare_input(context).await?.text);
                }
                let prepared = task_journal.prepare_input(input).await?;
                classify(&task_journal, &task_editor, &prepared, Priority::Park, None).await
            });
            rereading = Some(Rereading {
                ticket,
                generation,
                editor: editor.clone(),
                task,
            });
        }
        tokio::select! {
            biased;
            _ = stopping.changed() => return Ok(()),
            Some(ticket) = expired.recv() => {
                if foreground.as_ref() == Some(&ticket) { foreground = None; }
            },
            Some(result) = readings.join_next(), if !readings.is_empty() => {
                let (ticket, input, generation, result) = result?;
                if foreground.as_ref() == Some(&ticket) { foreground = None; }
                let result = if editor.status().generation != generation { Ok(Outcome::Defer) } else { result };
                let outcome = match result {
                    Ok(outcome) => outcome,
                    Err(_) if !editor.status().model_loaded => Outcome::Defer,
                    Err(error) if error.is::<InputTooLong>() => Outcome::Rephrase { generate: true },
                    Err(error) if error.is::<crate::editor::InferenceTimeout>() => Outcome::Asked("The Editor took too long. Choose where this should go.".into()),
                    Err(error) if error.is::<crate::attachment_screening::ReviewRequired>() => Outcome::Asked("Recheck the pasted references, then choose where this should go.".into()),
                    Err(_) if !input.sources.is_empty() => Outcome::Asked("The pasted request could not be read. Review its references or give it a one-line instruction.".into()),
                    Err(_) => Outcome::Asked("The Editor could not read this request. Please shorten it or try again.".into()),
                };
                journal.finish_reading(ticket, outcome).await?;
            },
            _ = wake.notified() => {},
            changed = status.changed() => { changed?; },
            result = async {
                match rereading.as_mut() {
                    Some(active) => (&mut active.task).await,
                    None => std::future::pending().await,
                }
            } => {
                let active = rereading.take().unwrap();
                let outcome = match result? {
                    Ok(outcome) => outcome,
                    Err(error) if error.is::<Preempted>() || !editor.status().model_loaded => Outcome::Defer,
                    Err(_) => Outcome::Asked("The parked request could not be read.".into()),
                };
                pending_write = Some(RereadWrite::Apply(active.ticket.clone(), outcome));
            },
            changed = changes.changed() => { changed?; },
        }
    }
}

fn outside_candidates(decision: &Decision) -> bool {
    decision.scores.get("review").is_none()
        && decision.track_id.is_none()
        && matches!(decision.kind, Kind::Extend | Kind::Schedule | Kind::Trim)
}

#[derive(Debug)]
pub(crate) struct InputTooLong;
impl std::fmt::Display for InputTooLong {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("The request exceeds the Editor's token budget.")
    }
}
impl std::error::Error for InputTooLong {}

async fn fit_request(
    journal: &Journal,
    editor: &Manager,
    input: &PendingInput,
    candidates: Vec<Candidate>,
    priority: Priority,
) -> Result<(Request, crate::screening::EditorRequest)> {
    let check_reasons = !matches!(input.target, crate::decision::Target::New)
        && journal
            .reason_review_possible(
                input.id.clone(),
                input.revision,
                candidates
                    .iter()
                    .map(|candidate| candidate.id.clone())
                    .collect(),
            )
            .await?;
    let mut request = Request::new(
        &input.text,
        candidates,
        input.target.clone(),
        input.selected.as_deref(),
    )?;
    if input.single_job {
        request.run_as_one()?;
    }
    if check_reasons {
        request.check_previous_reasons();
    }
    let mut tail: serde_json::Value = serde_json::from_str(request.wire["tail"].as_str().unwrap())?;
    input.history.apply(&mut tail);
    if input.has_attachments {
        tail["hasAttachments"] = true.into();
    }
    request.wire["tail"] = crate::input_context::serialize_tail(&tail)?.into();
    if let Some(context) = &input.context {
        request.reread(context)?;
    }
    let calibration = journal
        .editor_calibration(
            request.wire["prefix"].as_str().unwrap().to_owned(),
            request.wire["cachePrefix"].as_bool().unwrap(),
        )
        .await?;
    request.calibrate(calibration)?;
    let mut screened = journal
        .screen_input(ticket(input), request.wire.clone(), priority)
        .await?;
    let reading = (priority == Priority::Park).then(|| crate::input_readings::Ticket {
        input: input.id.clone(),
        revision: input.revision,
        index: input.reading_index,
    });
    loop {
        if editor
            .count_tokens(&screened, priority, reading.as_ref())
            .await?
            <= 500
        {
            request.use_screened_context(screened.params().clone())?;
            return Ok((request, screened));
        }
        if !screened.reduce_context()? {
            return Err(InputTooLong.into());
        }
    }
}

fn ticket(input: &PendingInput) -> crate::input_readings::Ticket {
    crate::input_readings::Ticket {
        input: input.id.clone(),
        revision: input.revision,
        index: input.reading_index,
    }
}

async fn expire(
    journal: &Journal,
    input: &PendingInput,
    deadlines: &mpsc::UnboundedSender<crate::input_readings::Ticket>,
) -> Result<()> {
    let ticket = ticket(input);
    journal.expire_input(ticket.clone()).await?;
    let _ = deadlines.send(ticket);
    Ok(())
}

async fn infer(
    journal: &Journal,
    editor: &Manager,
    input: &PendingInput,
    request: (Request, crate::screening::EditorRequest),
    pass: u8,
    priority: Priority,
    deadlines: Option<&mpsc::UnboundedSender<crate::input_readings::Ticket>>,
) -> Result<Decision> {
    let (request, screened) = request;
    let started = std::time::Instant::now();
    let reading = (priority == Priority::Park).then(|| crate::input_readings::Ticket {
        input: input.id.clone(),
        revision: input.revision,
        index: input.reading_index,
    });
    let mut expired = false;
    let response = async {
        // Admission headers arrive only after the scheduler has started inference.
        let inference = editor.decide(screened, priority, reading.as_ref()).await?;
        let response = inference.finish(std::time::Duration::from_secs(120));
        tokio::pin!(response);
        if let Some(deadlines) = deadlines {
            tokio::select! {
                result = &mut response => result,
                _ = tokio::time::sleep(std::time::Duration::from_millis(1200)) => {
                    expire(journal, input, deadlines).await?;
                    expired = true;
                    response.await
                }
            }
        } else {
            response.await
        }
    }
    .await;
    let round_trip_ms = started.elapsed().as_secs_f64() * 1000.0;
    let mut result = response.and_then(|value| {
        if value["preempted"] == true {
            Err(Preempted.into())
        } else {
            request.resolve(value)
        }
    });
    if let Ok(decision) = &mut result {
        if decision.timed_out
            && !expired
            && let Some(deadlines) = deadlines
        {
            expire(journal, input, deadlines).await?;
        }
        decision.timed_out |= expired;
    }
    let (inference_ms, outcome) = match &mut result {
        Ok(decision) => {
            decision.scores["roundTripMs"] = serde_json::json!(round_trip_ms);
            (
                Some(decision.inference_ms),
                if decision.timed_out {
                    "late"
                } else {
                    "complete"
                },
            )
        }
        Err(error) if error.is::<crate::editor::InferenceTimeout>() => (None, "timeout"),
        Err(error) if error.is::<Preempted>() => (None, "preempted"),
        Err(_) => (None, "failed"),
    };
    journal
        .append(
            "input.inference",
            serde_json::json!({
                "utteranceId":input.id, "revision":input.revision, "pass":pass,
                "readingIndex":input.reading_index,
                "roundTripMs":round_trip_ms, "inferenceMs":inference_ms, "outcome":outcome,
            }),
        )
        .await?;
    result
}

async fn classify(
    journal: &Journal,
    editor: &Manager,
    input: &PendingInput,
    priority: Priority,
    deadlines: Option<&mpsc::UnboundedSender<crate::input_readings::Ticket>>,
) -> Result<Outcome> {
    let request = fit_request(journal, editor, input, input.tracks.clone(), priority).await?;
    let mut first = infer(journal, editor, input, request, 1, priority, deadlines).await?;
    if !outside_candidates(&first) {
        first.scores["routing"] = serde_json::json!({"pass":1});
        return Ok(Outcome::Classified(first));
    }
    // Each pass has its own inference deadline. In particular, do not apply a
    // 1.2-second deadline to both passes or dispatch to a rejected provisional lane.
    let expanded = fit_request(
        journal,
        editor,
        input,
        input.expanded_tracks.clone(),
        priority,
    )
    .await?;
    let mut second = infer(journal, editor, input, expanded, 2, priority, deadlines).await?;
    second.timed_out |= first.timed_out;
    second.scores["routing"] = serde_json::json!({"pass":2,"previous":first.scores});
    Ok(Outcome::Classified(second))
}
