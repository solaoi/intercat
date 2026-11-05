use intercat_daemon::{Result, journal::Journal, timeline::Window};
use serde_json::json;

#[tokio::test]
async fn a_large_timeline_pages_old_tracks_and_delivers_only_changed_clips() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("events.sqlite");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let mut db = rusqlite::Connection::open(path)?;
    let tx = db.transaction()?;
    {
        let mut insert = tx.prepare("INSERT INTO timeline_clips VALUES(?1,?2,?3,?4)")?;
        for track in 0..300 {
            for ordinal in 0..300 {
                let id = format!("{track}-{ordinal}");
                let owner = format!("load-{track}");
                let at = 1_000 + ordinal * 1_000;
                let stamp =
                    |at| json!({"epochMs":at,"monoMs":at,"bootId":"fixture","timelineMs":at});
                let record = json!({"id":id,"trackId":owner,"repairRunId":null,"ordinal":ordinal+1,"sessionId":"fixture","turnId":"fixture","start":stamp(at),"end":stamp(at+500),"generationEnd":null,"category":"step","shellItem":null,"recovered":false});
                insert.execute(rusqlite::params![
                    id,
                    owner,
                    ordinal + 1,
                    record.to_string()
                ])?;
            }
        }
    }
    tx.commit()?;
    let request = Window {
        tracks: vec!["load-0".into(), "load-299".into()],
        start: 0,
        end: 400_000,
        since: None,
    };
    let first = journal.timeline_window(request.clone()).await?;
    assert_eq!(first["clips"].as_array().unwrap().len(), 600);
    assert!(
        first["clips"]
            .as_array()
            .unwrap()
            .iter()
            .any(|clip| clip["id"] == "0-0")
    );
    assert!(journal.snapshot().await?["timeline"].get("clips").is_none());
    let mut next = request.clone();
    next.since = first["revision"].as_i64();
    let unchanged = journal.timeline_window(next.clone()).await?;
    assert_eq!(unchanged["reset"], false);
    assert_eq!(unchanged["clips"], json!([]));
    db.execute(
        "UPDATE timeline_clips SET record=json_set(record,'$.recovered',json('true')) WHERE id='0-0'",
        [],
    )?;
    db.execute("DELETE FROM timeline_clips WHERE id='299-1'", [])?;
    db.execute(
        "UPDATE timeline_clips SET record=json_set(record,'$.recovered',json('true')) WHERE id='100-0'",
        [],
    )?;
    let changed = journal.timeline_window(next).await?;
    assert_eq!(changed["clips"].as_array().unwrap().len(), 1);
    assert_eq!(changed["clips"][0]["id"], "0-0");
    assert_eq!(changed["clips"][0]["approximate"], true);
    assert_eq!(changed["removed"], json!(["299-1"]));
    let narrow = journal
        .timeline_window(Window {
            tracks: vec!["load-0".into()],
            start: 1000,
            end: 1000,
            since: None,
        })
        .await?;
    assert_eq!(narrow["clips"].as_array().unwrap().len(), 1);
    assert_eq!(narrow["clips"][0]["id"], "0-0");
    let reset = journal
        .timeline_window(Window {
            since: Some(i64::MAX),
            ..request
        })
        .await?;
    assert_eq!(reset["reset"], true);
    assert_eq!(reset["clips"].as_array().unwrap().len(), 599);
    Ok(())
}
