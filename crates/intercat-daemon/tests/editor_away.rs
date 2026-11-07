use intercat_daemon::{Result, editor_away::Query, journal::Journal, secrets::Key};
use rusqlite::{Connection, params};
use serde_json::json;

async fn fixture() -> Result<(tempfile::TempDir, Journal, Connection)> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("dummy.db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let db = Connection::open(path)?;
    Ok((root, journal, db))
}
fn entry(db: &Connection, id: i64, at: i64, scene: &str) -> Result<()> {
    db.execute(
        "INSERT INTO editor_entries(id,source,track,scene,title,line,detail,created_at,expires_at)
        VALUES(?1,?2,'a',?3,'Dummy title','Original dummy speech','Original dummy detail',?4,0)",
        params![id, format!("dummy/{id}"), scene, at],
    )?;
    Ok(())
}

#[tokio::test]
async fn interval_keeps_expired_original_speech_and_pages_beyond_status_limit() -> Result<()> {
    let (root, journal, db) = fixture().await?;
    entry(&db, 1, 1000, "fork")?;
    for id in 2..=111 {
        entry(&db, id, 2000, "masked")?;
    }
    entry(&db, 112, 3000, "reply")?;
    entry(&db, 113, 2000, "interpretation")?;
    let mut before = None;
    let mut ids = vec![];
    loop {
        let page = journal
            .editor_away(Query {
                from: 1000,
                to: 2000,
                before,
            })
            .await?;
        assert_eq!(page["updates"], 110);
        assert_eq!(page["effects"], 0);
        for entry in page["entries"].as_array().unwrap() {
            assert_eq!(entry["line"], "Original dummy speech");
            ids.push(entry["id"].as_i64().unwrap());
        }
        before = page["before"].as_i64();
        if before.is_none() {
            break;
        }
    }
    assert_eq!(ids, (2..=111).rev().collect::<Vec<_>>());
    drop(journal);
    drop(db);
    let reopened = Journal::open(root.path().join("dummy.db"), Key::new([7; 32])).await?;
    assert_eq!(
        reopened
            .editor_away(Query {
                from: 1000,
                to: 2000,
                before: None
            })
            .await?["updates"],
        110
    );
    Ok(())
}

#[tokio::test]
async fn counts_recorded_effects_and_closed_clips_without_claiming_work_is_finished() -> Result<()>
{
    let (_root, journal, db) = fixture().await?;
    for (id, phase) in [
        ("a", "ready"),
        ("b", "approval"),
        ("c", "question"),
        ("d", "working"),
    ] {
        db.execute(
            "INSERT INTO tracks VALUES(?1,?2,'Dummy','waiting')",
            params![id, id.to_uppercase()],
        )?;
        db.execute(
            "INSERT INTO workers VALUES(?1,?2)",
            params![id, json!({"phase":phase}).to_string()],
        )?;
    }
    db.execute(
        "INSERT INTO report_questions VALUES('q','r','b','open','{}')",
        [],
    )?;
    for (id, at) in [("past", 1000), ("during", 2000), ("future", 3000)] {
        db.execute(
            "INSERT INTO effects VALUES(?1,'a','dummy-session','item',NULL,?2)",
            params![id, json!({"at":at}).to_string()],
        )?;
    }
    for (id, end) in [
        ("past", json!({"epochMs":1000})),
        ("during", json!({"epochMs":2000})),
        ("running", json!(null)),
    ] {
        db.execute(
            "INSERT INTO timeline_clips VALUES(?1,?1,1,?2)",
            params![id, json!({"end":end}).to_string()],
        )?;
    }
    let summary = journal
        .editor_away(Query {
            from: 1000,
            to: 2000,
            before: None,
        })
        .await?;
    assert_eq!(summary["waiting"], json!(["B", "C"]));
    assert_eq!(summary["effects"], 1);
    assert_eq!(summary["clips"], 1);
    assert!(summary.get("finished").is_none());
    Ok(())
}

#[tokio::test]
async fn rejects_reversed_or_unsafe_intervals_and_invalid_cursors() -> Result<()> {
    let (_root, journal, _db) = fixture().await?;
    for query in [
        Query {
            from: 2,
            to: 1,
            before: None,
        },
        Query {
            from: -1,
            to: 1,
            before: None,
        },
        Query {
            from: 0,
            to: i64::MAX,
            before: None,
        },
        Query {
            from: 0,
            to: 1,
            before: Some(0),
        },
    ] {
        assert!(journal.editor_away(query).await.is_err());
    }
    Ok(())
}
