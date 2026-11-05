//! Preserve receive order on disk while presenting requests ahead of bulky output.
use crate::{
    Result,
    history::Context,
    msp::{Connection, Event, Received},
};
use std::{collections::VecDeque, future::Future, pin::Pin};

type PendingBatch<'a> = Pin<Box<dyn Future<Output = Result<Vec<Received>>> + Send + 'a>>;

pub struct Inbox<'a> {
    context: Context<'a>,
    ready: VecDeque<Received>,
    received: VecDeque<Received>,
    saving: Option<PendingBatch<'a>>,
    closed: Option<String>,
}
impl<'a> Inbox<'a> {
    pub fn new(context: Context<'a>) -> Self {
        Self {
            context,
            ready: VecDeque::new(),
            received: VecDeque::new(),
            saving: None,
            closed: None,
        }
    }
    pub fn has_pending(&self) -> bool {
        !self.ready.is_empty() || !self.received.is_empty() || self.saving.is_some()
    }
    fn accept(&mut self, event: Option<Event>) {
        match event {
            Some(Event::Frame(frame)) => self.received.push_back(frame),
            Some(Event::Closed(reason)) => self.closed = Some(reason),
            None => self.closed = Some("Worker disconnected during delivery.".into()),
        }
    }
    async fn save_pending(&mut self) -> Result<()> {
        loop {
            if let Some(saving) = &mut self.saving {
                let result = saving.await;
                self.saving = None;
                self.ready.extend(result?);
            }
            if !self.received.is_empty() {
                let mut frames: Vec<_> = self.received.drain(..).collect();
                let context = self.context;
                self.saving = Some(Box::pin(async move {
                    let mut raw = Vec::new();
                    let mut indexes = Vec::new();
                    for (index, frame) in frames.iter_mut().enumerate() {
                        if frame.source.is_some() {
                            continue;
                        }
                        let display =
                            std::sync::Arc::new(context.display(frame.frame.clone()).await?);
                        raw.push(crate::journal::RawFrame {
                            stamp: frame.stamp.clone(),
                            frame: display.clone(),
                            lease: Some(frame.lease.clone()),
                        });
                        frame.recorded_display = Some(display);
                        indexes.push(index);
                    }
                    if !raw.is_empty() {
                        // Keep this bounded batch and its receive leases while
                        // storage is unavailable. The host may finish its current
                        // turn, but no new request is acted on before it is saved.
                        let saved = loop {
                            match context
                                .journal
                                .msp_raw_batch(
                                    context.track.into(),
                                    context.session.into(),
                                    raw.clone(),
                                )
                                .await
                            {
                                Ok(saved) => break saved,
                                Err(error) if context.journal.write_unavailable(error.as_ref()) => {
                                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                                }
                                Err(error) => return Err(error),
                            }
                        };
                        for (index, source) in indexes.into_iter().zip(saved) {
                            frames[index].source = Some(source);
                        }
                    }
                    Ok(frames)
                }));
                continue;
            }
            break;
        }
        Ok(())
    }
    /// All in-flight state stays in the inbox when a select! branch cancels this
    /// future. A shared receive lease also stays with the writer until commit.
    pub async fn next(&mut self, connection: &mut Connection) -> Result<Received> {
        loop {
            self.save_pending().await?;
            if self.ready.is_empty() && self.closed.is_none() {
                let event = connection.next_event().await;
                self.accept(event);
            }
            // Leases bound all held frames to the host's 8 MiB budget. Drain
            // the available prefix so a count limit cannot hide a small request.
            while self.closed.is_none() {
                match connection.try_next_event() {
                    Ok(event) => self.accept(Some(event)),
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        self.accept(None);
                        break;
                    }
                }
            }
            if !self.received.is_empty() {
                continue;
            }
            if !self.ready.is_empty() {
                let mut selected = 0;
                for (index, frame) in self.ready.iter().enumerate() {
                    let method = frame.frame["method"].as_str().unwrap_or("");
                    if matches!(method, "approval/request" | "userInput/request") {
                        selected = index;
                        break;
                    }
                    // Lifecycle, decisions, gaps and responses retain their order.
                    if !method.starts_with("item/")
                        && !matches!(
                            method,
                            "session/statusChanged" | "session/tokenUsage" | "session/contextUsage"
                        )
                    {
                        break;
                    }
                }
                return Ok(self.ready.remove(selected).unwrap());
            }
            if let Some(reason) = &self.closed {
                return Err(crate::msp::ConnectionLost(reason.clone()).into());
            }
        }
    }
    /// History recovery takes the same recorded frames in physical order. Call
    /// only after next() returns, when no raw receipt is in flight.
    pub async fn flush(&mut self, connection: &mut Connection) -> Result<()> {
        self.save_pending().await?;
        let mut frames: Vec<_> = self.ready.drain(..).map(Event::Frame).collect();
        if let Some(reason) = self.closed.take() {
            frames.push(Event::Closed(reason));
        }
        connection.restore_events(frames);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::Journal;
    use serde_json::{Value, json};
    use std::time::Duration;
    use uuid::Uuid;

    async fn buffered(values: Vec<Value>) -> Connection {
        let bytes = values
            .iter()
            .map(|v| format!("{v}\n"))
            .collect::<String>()
            .into_bytes();
        let mut connection = Connection::new(std::io::Cursor::new(bytes), tokio::io::sink());
        let mut frames = Vec::new();
        for _ in &values {
            let Some(event) = connection.next_event().await else {
                panic!("Missing fixture frame");
            };
            assert!(matches!(event, Event::Frame(_)));
            frames.push(event);
        }
        connection.restore_events(frames);
        connection
    }
    fn frame(method: &str, ordinal: i64, text: &str) -> Value {
        json!({"jsonrpc":"2.0","method":method,"params":{"sessionId":"session","ordinal":ordinal,"viewCursor":format!("cursor-{ordinal}"),"sourceRange":{},"item":{"itemId":format!("item-{ordinal}"),"kind":"agentMessage","text":text,"messagePhase":"commentary","turnId":"01900000-0000-7000-8000-000000000001"},"approvalId":"approval","currentRequirementId":{"stage":0}}})
    }
    fn context(journal: &Journal) -> Context<'_> {
        Context {
            journal,
            track: "track",
            session: "session",
            scanner: None,
        }
    }
    async fn apply(journal: &Journal, frame: Received) -> Result<()> {
        journal
            .try_msp(
                "track".into(),
                "session".into(),
                frame.stamp,
                frame.frame,
                Some(frame.lease),
                frame.source,
            )?
            .await??;
        Ok(())
    }
    #[tokio::test]
    async fn request_passes_bulky_output_but_raw_order_and_durable_cursor_do_not_change()
    -> Result<()> {
        let root = tempfile::tempdir()?;
        let journal =
            Journal::open(root.path().join("db"), crate::secrets::Key::new([7; 32])).await?;
        let mut frames = vec![frame("turn/started", 0, "")];
        frames[0]["params"]["turnId"] = json!(Uuid::now_v7());
        frames[0]["params"]["commandId"] = frames[0]["params"]["turnId"].clone();
        for ordinal in 1..=7 {
            frames.push(frame("item/completed", ordinal, &"x".repeat(700_000)));
        }
        frames.push(frame("approval/request", 8, ""));
        frames.push(frame("approval/requested", 9, ""));
        let mut connection = buffered(frames).await;
        let mut inbox = Inbox::new(context(&journal));
        let first = inbox.next(&mut connection).await?;
        assert_eq!(first.frame["method"], "turn/started");
        apply(&journal, first).await?;
        let request = inbox.next(&mut connection).await?;
        assert_eq!(request.frame["method"], "approval/request");
        apply(&journal, request).await?;
        assert!(inbox.has_pending());
        assert!(connection.client.output_flow().queued_bytes > 4 * 1024 * 1024);
        assert_eq!(
            journal
                .view_cursor("track".into(), "session".into())
                .await?
                .as_deref(),
            Some("cursor-0")
        );
        let db = rusqlite::Connection::open(root.path().join("db"))?;
        let raw=db.prepare("SELECT json_extract(payload,'$.params.ordinal') FROM events WHERE kind='msp.received' ORDER BY seq")?.query_map([],|r|r.get::<_,i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(raw, (0..=9).collect::<Vec<_>>());
        for ordinal in 1..=7 {
            let next = inbox.next(&mut connection).await?;
            assert_eq!(next.frame["params"]["ordinal"], ordinal);
            apply(&journal, next).await?;
        }
        let canonical = inbox.next(&mut connection).await?;
        assert_eq!(canonical.frame["method"], "approval/requested");
        apply(&journal, canonical).await?;
        assert_eq!(
            journal
                .view_cursor("track".into(), "session".into())
                .await?
                .as_deref(),
            Some("cursor-9")
        );
        assert_eq!(connection.client.output_flow().queued_bytes, 0);
        Ok(())
    }
    #[tokio::test]
    async fn request_never_passes_a_lifecycle_or_decision_boundary() -> Result<()> {
        for method in [
            "turn/started",
            "turn/completed",
            "approval/resolved",
            "approval/updated",
            "userInput/settled",
            "view/gap",
        ] {
            let root = tempfile::tempdir()?;
            let journal =
                Journal::open(root.path().join("db"), crate::secrets::Key::new([7; 32])).await?;
            let mut connection = buffered(vec![
                frame("item/completed", 0, "output"),
                frame(method, 1, ""),
                frame("approval/request", 2, ""),
            ])
            .await;
            let mut inbox = Inbox::new(context(&journal));
            assert_eq!(
                inbox.next(&mut connection).await?.frame["params"]["ordinal"],
                0
            );
            assert_eq!(inbox.next(&mut connection).await?.frame["method"], method);
            assert_eq!(
                inbox.next(&mut connection).await?.frame["method"],
                "approval/request"
            );
        }
        Ok(())
    }
    #[tokio::test]
    async fn cancelled_receive_keeps_frames_and_flush_restores_their_physical_order() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        let path = root.path().join("db");
        let journal = Journal::open(&path, crate::secrets::Key::new([7; 32])).await?;
        let mut lock = rusqlite::Connection::open(&path)?;
        let tx = lock.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut connection = buffered(vec![
            frame("item/completed", 0, "output"),
            frame("approval/request", 1, ""),
        ])
        .await;
        let mut inbox = Inbox::new(context(&journal));
        assert!(
            tokio::time::timeout(Duration::from_millis(30), inbox.next(&mut connection))
                .await
                .is_err()
        );
        assert!(inbox.has_pending());
        assert!(connection.client.output_flow().queued_bytes > 0);
        tx.commit()?;
        inbox.flush(&mut connection).await?;
        for ordinal in 0..=1 {
            let Some(Event::Frame(next)) = connection.next_event().await else {
                panic!("Lost a held frame");
            };
            assert_eq!(next.frame["params"]["ordinal"], ordinal);
            assert!(next.source.is_some());
        }
        let db = rusqlite::Connection::open(&path)?;
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM events WHERE kind='msp.received'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            2
        );
        Ok(())
    }
    #[tokio::test]
    async fn dropping_an_inbox_does_not_release_bytes_while_its_raw_write_is_pending() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        let path = root.path().join("db");
        let journal = Journal::open(&path, crate::secrets::Key::new([7; 32])).await?;
        let mut lock = rusqlite::Connection::open(&path)?;
        let tx = lock.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut connection = buffered(vec![frame("item/completed", 0, "output")]).await;
        let mut inbox = Inbox::new(context(&journal));
        assert!(
            tokio::time::timeout(Duration::from_millis(30), inbox.next(&mut connection))
                .await
                .is_err()
        );
        drop(inbox);
        assert!(connection.client.output_flow().queued_bytes > 0);
        tx.commit()?;
        journal.snapshot().await?;
        assert_eq!(connection.client.output_flow().queued_bytes, 0);
        Ok(())
    }
    #[cfg(all(feature = "fault-injection", debug_assertions))]
    #[tokio::test]
    async fn sqlite_full_retains_the_same_bounded_batch_until_storage_recovers() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("db");
        let journal = Journal::open(&path, crate::secrets::Key::new([7; 32])).await?;
        journal
            .test_fault(crate::fault_injection::Request::Fill)
            .await?;
        let mut connection = buffered(vec![frame("item/completed", 0, "small output")]).await;
        let mut inbox = Inbox::new(context(&journal));
        assert!(
            tokio::time::timeout(Duration::from_millis(150), inbox.next(&mut connection))
                .await
                .is_err()
        );
        assert!(inbox.has_pending());
        assert!(connection.client.output_flow().queued_bytes > 0);
        journal
            .test_fault(crate::fault_injection::Request::Restore)
            .await?;
        let received =
            tokio::time::timeout(Duration::from_secs(2), inbox.next(&mut connection)).await??;
        assert_eq!(received.frame["params"]["ordinal"], 0);
        assert!(received.source.is_some());
        drop(received);
        let db = rusqlite::Connection::open(path)?;
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM events WHERE kind='msp.received'",
                [],
                |row| row.get::<_, i64>(0)
            )?,
            1
        );
        assert_eq!(connection.client.output_flow().queued_bytes, 0);
        Ok(())
    }
    #[tokio::test]
    async fn prioritized_repair_requests_are_redacted_before_receipt_is_saved() -> Result<()> {
        let root = tempfile::tempdir()?;
        let journal =
            Journal::open(root.path().join("db"), crate::secrets::Key::new([7; 32])).await?;
        let secret = "private-priority-fixture";
        let scanner = crate::secret_scan::Engine::capture()?.start(&[secret.into()])?;
        let mut connection = buffered(vec![
            frame("item/completed", 0, secret),
            frame("approval/request", 1, secret),
        ])
        .await;
        let mut inbox = Inbox::new(Context {
            scanner: Some(&scanner),
            ..context(&journal)
        });
        assert_eq!(
            inbox.next(&mut connection).await?.frame["method"],
            "approval/request"
        );
        let db = rusqlite::Connection::open(root.path().join("db"))?;
        let raw = db
            .prepare("SELECT payload FROM events WHERE kind='msp.received'")?
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        assert_eq!(raw.len(), 2);
        assert!(raw.iter().all(|value| !value.contains(secret)));
        Ok(())
    }
    #[tokio::test]
    async fn request_is_not_hidden_after_many_small_output_frames() -> Result<()> {
        let root = tempfile::tempdir()?;
        let journal =
            Journal::open(root.path().join("db"), crate::secrets::Key::new([7; 32])).await?;
        let frames = (0..1000)
            .map(|n| frame("item/updated", n, "small output"))
            .chain(std::iter::once(frame("approval/request", 1000, "")))
            .collect();
        let mut connection = buffered(frames).await;
        let mut inbox = Inbox::new(context(&journal));
        let request = inbox.next(&mut connection).await?;
        assert_eq!(request.frame["method"], "approval/request");
        let first = inbox.next(&mut connection).await?;
        assert_eq!(first.frame["params"]["ordinal"], 0);
        let db = rusqlite::Connection::open(root.path().join("db"))?;
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM events WHERE kind='msp.received'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            1001
        );
        Ok(())
    }
    #[tokio::test]
    async fn a_failed_batch_never_leaves_a_partial_raw_prefix() -> Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("db");
        let journal = Journal::open(&path, crate::secrets::Key::new([7; 32])).await?;
        let db = rusqlite::Connection::open(&path)?;
        db.execute_batch("CREATE TRIGGER reject_second_raw BEFORE INSERT ON events WHEN NEW.kind='msp.received' AND json_extract(NEW.payload,'$.params.ordinal')=1 BEGIN SELECT RAISE(FAIL,'injected batch failure'); END;")?;
        let frames = (0..3)
            .map(|ordinal| crate::journal::RawFrame {
                stamp: crate::clock::Stamp::now(),
                frame: std::sync::Arc::new(frame("item/updated", ordinal, "")),
                lease: None,
            })
            .collect();
        assert!(
            journal
                .msp_raw_batch("track".into(), "session".into(), frames)
                .await
                .is_err()
        );
        assert_eq!(
            db.query_row(
                "SELECT COUNT(*) FROM events WHERE kind='msp.received'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            0
        );
        Ok(())
    }
}
