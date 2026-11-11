//! One repair turn on its own host. The control handle has an immediate Stop
//! path; journals and policy work cannot delay that interrupt.
use crate::{
    Result,
    execution::Approval,
    journal::{Admission, Journal},
    msp::Client,
    questions::{Phase as PromptPhase, Prompt, Response},
    repair_host::Hosted,
    repair_run::{Control, Event, Failure, Outcome, Phase, Run},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

enum Input {
    Stop(Uuid),
    Secret {
        target: crate::secret_input::Target,
        reply: oneshot::Sender<Result<()>>,
    },
    Answer {
        id: Uuid,
        approval: String,
        requirement: Value,
        response: crate::approval::Response,
        reply: oneshot::Sender<Result<()>>,
    },
    Respond {
        id: Uuid,
        prompt: String,
        revision: String,
        response: Response,
        sent: bool,
        reply: oneshot::Sender<Result<()>>,
    },
}

#[cfg(test)]
#[path = "repair_execution_oracle.rs"]
mod oracle_tests;

#[derive(Clone)]
pub struct Port {
    run: Uuid,
    track: String,
    session: String,
    client: Client,
    stopped: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    prompts: Arc<Mutex<Vec<Prompt>>>,
    shown_prompts: Arc<Mutex<Vec<Prompt>>>,
    sender: mpsc::UnboundedSender<Input>,
}

impl Port {
    pub fn secret_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub async fn quiesce_for_secret(&self, target: crate::secret_input::Target) -> Result<()> {
        if target.owner != (crate::secret_input::Owner::Repair { run_id: self.run })
            || target.track_id != self.track
            || target.session_id != self.session
        {
            return Err("The secret belongs to another repair worker.".into());
        }
        let (reply, result) = oneshot::channel();
        self.sender.send(Input::Secret { target, reply })?;
        result.await?
    }
    pub fn track_id(&self) -> &str {
        &self.track
    }
    pub fn set_redaction(&self, session: &str, mask: crate::progress::Mask) {
        if self.session == session {
            self.client.set_redaction(mask);
        }
    }
    pub fn progress(&self) -> Option<crate::progress::Preview> {
        self.client.progress()
    }
    pub fn output_flow(&self) -> crate::output_flow::Status {
        self.client.output_flow()
    }
    fn interrupt(&self) {
        self.stopped.store(true, Ordering::Release);
        for prompt in self.prompts.lock().unwrap().iter() {
            if let Ok((method, params)) =
                prompt.command(Uuid::now_v7(), &Response::Stop { answers: vec![] })
            {
                let _ = self.client.control_now(method, params);
            }
        }
        let _ = self.client.interrupt_now(&self.session, None);
    }
    pub fn stop(&self, id: Uuid) -> Result<()> {
        self.cancelled.store(true, Ordering::Release);
        self.interrupt();
        self.sender.send(Input::Stop(id))?;
        Ok(())
    }
    pub async fn answer(
        &self,
        id: Uuid,
        approval: String,
        requirement: Value,
        response: crate::approval::Response,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.sender.send(Input::Answer {
            id,
            approval,
            requirement,
            response,
            reply,
        })?;
        result.await?
    }
    pub async fn respond(
        &self,
        id: Uuid,
        prompt: String,
        revision: String,
        response: Response,
    ) -> Result<()> {
        let sent = matches!(response, Response::Stop { .. });
        if sent {
            let raw = self
                .prompts
                .lock()
                .unwrap()
                .iter()
                .find(|p| p.request.user_input_id == prompt && p.revision == revision)
                .cloned()
                .ok_or("The repair question changed.")?;
            let shown = self
                .shown_prompts
                .lock()
                .unwrap()
                .iter()
                .find(|p| p.request.user_input_id == prompt && p.revision == revision)
                .cloned()
                .ok_or("The repair question changed.")?;
            let mapped = original_response(&shown, &raw, response.clone())?;
            let (method, params) = raw.command(id, &mapped)?;
            self.cancelled.store(true, Ordering::Release);
            self.client.control_now(method, params)?;
            self.stopped.store(true, Ordering::Release);
            for other in self
                .prompts
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.request.user_input_id != prompt)
            {
                let (method, params) =
                    other.command(Uuid::now_v7(), &Response::Stop { answers: vec![] })?;
                self.client.control_now(method, params)?;
            }
            let _ = self.client.interrupt_now(&self.session, None);
        }
        let (reply, result) = oneshot::channel();
        self.sender.send(Input::Respond {
            id,
            prompt,
            revision,
            response,
            sent,
            reply,
        })?;
        result.await?
    }
}

pub struct Runner {
    port: Port,
    receiver: mpsc::UnboundedReceiver<Input>,
    submitted: Option<Uuid>,
    quiesced: Option<Uuid>,
    // An uncertain start keeps its slot until host cleanup confirms the end.
    permit: Option<crate::scheduler::Permit>,
    policies: tokio::task::JoinSet<Result<PreparedApproval>>,
}
struct PreparedApproval {
    request: Value,
    verdict: crate::policy::Verdict,
    deletion: Option<crate::deletion::Snapshot>,
}
pub struct Task {
    pub instruction: String,
    pub capacity: Arc<crate::scheduler::Capacity>,
    pub shutdown: watch::Receiver<bool>,
    pub project_secrets: crate::project_secrets::Store,
}

/// A turn uses the transport and process tracker; the owner keeps host lifetime.
struct SessionIo<'a> {
    connection: &'a mut crate::msp::Connection,
    tracker: Arc<Mutex<crate::process::Tracker>>,
}

pub fn channel(host: &mut Hosted, run: &Run) -> Result<(Port, Runner)> {
    if host.run_id() != run.id {
        return Err("The repair host is not ready for this Run.".into());
    }
    channel_on(run, host.host_mut().connection.client.clone())
}

fn channel_on(run: &Run, client: Client) -> Result<(Port, Runner)> {
    if run.phase != Phase::Preparing || !run.session_ready {
        return Err("The repair host is not ready for this Run.".into());
    }
    let session = run.session.clone().ok_or("Missing repair session.")?;
    let (sender, receiver) = mpsc::unbounded_channel();
    let port = Port {
        run: run.id,
        track: run.track.clone(),
        session,
        client,
        stopped: Arc::new(AtomicBool::new(false)),
        cancelled: Arc::new(AtomicBool::new(false)),
        prompts: Arc::new(Mutex::new(vec![])),
        shown_prompts: Arc::new(Mutex::new(vec![])),
        sender,
    };
    Ok((
        port.clone(),
        Runner {
            port,
            receiver,
            submitted: None,
            quiesced: None,
            permit: None,
            policies: tokio::task::JoinSet::new(),
        },
    ))
}

async fn update(journal: &Journal, run: &mut Run, event: Event) -> Result<()> {
    *run = journal.repair_source(run.id).await?.0;
    *run = journal
        .repair_transition(Uuid::now_v7(), run.id, run.revision, event)
        .await?;
    Ok(())
}

async fn present(
    journal: &Journal,
    run: &mut Run,
    port: &Port,
    raw: &HashMap<String, Prompt>,
) -> Result<()> {
    let event = Event::Presentation {
        approvals: run.approvals.clone(),
        prompts: run.prompts.clone(),
        output: run.output.clone(),
    };
    update(journal, run, event).await?;
    *port.prompts.lock().unwrap() = raw.values().cloned().collect();
    *port.shown_prompts.lock().unwrap() = run.prompts.clone();
    Ok(())
}

async fn stop(journal: &Journal, run: &mut Run, port: &Port, id: Uuid) -> Result<()> {
    if !port.stopped.load(Ordering::Acquire) {
        port.interrupt();
    }
    record_stop(journal, run, id).await
}
async fn record_stop(journal: &Journal, run: &mut Run, id: Uuid) -> Result<()> {
    *run = journal.repair_source(run.id).await?.0;
    if let Some(operation) = journal.operation(id).await? {
        if operation["kind"] == "repair.control"
            && operation["payload"]["request"]["id"] == run.id.to_string()
            && operation["payload"]["request"]["action"] == "stop"
        {
            return Ok(());
        }
        return Err("Operation ID was used for a different repair action.".into());
    }
    if matches!(
        run.phase,
        Phase::Preparing | Phase::Running | Phase::Stopping
    ) {
        *run = journal
            .repair_control(crate::repair_run::Command {
                op_id: id,
                id: run.id,
                revision: run.revision,
                action: Control::Stop,
            })
            .await?;
    }
    Ok(())
}

async fn safe(scanner: &crate::secret_scan::Scanner, text: &str) -> Result<String> {
    if text.len() > 1024 * 1024 {
        return Ok("[output withheld: message exceeded the limit]".into());
    }
    Ok(scanner
        .redact(&[text.to_owned()], watch::channel(false).1)
        .await?
        .remove(0))
}

async fn visible_prompt(scanner: &crate::secret_scan::Scanner, raw: &Prompt) -> Result<Prompt> {
    let mut displayed: Prompt = serde_json::from_value(
        crate::recording::redact(scanner, serde_json::to_value(raw)?).await?,
    )?;
    if displayed.request.session_id != raw.request.session_id
        || displayed.request.user_input_id != raw.request.user_input_id
        || displayed.request.turn_id != raw.request.turn_id
    {
        return Err("A repair question used sensitive data as a technical ID.".into());
    }
    displayed.revision = raw.revision.clone();
    for (shown, original) in displayed
        .request
        .questions
        .iter_mut()
        .zip(&raw.request.questions)
    {
        if shown.id != original.id {
            return Err("A repair question used sensitive data as an ID.".into());
        }
        for (index, (shown, original)) in
            shown.options.iter_mut().zip(&original.options).enumerate()
        {
            if shown.label != original.label {
                shown.label = format!("Redacted option {}", index + 1);
            }
        }
    }
    Ok(displayed)
}

fn original_response(display: &Prompt, raw: &Prompt, mut response: Response) -> Result<Response> {
    display.validate(&response)?;
    let answers = match &mut response {
        Response::Answer { answers } | Response::Stop { answers } => answers,
        Response::Explain { .. } => return Ok(response),
    };
    for answer in answers {
        let shown = display
            .request
            .questions
            .iter()
            .find(|q| q.id == answer.question_id)
            .ok_or("Unknown repair question.")?;
        let original = raw
            .request
            .questions
            .iter()
            .find(|q| q.id == answer.question_id)
            .ok_or("Unknown original repair question.")?;
        let map = |label: &str| -> Result<String> {
            let index = shown
                .options
                .iter()
                .position(|o| o.label == label)
                .ok_or("Unknown repair option.")?;
            Ok(original.options[index].label.clone())
        };
        if let Some(label) = &mut answer.selected_label {
            *label = map(label)?;
        }
        for label in &mut answer.selected_labels {
            *label = map(label)?;
        }
    }
    raw.validate(&response)?;
    Ok(response)
}

fn request_hash(value: &Value) -> Result<String> {
    Ok(Sha256::digest(serde_json::to_vec(value)?)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub(crate) async fn completed_request(
    journal: &Journal,
    id: Uuid,
    method: &str,
    control: &Value,
) -> Result<bool> {
    let Some(operation) = journal.operation(id).await? else {
        return Ok(false);
    };
    if operation["kind"] != method || operation["payload"]["controlHash"] != request_hash(control)?
    {
        return Err("This operation ID belongs to a different repair action.".into());
    }
    if operation["state"] != "complete" {
        return Err("The repair action outcome is unknown. Recovery is required.".into());
    }
    Ok(true)
}

async fn send(
    journal: &Journal,
    port: &Port,
    scanner: &crate::secret_scan::Scanner,
    id: Uuid,
    method: &str,
    params: Value,
    control: Value,
) -> Result<Option<Value>> {
    let screened = if method == "approval/decide" {
        Some(journal.screen_start(params.clone()).await?)
    } else {
        None
    };
    let saved = json!({"requestHash":request_hash(&params)?,"controlHash":request_hash(&control)?,"request":crate::recording::redact(scanner,params.clone()).await?});
    match journal.intent(&id.to_string(), method, saved).await? {
        Admission::Replay(state) if state == "complete" => return Ok(None),
        Admission::Replay(_) => {
            return Err("The repair command has an unknown outcome. Recovery is required.".into());
        }
        Admission::New => {}
    }
    if port.stopped.load(Ordering::Acquire) {
        journal.finish(&id.to_string(), "superseded").await?;
        return Err("The repair Run is stopping.".into());
    }
    let response = match screened {
        Some(screened) => port.client.decide(screened).await?,
        None => port.client.request(method, params.clone()).await?,
    };
    if response["commandId"] != id.to_string() || response["status"] != "accepted" {
        return Err("The host did not confirm this repair action.".into());
    }
    if method == "approval/decide" && response["approvalId"] != params["approvalId"]
        || method.starts_with("userInput/") && response["userInputId"] != params["userInputId"]
    {
        return Err("The host confirmed a different repair action.".into());
    }
    if method == "approval/decide" && !response["terminal"].is_boolean() {
        return Err("The host omitted the approval terminal state.".into());
    }
    journal.finish(&id.to_string(), "dispatched").await?;
    journal.finish(&id.to_string(), "complete").await?;
    Ok(Some(response))
}

impl Runner {
    pub async fn execute(
        mut self,
        mut hosted: Hosted,
        journal: Journal,
        workspace: &crate::run_workspace::Workspace,
        task: Task,
    ) -> Result<Run> {
        if hosted.run_id() != self.port.run {
            return Err("The control handle belongs to another repair Run.".into());
        }
        let prepared = async {
            let (run, source) = journal.repair_source(hosted.run_id()).await?;
            workspace.validate_source(&source, &run.track)?;
            let scanner = source.settings().scanner()?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>((run, scanner))
        }
        .await;
        let (mut run, scanner) = match prepared {
            Ok(value) => value,
            Err(_) => {
                return hosted
                    .close(
                        &journal,
                        Outcome::Failed {
                            failure: Failure::Preparation,
                        },
                    )
                    .await;
            }
        };
        let host = hosted.host_mut();
        let tracker = host.tracker.clone();
        let result = self
            .execute_inner(
                SessionIo {
                    connection: &mut host.connection,
                    tracker,
                },
                &journal,
                workspace,
                &mut run,
                &scanner,
                task,
            )
            .await;
        // A priority Stop interrupts the host immediately. Finish any private
        // snapshot already being written before this workspace can be resumed.
        while self.policies.join_next().await.is_some() {}
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                self.port.interrupt();
                run.output = Some(
                    safe(&scanner, &error.to_string())
                        .await
                        .unwrap_or_else(|_| "Repair failed; details withheld.".into()),
                );
                let _ = present(&journal, &mut run, &self.port, &HashMap::new()).await;
                Outcome::Failed {
                    failure: Failure::Protocol,
                }
            }
        };
        let result = hosted.close(&journal, outcome).await?;
        if result.phase == Phase::Held
            && !self.port.secret_cancelled()
            && let Some(id) = self.quiesced
        {
            journal.secret_settled(id).await?;
        }
        if matches!(result.phase, Phase::Captured | Phase::Held)
            && let Some(command) = self.submitted
        {
            journal.finish(&command.to_string(), "complete").await?;
        }
        Ok(result)
    }

    async fn execute_inner(
        &mut self,
        io: SessionIo<'_>,
        journal: &Journal,
        workspace: &crate::run_workspace::Workspace,
        run: &mut Run,
        scanner: &crate::secret_scan::Scanner,
        mut task: Task,
    ) -> Result<Outcome> {
        let delivery_track = run.track.clone();
        let delivery_session = self.port.session.clone();
        self.port.client.configure_progress(
            delivery_session.clone(),
            crate::progress::Privacy::Rules,
            run.turn.map(|turn| turn.to_string()),
        );
        let mut inbox = crate::delivery::Inbox::new(crate::history::Context {
            journal,
            track: &delivery_track,
            session: &delivery_session,
            scanner: Some(scanner),
        });
        let permit = task
            .capacity
            .request(run.id.to_string(), crate::scheduler::Priority::User)
            .acquire();
        tokio::pin!(permit);
        let permit = loop {
            tokio::select! {
                biased;
                changed=task.shutdown.changed()=>{if changed.is_err()||*task.shutdown.borrow(){stop(journal,run,&self.port,Uuid::now_v7()).await?;return Ok(Outcome::Stopped);}},
                input=self.receiver.recv()=>match input {
                    Some(Input::Stop(id))=>{stop(journal,run,&self.port,id).await?;return Ok(Outcome::Stopped);},
                    Some(Input::Answer{reply,..})|Some(Input::Respond{reply,..})|Some(Input::Secret{reply,..})=>{let _=reply.send(Err("The repair turn has not started.".into()));},
                    None=>return Err("Repair controls closed.".into()),
                },
                permit=&mut permit=>break permit,
            }
        };
        self.permit = Some(permit);
        if *task.shutdown.borrow() || self.port.stopped.load(Ordering::Acquire) {
            stop(journal, run, &self.port, Uuid::now_v7()).await?;
            return Ok(Outcome::Stopped);
        }
        let command = Uuid::now_v7();
        let project = &journal.snapshot().await?["projectSecrets"];
        if !task.project_secrets.ready(project)
            || (!project.is_null() && project["revision"].as_u64() != run.secret_revision)
        {
            return Ok(Outcome::Reconfigure);
        }
        let instruction = task.instruction.clone();
        let params = json!({"commandId":command,"sessionId":self.port.session,"reasoningEffort":"minimal","input":[{"type":"skill","source":"user","selector":"intercat-report","arguments":instruction}]});
        let prepared = journal.screen_start(params).await?;
        let params = prepared.params().clone();
        if let Err(error) = update(journal, run, Event::ReserveStart { command }).await {
            if error.is::<crate::journal::StartSuperseded>() {
                return Ok(Outcome::Reconfigure);
            }
            return Err(error);
        }
        if journal
            .intent(&command.to_string(), "turn.start", params.clone())
            .await?
            != Admission::New
        {
            return Err("The repair start was already admitted.".into());
        }
        *run = journal.repair_source(run.id).await?.0;
        if run.phase == Phase::Stopping
            || self.port.stopped.load(Ordering::Acquire)
            || *task.shutdown.borrow()
        {
            journal.finish(&command.to_string(), "superseded").await?;
            stop(journal, run, &self.port, Uuid::now_v7()).await?;
            return Ok(Outcome::Stopped);
        }
        let client = self.port.client.clone();
        self.submitted = Some(command);
        let (started_tx, mut started_rx) = oneshot::channel();
        let guard = crate::msp::StartGuard::Stopped(self.port.stopped.clone());
        tokio::spawn(async move {
            let _ = started_tx.send(client.start(prepared, guard).await);
        });
        let mut starting = true;
        let mut queued: Option<(
            crate::start_queue::QueuedStart,
            crate::msp::PendingRequest,
            std::time::Instant,
        )> = None;
        let mut early_started = None;
        let mut early_unqueued = None;
        let mut admitted_turn = None;
        let mut terminals = HashMap::new();
        let mut messages = HashMap::new();
        let mut final_reports = HashSet::new();
        let outcome = async {
        let mut approval_stages = crate::approval::Stages::default();
        let mut approval_queue = std::collections::VecDeque::<Value>::new();
        let mut prompts = HashMap::<String, Prompt>::new();
        let mut settled = HashSet::new();
        let mut timer = tokio::time::interval(Duration::from_millis(200));
        let mut changes = journal.subscribe();
        let mut quiescing: Option<(Uuid, std::time::Instant)> = None;
        loop {
            while let Some(request)=approval_queue.pop_front() {
                if !approval_stages.current(&request) {continue;}
                let params=&request;
                if quiescing.is_some(){self.port.interrupt();continue;}
                let files=workspace.snapshot_workspace(run.id); let owner=run.id;
                let request=params.clone(); let journal=journal.clone();
                self.policies.spawn(async move {
                    let requested=crate::deletion::requested(&request,&files.path);
                    let deletion=if requested {
                        match crate::deletion::prepare(files.clone(),Some(owner),request.clone(),files.base.clone(),journal.clone()).await {
                            Ok(snapshot)=>Some(snapshot),
                            Err(_)=>{journal.append("deletion.failed",json!({"trackId":files.track_id,"repairRunId":owner,"itemId":request["itemId"],"error":"The private deletion snapshot could not be saved."})).await?;None}
                        }
                    }else{None};
                    let source=files.path.clone();
                    let hosts=tokio::task::spawn_blocking(move||intercat_git::Repository::open(source)?.remote_hosts()).await??;
                    let verdict=journal.evaluate_approval(files.track_id,files.path,hosts,request.clone(),deletion.clone()).await?;
                    Ok(PreparedApproval {request,verdict,deletion})
                });
            }
            if let Some(terminal) = run.turn.and_then(|turn| terminals.get(&turn))
                && !starting
                && self.policies.is_empty()
                && (quiescing.is_none() || (prompts.is_empty() && approval_stages.is_empty()))
            {
                if self.port.client.output_flow().interrupted {
                    return Err("output too fast to record".into());
                }
                if terminal == "completed" && !final_reports.contains(&run.turn.unwrap()) {
                    return Err("The repair ended without a final report.".into());
                }
                self.quiesced = quiescing.map(|(id,_)|id);
                self.permit = None;
                return Ok(
                    if self.port.stopped.load(Ordering::Acquire)
                        || terminal != "completed"
                        || !prompts.is_empty()
                        || !run.approvals.is_empty()
                        || run.output.as_ref().is_some_and(|text| {
                            text.lines()
                                .any(|line| line.trim_start().starts_with("Waiting for you:"))
                        })
                    {
                        Outcome::Stopped
                    } else {
                        Outcome::Completed
                    },
                );
            }
            tokio::select! {
                biased;
                changed=task.shutdown.changed()=>{if changed.is_err()||*task.shutdown.borrow(){stop(journal,run,&self.port,Uuid::now_v7()).await?;return Ok(Outcome::Stopped);}},
                input=self.receiver.recv()=>match input {
                    Some(Input::Stop(id))=>{stop(journal,run,&self.port,id).await?;return Ok(Outcome::Stopped);},
                    Some(Input::Secret{target,reply})=>{
                        let current=journal.repair_source(run.id).await?.0;
                        let matches=match &target.question {
                            crate::secret_input::Question::Msp{id,revision} => prompts.get(id).is_some_and(|p|p.revision==*revision && p.request.turn_id==target.turn_id.to_string())
                                && current.prompts.iter().any(|p|p.request.user_input_id==*id&&p.revision==*revision&&p.phase==PromptPhase::Open),
                            _=>false,
                        };
                        if !matches || current.secret_input!=Some(target.op_id) || current.turn!=Some(target.turn_id) || current.phase!=Phase::Running || self.port.stopped.load(Ordering::Acquire) {
                            let _=reply.send(Err("The repair secret question changed.".into()));
                        } else {
                            quiescing=Some((target.op_id,std::time::Instant::now()));
                            self.port.interrupt();
                            let _=reply.send(Ok(()));
                        }
                    },
                    Some(Input::Answer{id,approval,requirement,response,reply})=>{
                        let scope=crate::secret_input::Owner::Repair{run_id:run.id}.key(&run.track);
                        let control=json!({"scope":scope,"approval":approval,"requirement":requirement,"response":response});
                        let mut sent=false;
                        let result=async {
                            let replay=if journal.operation(id).await?.is_some_and(|saved|saved["kind"]=="approval.reply") {crate::approval::replay(journal,id,&scope,&approval,&requirement,&response).await} else {completed_request(journal,id,"approval/decide",&control).await};
                            if replay.is_err(){sent=true;}
                            if replay? {return Ok(());}
                            if self.port.stopped.load(Ordering::Acquire){return Err("The repair Run is stopping.".into());}
                            let request=approval_stages.get(&approval).filter(|r|r["currentRequirementId"]==requirement).ok_or("The repair approval changed.")?;
                            let card=run.approvals.iter().find(|card|card.id==approval&&card.requirement==requirement).ok_or("The repair approval is not open.")?;
                            if matches!(response,crate::approval::Response::Allow) && !card.can_allow
                                || matches!(response,crate::approval::Response::Deny) && !card.can_deny {return Err("This repair action is unavailable.".into());}
                            let (choice,feedback)=match response.prepare(request)? {
                                crate::approval::Prepared::Decision{choice,feedback}=>(choice,feedback),
                                crate::approval::Prepared::Restart{text}=>{
                                    let request=crate::approval_reply::Request{op_id:id,owner:crate::secret_input::Owner::Repair{run_id:run.id},track_id:run.track.clone(),session_id:self.port.session.clone(),turn_id:run.turn.ok_or("The approval turn has already ended.")?,approval_id:approval.clone(),requirement:requirement.clone(),text};
                                    self.port.stop(request.stop_id())?;
                                    journal.queue_approval_reply(request).await?;
                                    return Ok(());
                                }
                            };
                            sent=true;
                            if !approval_stages.begin(&approval,&requirement) {return Err("The repair approval was already answered.".into());}
                            let ack=send(journal,&self.port,scanner,id,"approval/decide",json!({"commandId":id,"sessionId":self.port.session,"approvalId":approval,"requirementId":requirement,"choiceId":choice,"feedback":feedback}),control).await?.ok_or("Missing new repair approval acknowledgement")?;
                            if let Some(request)=approval_stages.acknowledged(&approval,ack["terminal"]==true)? {approval_queue.push_back(request);}
                            run.approvals.retain(|card|card.id!=approval);
                            present(journal,run,&self.port,&prompts).await
                        }.await;
                        let uncertain=sent&&result.is_err();
                        let _=reply.send(result);
                        if uncertain{return Err("The repair approval outcome is unknown. Recovery is required.".into());}
                    },
                    Some(Input::Respond{id,prompt,revision,response,sent,reply})=>{
                        if !sent && self.port.stopped.load(Ordering::Acquire) {let _=reply.send(Err("The repair is stopping.".into()));continue;}
                        if sent && !prompts.contains_key(&prompt) {
                            let result=async {journal.append("repair.promptInterrupted",json!({"commandId":id,"sessionId":self.port.session,"userInputId":prompt})).await?;record_stop(journal,run,id).await}.await;
                            let _=reply.send(result);return Ok(Outcome::Stopped);
                        }
                        let control=json!({"prompt":prompt,"revision":revision,"response":response});
                        let method=match &response {Response::Answer{..}=>"userInput/answer",Response::Explain{..}=>"userInput/clarify",Response::Stop{..}=>"userInput/interrupt"};
                        let mut admitted=false;
                        let result=async {
                            if !sent {
                                let replay=completed_request(journal,id,method,&control).await;
                                if replay.is_err(){admitted=true;}
                                if replay? {return Ok(());}
                            }
                            let raw=prompts.get(&prompt).filter(|p|p.revision==revision).ok_or("The repair question changed.")?;
                            let displayed=run.prompts.iter().find(|p|p.request.user_input_id==prompt&&p.revision==revision).ok_or("The repair question is not open.")?;
                            if !sent&&displayed.phase!=PromptPhase::Open {return Err("This repair question is no longer accepting answers.".into());}
                            let response=original_response(displayed,raw,response)?;
                            let (method,params)=raw.command(id,&response)?;
                            admitted=true;
                            if sent {journal.append("repair.promptInterrupted",crate::recording::redact(scanner,params).await?).await?;record_stop(journal,run,id).await?;}
                            else {send(journal,&self.port,scanner,id,method,params,control).await?;if let Some(p)=run.prompts.iter_mut().find(|p|p.request.user_input_id==prompt){p.phase=PromptPhase::AwaitingSettlement;p.operation=Some(id);}present(journal,run,&self.port,&prompts).await?;}
                            Ok(())
                        }.await;
                        let uncertain=admitted&&result.is_err();
                        let _=reply.send(result);
                        if uncertain{return Err("The repair answer outcome is unknown. Recovery is required.".into());}
                        if self.port.stopped.load(Ordering::Acquire){return Ok(Outcome::Stopped);}
                    },
                    None=>return Err("Repair controls closed.".into()),
                },
                started=&mut started_rx,if starting=>{
                    starting=false;
                    let result=started?;
                    if result.as_ref().is_err_and(|error|error.is::<crate::msp::StartCancelled>() || crate::msp::rejected_start_code(error.as_ref()).is_some()) {
                        journal.finish(&command.to_string(),"superseded").await?;
                        self.submitted=None;self.permit=None;
                        run.output=Some("The repair request could not start. Review it and try again.".into());
                        present(journal,run,&self.port,&prompts).await?;
                        return Ok(Outcome::Stopped);
                    }
                    let result=result?;
                    let turn:Uuid=result["turnId"].as_str().ok_or("Missing repair turn ID.")?.parse()?;
                    if result["commandId"]!=command.to_string() {return Err("The repair start reply names another request.".into());}
                    match result["disposition"].as_str() {
                        Some("queued")=>{
                            let cancel=Uuid::new_v5(&command,b"unqueue-before-launch");
                            let params=json!({"commandId":cancel,"sessionId":self.port.session,"turnId":turn});
                            let wire=self.port.client.query("turn/unqueue",params.clone()).await?;
                            let mut facts=crate::start_queue::QueuedStart::new(self.port.session.clone(),command,turn,cancel);
                            if let Some(event)=&early_started {facts.observe("turn/started",event)?;}
                            if let Some(event)=&early_unqueued {facts.observe("turn/unqueued",event)?;}
                            if terminals.contains_key(&turn) {facts.observe("turn/completed",&json!({"sessionId":self.port.session,"turnId":turn}))?;}
                            journal.finish(&command.to_string(),"dispatched").await?;
                            let mut intent=params;intent["reclaimedStart"]=json!(command);
                            journal.intent(&cancel.to_string(),"turn.unqueue",intent).await?;
                            facts.logged();
                            queued=Some((facts,wire,std::time::Instant::now()));
                        },
                        Some("started"|"steered")=>{
                            if result["disposition"]=="steered" {journal.append("repair.startSteered",json!({"runId":run.id,"commandId":command,"turnId":turn})).await?;}
                            admitted_turn=Some(turn);
                        },
                        _=>return Err("The repair start outcome is unknown. Reconcile before continuing.".into()),
                    }
                },
                prepared=self.policies.join_next(),if !self.policies.is_empty()=>{
                    let PreparedApproval {request,verdict,deletion}=prepared.ok_or("Missing repair policy task.")???;
                    let id=request["approvalId"].as_str().ok_or("Missing approval ID.")?.to_owned();
                    if self.port.stopped.load(Ordering::Acquire){if quiescing.is_some(){continue;}return Ok(Outcome::Stopped);}
                    if !approval_stages.current(&request){continue;}
                    let requested=crate::deletion::requested(&request,&workspace.checkout().workdir);
                    let missing=requested&&deletion.is_none();
                    let review=if missing {Some(crate::policy::Reason{kind:"delete".into(),message:"Deletion snapshot could not be saved. Stop this repair and retry after fixing the workspace.".into()})}else{verdict.review.or_else(||crate::approval::offered(&request,true).is_none().then(||crate::policy::Reason{kind:"unknown".into(),message:"Review this repair action.".into()}))};
                    if let Some(reason)=review {
                        let subject=safe(scanner,&request["subject"].to_string()).await?;
                        run.approvals.retain(|card|card.id!=id);
                        run.approvals.push(Approval{id:id.clone(),requirement:request["currentRequirementId"].clone(),subject,reason,can_allow:!missing&&verdict.allow_once&&crate::approval::offered(&request,true).is_some(),can_deny:crate::approval::offered(&request,false).is_some(),reply_mode:Some(crate::approval::reply_mode(&request)),deletion:deletion.as_ref().map(|s|s.summary())});
                        present(journal,run,&self.port,&prompts).await?;
                    } else if let Some(choice)=crate::approval::offered(&request,true)
                        && approval_stages.begin(&id,&request["currentRequirementId"]) {
                        let op=Uuid::now_v7();let ack=send(journal,&self.port,scanner,op,"approval/decide",json!({"commandId":op,"sessionId":self.port.session,"approvalId":id,"requirementId":request["currentRequirementId"],"choiceId":choice}),Value::Null).await?.ok_or("Missing new repair approval acknowledgement")?;
                        if let Some(request)=approval_stages.acknowledged(&id,ack["terminal"]==true)? {approval_queue.push_back(request);}
                    }
                },
                changed=changes.changed()=>{
                    if changed.is_err(){return Err("The repair journal closed.".into());}
                    let current=journal.repair_source(run.id).await?.0;
                    if current.phase==Phase::Stopping {self.port.interrupt();*run=current;return Ok(Outcome::Stopped);}
                },
                _=timer.tick()=>{
                    if queued.as_ref().is_some_and(|(_,_,began)|began.elapsed()>Duration::from_secs(30)) {return Err("The repair reclaim outcome is unknown. Recovery is required.".into());}
                    if quiescing.is_some_and(|(_,began)|began.elapsed()>Duration::from_secs(30)) {return Err("The repair did not confirm its stop. Re-enter the secret after recovery.".into());}
                    let tracker=io.tracker.clone();tokio::task::spawn_blocking(move||tracker.lock().unwrap().refresh()).await??;
                },
                received=inbox.next(io.connection)=>{
                    let received=received?;
                    let input_engaged=received.input_engaged;
                    let frame=received.frame;
                    let params=&frame["params"];
                    let display=match received.recorded_display.as_deref() {Some(display)=>display.clone(),None=>crate::recording::redact(scanner,frame.clone()).await?};
                    self.port.client.scanned_progress(&display);
                    journal.try_msp(run.track.clone(),self.port.session.clone(),received.stamp,display,Some(received.lease), received.source)?.await??;
                    if params["sessionId"]==self.port.session && params["commandId"]==command.to_string() {
                        match frame["method"].as_str() {
                            Some("turn/started")=>early_started=Some(params.clone()),
                            Some("turn/unqueued")=>early_unqueued=Some(params.clone()),
                            _=>{},
                        }
                    }
                    if let Some((facts,wire,_))=queued.as_mut() {
                        if frame.get("method").is_none() && frame["id"].as_u64()==Some(wire.id) {facts.reply(&frame)?;}
                        else if let Some(method)=frame["method"].as_str() {facts.observe(method,params)?;}
                    }
                    if params["sessionId"] == self.port.session && frame["method"] == "view/gap" {
                        let head=params["next"].as_str().ok_or("Repair delivery gap has no next cursor.")?;
                        inbox.flush(io.connection).await?;
                        crate::history::recover(io.connection, crate::history::Context{journal,track:&run.track,session:&self.port.session,scanner:Some(scanner)},head,run.turn).await?;
                        continue;
                    }
                    if params["sessionId"] == self.port.session && frame["method"] == "session/viewHealthChanged" {
                        return Err("The repair history is unavailable. Reconnect before sending work.".into());
                    }
                    if params["sessionId"].as_str().is_some_and(|s|s!=self.port.session){continue;}
                    match frame["method"].as_str() {
                        Some("turn/completed")=>{
                            let turn:Uuid=params["turnId"].as_str().ok_or("Missing completed repair turn.")?.parse()?; terminals.insert(turn,params["terminal"].as_str().unwrap_or("unknown").to_owned());
                        },
                        Some("item/completed") if params["item"]["kind"]=="agentMessage"=>{
                            if let (Some(turn),Some(text))=(params["item"]["turnId"].as_str(),params["item"]["text"].as_str()) {let turn:Uuid=turn.parse()?;if params["item"]["messagePhase"]=="finalAnswer" {final_reports.insert(turn);}let text=safe(scanner,text).await?;if run.turn==Some(turn){run.output=Some(text);present(journal,run,&self.port,&prompts).await?;}else if starting || queued.is_some() {messages.insert(turn,text);}}
                        },
                        Some("approval/request"|"approval/requested"|"approval/updated"|"approval/resolved")=>{
                            if let Some(request)=approval_stages.observe(frame["method"].as_str().unwrap(),params)? {approval_queue.push_back(request);}
                            if let Some(id)=params["approvalId"].as_str() && approval_stages.is_resolved(id) {
                                run.approvals.retain(|card|card.id!=id);present(journal,run,&self.port,&prompts).await?;
                            }
                            if approval_stages.len()>32 {return Err("Too many open repair approvals.".into());}
                        },
                        Some("userInput/request"|"userInput/requested")=>{
                            let prompt=Prompt::parse(params.clone())?;
                            if settled.contains(&prompt.request.user_input_id){continue;}
                            if !input_engaged {self.port.client.engage_input(&frame)?;}
                            if prompts.get(&prompt.request.user_input_id).is_some_and(|old|old.revision==prompt.revision){continue;}
                            let visible=visible_prompt(scanner,&prompt).await?;
                            run.prompts.retain(|p|p.request.user_input_id!=prompt.request.user_input_id);
                            run.prompts.push(visible);
                            prompts.insert(prompt.request.user_input_id.clone(),prompt);
                            present(journal,run,&self.port,&prompts).await?;
                            if quiescing.is_some(){self.port.interrupt();}
                        },
                        Some("userInput/settled")=>{
                            if let Some(id)=params["userInputId"].as_str(){settled.insert(id.to_owned());prompts.remove(id);run.prompts.retain(|p|p.request.user_input_id!=id);present(journal,run,&self.port,&prompts).await?;}
                        },
                        _=>{},
                    }
                },
            }
            if let Some(outcome)=queued.as_ref().map(|(facts,_,_)|facts.outcome()).transpose()?.flatten() {
                let (facts,_,_)=queued.take().unwrap();
                match outcome {
                    crate::start_queue::Outcome::Reclaimed=>{
                        journal.finish(&facts.command.to_string(),"dispatched").await?;
                        journal.finish(&facts.command.to_string(),"complete").await?;
                        journal.finish(&command.to_string(),"complete").await?;
                        self.submitted=None;self.permit=None;
                        run.output=Some("The repair request was held before it started.".into());
                        present(journal,run,&self.port,&prompts).await?;
                        return Ok(Outcome::Stopped);
                    },
                    crate::start_queue::Outcome::Running=>{
                        journal.finish(&facts.command.to_string(),"superseded").await?;
                        admitted_turn=Some(facts.turn);
                    },
                }
            }
            if let Some(turn)=admitted_turn.take() {
                if let Some(permit)=self.permit.as_mut() {permit.bind_turn(&run.id.to_string(),turn)?;}
                update(journal,run,Event::Started{command,turn}).await?;
                if let Some(message)=messages.remove(&turn) {run.output=Some(message);present(journal,run,&self.port,&prompts).await?;}
                journal.finish(&command.to_string(),"dispatched").await?;
                if self.port.stopped.load(Ordering::Acquire){self.port.interrupt();if quiescing.is_none(){return Ok(Outcome::Stopped);}}
            }
        }
        }.await;
        inbox.flush(io.connection).await?;
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    fn test_port(client: Client, session: &str) -> Port {
        let (sender, _) = mpsc::unbounded_channel();
        Port {
            run: Uuid::now_v7(),
            track: "test-track".into(),
            session: session.into(),
            client,
            stopped: Arc::new(AtomicBool::new(false)),
            cancelled: Arc::new(AtomicBool::new(false)),
            prompts: Arc::new(Mutex::new(vec![])),
            shown_prompts: Arc::new(Mutex::new(vec![])),
            sender,
        }
    }

    #[tokio::test]
    async fn repair_stop_uses_its_priority_pipe_without_waiting_for_a_journal() -> Result<()> {
        let (one, server_one) = tokio::io::duplex(8192);
        let (two, server_two) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(one);
        let connection = crate::msp::Connection::new(read, write);
        let (read, write) = tokio::io::split(two);
        let other = crate::msp::Connection::new(read, write);
        let port = test_port(connection.client.clone(), "repair-session");
        let _other = test_port(other.client.clone(), "main-session");
        // Dropping the control consumer cannot prevent the priority interrupt.
        assert!(port.stop(Uuid::now_v7()).is_err());
        let (read, _write) = tokio::io::split(server_one);
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut line)).await??;
        let request: Value = serde_json::from_str(&line)?;
        assert_eq!(request["method"], "turn/interrupt");
        assert_eq!(request["params"]["sessionId"], "repair-session");
        let (read, _write) = tokio::io::split(server_two);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(100),
                BufReader::new(read).read_line(&mut String::new())
            )
            .await
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn repair_approval_requires_an_explicit_terminal_ack() -> Result<()> {
        for terminal in [None, Some(false)] {
            let root = tempfile::tempdir()?;
            let journal =
                Journal::open(root.path().join("db"), crate::secrets::Key::new([7; 32])).await?;
            let scanner = crate::secret_scan::Engine::capture()?.start(&[])?;
            let (client, server) = tokio::io::duplex(16384);
            let (read, write) = tokio::io::split(client);
            let connection = crate::msp::Connection::new(read, write);
            let port = test_port(connection.client.clone(), "repair");
            let host = tokio::spawn(async move {
                let (read, mut write) = tokio::io::split(server);
                let mut line = String::new();
                BufReader::new(read).read_line(&mut line).await.unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let mut result = json!({"commandId":request["params"]["commandId"],"approvalId":"approval","status":"accepted"});
                if let Some(value) = terminal {
                    result["terminal"] = json!(value);
                }
                write
                    .write_all(
                        format!(
                            "{}\n",
                            json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            });
            let id = Uuid::now_v7();
            let result = send(&journal,&port,&scanner,id,"approval/decide",
                json!({"commandId":id,"sessionId":"repair","approvalId":"approval","choiceId":"abort"}),Value::Null).await;
            host.await?;
            let operation = journal.operation(id).await?.unwrap();
            if terminal.is_some() {
                assert_eq!(result?.unwrap()["terminal"], false);
                assert_eq!(operation["state"], "complete");
            } else {
                assert!(result.unwrap_err().to_string().contains("terminal"));
                assert_eq!(operation["state"], "prepared");
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn repair_action_replay_keeps_secrets_out_of_the_journal_and_sends_once() -> Result<()> {
        let root = tempfile::tempdir()?;
        let journal = Journal::open(
            root.path().join("events.sqlite"),
            crate::secrets::Key::new([7; 32]),
        )
        .await?;
        let scanner = crate::secret_scan::Engine::capture()?.start(&[])?;
        let (client, server) = tokio::io::duplex(16384);
        let (read, write) = tokio::io::split(client);
        let connection = crate::msp::Connection::new(read, write);
        let port = test_port(connection.client.clone(), "repair");
        let server = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut reader = BufReader::new(read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let response = json!({"jsonrpc":"2.0","id":request["id"],"result":{"commandId":request["params"]["commandId"],"status":"accepted","userInputId":"prompt"}});
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
            request
        });
        let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
        let id = Uuid::now_v7();
        let params = json!({"commandId":id,"sessionId":"repair","userInputId":"prompt","clarification":{"format":"text","content":token}});
        send(
            &journal,
            &port,
            &scanner,
            id,
            "userInput/clarify",
            params.clone(),
            params.clone(),
        )
        .await?;
        assert_eq!(server.await?["params"]["clarification"]["content"], token);
        send(
            &journal,
            &port,
            &scanner,
            id,
            "userInput/clarify",
            params.clone(),
            params.clone(),
        )
        .await?;
        let saved = journal.operation(id).await?.unwrap();
        assert_eq!(saved["state"], "complete");
        assert!(!saved.to_string().contains(token));
        assert!(completed_request(&journal, id, "userInput/clarify", &params).await?);
        assert!(
            completed_request(&journal, id, "approval/decide", &params)
                .await
                .is_err()
        );
        let mut changed = params;
        changed["clarification"]["content"] = json!(format!("{token} changed"));
        assert!(
            send(
                &journal,
                &port,
                &scanner,
                id,
                "userInput/clarify",
                changed.clone(),
                changed
            )
            .await
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn repair_question_stop_sends_selected_raw_option_before_control_consumption()
    -> Result<()> {
        let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
        let raw = Prompt::parse(
            json!({"sessionId":"repair","userInputId":"prompt","turnId":"turn","itemId":"item","toolCallId":"call","toolName":"request_user_input","viewCursor":"1","questions":[{"id":"choice","header":"Choose","question":"Choose","selection":{"mode":"single"},"options":[{"label":token},{"label":"Keep"}]}]}),
        )?;
        let scanner = crate::secret_scan::Engine::capture()?.start(&[])?;
        let shown = visible_prompt(&scanner, &raw).await?;
        let (client, server) = tokio::io::duplex(8192);
        let (read, write) = tokio::io::split(client);
        let connection = crate::msp::Connection::new(read, write);
        let port = test_port(connection.client.clone(), "repair");
        *port.prompts.lock().unwrap() = vec![raw.clone()];
        *port.shown_prompts.lock().unwrap() = vec![shown.clone()];
        let response: Response = serde_json::from_value(
            json!({"action":"stop","answers":[{"questionId":"choice","selectedLabel":shown.request.questions[0].options[0].label}]}),
        )?;
        assert!(
            port.respond(Uuid::now_v7(), "prompt".into(), raw.revision, response)
                .await
                .is_err()
        );
        let (read, _write) = tokio::io::split(server);
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut line)).await??;
        let request: Value = serde_json::from_str(&line)?;
        assert_eq!(request["method"], "userInput/interrupt");
        assert_eq!(request["params"]["answers"][0]["selectedLabel"], token);
        line.clear();
        reader.read_line(&mut line).await?;
        assert_eq!(
            serde_json::from_str::<Value>(&line)?["method"],
            "turn/interrupt"
        );
        Ok(())
    }

    #[tokio::test]
    async fn repair_question_redacts_labels_but_maps_the_selected_option_back() -> Result<()> {
        let token = concat!("ghp_", "A6k9P2mR7x4T1v8B5n0Q3s6Z9c2Y7w4L8d1H");
        let raw = Prompt::parse(
            json!({"sessionId":"session","userInputId":"prompt","turnId":"turn","itemId":"item","toolCallId":"call","toolName":"request_user_input","viewCursor":"1","questions":[{"id":"choice","header":"Choose","question":format!("Remove {token}?"),"selection":{"mode":"single"},"options":[{"label":token},{"label":"Keep"}]}]}),
        )?;
        let scanner = crate::secret_scan::Engine::capture()?.start(&[])?;
        let shown = visible_prompt(&scanner, &raw).await?;
        assert!(!serde_json::to_string(&shown)?.contains(token));
        assert_eq!(shown.revision, raw.revision);
        let response: Response = serde_json::from_value(
            json!({"action":"answer","answers":[{"questionId":"choice","selectedLabel":shown.request.questions[0].options[0].label}]}),
        )?;
        let original = original_response(&shown, &raw, response)?;
        assert_eq!(
            serde_json::to_value(original)?["answers"][0]["selectedLabel"],
            token
        );
        Ok(())
    }
}
