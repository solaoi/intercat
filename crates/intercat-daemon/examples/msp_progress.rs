//! Replay streaming notifications through the actual MSP reader and preview.
use intercat_daemon::{Result, msp::Connection, progress::Privacy};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> Result<()> {
    let input = std::env::args_os().nth(1).ok_or("Expected fixture path")?;
    let output = std::env::args_os().nth(2).ok_or("Expected output path")?;
    let fixture: Value = serde_json::from_slice(&std::fs::read(input)?)?;
    let temporary = tempfile::tempdir()?;
    let journal = intercat_daemon::journal::Journal::open(
        temporary.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    let mut cases = Vec::new();
    for case in fixture["cases"].as_array().ok_or("Expected cases")? {
        let session = case["sessionId"].as_str().ok_or("Expected session")?;
        let (mut host, incoming) = tokio::io::duplex(256 * 1024);
        let (outgoing, requests) = tokio::io::duplex(4096);
        let mut requests = tokio::io::BufReader::new(requests);
        let mut connection = Connection::new(incoming, outgoing);
        connection.client.configure_progress(
            session.into(),
            Privacy::KnownValues,
            case["initialTurn"].as_str().map(str::to_owned),
        );
        let mut steps = Vec::new();
        for action in case["actions"].as_array().ok_or("Expected actions")? {
            if let Some(params) = action.get("send") {
                let client = connection.client.clone();
                let params = params.clone();
                let prepared = journal.screen_start(params).await?;
                let guard = intercat_daemon::msp::StartGuard::Stopped(std::sync::Arc::new(
                    std::sync::atomic::AtomicBool::new(false),
                ));
                let request = tokio::spawn(async move { client.start(prepared, guard).await });
                let mut line = String::new();
                requests.read_line(&mut line).await?;
                let submitted: Value = serde_json::from_str(&line)?;
                host.write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":submitted["id"],"result":{"accepted":true}})
                    )
                    .as_bytes(),
                )
                .await?;
                request.await??;
                connection
                    .next_event()
                    .await
                    .ok_or("Missing request receipt")?;
            } else {
                let frame = &action["frame"];
                host.write_all(format!("{frame}\n").as_bytes()).await?;
                let Some(intercat_daemon::msp::Event::Frame(received)) =
                    connection.next_event().await
                else {
                    return Err("Missing notification".into());
                };
                if frame["method"] == "item/delta"
                    && received.frame["params"].get("delta").is_some()
                {
                    return Err("A raw delta entered the persistent queue".into());
                }
            }
            steps.push(serde_json::to_value(connection.client.progress())?);
        }
        cases.push(json!({"id":case["id"],"steps":steps}));
    }
    std::fs::write(output, serde_json::to_vec(&json!({"cases":cases}))?)?;
    Ok(())
}
