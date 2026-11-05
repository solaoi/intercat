//! Replay a captured native history response through transport and persistence.
use intercat_daemon::{
    Result,
    history::{self, Context},
    journal::Journal,
    msp::Connection,
    secrets::Key,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() -> Result<()> {
    let input = std::env::args_os().nth(1).ok_or("response file")?;
    let output = std::env::args_os().nth(2).ok_or("result file")?;
    let read: Value = serde_json::from_slice(&std::fs::read(input)?)?;
    let response_bytes = serde_json::to_vec(&read)?.len();
    let session = read["session"]["sessionId"]
        .as_str()
        .ok_or("session id")?
        .to_owned();
    let root = tempfile::tempdir()?;
    let path = root.path().join("journal.sqlite");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let (incoming, mut output_stream) = tokio::io::duplex(65536);
    let (outgoing, input_stream) = tokio::io::duplex(4096);
    let mut connection = Connection::new(incoming, outgoing);
    let served = read.clone();
    let server = tokio::spawn(async move {
        let mut input = BufReader::new(input_stream);
        let mut line = String::new();
        input.read_line(&mut line).await?;
        let request: Value = serde_json::from_str(&line)?;
        if request["method"] != "session/resume" {
            return Err("unexpected query".into());
        }
        let frame = json!({"jsonrpc":"2.0","id":request["id"],"result":served});
        output_stream
            .write_all(&serde_json::to_vec(&frame)?)
            .await?;
        output_stream.write_all(b"\n").await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    history::resume(
        &mut connection,
        Context {
            journal: &journal,
            track: "track",
            session: &session,
            scanner: None,
        },
        uuid::Uuid::now_v7(),
    )
    .await?;
    server.await??;
    if connection.client.output_flow().interrupted {
        return Err("Valid history interrupted the host".into());
    }
    let local = journal.reconcile_local(session.clone()).await?;
    let state = local.view.snapshot_state();
    if read["history"]["mode"] == "snapshot"
        && state.as_ref() != read.pointer("/history/snapshot/state")
    {
        return Err("Captured snapshot state changed".into());
    }
    if read["history"]["mode"] == "none"
        && journal
            .view_cursor("track".into(), session.clone())
            .await?
            .is_some()
    {
        return Err("Omitted history advanced the saved cursor".into());
    }
    connection.close();
    drop(journal);
    let reopened = Journal::open(&path, Key::new([7; 32])).await?;
    if reopened
        .reconcile_local(session)
        .await?
        .view
        .snapshot_state()
        != state
    {
        return Err("Reopen changed recovered state".into());
    }
    let digest = Sha256::digest(serde_json::to_vec(&state)?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    std::fs::write(
        output,
        serde_json::to_vec_pretty(
            &json!({"responseBytes":response_bytes,"mode":read["history"]["mode"],"items":local.view.items.len(),"stateSha256":digest,"stateMatches":true,"restartMatches":true,"outputInterrupted":false,"schema":intercat_daemon::journal::SCHEMA_VERSION}),
        )?,
    )?;
    Ok(())
}
