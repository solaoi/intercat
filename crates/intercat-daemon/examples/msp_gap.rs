//! Run the production page walk against deterministic fake-host delivery schedules.
use intercat_daemon::{
    Result, history,
    journal::Journal,
    msp::{Connection, Event, Received},
    secrets::Key,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[path = "support/msp_transport_types.rs"]
mod wire_types;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    id: String,
    session_id: String,
    initial: Vec<Value>,
    gap: Value,
    history: Vec<Value>,
    page_size: usize,
    during: Vec<Value>,
    tail: Vec<Value>,
    fault: Option<String>,
    fault_cursor: Option<String>,
}
#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

async fn save(journal: &Journal, session: &str, value: Received) -> Result<()> {
    journal
        .try_msp(
            "track".into(),
            session.into(),
            value.stamp,
            value.frame,
            Some(value.lease),
            value.source,
        )?
        .await??;
    Ok(())
}
async fn receive(connection: &mut Connection) -> Result<Received> {
    match connection.next_event().await {
        Some(Event::Frame(value)) => Ok(value),
        Some(Event::Closed(error)) => Err(error.into()),
        None => Err("Missing fixture event".into()),
    }
}
async fn projection(journal: &Journal, session: &str) -> Result<Value> {
    let local = journal.reconcile_local(session.into()).await?;
    let v = local.view;
    Ok(
        json!({"items":v.items,"turns":v.ordered_turns(),"activeTurnId":v.active_turn,
        "sessionState":v.session_state,"pendingApprovals":v.pending_approvals,
        "resolvedApprovals":v.resolved_approvals,"pendingUserInputs":v.pending_user_inputs,
        "settledUserInputs":v.settled_user_inputs}),
    )
}
#[tokio::main]
async fn main() -> Result<()> {
    let fixture: Fixture = serde_json::from_slice(&std::fs::read(
        std::env::args_os().nth(1).ok_or("fixture")?,
    )?)?;
    let output = std::env::args_os().nth(2).ok_or("output")?;
    let root = tempfile::tempdir()?;
    let mut results = vec![];
    let mut validated = 0;
    for test in fixture.cases {
        for frame in test
            .initial
            .iter()
            .chain(std::iter::once(&test.gap))
            .chain(&test.history)
            .chain(&test.during)
            .chain(&test.tail)
        {
            let name = match frame["method"].as_str() {
                Some("session/nameChanged") => "SessionNameChangedParams",
                Some("session/statusChanged") => "SessionStatusChangedParams",
                Some("view/gap") => "ViewGapParams",
                _ => return Err("Unknown gap fixture frame".into()),
            };
            wire_types::validate(&wire_types::Validation {
                name: name.into(),
                value: frame["params"].clone(),
            })?;
            validated += 1;
        }
        let path = root.path().join(format!("{}.sqlite", test.id));
        let journal = Journal::open(&path, Key::new([7; 32])).await?;
        let (incoming, mut output) = tokio::io::duplex(65536);
        let (outgoing, input) = tokio::io::duplex(65536);
        let mut connection = Connection::new(incoming, outgoing);
        let (finish, mut finished) = tokio::sync::oneshot::channel::<()>();
        let initial_count = test.initial.len();
        let tail_count = test.tail.len();
        let session = test.session_id.clone();
        let id = test.id.clone();
        let target = test.gap["params"]["next"]
            .as_str()
            .ok_or("target")?
            .to_owned();
        let server = tokio::spawn(async move {
            for frame in test.initial.iter().chain(std::iter::once(&test.gap)) {
                output.write_all(format!("{frame}\n").as_bytes()).await?;
            }
            let mut input = BufReader::new(input);
            let mut sent = vec![];
            let mut injected = false;
            loop {
                let mut line = String::new();
                tokio::select! {
                    _=&mut finished=>{for frame in &test.tail {output.write_all(format!("{frame}\n").as_bytes()).await?;} break;},
                    read=input.read_line(&mut line)=>{if read?==0{break;}}
                }
                let request: Value = serde_json::from_str(&line)?;
                if request["method"] != "view/page" {
                    return Err("Unexpected method".into());
                }
                let cursor = request["params"]["cursor"].as_str().ok_or("cursor")?;
                if !injected {
                    injected = true;
                    for frame in &test.during {
                        output.write_all(format!("{frame}\n").as_bytes()).await?;
                    }
                }
                let offset = test
                    .history
                    .iter()
                    .position(|f| f["params"]["viewCursor"] == cursor)
                    .map_or(0, |i| i + 1);
                let mut events = test
                    .history
                    .iter()
                    .skip(offset)
                    .take(test.page_size)
                    .cloned()
                    .collect::<Vec<_>>();
                let mut next = events
                    .last()
                    .map(|f| f["params"]["viewCursor"].clone())
                    .unwrap_or(Value::Null);
                if events.is_empty() || events.last() == test.history.last() {
                    next = Value::Null;
                }
                let mut reply = json!({"jsonrpc":"2.0","id":request["id"],"result":{"events":events,"nextCursor":next}});
                if test.fault_cursor.as_deref() == Some(cursor) {
                    match test.fault.as_deref() {
                        Some("error") => {
                            reply = json!({"jsonrpc":"2.0","id":request["id"],"error":{"code":-32000,"message":"fixture failure"}})
                        }
                        Some("stall") => reply["result"] = json!({"events":[],"nextCursor":cursor}),
                        Some("foreign") => {
                            events[0]["params"]["sessionId"] = json!("foreign");
                            reply["result"]["events"] = json!(events);
                        }
                        _ => {}
                    }
                }
                output.write_all(format!("{reply}\n").as_bytes()).await?;
                sent.push(request);
            }
            output.shutdown().await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(sent)
        });
        for _ in 0..=initial_count {
            save(&journal, &session, receive(&mut connection).await?).await?;
        }
        drop(journal);
        let journal = Journal::open(&path, Key::new([7; 32])).await?;
        let result = history::recover(
            &mut connection,
            history::Context {
                journal: &journal,
                track: "track",
                session: &session,
                scanner: None,
            },
            &target,
            None,
        )
        .await;
        while let Ok(Event::Frame(value)) = connection.try_next_event() {
            save(&journal, &session, value).await?;
        }
        let _ = finish.send(());
        for _ in 0..tail_count {
            save(&journal, &session, receive(&mut connection).await?).await?;
        }
        let sent = server.await??;
        let final_state = projection(&journal, &session).await?;
        let pending_gap = journal.view_gap("track".into(), session.clone()).await?;
        connection.close();
        drop(journal);
        let reopened = Journal::open(&path, Key::new([7; 32])).await?;
        if projection(&reopened, &session).await? != final_state {
            return Err("Restart changed the view".into());
        }
        if reopened.view_gap("track".into(), session.clone()).await? != pending_gap {
            return Err("Restart changed the pending gap".into());
        }
        results.push(json!({"id":id,"final":final_state,"current":pending_gap.is_none(),"pendingGap":pending_gap,"recoverySucceeded":result.is_ok(),"error":result.err().map(|e|e.to_string()),"sent":sent,"restartBeforeFill":true,"restartVerified":true}));
    }
    std::fs::write(
        output,
        serde_json::to_vec(
            &json!({"cases":results,"validatedFrames":validated,"schemaVersion":intercat_daemon::journal::SCHEMA_VERSION}),
        )?,
    )?;
    println!(
        "{}",
        json!({"cases":results.len(),"validatedFrames":validated})
    );
    Ok(())
}
