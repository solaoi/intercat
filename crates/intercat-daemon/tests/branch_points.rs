use intercat_daemon::{
    Result,
    branch_points::{Page, Query},
    checkpoint::Checkpoint,
    journal::Journal,
    preparation::Workspace,
    secrets::Key,
};
use rusqlite::{Connection, params};
use serde_json::json;
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal, Connection)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let db = Connection::open(root.path().join("db"))?;
    for track in ["A", "B"] {
        db.execute(
            "INSERT INTO tracks VALUES(?1,?1,'Dummy saved work','done')",
            [track],
        )?;
        let workspace = Workspace {
            track_id: track.into(),
            path: root.path().join(track),
            reference: format!("refs/intercat/dummy/tracks/{track}"),
            base: "0".repeat(40),
            state: "ready".into(),
            error: None,
            attachment_roots: vec![],
        };
        journal.workspace_result(workspace).await?;
    }
    Ok((root, journal, db))
}

async fn checkpoint(
    journal: &Journal,
    db: &Connection,
    track: &str,
    number: i64,
    pending: bool,
) -> Result<Uuid> {
    let turn = Uuid::now_v7();
    db.execute(
        "INSERT INTO timeline_turns VALUES(?1,?2,?3)",
        params![
            format!("session-{track}"),
            turn.to_string(),
            json!({"track":track,"repair_run_id":null,"current":null,"index":1,"terminal":true})
                .to_string()
        ],
    )?;
    let clip = Uuid::now_v7().to_string();
    let stamp =
        json!({"epochMs":number*1000,"timelineMs":number*100,"monoMs":number*100,"bootId":"dummy"});
    db.execute("INSERT INTO timeline_clips VALUES(?1,?2,?3,?4)",params![clip,track,number,json!({"id":clip,"trackId":track,"turnId":turn,"ordinal":number,"sessionId":format!("session-{track}"),"start":stamp,"end":stamp,"generationEnd":null,"repairRunId":null,"category":"step","shellItem":null,"recovered":false}).to_string()])?;
    let id = Uuid::new_v5(&turn, b"checkpoint").to_string();
    let value = Checkpoint {
        turn_id: turn,
        sha: format!("{number:040x}"),
        reference: format!("refs/intercat/dummy/tracks/{track}-checkpoints/{turn}"),
        unmerged_paths: vec![],
        live_processes: number == 2,
    };
    journal
        .intent(
            &id,
            "checkpoint.save",
            json!({"trackId":track,"checkpoint":value}),
        )
        .await?;
    if !pending {
        journal.finish(&id, "dispatched").await?;
        journal.finish(&id, "complete").await?;
    }
    Ok(turn)
}

async fn page(journal: &Journal, track: &str, before: Option<i64>) -> Result<Page> {
    journal
        .branch_points(Query {
            track_id: track.into(),
            before,
        })
        .await
}

#[tokio::test]
async fn points_are_paged_without_losing_old_turns_when_new_ones_arrive() -> Result<()> {
    let (_root, journal, db) = fixture().await?;
    let mut turns = Vec::new();
    for number in 1..=55 {
        turns.push(checkpoint(&journal, &db, "A", number, false).await?);
    }
    checkpoint(&journal, &db, "B", 1, false).await?;
    checkpoint(&journal, &db, "A", 56, true).await?;
    let first = page(&journal, "A", None).await?;
    assert_eq!(first.points.len(), 50);
    assert_eq!(first.points[0].turn_id, turns[54]);
    assert!(first.points.iter().all(|p| p.available));
    checkpoint(&journal, &db, "A", 57, false).await?;
    let second = page(&journal, "A", first.before).await?;
    assert_eq!(second.points.len(), 5);
    assert!(second.before.is_none());
    let all = first
        .points
        .iter()
        .chain(&second.points)
        .map(|p| p.turn_id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(all.len(), 55);
    assert!(turns.iter().all(|t| all.contains(t)));
    assert_eq!(
        db.query_row("SELECT count(*) FROM branches", [], |row| row
            .get::<_, i64>(0))?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn points_use_saved_turn_ownership_and_end_times() -> Result<()> {
    let (_root, journal, db) = fixture().await?;
    let turn = checkpoint(&journal, &db, "A", 2, false).await?;
    let result = page(&journal, "A", None).await?;
    let point = &result.points[0];
    assert_eq!(point.turn_id, turn);
    assert_eq!(point.ended_at, Some(2000));
    assert_eq!(point.timeline_at, Some(200));
    assert!(point.shell_was_running);
    assert!(point.available);
    db.execute("UPDATE timeline_clips SET record=json_set(record,'$.recovered',json('true')) WHERE track='A'",[])?;
    let recovered = page(&journal, "A", None).await?;
    assert!(recovered.points[0].ended_at.is_none());
    assert!(recovered.points[0].timeline_at.is_none());
    // A duplicate turn owner cannot silently become a valid branch target.
    db.execute(
        "INSERT INTO timeline_turns VALUES('other-session',?1,?2)",
        params![turn.to_string(), json!({"track":"A"}).to_string()],
    )?;
    assert!(!page(&journal, "A", None).await?.points[0].available);
    Ok(())
}

#[tokio::test]
async fn unresolved_and_changed_checkpoint_sources_are_disabled() -> Result<()> {
    let (_root, journal, db) = fixture().await?;
    let turn = checkpoint(&journal, &db, "A", 1, false).await?;
    let id = Uuid::new_v5(&turn, b"checkpoint").to_string();
    db.execute("UPDATE operations SET payload=json_set(payload,'$.checkpoint.unmergedPaths',json('[\"dummy.txt\"]')) WHERE id=?1",[&id])?;
    assert!(!page(&journal, "A", None).await?.points[0].available);
    db.execute("UPDATE operations SET payload=json_set(payload,'$.checkpoint.unmergedPaths',json('[]'),'$.checkpoint.reference','refs/wrong') WHERE id=?1",[&id])?;
    assert!(!page(&journal, "A", None).await?.points[0].available);
    assert_eq!(
        db.query_row("SELECT count(*) FROM tracks", [], |row| row
            .get::<_, i64>(0))?,
        2
    );
    Ok(())
}

#[tokio::test]
async fn invalid_queries_fail_and_empty_tracks_have_no_checkpoint() -> Result<()> {
    let (_root, journal, _db) = fixture().await?;
    assert!(page(&journal, "A", None).await?.points.is_empty());
    assert!(page(&journal, "missing", None).await.is_err());
    assert!(page(&journal, "A", Some(0)).await.is_err());
    assert!(page(&journal, "A", Some(-1)).await.is_err());
    Ok(())
}

#[tokio::test]
async fn authenticated_rpc_reads_points_without_creating_a_branch() -> Result<()> {
    use intercat_daemon::{
        editor::Manager,
        lock::DaemonLock,
        rpc::{Server, Token},
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let (root, journal, db) = fixture().await?;
    let turn = checkpoint(&journal, &db, "A", 1, false).await?;
    let socket = tempfile::tempdir_in("/private/tmp")?;
    let _lock = DaemonLock::acquire(socket.path())?.unwrap();
    std::fs::create_dir(root.path().join("editor-source"))?;
    let editor = Manager::new(
        &root.path().join("editor-source"),
        &root.path().join("editor.log"),
    )?;
    let token = "b7".repeat(32);
    let server = Server::bind(
        socket.path(),
        Token::parse(&token)?,
        journal.clone(),
        editor.clone(),
        &root.path().join("workspaces"),
    )?;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(async {
        let _ = stopped.await;
    }));
    let result = async {
        let mut stream =
            BufReader::new(tokio::net::UnixStream::connect(socket.path().join("rpc.sock")).await?);
        for (method, params) in [
            ("rpc.authenticate", json!({"token":token})),
            ("branch.points", json!({"trackId":"A","before":null})),
        ] {
            stream
                .get_mut()
                .write_all(
                    format!(
                        "{}\n",
                        json!({"jsonrpc":"2.0","id":method,"method":method,"params":params})
                    )
                    .as_bytes(),
                )
                .await?;
            let mut line = String::new();
            stream.read_line(&mut line).await?;
            let response: serde_json::Value = serde_json::from_str(&line)?;
            assert!(response.get("error").is_none(), "{response}");
            if method == "branch.points" {
                assert_eq!(response["result"]["points"][0]["turnId"], turn.to_string());
            }
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    let _ = stop.send(());
    task.await??;
    editor.stop().await;
    result?;
    assert_eq!(
        db.query_row("SELECT count(*) FROM branches", [], |r| r.get::<_, i64>(0))?,
        0
    );
    Ok(())
}
