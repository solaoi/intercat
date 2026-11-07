use intercat_daemon::{
    Result,
    journal::{Admission, Journal},
    lock::DaemonLock,
    msp::{Connection, Event},
    run::{Effect, Hold, Run},
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

#[test]
fn stop_while_intent_is_saving_never_dispatches() {
    let mut run = Run::new();
    let id = Uuid::now_v7();
    assert_eq!(run.request_start(id), Some(Effect::PersistStart(id)));
    assert!(!run.can_start());
    assert_eq!(run.stop(), None);
    assert_eq!(run.start_persisted(id), Some(Effect::Superseded(id)));
    assert!(!run.can_start());
    run.set_hold(Hold::Stopping, false);
    assert!(run.can_start());
}

#[test]
fn boundary_hold_allows_the_accepted_turn_to_finish_and_blocks_the_next() {
    let mut run = Run::new();
    let id = Uuid::now_v7();
    let turn = Uuid::now_v7();
    assert_eq!(run.request_start(id), Some(Effect::PersistStart(id)));
    assert_eq!(run.start_persisted(id), Some(Effect::SendStart(id)));
    run.set_hold(Hold::Boundary, true);
    assert_eq!(run.start_acknowledged(id, turn), None);
    assert_eq!(run.active_turn(), Some(turn));
    assert_eq!(run.completed(turn), Some(Effect::Checkpoint(turn)));
    run.checkpoint_saved(turn);
    assert!(!run.can_start());
    run.set_hold(Hold::Boundary, false);
    assert!(run.can_start());
}

#[test]
fn draining_output_does_not_clear_an_independent_log_failure() {
    let mut run = Run::new();
    run.set_hold(Hold::Log, true);
    run.set_hold(Hold::Output, true);
    assert!(!run.can_start());
    run.set_hold(Hold::Output, false);
    assert!(!run.can_start());
    run.set_hold(Hold::Log, false);
    assert!(run.can_start());
}

#[test]
fn stop_during_start_waits_for_the_exact_turn_and_stale_ack_is_ignored() {
    let mut run = Run::new();
    let id = Uuid::now_v7();
    let turn = Uuid::now_v7();
    run.request_start(id);
    assert_eq!(run.start_persisted(id), Some(Effect::SendStart(id)));
    run.stop();
    assert_eq!(run.start_acknowledged(Uuid::now_v7(), turn), None);
    assert_eq!(run.active_turn(), None);
    assert_eq!(
        run.start_acknowledged(id, turn),
        Some(Effect::Interrupt(turn))
    );
    assert_eq!(run.active_turn(), Some(turn));
}

#[test]
fn checkpoint_question_and_reconnection_all_gate_the_next_turn() {
    let mut run = Run::new();
    let id = Uuid::now_v7();
    run.request_start(id);
    run.start_persisted(id);
    run.start_acknowledged(id, id);
    assert_eq!(run.completed(id), Some(Effect::Checkpoint(id)));
    assert!(!run.can_start());
    let question = Uuid::now_v7();
    run.question(question);
    run.checkpoint_saved(Uuid::now_v7());
    assert!(!run.can_start());
    run.checkpoint_saved(id);
    assert!(!run.can_start());
    run.answer(question);
    assert!(run.can_start());
    run.disconnected();
    assert!(!run.can_start());
    run.reconciled(None);
    assert!(run.can_start());
}

#[test]
fn one_daemon_lock_per_directory_and_release_on_drop() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let first = DaemonLock::acquire(temp.path())?.unwrap();
    assert!(DaemonLock::acquire(temp.path())?.is_none());
    drop(first);
    assert!(DaemonLock::acquire(temp.path())?.is_some());
    Ok(())
}

#[tokio::test]
async fn intent_and_effect_share_one_transaction_and_replay_survives_reopen() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("events.sqlite");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let payload = json!({"run":"A", "text":"add a test"});
    assert_eq!(
        journal.intent("op", "turn.start", payload.clone()).await?,
        Admission::New
    );
    assert_eq!(
        journal.intent("op", "turn.start", payload.clone()).await?,
        Admission::Replay("prepared".into())
    );
    assert!(
        journal
            .intent("op", "turn.start", json!({"text":"different"}))
            .await
            .is_err()
    );
    journal.finish("op", "dispatched").await?;
    journal.finish("op", "complete").await?;
    assert!(journal.finish("op", "dispatched").await.is_err());
    drop(journal);
    let reopened = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert_eq!(
        reopened.intent("op", "turn.start", payload).await?,
        Admission::Replay("complete".into())
    );
    let conn = rusqlite::Connection::open(path)?;
    let intents: i64 = conn.query_row(
        "SELECT count(*) FROM events WHERE kind='intent.turn.start'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(intents, 1);
    Ok(())
}

#[tokio::test]
async fn writer_serializes_concurrent_producers() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let path = temp.path().join("events.sqlite");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let mut tasks = Vec::new();
    for i in 0..20 {
        let journal = journal.clone();
        tasks.push(tokio::spawn(async move {
            journal.append("observed", json!({"i":i})).await.unwrap()
        }));
    }
    let mut sequence = Vec::new();
    for task in tasks {
        sequence.push(task.await?);
    }
    sequence.sort();
    assert_eq!(sequence, (1..=20).collect::<Vec<_>>());
    let conn = rusqlite::Connection::open(path)?;
    let count: i64 = conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?;
    assert_eq!(count, 20);
    Ok(())
}

#[tokio::test]
async fn response_is_delivered_before_consumer_drains_notifications() -> Result<()> {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(client_io);
    let mut connection = Connection::new(read, write);
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server_io);
        let mut line = String::new();
        server.read_line(&mut line).await.unwrap();
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        let notification =
            json!({"jsonrpc":"2.0","method":"turn/completed","params":{"turnId":"A"}});
        let response = json!({"jsonrpc":"2.0","id":request["id"],"result":{"ok":true}});
        server
            .get_mut()
            .write_all(format!("{notification}\n{response}\n").as_bytes())
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });
    assert_eq!(
        connection.client.request("session/read", json!({})).await?,
        json!({"ok":true})
    );
    let Some(Event::Frame(received)) = connection.next_event().await else {
        panic!("expected notification")
    };
    assert_eq!(received.frame["method"], "turn/completed");
    server.await?;
    Ok(())
}

#[tokio::test]
async fn disconnect_fails_pending_request_instead_of_resending_it() -> Result<()> {
    let (client_io, server_io) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(client_io);
    let connection = Connection::new(read, write);
    let server = tokio::spawn(async move {
        let mut server = BufReader::new(server_io);
        let mut line = String::new();
        server.read_line(&mut line).await.unwrap();
    });
    assert!(
        connection
            .client
            .request("fixture/request", json!({}))
            .await
            .is_err()
    );
    assert!(
        connection
            .client
            .request("fixture/request", json!({}))
            .await
            .is_err()
    );
    server.await?;
    Ok(())
}

#[tokio::test]
async fn emergency_stop_uses_the_writable_pipe_after_reader_failure() -> Result<()> {
    let (incoming, mut host_output) = tokio::io::duplex(128);
    let (outgoing, host_input) = tokio::io::duplex(4096);
    let mut connection = Connection::new(incoming, outgoing);
    host_output.shutdown().await?;
    assert!(matches!(
        connection.next_event().await,
        Some(Event::Closed(_))
    ));
    connection.client.interrupt_now("fixture-session", None)?;
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        BufReader::new(host_input).read_line(&mut line),
    )
    .await??;
    let frame: serde_json::Value = serde_json::from_str(&line)?;
    assert_eq!(frame["method"], "turn/interrupt");
    assert_eq!(frame["params"]["sessionId"], "fixture-session");
    Ok(())
}
