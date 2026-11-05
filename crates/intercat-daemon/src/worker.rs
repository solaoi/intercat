//! Diagnostic one-turn runner. The reducer stays responsive while SQLite and MSP wait.

use crate::{
    Result,
    host::Host,
    journal::{Admission, Journal},
    msp::{Client, Event},
    process::Scan,
    recording,
    run::{Effect, Run},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Duration,
};
use tokio::sync::mpsc;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeRequest {
    pub repository: std::path::PathBuf,
    pub instruction: String,
    #[serde(default)]
    pub allowed_shell_commands: Vec<String>,
}

#[derive(Serialize)]
pub struct Report {
    pub session_id: String,
    pub turn_id: String,
    pub terminal: String,
    pub messages: Vec<String>,
    pub observed_processes: Vec<crate::process::Process>,
}

#[derive(Serialize)]
pub struct StopVerification {
    target: crate::process::Identity,
    stopped: bool,
    others_preserved: bool,
    other_processes: usize,
}

pub async fn verify_isolated_stop(
    host: &Host,
    report: &Report,
) -> Result<Option<StopVerification>> {
    let mut candidates: Vec<_> = report
        .observed_processes
        .iter()
        .map(|p| p.identity)
        .filter(|id| *id != host.identity && crate::process::is_alive(*id))
        .collect();
    candidates.sort_by_key(|id| id.pid);
    let Some(target) = candidates.first().copied() else {
        return Ok(None);
    };
    let others = &candidates[1..];
    let tracker = host.tracker.clone();
    tokio::task::spawn_blocking(move || tracker.lock().unwrap().terminate(target, false)).await??;
    let until = tokio::time::Instant::now() + Duration::from_secs(3);
    while crate::process::is_alive(target) && tokio::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stopped = !crate::process::is_alive(target);
    let others_preserved = others.iter().all(|id| crate::process::is_alive(*id));
    if !stopped || !others_preserved {
        return Err(std::io::Error::other("isolated process stop verification failed").into());
    }
    Ok(Some(StopVerification {
        target,
        stopped,
        others_preserved,
        other_processes: others.len(),
    }))
}

enum Work {
    StartSaved(Result<Admission>),
    Started(Result<Value>),
    Recorded(Result<i64>),
    ApprovalSaved {
        result: Result<Admission>,
        params: Value,
    },
    CommandFinished {
        id: String,
        persisted: bool,
        result: Result<Value>,
    },
    Committed(Result<()>),
    Scanned(std::io::Result<Scan>),
}

fn completion(
    tx: &mpsc::UnboundedSender<Work>,
    journal: &Journal,
    kind: &str,
    frame: Value,
    lease: Option<crate::output_flow::Lease>,
) -> Result<()> {
    let receipt = journal.try_append(kind, recording::sanitize(frame, &[]))?;
    let tx = tx.clone();
    tokio::spawn(async move {
        let result = match receipt.await {
            Ok(result) => result,
            Err(error) => Err(error.into()),
        };
        drop(lease);
        let _ = tx.send(Work::Recorded(result));
    });
    Ok(())
}

fn send_command(
    tx: &mpsc::UnboundedSender<Work>,
    client: &Client,
    method: &'static str,
    params: Value,
    persisted: bool,
) {
    let client = client.clone();
    let tx = tx.clone();
    tokio::spawn(async move {
        let id = params["commandId"].as_str().unwrap().to_owned();
        let result = client.request(method, params).await;
        let _ = tx.send(Work::CommandFinished {
            id,
            persisted,
            result,
        });
    });
}

/// This probe only auto-allows exact fixture commands inside the declared workspace.
fn allow_fixture(params: &Value, workspace: &Path, allowed: &[String]) -> bool {
    let subject = &params["subject"];
    if subject["kind"] != "shell" || params["protectedWrite"] == true {
        return false;
    }
    let Some(command) = subject["command"].as_str() else {
        return false;
    };
    if !allowed.iter().any(|candidate| candidate == command) {
        return false;
    }
    if subject["workspaceRoot"].as_str() != workspace.to_str() {
        return false;
    }
    let Ok(arguments) = serde_json::from_str::<Value>(params["rawArgs"].as_str().unwrap_or(""))
    else {
        return false;
    };
    arguments.as_object().is_some_and(|args| {
        args.keys().all(|key| {
            [
                "command",
                "description",
                "timeout_ms",
                "timeout",
                "yield_time_ms",
            ]
            .contains(&key.as_str())
        })
    })
}

pub async fn execute_once(
    host: &mut Host,
    journal: &Journal,
    workspace: &Path,
    session: &str,
    request: ProbeRequest,
) -> Result<Report> {
    let result = execute_inner(host, journal, workspace, session, request).await;
    if result.is_err() {
        // Stopping must not wait for a failed journal or a slow output consumer.
        let _ = host.connection.client.interrupt_now(session, None);
    }
    result
}

async fn execute_inner(
    host: &mut Host,
    journal: &Journal,
    workspace: &Path,
    session: &str,
    request: ProbeRequest,
) -> Result<Report> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let client = host.connection.client.clone();
    let start_id = Uuid::now_v7();
    let mut run = Run::new();
    let params = json!({"commandId":start_id,"sessionId":session,"reasoningEffort":"minimal","input":[{"type":"skill","source":"user","selector":"intercat-report","arguments":request.instruction}]});
    assert_eq!(
        run.request_start(start_id),
        Some(Effect::PersistStart(start_id))
    );
    let start_params = params.clone();
    let writer = journal.clone();
    let writer_tx = tx.clone();
    tokio::spawn(async move {
        let _ = writer_tx.send(Work::StartSaved(
            writer
                .intent(&start_id.to_string(), "turn.start", start_params)
                .await,
        ));
    });
    let mut pending_records = 0usize;
    let mut pending_commands = 0usize;
    let mut pending_commits = 0usize;
    let mut turn_id = None;
    let mut terminals = HashMap::new();
    let mut messages = Vec::new();
    let mut approvals = HashSet::new();
    let mut stopping = false;
    let mut tracker_busy = false;
    let mut observed = HashMap::new();
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    let deadline = tokio::time::sleep(Duration::from_secs(120));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => {
                if let Some(id) = turn_id { let _ = client.request("turn/interrupt", json!({"commandId":Uuid::now_v7(),"sessionId":session,"turnId":id})).await; }
                return Err(std::io::Error::new(std::io::ErrorKind::TimedOut,"diagnostic turn exceeded 120 seconds").into());
            }
            _ = tokio::signal::ctrl_c(), if !stopping => {
                stopping = true;
                if let Some(Effect::Interrupt(id)) = run.stop() {
                    let interrupt = json!({"commandId":Uuid::now_v7(),"sessionId":session,"turnId":id});
                    send_command(&tx, &client, "turn/interrupt", interrupt, false);
                    pending_commands += 1;
                }
            }
            _ = interval.tick(), if !tracker_busy => {
                let current = host.tracker.clone(); let scan_tx = tx.clone(); tracker_busy = true;
                tokio::task::spawn_blocking(move || { let scan = current.lock().unwrap().refresh(); let _ = scan_tx.send(Work::Scanned(scan)); });
            }
            work = rx.recv() => match work.expect("sender lives for this loop") {
                Work::StartSaved(result) => {
                    if result? != Admission::New { return Err(std::io::Error::other("diagnostic operation was already accepted").into()); }
                    match run.start_persisted(start_id) {
                        Some(Effect::SendStart(_)) => {
                            let client = client.clone(); let start_tx = tx.clone(); let params = params.clone();
                            tokio::spawn(async move { let _ = start_tx.send(Work::Started(client.request("turn/start",params).await)); });
                        }
                        _ => { journal.finish(&start_id.to_string(),"superseded").await?; return Err(std::io::Error::other("turn stopped before dispatch").into()); }
                    }
                }
                Work::Started(result) => {
                    let result = result?;
                    let id: Uuid = result["turnId"].as_str().ok_or_else(|| std::io::Error::other("missing turnId"))?.parse()?;
                    if result["disposition"] != "started" {
                        let _ = client.request("turn/unqueue",json!({"commandId":Uuid::now_v7(),"sessionId":session,"turnId":id})).await?;
                        return Err(std::io::Error::other("diagnostic turn was queued; reclaimed without replay").into());
                    }
                    turn_id = Some(id);
                    if let Some(Effect::Interrupt(id)) = run.start_acknowledged(start_id,id) {
                        send_command(&tx,&client,"turn/interrupt",json!({"commandId":Uuid::now_v7(),"sessionId":session,"turnId":id}),false); pending_commands += 1;
                    }
                    let writer = journal.clone(); let commit_tx = tx.clone(); pending_commits += 1;
                    tokio::spawn(async move { let result = writer.finish(&start_id.to_string(),"dispatched").await; let _ = commit_tx.send(Work::Committed(result)); });
                }
                Work::Recorded(result) => { pending_records -= 1; result?; }
                Work::ApprovalSaved { result, params } => {
                    result?;
                    if !stopping { send_command(&tx,&client,"approval/decide",params,true); pending_commands += 1; }
                    else {
                        let writer = journal.clone(); let commit_tx = tx.clone(); pending_commits += 1;
                        tokio::spawn(async move { let result = writer.finish(params["commandId"].as_str().unwrap(),"superseded").await; let _ = commit_tx.send(Work::Committed(result)); });
                    }
                }
                Work::CommandFinished { id, persisted, result } => {
                    result?; pending_commands -= 1;
                    if persisted {
                        let writer = journal.clone(); let commit_tx = tx.clone(); pending_commits += 1;
                        tokio::spawn(async move { let result = async { writer.finish(&id,"dispatched").await?; writer.finish(&id,"complete").await }.await; let _ = commit_tx.send(Work::Committed(result)); });
                    } else { completion(&tx,journal,"stop.sent",json!({"commandId":id}),None)?; pending_records += 1; }
                }
                Work::Committed(result) => { pending_commits -= 1; result?; }
                Work::Scanned(scan) => {
                    for process in scan?.processes { observed.insert(process.identity, process); }
                    tracker_busy = false;
                }
            },
            event = host.connection.next_event() => {
                let Some(Event::Frame(received)) = event else { return Err(std::io::Error::other("host disconnected during turn; do not retry automatically").into()); };
                let frame = received.frame;
                completion(&tx,journal,"msp.received",frame.clone(),Some(received.lease))?; pending_records += 1;
                let event = &frame["params"];
                match frame["method"].as_str() {
                    Some("turn/completed") if event["sessionId"] == session => {
                        if let Some(id) = event["turnId"].as_str() { terminals.insert(id.to_owned(),event["terminal"].as_str().unwrap_or("unknown").to_owned()); }
                    }
                    Some("item/completed") if event["item"]["kind"] == "agentMessage" => {
                        if let Some(text) = event["item"]["text"].as_str() { messages.push(text.to_owned()); }
                    }
                    Some("approval/request") => {
                        let key = format!("{}:{}",event["approvalId"],event["currentRequirementId"]);
                        if approvals.insert(key) {
                            let allowed = !stopping && allow_fixture(event,workspace,&request.allowed_shell_commands);
                            let choice = if allowed { "allow_once" } else { "abort" };
                            if !event["availableChoices"].as_array().is_some_and(|values| values.iter().any(|v| v["choiceId"] == choice)) { return Err(std::io::Error::other("required fixture approval choice missing").into()); }
                            let id = Uuid::now_v7().to_string();
                            let answer = json!({"commandId":id,"sessionId":session,"approvalId":event["approvalId"],"requirementId":event["currentRequirementId"],"choiceId":choice});
                            if allowed {
                                let journal = journal.clone(); let approval_tx = tx.clone();
                                tokio::spawn(async move { let result = journal.intent(&id,"approval.decide",answer.clone()).await; let _ = approval_tx.send(Work::ApprovalSaved {result,params:answer}); });
                            } else {
                                send_command(&tx,&client,"approval/decide",answer,false); pending_commands += 1;
                            }
                        }
                    }
                    Some("userInput/request") => {
                        client.notify("userInput/engaged",json!({"sessionId":session,"requestId":event["requestId"]})).await?;
                        return Err(std::io::Error::other("worker requested user input; diagnostic stopped").into());
                    }
                    _ => {},
                }
            }
        }
        if let Some(id) = turn_id
            && let Some(terminal) = terminals.get(&id.to_string())
            && pending_records == 0
            && pending_commands == 0
            && pending_commits == 0
            && !tracker_busy
        {
            run.completed(id);
            journal.finish(&start_id.to_string(), "complete").await?;
            return Ok(Report {
                session_id: session.into(),
                turn_id: id.to_string(),
                terminal: terminal.clone(),
                messages,
                observed_processes: observed.into_values().collect(),
            });
        }
    }
}
