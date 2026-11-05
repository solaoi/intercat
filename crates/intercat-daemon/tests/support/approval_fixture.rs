//! The exact fake-host protocol shared by normal and repair execution probes.
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;
type ProbeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fixture {
    pub session: String,
    pub turn: Uuid,
    pub workspace: PathBuf,
    pub request: Value,
    pub update: Value,
    pub terminal_at: usize,
    pub update_before_ack: bool,
    pub duplicate_request: bool,
    pub server_request: bool,
}

pub async fn serve(
    server: tokio::io::DuplexStream,
    fixture: Fixture,
    expect_start: bool,
) -> ProbeResult<Value> {
    let (read, mut write) = tokio::io::split(server);
    let mut reader = BufReader::new(read);
    let mut starts = Vec::new();
    if expect_start {
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line)).await??;
        let start: Value = serde_json::from_str(&line)?;
        if start["method"] != "turn/start" {
            return Err("Repair did not begin with turn/start".into());
        }
        let _: muse_code_sdk::msp::TurnStartParams =
            serde_json::from_value(start["params"].clone())?;
        let result = json!({"commandId":start["params"]["commandId"],"turnId":fixture.turn,"disposition":"started","status":"accepted","startedNewTurn":true});
        let _: muse_code_sdk::msp::TurnStartResult = serde_json::from_value(result.clone())?;
        let started = json!({"sessionId":fixture.session,"turnId":fixture.turn,"commandId":start["params"]["commandId"],"viewCursor":"turn-start","sourceRange":fixture.request["sourceRange"]});
        let _: muse_code_sdk::msp::TurnStartedParams = serde_json::from_value(started.clone())?;
        write
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":start["id"],"result":result})
                )
                .as_bytes(),
            )
            .await?;
        write
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","method":"turn/started","params":started})
                )
                .as_bytes(),
            )
            .await?;
        starts.push(start);
    }
    let request = fixture.request;
    let update = fixture.update;
    let first = if fixture.server_request {
        json!({"jsonrpc":"2.0","id":1001,"method":"approval/request","params":request})
    } else {
        json!({"jsonrpc":"2.0","method":"approval/requested","params":request})
    };
    write.write_all(format!("{first}\n").as_bytes()).await?;
    let mut sent = Vec::new();
    let mut receipts = Vec::new();
    let mut unexpected = Vec::new();
    if fixture.duplicate_request {
        write
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","method":"approval/requested","params":request})
                )
                .as_bytes(),
            )
            .await?;
    }
    loop {
        let mut line = String::new();
        let timeout = if sent.len() >= fixture.terminal_at {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(5)
        };
        match tokio::time::timeout(timeout, reader.read_line(&mut line)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Err(error)) => return Err(error.into()),
            Ok(Ok(_)) => {}
        }
        let frame: Value = serde_json::from_str(&line)?;
        if frame.get("method").is_none() {
            receipts.push(frame);
            continue;
        }
        if frame["method"] == "turn/start" {
            starts.push(frame);
            continue;
        }
        if frame["method"] != "approval/decide" {
            unexpected.push(frame);
            continue;
        }
        sent.push(frame.clone());
        let terminal = sent.len() >= fixture.terminal_at;
        if sent.len() == 1 && fixture.update_before_ack {
            write
                .write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","method":"approval/updated","params":update})
                    )
                    .as_bytes(),
                )
                .await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        write.write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":frame["id"],"result":{"commandId":frame["params"]["commandId"],"approvalId":request["approvalId"],"status":"accepted","terminal":terminal}})).as_bytes()).await?;
        if sent.len() == 1 && !fixture.update_before_ack {
            write
                .write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","method":"approval/updated","params":update})
                    )
                    .as_bytes(),
                )
                .await?;
        }
        if terminal && fixture.duplicate_request {
            write
                .write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","method":"approval/requested","params":request})
                    )
                    .as_bytes(),
                )
                .await?;
        }
    }
    let complete = sent.len() == fixture.terminal_at
        && starts.len() == usize::from(expect_start)
        && unexpected.is_empty();
    Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
        json!({"sent":sent,"receipts":receipts,"complete":complete,"starts":starts,"unexpected":unexpected}),
    )
}
