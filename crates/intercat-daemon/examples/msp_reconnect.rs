//! Replay reconnection schedules through the production reconciliation path.
use intercat_daemon::{
    Result,
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Target},
    history::Context,
    journal::Journal,
    msp::{Connection, StartGuard},
    reconcile::{self, Expected},
    secrets::Key,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicU64};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    let input = std::env::args_os().nth(1).ok_or("fixture")?;
    let output = std::env::args_os().nth(2).ok_or("output")?;
    let fixture: Value = serde_json::from_slice(&std::fs::read(input)?)?;
    let root = tempfile::tempdir()?;
    let mut results = Vec::new();
    for case in fixture["cases"].as_array().ok_or("cases")? {
        let session = case["sessionId"].as_str().ok_or("session")?;
        let path = root
            .path()
            .join(format!("{}.sqlite", case["id"].as_str().unwrap()));
        let journal = Journal::open(&path, Key::new([7; 32])).await?;
        let operations = case["operations"].as_array().ok_or("operations")?;
        if operations.len() != 1 {
            return Err("Expected one interrupted user input per case".into());
        }
        let operation = &operations[0];
        let text = operation["input"][0]["text"].as_str().ok_or("input text")?;
        journal
            .stage_utterance(Uuid::now_v7(), text.into(), Target::New, None, Vec::new())
            .await?;
        let input = journal.claim_input().await?.ok_or("input")?;
        journal
            .apply_input(
                input.id.clone(),
                input.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Fork,
                    track_id: None,
                    boundary: Some("now".into()),
                    command: None,
                    confidence: 1.,
                    inference_ms: 1.,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await?;
        let track = journal.snapshot().await?["tracks"][0]["id"]
            .as_str()
            .ok_or("track")?
            .to_owned();
        journal
            .reserve_turn(
                operation["commandId"]
                    .as_str()
                    .ok_or("commandId")?
                    .parse()?,
                input.id.clone(),
                input.revision,
                track.clone(),
                operation.clone(),
                None,
            )
            .await?;
        for frame in case["before"].as_array().ok_or("before")? {
            journal
                .try_msp(
                    track.clone(),
                    session.into(),
                    Stamp::now(),
                    frame.clone(),
                    None,
                    None,
                )?
                .await??;
        }
        drop(journal);
        let journal = Journal::open(&path, Key::new([7; 32])).await?;
        let steps = case["steps"].as_array().ok_or("steps")?.clone();
        let (read, mut output) = tokio::io::duplex(1024 * 1024);
        let (write, host_input) = tokio::io::duplex(1024 * 1024);
        let mut connection = Connection::new(read, write);
        let server = tokio::spawn(async move {
            let mut input = BufReader::new(host_input);
            let mut sent = Vec::new();
            for step in steps {
                let mut line = String::new();
                if input.read_line(&mut line).await? == 0 {
                    break;
                }
                let frame: Value = serde_json::from_str(&line)?;
                sent.push(frame.clone());
                if frame["method"] != step["method"] {
                    return Err(format!("Unexpected request: {}", frame["method"]).into());
                }
                if let Some(notifications) = step["notifications"].as_array() {
                    for notification in notifications {
                        output
                            .write_all(format!("{notification}\n").as_bytes())
                            .await?;
                    }
                }
                if step["disconnect"] == true {
                    break;
                }
                let reply = if step.get("error").is_some() {
                    json!({"jsonrpc":"2.0","id":frame["id"],"error":step["error"]})
                } else {
                    json!({"jsonrpc":"2.0","id":frame["id"],"result":step["result"]})
                };
                output.write_all(format!("{reply}\n").as_bytes()).await?;
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(sent)
        });
        let result = reconcile::prepare(
            &mut connection,
            Context {
                journal: &journal,
                track: &track,
                session,
                scanner: None,
            },
            Expected {
                replay: (case["allowReplay"] == true).then(|| {
                    StartGuard::new(
                        Arc::new(AtomicU64::new(0)),
                        std::sync::Arc::new(intercat_daemon::dispatch_control::Gate::new(false)),
                    )
                }),
                ..Expected::default()
            },
        )
        .await;
        connection.close();
        let sent = server.await??;
        let outcome = match result {
            Ok(state) => {
                json!({"ok":true,"activeTurn":state.active_turn,"resumeTurn":state.resume_turn,"command":state.command})
            }
            Err(error) => json!({"ok":false,"error":error.to_string()}),
        };
        let mut operations = Vec::new();
        for operation in case["operations"].as_array().unwrap() {
            let id: Uuid = operation["commandId"].as_str().unwrap().parse()?;
            operations.push(journal.operation(id).await?.ok_or("missing operation")?);
        }
        let view = journal.reconcile_local(session.into()).await?.view;
        let saved_input = journal.snapshot().await?["utterances"]
            .as_array()
            .ok_or("utterances")?
            .iter()
            .find(|row| row["id"] == input.id)
            .ok_or("saved input")?
            .clone();
        drop(journal);
        let journal = Journal::open(&path, Key::new([7; 32])).await?;
        let reopened = journal.reconcile_local(session.into()).await?.view;
        if view.snapshot_state() != reopened.snapshot_state() {
            return Err("Reopen changed recovered state".into());
        }
        for operation in &operations {
            let id: Uuid = operation["payload"]["commandId"]
                .as_str()
                .ok_or("operation id")?
                .parse()?;
            if journal.operation(id).await?.as_ref() != Some(operation) {
                return Err("Reopen changed the start intent or its outcome".into());
            }
        }
        let reopened_input = journal.snapshot().await?["utterances"]
            .as_array()
            .ok_or("utterances")?
            .iter()
            .find(|row| row["id"] == input.id)
            .ok_or("reopened input")?
            .clone();
        if saved_input != reopened_input {
            return Err("Reopen changed the user's input".into());
        }
        let logical_state = if outcome["ok"] == true && !outcome["activeTurn"].is_null() {
            "running"
        } else if operations.iter().all(|op| op["state"] == "superseded") {
            "held"
        } else {
            "pending"
        };
        results.push(json!({"id":case["id"],"outcome":outcome,"sent":sent,"operations":operations,"restartVerified":true,"logicalState":logical_state,"input":saved_input}));
    }
    std::fs::write(
        output,
        serde_json::to_vec_pretty(&json!({"cases":results}))?,
    )?;
    Ok(())
}
