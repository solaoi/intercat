//! Offline wire-contract probe using the actual daemon transport.
use intercat_daemon::{
    Result,
    msp::{Connection, Event},
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[path = "support/msp_transport_types.rs"]
mod wire_types;

#[derive(Deserialize)]
struct Call {
    method: String,
    params: Value,
    result: Value,
}
#[derive(Deserialize)]
struct Notification {
    frame: Value,
}
#[derive(Deserialize)]
struct Fixture {
    calls: Vec<Call>,
    notifications: Vec<Notification>,
    requests: Vec<Notification>,
    notify: Value,
    validation: Vec<wire_types::Validation>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(15), run()).await?
}

async fn run() -> Result<()> {
    let path = std::env::args_os().nth(1).ok_or("Expected a fixture")?;
    let fixture: Fixture = serde_json::from_slice(&std::fs::read(path)?)?;
    let validated_types = fixture.validation.len();
    for value in &fixture.validation {
        wire_types::validate(value)?;
    }
    if fixture.calls[0].result["schema"]["fingerprint"] != intercat_daemon::msp::FINGERPRINT {
        return Err("Fixture schema fingerprint is not pinned".into());
    }
    let (client_read, host_write) = tokio::io::duplex(1024 * 1024);
    let (host_read, client_write) = tokio::io::duplex(1024 * 1024);
    let replies: Vec<_> = fixture
        .calls
        .iter()
        .map(|call| call.result.clone())
        .collect();
    let frames: Vec<_> = fixture
        .notifications
        .iter()
        .map(|entry| entry.frame.clone())
        .collect();
    let expected_count = frames.len();
    let requests: Vec<_> = fixture
        .requests
        .iter()
        .map(|entry| entry.frame.clone())
        .collect();
    let expected_requests = requests.len();
    let host = tokio::spawn(async move {
        let mut incoming = BufReader::new(host_read);
        let mut outgoing = host_write;
        let mut sent = Vec::new();
        for result in replies {
            let mut line = String::new();
            incoming.read_line(&mut line).await?;
            let frame: Value = serde_json::from_str(&line)?;
            outgoing
                .write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":frame["id"],"result":result})
                    )
                    .as_bytes(),
                )
                .await?;
            sent.push(frame);
        }
        let mut line = String::new();
        incoming.read_line(&mut line).await?;
        sent.push(serde_json::from_str(&line)?);
        for frame in frames {
            outgoing.write_all(format!("{frame}\n").as_bytes()).await?;
        }
        for request in requests {
            outgoing
                .write_all(format!("{request}\n").as_bytes())
                .await?;
            loop {
                let mut line = String::new();
                incoming.read_line(&mut line).await?;
                let response: Value = serde_json::from_str(&line)?;
                let complete = response.get("method").is_none() && response["id"] == request["id"];
                sent.push(response);
                if complete {
                    break;
                }
            }
        }
        outgoing.shutdown().await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(sent)
    });
    let mut connection = Connection::new(client_read, client_write);
    let mut results = Vec::new();
    for call in fixture.calls {
        results.push(connection.client.request(&call.method, call.params).await?);
    }
    connection
        .client
        .notify(
            fixture.notify["method"].as_str().unwrap(),
            fixture.notify["params"].clone(),
        )
        .await?;
    let mut notifications = Vec::new();
    let mut requests = Vec::new();
    while notifications.len() < expected_count || requests.len() < expected_requests {
        match connection.next_event().await {
            Some(Event::Frame(received))
                if received.frame.get("method").is_some() && received.frame.get("id").is_some() =>
            {
                requests.push(received.frame);
            }
            Some(Event::Frame(received)) if received.frame.get("method").is_some() => {
                notifications.push(received.frame)
            }
            Some(Event::Frame(_)) => {}
            Some(Event::Closed(reason)) => return Err(reason.into()),
            None => return Err("Transport ended before all notifications".into()),
        }
    }
    let sent = host.await??;
    connection.close();
    println!(
        "{}",
        json!({"sent":sent,"results":results,"notifications":notifications,"requests":requests,
            "validatedTypes":validated_types,"schemaFingerprint":intercat_daemon::msp::FINGERPRINT})
    );
    Ok(())
}
