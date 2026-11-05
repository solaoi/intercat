//! Replay recorded sessions through the production transport and journal.
use intercat_daemon::{
    Result,
    clock::Stamp,
    journal::Journal,
    msp::{Connection, Event},
    secrets::Key,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use tokio::io::AsyncWriteExt;

#[path = "support/msp_transport_types.rs"]
mod wire_types;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Case {
    id: String,
    session_id: String,
    frames: Vec<Value>,
}
#[derive(Deserialize)]
struct Fixture {
    cases: Vec<Case>,
    #[serde(default)]
    validation: Vec<wire_types::Validation>,
}

async fn projection(journal: &Journal, session: &str) -> Result<Value> {
    let local = journal.reconcile_local(session.into()).await?;
    let view = local.view;
    Ok(json!({
        "items":view.items,
        "turns":view.ordered_turns(),
        "activeTurnId":view.active_turn,
        "sessionState":view.session_state,
        "pendingApprovals":view.pending_approvals,
        "resolvedApprovals":view.resolved_approvals,
        "pendingUserInputs":view.pending_user_inputs,
        "settledUserInputs":view.settled_user_inputs,
    }))
}

// Compare numerical values, not JSON's language-specific decimal formatting.
// The full IEEE-754 representation keeps one-bit parser errors observable.
fn digest(value: &Value) -> String {
    fn encode(value: &Value, out: &mut String) {
        match value {
            Value::Null => out.push('z'),
            Value::Bool(value) => out.push(if *value { 't' } else { 'f' }),
            Value::Number(value) => {
                out.push_str(&format!("n{:016x}", value.as_f64().unwrap().to_bits()))
            }
            Value::String(value) => {
                out.push('s');
                out.push_str(&serde_json::to_string(value).unwrap());
            }
            Value::Array(values) => {
                out.push('[');
                for value in values {
                    encode(value, out);
                    out.push(',');
                }
                out.push(']');
            }
            Value::Object(values) => {
                out.push('{');
                for (key, value) in values {
                    out.push_str(&serde_json::to_string(key).unwrap());
                    out.push(':');
                    encode(value, out);
                    out.push(',');
                }
                out.push('}');
            }
        }
    }
    let mut encoded = String::new();
    encode(value, &mut encoded);
    Sha256::digest(encoded.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[tokio::main]
async fn main() -> Result<()> {
    let input = std::env::args_os().nth(1).ok_or("Expected fixture path")?;
    let output = std::env::args_os().nth(2).ok_or("Expected result path")?;
    let fixture: Fixture = serde_json::from_slice(&std::fs::read(input)?)?;
    for value in &fixture.validation {
        wire_types::validate(value)?;
    }
    let progress = std::path::Path::new(&output).with_extension("progress.jsonl");
    let mut progress = std::fs::File::create(progress)?;
    let root = tempfile::tempdir()?;
    let mut results = Vec::new();
    let total = fixture.cases.len();
    for (case_index, test) in fixture.cases.into_iter().enumerate() {
        let path = root.path().join(format!("{}.sqlite", test.id));
        let journal = Journal::open(&path, Key::new([7; 32])).await?;
        let (client_read, mut host_write) = tokio::io::duplex(1024 * 1024);
        let (_host_read, client_write) = tokio::io::duplex(1024);
        let frames = test.frames;
        let count = frames.len();
        let host = tokio::spawn(async move {
            for frame in frames {
                host_write
                    .write_all(format!("{frame}\n").as_bytes())
                    .await?;
            }
            host_write.shutdown().await
        });
        let mut connection = Connection::new(client_read, client_write);
        let mut steps = Vec::new();
        for index in 0..count {
            let Some(Event::Frame(received)) = connection.next_event().await else {
                return Err(format!("Replay {} ended at {index}/{count}", test.id).into());
            };
            journal
                .try_msp(
                    test.id.clone(),
                    test.session_id.clone(),
                    Stamp::now(),
                    received.frame,
                    Some(received.lease),
                    None,
                )?
                .await??;
            let current = projection(&journal, &test.session_id).await?;
            steps.push(digest(&current));
        }
        let final_state = projection(&journal, &test.session_id).await?;
        connection.close();
        host.await??;
        drop(journal);
        let reopened = Journal::open(&path, Key::new([7; 32])).await?;
        if projection(&reopened, &test.session_id).await? != final_state {
            return Err(format!("Replay {} changed after reopening the journal", test.id).into());
        }
        let result = json!({"id":test.id,"steps":steps,"final":final_state,"restartVerified":true});
        serde_json::to_writer(&mut progress, &result)?;
        writeln!(&mut progress)?;
        results.push(result);
        if (case_index + 1) % 100 == 0 {
            eprintln!("Replayed {}/{} sessions", case_index + 1, total);
        }
    }
    std::fs::write(output, serde_json::to_vec(&json!({"cases":results}))?)?;
    println!("{}", json!({"cases":results.len()}));
    Ok(())
}
