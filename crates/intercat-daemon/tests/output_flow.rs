use intercat_daemon::{
    Result,
    journal::Journal,
    msp::{Connection, Event},
    output_flow::LIMIT,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn put(writer: &mut (impl AsyncWriteExt + Unpin), value: &Value) {
    writer
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
}

#[tokio::test]
async fn even_a_small_backlog_is_reported_when_it_has_not_been_saved_for_two_seconds() {
    let flow = intercat_daemon::output_flow::Flow::default();
    let lease = flow.reserve(100).unwrap();
    assert!(!flow.status().lagging);
    tokio::time::sleep(Duration::from_millis(2050)).await;
    assert!(flow.status().lagging);
    assert!(flow.status().oldest_queued_ms >= 2000);
    drop(lease);
    assert!(!flow.status().lagging);
    assert_eq!(flow.status().queued_bytes, 0);
}

#[tokio::test]
async fn delta_metadata_keeps_receive_order_without_fragments_and_final_output_survives()
-> Result<()> {
    let (incoming, mut output) = tokio::io::duplex(64 * 1024);
    let (outgoing, _input) = tokio::io::duplex(4096);
    let mut connection = Connection::new(incoming, outgoing);
    let producer = tokio::spawn(async move {
        for index in 0..1000 {
            put(&mut output, &json!({"jsonrpc":"2.0","method":"item/delta","params":{"sessionId":"s","itemId":"i","delta":"split-secret-fragment","ordinal":index}})).await;
        }
        put(&mut output, &json!({"jsonrpc":"2.0","method":"item/completed","params":{"item":{"itemId":"i","text":"complete output"}}})).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    });
    let mut summaries = 0;
    let mut frames = 0;
    let mut bytes = 0;
    loop {
        let Some(Event::Frame(received)) = connection.next_event().await else {
            panic!("Missing final output");
        };
        if received.frame["method"] == "item/completed" {
            assert_eq!(received.frame["params"]["item"]["text"], "complete output");
            break;
        }
        assert!(received.frame["params"].get("delta").is_none());
        assert_eq!(received.frame["params"]["ordinal"], frames);
        summaries += 1;
        frames += 1;
        bytes += received.frame["params"]["deltaBytes"].as_u64().unwrap();
    }
    assert_eq!(frames, 1000);
    assert_eq!(bytes, 1000 * "split-secret-fragment".len() as u64);
    assert_eq!(summaries, 1000);
    assert!(!connection.client.output_flow().interrupted);
    producer.await?;
    Ok(())
}

#[tokio::test]
async fn journal_backlog_keeps_the_receive_budget_and_drops_only_deltas() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("events.sqlite");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let mut lock = rusqlite::Connection::open(path)?;
    let transaction = lock.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (incoming, mut output) = tokio::io::duplex(64 * 1024);
    let (outgoing, input) = tokio::io::duplex(4096);
    let mut input = BufReader::new(input);
    let mut connection = Connection::new(incoming, outgoing);
    let producer = tokio::spawn(async move {
        for _ in 0..7 {
            put(&mut output, &json!({"jsonrpc":"2.0","method":"diagnostic","params":{"text":"x".repeat(700_000)}})).await;
        }
        (output, ())
    });
    let mut receipts = Vec::new();
    for _ in 0..7 {
        let Some(Event::Frame(received)) = connection.next_event().await else {
            panic!("Missing frame");
        };
        receipts.push(journal.try_msp(
            "t".into(),
            "s".into(),
            received.stamp,
            received.frame,
            Some(received.lease),
            None,
        )?);
    }
    let (mut output, ()) = producer.await?;
    assert!(connection.client.output_flow().queued_bytes > 4 * 1024 * 1024);
    for _ in 0..100 {
        put(&mut output, &json!({"jsonrpc":"2.0","method":"item/delta","params":{"delta":"discard","itemId":"i"}})).await;
    }
    put(&mut output, &json!({"jsonrpc":"2.0","method":"item/completed","params":{"item":{"itemId":"i","text":"authoritative final output"}}})).await;
    let Some(Event::Frame(completed)) = connection.next_event().await else {
        panic!("Final output was lost");
    };
    assert_eq!(
        completed.frame["params"]["item"]["text"],
        "authoritative final output"
    );
    receipts.push(journal.try_msp(
        "t".into(),
        "s".into(),
        completed.stamp,
        completed.frame,
        Some(completed.lease),
        None,
    )?);
    put(
        &mut output,
        &json!({"jsonrpc":"2.0","method":"approval/request","params":{"approvalId":"a"}}),
    )
    .await;
    let Some(Event::Frame(approval)) =
        tokio::time::timeout(Duration::from_secs(1), connection.next_event()).await?
    else {
        panic!("Approval was delayed by delta output");
    };
    assert_eq!(approval.frame["method"], "approval/request");
    assert_eq!(connection.client.output_flow().dropped_deltas, 100);
    let client = connection.client.clone();
    let response = tokio::spawn(async move { client.request("diagnostic/ping", json!({})).await });
    let mut line = String::new();
    input.read_line(&mut line).await?;
    let request: Value = serde_json::from_str(&line)?;
    put(
        &mut output,
        &json!({"jsonrpc":"2.0","id":request["id"],"result":{"pong":true}}),
    )
    .await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), response).await???["pong"],
        true
    );
    drop(approval);
    transaction.commit()?;
    for receipt in receipts {
        receipt.await??;
    }
    while let Ok(frame) = connection.try_next_event() {
        drop(frame);
    }
    assert_eq!(connection.client.output_flow().queued_bytes, 0);
    Ok(())
}

#[tokio::test]
async fn sustained_non_delta_overflow_interrupts_only_its_host_before_stopping_the_reader()
-> Result<()> {
    let (incoming, mut output) = tokio::io::duplex(64 * 1024);
    let (outgoing, input) = tokio::io::duplex(4096);
    let mut input = BufReader::new(input);
    let mut connection = Connection::new(incoming, outgoing);
    let (_quiet_output, quiet_incoming) = tokio::io::duplex(1024);
    let (quiet_outgoing, _quiet_input) = tokio::io::duplex(1024);
    let quiet = Connection::new(quiet_incoming, quiet_outgoing);
    let turn = uuid::Uuid::now_v7();
    let producer = tokio::spawn(async move {
        put(&mut output,&json!({"jsonrpc":"2.0","method":"turn/started","params":{"sessionId":"noisy","turnId":turn}})).await;
        let frame = format!(
            "{}\n",
            json!({"jsonrpc":"2.0","method":"diagnostic","params":{"text":"x".repeat(1_000_000)}})
        );
        for _ in 0..12 {
            if output.write_all(frame.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(3), input.read_line(&mut line)).await??;
    let interrupt: Value = serde_json::from_str(&line)?;
    assert_eq!(interrupt["method"], "turn/interrupt");
    assert_eq!(interrupt["params"]["sessionId"], "noisy");
    assert_eq!(interrupt["params"]["turnId"], turn.to_string());
    let mut held = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(3), connection.next_event()).await? {
            Some(Event::Frame(frame)) => held.push(frame),
            Some(Event::Closed(reason)) => {
                assert_eq!(reason, "output too fast to record");
                break;
            }
            None => panic!("Missing overflow state"),
        }
    }
    assert!(connection.client.output_flow().queued_bytes <= LIMIT);
    assert!(connection.client.output_flow().interrupted);
    assert!(!quiet.client.output_flow().interrupted);
    drop(held);
    producer.await?;
    Ok(())
}
