use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Target},
    edit_hold::{Action, Boundary, Edit, Request},
    input_feedback::Misread,
    journal::Journal,
    secrets::Key,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    Ok((root, journal))
}

async fn placed(journal: &Journal, text: &str) -> Result<(Uuid, String)> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, text.into(), Target::None, None, vec![])
        .await?;
    let pending = journal.claim_input().await?.unwrap();
    assert_eq!(pending.id, id.to_string());
    journal
        .apply_input(
            pending.id,
            pending.revision,
            Outcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: Some("now".into()),
                command: None,
                confidence: 0.95,
                inference_ms: 8.0,
                timed_out: false,
                scores: json!({"model":"original"}),
            }),
        )
        .await?;
    let word = journal.read_utterance(id.to_string()).await?;
    Ok((id, word["decision"]["trackId"].as_str().unwrap().into()))
}

async fn landed(journal: &Journal) -> Result<Uuid> {
    let (id, _) = placed(journal, "Original instruction 日本語").await?;
    // The execution transition itself is covered by storyline's MSP receipt test.
    Ok(id)
}

fn arrive(root: &tempfile::TempDir, id: Uuid) -> Result<()> {
    let db = Connection::open(root.path().join("db"))?;
    intercat_daemon::clock::register(&db)?;
    db.execute(
        "UPDATE utterances SET state='landed' WHERE id=?1",
        [id.to_string()],
    )?;
    Ok(())
}

fn events(root: &tempfile::TempDir) -> Result<Vec<Value>> {
    let db = Connection::open(root.path().join("db"))?;
    db.prepare("SELECT payload FROM events WHERE kind='utterance.corrected' ORDER BY seq")?
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|row| Ok(serde_json::from_str(&row?)?))
        .collect()
}

async fn read_as(journal: &Journal, input: Uuid, track: &str, boundary: Boundary) -> Result<Value> {
    let word = journal.read_utterance(input.to_string()).await?;
    let (owner, token) = (Uuid::now_v7(), Uuid::now_v7());
    let opened = journal
        .edit_input(Request {
            owner,
            action: Action::Begin {
                token,
                input: input.to_string(),
                revision: word["revision"].as_i64().unwrap(),
            },
        })
        .await?;
    journal
        .edit_input(Request {
            owner,
            action: Action::End {
                token,
                revision: opened["revision"].as_i64().unwrap(),
                edit: Edit::ReadAs {
                    track: track.into(),
                    boundary,
                    turn: None,
                },
            },
        })
        .await
}

#[tokio::test]
async fn misread_is_durable_idempotent_and_does_not_change_the_sent_instruction() -> Result<()> {
    let (root, journal) = fixture().await?;
    let id = landed(&journal).await?;
    arrive(&root, id)?;
    let before = journal.read_utterance(id.to_string()).await?;
    let request = Misread {
        op_id: Uuid::now_v7(),
        utterance_id: id,
        revision: 1,
    };
    for request in [
        request.clone(),
        request.clone(),
        Misread {
            op_id: Uuid::now_v7(),
            ..request.clone()
        },
    ] {
        let result = journal.mark_misread(request).await?;
        assert_eq!(
            result["correction"],
            json!({"explicit":1,"inferred":0,"misread":true})
        );
        for field in ["text", "state", "revision", "decision", "target"] {
            assert_eq!(result[field], before[field]);
        }
    }
    let recorded = events(&root)?;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["kind"], "explicit");
    assert_eq!(recorded[0]["reason"], "misread");
    assert_eq!(recorded[0]["before"]["scores"]["model"], "original");
    assert!(recorded[0]["decisionSequence"].as_i64().is_some());
    drop(journal);
    let reopened = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(
        reopened.mark_misread(request).await?["correction"]["explicit"],
        1
    );
    let next = Uuid::now_v7();
    reopened
        .stage_utterance(
            next,
            "Follow the previous request".into(),
            Target::None,
            None,
            vec![],
        )
        .await?;
    let context = reopened.claim_input().await?.unwrap().history;
    let row = context
        .decisions
        .iter()
        .find(|row| row["title"] == "Original instruction 日本語")
        .unwrap();
    assert_eq!(row["corrected"], true);
    assert_eq!(row["misread"], true);
    assert_eq!(row["correctionKind"], "explicit");
    Ok(())
}

#[tokio::test]
async fn only_a_landed_current_revision_accepts_misread_and_operation_ids_cannot_be_reused()
-> Result<()> {
    let (root, journal) = fixture().await?;
    let (id, _) = placed(&journal, "Not sent yet").await?;
    let request = Misread {
        op_id: Uuid::now_v7(),
        utterance_id: id,
        revision: 1,
    };
    assert!(journal.mark_misread(request.clone()).await.is_err());
    arrive(&root, id)?;
    assert!(
        journal
            .mark_misread(Misread {
                revision: 2,
                ..request.clone()
            })
            .await
            .is_err()
    );
    assert!(
        journal
            .mark_misread(Misread {
                op_id: Uuid::nil(),
                ..request.clone()
            })
            .await
            .is_err()
    );
    assert!(events(&root)?.is_empty());
    journal.mark_misread(request.clone()).await?;
    assert!(
        journal
            .mark_misread(Misread {
                revision: 2,
                ..request
            })
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn changed_read_as_is_explicit_but_same_reading_and_automatic_placement_are_not() -> Result<()>
{
    let (root, journal) = fixture().await?;
    let (id, first) = placed(&journal, "Original instruction").await?;
    let (_, second) = placed(&journal, "Second worker").await?;
    assert!(events(&root)?.is_empty());
    read_as(&journal, id, &first, Boundary::WhenTold).await?;
    let first_event = events(&root)?;
    assert_eq!(first_event.len(), 1);
    assert_eq!(first_event[0]["before"]["kind"], "fork");
    assert_eq!(first_event[0]["after"]["boundary"], "when-told");
    read_as(&journal, id, &first, Boundary::WhenTold).await?;
    assert_eq!(events(&root)?.len(), 1);
    read_as(&journal, id, &second, Boundary::WhenTold).await?;
    read_as(&journal, id, &second, Boundary::AfterTurn).await?;
    assert_eq!(events(&root)?.len(), 3);
    let current = journal.read_utterance(id.to_string()).await?;
    assert_eq!(
        current["correction"],
        json!({"explicit":3,"inferred":0,"misread":false})
    );
    journal.cut_input(Uuid::now_v7(), id.to_string()).await?;
    assert_eq!(events(&root)?.len(), 3);
    Ok(())
}

#[tokio::test]
async fn a_failed_correction_rolls_back_its_receipt_and_the_same_operation_can_retry() -> Result<()>
{
    let (root, journal) = fixture().await?;
    let id = landed(&journal).await?;
    arrive(&root, id)?;
    let db = Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER refuse_correction BEFORE INSERT ON operations WHEN NEW.kind='utterance.misread' BEGIN SELECT RAISE(ABORT,'injected write failure'); END;")?;
    let request = Misread {
        op_id: Uuid::now_v7(),
        utterance_id: id,
        revision: 1,
    };
    assert!(journal.mark_misread(request.clone()).await.is_err());
    assert!(events(&root)?.is_empty());
    assert!(journal.read_utterance(id.to_string()).await?["correction"].is_null());
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM operations WHERE id=?1",
            [request.op_id.to_string()],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    db.execute_batch("DROP TRIGGER refuse_correction;")?;
    let other = Misread {
        op_id: Uuid::now_v7(),
        ..request.clone()
    };
    let (a, b) = tokio::join!(journal.mark_misread(request), journal.mark_misread(other));
    assert_eq!(a?["correction"]["explicit"], 1);
    assert_eq!(b?["correction"]["explicit"], 1);
    assert_eq!(events(&root)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn read_as_failure_rolls_back_both_the_correction_and_the_decision() -> Result<()> {
    let (root, journal) = fixture().await?;
    let (id, first) = placed(&journal, "Original instruction").await?;
    let db = Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER refuse_reading BEFORE UPDATE OF decision ON utterances BEGIN SELECT RAISE(ABORT,'injected reading failure'); END;")?;
    assert!(
        read_as(&journal, id, &first, Boundary::WhenTold)
            .await
            .is_err()
    );
    assert!(events(&root)?.is_empty());
    assert_eq!(
        journal.read_utterance(id.to_string()).await?["decision"]["kind"],
        "fork"
    );
    db.execute("DELETE FROM operations WHERE kind='utterance.edit'", [])?;
    db.execute_batch("DROP TRIGGER refuse_reading;")?;
    read_as(&journal, id, &first, Boundary::WhenTold).await?;
    assert_eq!(events(&root)?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn authenticated_rpc_marks_old_requests_outside_the_status_window() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (root, journal) = fixture().await?;
    let id = landed(&journal).await?;
    arrive(&root, id)?;
    let db = Connection::open(root.path().join("db"))?;
    intercat_daemon::clock::register(&db)?;
    for index in 0..101 {
        db.execute("INSERT INTO utterances(id,text,created_at,target,state,revision) VALUES(?1,'Later',?2,'{\"mode\":\"none\"}','landed',1)", params![Uuid::now_v7().to_string(), i64::MAX - index])?;
    }
    assert!(
        !journal.snapshot().await?["utterances"]
            .as_array()
            .unwrap()
            .iter()
            .any(|word| word["id"] == id.to_string())
    );
    let _lock = intercat_daemon::lock::DaemonLock::acquire(root.path())?.unwrap();
    let token = "a5".repeat(32);
    let server = intercat_daemon::rpc::Server::bind(
        root.path(),
        intercat_daemon::rpc::Token::parse(&token)?,
        journal,
        intercat_daemon::editor::Manager::new(root.path(), &root.path().join("editor.log"))?,
        &root.path().join("workspaces"),
    )?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    // Authentication failure closes the connection before any mutation is routed.
    let mut unauthenticated =
        BufReader::new(tokio::net::UnixStream::connect(root.path().join("rpc.sock")).await?);
    unauthenticated.get_mut().write_all(format!("{}\n", json!({"jsonrpc":"2.0","id":1,"method":"utterance.misread","params":{"opId":Uuid::now_v7(),"utteranceId":id,"revision":1}})).as_bytes()).await?;
    let mut rejected = String::new();
    unauthenticated.read_line(&mut rejected).await?;
    assert!(serde_json::from_str::<Value>(&rejected)?["error"].is_object());
    assert!(events(&root)?.is_empty());
    let mut stream =
        BufReader::new(tokio::net::UnixStream::connect(root.path().join("rpc.sock")).await?);
    let request = json!({"opId":Uuid::now_v7(),"utteranceId":id,"revision":1});
    for (method, params, accepted) in [
        ("rpc.authenticate", json!({"token":token}), true),
        ("utterance.misread", request.clone(), true),
        ("utterance.misread", request, true),
        (
            "utterance.misread",
            json!({"opId":Uuid::now_v7(),"utteranceId":id,"revision":1,"unknown":true}),
            false,
        ),
        ("utterance.read", json!({"utteranceId":id}), true),
    ] {
        stream
            .get_mut()
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await?;
        let mut line = String::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            stream.read_line(&mut line),
        )
        .await??;
        let result: Value = serde_json::from_str(&line)?;
        assert_eq!(result.get("result").is_some(), accepted, "{result}");
        if accepted && method != "rpc.authenticate" {
            assert_eq!(result["result"]["correction"]["explicit"], 1);
        }
    }
    stop.send(()).unwrap();
    task.await??;
    assert_eq!(events(&root)?.len(), 1);
    Ok(())
}
