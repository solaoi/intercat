//! Explicit development-only storage faults used by the native acceptance tests.
use crate::Result;
use rusqlite::{Connection, ErrorCode, params};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    Fill,
    Restore,
    Pause { millis: u64 },
}

pub fn apply(connection: &mut Connection, request: Request) -> Result<Value> {
    match request {
        Request::Fill => {
            let pages: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
            connection.pragma_update(None, "max_page_count", pages)?;
            let mut added_rows = 0;
            let mut added_bytes = 0;
            for size in [
                65536, 32768, 16384, 8192, 4096, 2048, 1024, 512, 256, 128, 64, 32, 16, 8, 4, 2, 1,
                0,
            ] {
                let payload = if size == 0 {
                    "0".to_owned()
                } else {
                    json!({"padding":"x".repeat(size)}).to_string()
                };
                loop {
                    if added_rows >= 128 {
                        connection.pragma_update(None, "max_page_count", 1_073_741_823_i64)?;
                        connection.execute("DELETE FROM events WHERE kind='!'", [])?;
                        return Err(
                            "The fixture has too much free space for this bounded fault test."
                                .into(),
                        );
                    }
                    if added_bytes + payload.len() > 65536 {
                        break;
                    }
                    match connection.execute(
                        "INSERT INTO events(ts,kind,payload) VALUES(0,'!',?1)",
                        params![payload],
                    ) {
                        Ok(_) => {
                            added_rows += 1;
                            added_bytes += payload.len();
                        }
                        Err(rusqlite::Error::SqliteFailure(error, _))
                            if error.code == ErrorCode::DiskFull =>
                        {
                            break;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
            }
            if added_rows >= 128 || added_bytes + 1 > 65536 {
                connection.pragma_update(None, "max_page_count", 1_073_741_823_i64)?;
                connection.execute("DELETE FROM events WHERE kind='!'", [])?;
                return Err("The fixture exceeds the bounded fault-test budget.".into());
            }
            let error = connection
                .execute("INSERT INTO events(ts,kind,payload) VALUES(0,'!','0')", [])
                .unwrap_err();
            if !matches!(error,rusqlite::Error::SqliteFailure(ref value,_) if value.code==ErrorCode::DiskFull)
            {
                return Err(error.into());
            }
            let after_pages: i64 =
                connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
            if after_pages != pages {
                return Err("The bounded fault test unexpectedly grew the database.".into());
            }
            Ok(
                json!({"errorCode":"SQLITE_FULL","pages":pages,"addedRows":added_rows,"addedBytes":added_bytes,"databasePagesUnchanged":true}),
            )
        }
        Request::Restore => {
            connection.pragma_update(None, "max_page_count", 1_073_741_823_i64)?;
            connection.execute("DELETE FROM events WHERE kind='!'", [])?;
            Ok(json!({"restored":true}))
        }
        Request::Pause { .. } => Err("Pause is handled by the writer queue.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fills_real_sqlite_pages_and_can_restore_writes() -> Result<()> {
        let root = tempfile::tempdir()?;
        let mut db = Connection::open(root.path().join("db"))?;
        db.execute_batch("CREATE TABLE events(seq INTEGER PRIMARY KEY AUTOINCREMENT,ts INTEGER NOT NULL,kind TEXT NOT NULL,payload TEXT NOT NULL);")?;
        let result = apply(&mut db, Request::Fill)?;
        assert_eq!(result["errorCode"], "SQLITE_FULL");
        assert_eq!(result["databasePagesUnchanged"], true);
        assert!(result["addedRows"].as_u64().unwrap() <= 128);
        assert!(result["addedBytes"].as_u64().unwrap() <= 65536);
        assert!(
            matches!(db.execute("INSERT INTO events(ts,kind,payload) VALUES(1,'real','{}')",[]),Err(rusqlite::Error::SqliteFailure(error,_)) if error.code==ErrorCode::DiskFull)
        );
        apply(&mut db, Request::Restore)?;
        db.execute(
            "INSERT INTO events(ts,kind,payload) VALUES(1,'real','{}')",
            [],
        )?;
        Ok(())
    }
    #[tokio::test]
    async fn an_unsaved_stop_precedes_later_writes_and_is_marked_after_recovery() -> Result<()> {
        use crate::{
            decision::{Decision, Kind, Outcome, Target},
            journal::Journal,
        };
        for global in [false, true] {
            let root = tempfile::tempdir()?;
            let journal =
                Journal::open(root.path().join("db"), crate::secrets::Key::new([7; 32])).await?;
            journal
                .stage_utterance(
                    uuid::Uuid::now_v7(),
                    "Existing request".into(),
                    Target::New,
                    None,
                    vec![],
                )
                .await?;
            let word = journal.claim_input().await?.unwrap();
            journal
                .apply_input(
                    word.id,
                    word.revision,
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
                .unwrap()
                .to_owned();
            journal.test_fault(Request::Fill).await?;
            let held = journal.clone();
            let id = uuid::Uuid::now_v7();
            let waiting = tokio::spawn(async move {
                if global {
                    held.pause_dispatch(id, true).await
                } else {
                    held.hold_inputs(id, track).await
                }
            });
            for _ in 0..100 {
                if journal.snapshot().await?["logUnavailable"] == true {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
            assert_eq!(journal.snapshot().await?["logUnavailable"], true);
            assert!(!waiting.is_finished());
            assert!(
                journal
                    .stage_utterance(
                        uuid::Uuid::now_v7(),
                        "Must not be saved".into(),
                        Target::New,
                        None,
                        vec![]
                    )
                    .await
                    .is_err()
            );
            journal.test_fault(Request::Restore).await?;
            waiting.await??;
            let state = journal.snapshot().await?;
            assert_eq!(state["logUnavailable"], false);
            assert_eq!(state["utterances"][0]["state"], "held");
            assert_eq!(state["unloggedControls"][0]["opId"], id.to_string());
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_stop_waiting_behind_a_slow_write_holds_that_input_and_marks_the_delay() -> Result<()>
    {
        use crate::{decision::Target, journal::Journal};
        let root = tempfile::tempdir()?;
        let journal =
            Journal::open(root.path().join("db"), crate::secrets::Key::new([7; 32])).await?;
        journal.test_fault(Request::Pause { millis: 2000 }).await?;
        let writer = journal.clone();
        let input = tokio::spawn(async move {
            writer
                .stage_utterance(
                    uuid::Uuid::now_v7(),
                    "Queued before Stop".into(),
                    Target::New,
                    None,
                    vec![],
                )
                .await
        });
        tokio::task::yield_now().await;
        let stop = uuid::Uuid::now_v7();
        journal.pause_dispatch(stop, true).await?;
        input.await??;
        let state = journal.snapshot().await?;
        assert_eq!(state["utterances"][0]["state"], "held");
        assert_eq!(state["unloggedControls"][0]["opId"], stop.to_string());
        Ok(())
    }
}
