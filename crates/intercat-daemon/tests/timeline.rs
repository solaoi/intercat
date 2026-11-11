use intercat_daemon::{Result, clock::Stamp, journal::Journal};
use serde_json::{Value, json};

async fn read_clips(j: &Journal) -> Result<Value> {
    Ok(read_window(j).await?["clips"].clone())
}
async fn read_window(j: &Journal) -> Result<Value> {
    j.timeline_window(intercat_daemon::timeline::Window {
        tracks: vec!["track".into()],
        start: 0,
        end: i64::MAX,
        since: None,
    })
    .await
}

fn at(ms: i64) -> Stamp {
    Stamp {
        epoch_ms: 1_000 + ms,
        mono_ms: ms,
        boot_id: "boot".into(),
        timeline_ms: 1_000 + ms,
    }
}
fn item(id: &str, kind: &str, revision: i64, status: &str) -> Value {
    json!({"method":"item/updated","params":{"sessionId":"session","item":{"itemId":id,"turnId":"turn","kind":kind,"revision":revision,"status":status}}})
}
async fn put(j: &Journal, ms: i64, value: Value) -> Result<()> {
    j.try_msp("track".into(), "session".into(), at(ms), value, None, None)?
        .await??;
    Ok(())
}

#[tokio::test]
async fn parallel_bundle_revision_replay_and_late_updates_keep_addresses() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let j = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    let mut frames = vec![(0, item("user", "userMessage", 1, "completed"))];
    for (id, tool) in [("a", "read_file"), ("b", "edit_file")] {
        let mut v = item(id, "toolCall", 1, "inProgress");
        v["params"]["item"]["tool"] = json!(tool);
        v["params"]["item"]["args"] = json!("{\"path\":\"math.cjs\"}");
        frames.push((10, v));
    }
    let mut a = frames[1].1.clone();
    a["params"]["item"]["revision"] = json!(2);
    a["params"]["item"]["status"] = json!("completed");
    frames.push((20, a));
    for (ms, v) in &frames {
        put(&j, *ms, v.clone()).await?;
    }
    assert_eq!(
        read_clips(&j).await?.as_array().unwrap().len(),
        1,
        "one outstanding tool keeps the bundle open"
    );
    let mut b = frames[2].1.clone();
    b["params"]["item"]["revision"] = json!(2);
    b["params"]["item"]["status"] = json!("rejected");
    frames.push((30, b.clone()));
    put(&j, 30, b.clone()).await?;
    let clip_values = read_clips(&j).await?;
    let clips = clip_values.as_array().unwrap();
    assert_eq!(clips.len(), 2);
    assert_eq!(clips[0]["kind"], "edit");
    assert_eq!(clips[0]["end"], 1030);
    assert_eq!(clips[1]["start"], 1030);
    let id = clips[0]["id"].as_str().unwrap().to_owned();
    // A late higher revision belongs to the original step; a stale history replay cannot overwrite it.
    b["params"]["item"]["revision"] = json!(3);
    b["params"]["item"]["visibleOutput"] = json!("denied by user");
    frames.push((40, b.clone()));
    put(&j, 40, b).await?;
    frames.push((45, frames[2].1.clone()));
    put(&j, 45, frames[2].1.clone()).await?;
    let end = json!({"method":"turn/completed","params":{"sessionId":"session","turnId":"turn","terminal":"completed"}});
    frames.push((50, end.clone()));
    put(&j, 50, end).await?;
    assert_eq!(
        j.clip_detail(id.clone()).await?["items"][2]["visibleOutput"],
        "denied by user"
    );
    let expected = read_clips(&j).await?.clone();
    drop(j);
    let j = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert_eq!(read_clips(&j).await?, expected);
    let replay = Journal::open(
        root.path().join("replay"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    for (ms, frame) in frames {
        put(&replay, ms, frame).await?;
    }
    assert_eq!(read_clips(&replay).await?, expected);
    assert_eq!(
        read_window(&replay).await?["marks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let mut child = item("child-item", "agentMessage", 1, "completed");
    child["params"]["sessionId"] = json!("reminder-session");
    put(&replay, 55, child).await?;
    assert_eq!(
        read_clips(&replay).await?,
        expected,
        "child-session events cannot create their own steps"
    );
    // A failed projection retains the raw receipt without changing the view.
    let before = j.snapshot().await?["sequence"].clone();
    assert!(
        j.try_msp(
            "other".into(),
            "session".into(),
            at(60),
            item("wrong", "agentMessage", 1, "completed"),
            None,
            None
        )?
        .await?
        .is_err()
    );
    assert_eq!(
        j.snapshot().await?["sequence"],
        before.as_i64().unwrap() + 1
    );
    assert_eq!(read_clips(&j).await?, expected);
    Ok(())
}

#[tokio::test]
async fn shell_tail_closes_only_on_terminal_and_output_cache_survives_reopen() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let j = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    put(&j, 0, item("user", "userMessage", 1, "completed")).await?;
    let mut shell = item("sh", "toolCall", 1, "inProgress");
    shell["params"]["item"]["tool"] = json!("bash");
    shell["params"]["item"]["patchRef"] = json!({"id":"patch"});
    put(&j, 10, shell.clone()).await?;
    put(
        &j,
        20,
        json!({"method":"turn/completed","params":{"sessionId":"session","turnId":"turn"}}),
    )
    .await?;
    assert_eq!(read_clips(&j).await?[1]["category"], "shell");
    assert_eq!(read_clips(&j).await?[1]["running"], true);
    shell["params"]["item"]["revision"] = json!(2);
    shell["params"]["item"]["status"] = json!("cancelled");
    put(&j, 30, shell).await?;
    assert_eq!(read_clips(&j).await?[1]["end"], 1030);
    assert_eq!(read_clips(&j).await?[1]["running"], false);
    let id = read_clips(&j).await?[1]["id"].as_str().unwrap().to_owned();
    j.cache_output(
        "session".into(),
        "sh".into(),
        "patch".into(),
        json!({"content":"actual patch","encoding":"utf8","eof":true}),
    )
    .await?;
    drop(j);
    let j = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert_eq!(
        j.clip_detail(id).await?["items"][0]["outputs"]["patchRef"]["content"],
        "actual patch"
    );
    Ok(())
}

#[test]
fn elapsed_uses_continuous_clock_and_restart_fallback_never_goes_negative() {
    let start = at(10);
    let mut end = at(35);
    end.epoch_ms = 0;
    assert_eq!(end.elapsed(&start), (25, false));
    end.boot_id = "another process".into();
    assert_eq!(end.elapsed(&start), (0, true));
    end.epoch_ms = 1510;
    assert_eq!(end.elapsed(&start), (500, true));
}

#[tokio::test]
async fn received_message_lands_before_turn_start_acknowledgement() -> Result<()> {
    use intercat_daemon::decision::{Decision, Kind, Outcome, Target};
    let root = tempfile::tempdir()?;
    let j = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    j.stage_utterance(
        uuid::Uuid::now_v7(),
        "Fix the dummy function".into(),
        Target::New,
        None,
        Vec::new(),
    )
    .await?;
    let input = j.claim_input().await?.unwrap();
    j.apply_input(
        input.id.clone(),
        input.revision,
        Outcome::Classified(Decision {
            kind: Kind::Fork,
            track_id: None,
            boundary: Some("now".into()),
            command: None,
            confidence: 1.0,
            inference_ms: 1.0,
            timed_out: false,
            scores: json!({}),
        }),
    )
    .await?;
    let track = j.snapshot().await?["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let command = uuid::Uuid::now_v7();
    j.reserve_turn(
        command,
        input.id.clone(),
        input.revision,
        track.clone(),
        json!({"commandId":command,"sessionId":"session"}),
        None,
    )
    .await?;
    let mut message = item("user", "userMessage", 1, "completed");
    message["params"]["item"]["commandId"] = json!(command);
    j.try_msp(track, "session".into(), at(10), message, None, None)?
        .await??;
    assert_eq!(j.snapshot().await?["utterances"][0]["state"], "landed");
    j.input_execution(input.id.clone(), input.revision, "sent")
        .await?;
    j.input_execution(input.id.clone(), input.revision, "held")
        .await?;
    let snapshot = j.snapshot().await?;
    assert_eq!(snapshot["utterances"][0]["state"], "landed");
    assert_eq!(
        read_window(&j).await?["marks"][0]["text"],
        "Fix the dummy function"
    );
    Ok(())
}

#[tokio::test]
async fn marks_keep_full_history_and_update_without_a_clip_change() -> Result<()> {
    use intercat_daemon::timeline::Window;
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    drop(Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?);
    let mut db = rusqlite::Connection::open(&path)?;
    let tx = db.transaction()?;
    for index in 0..600 {
        tx.execute(
            "INSERT INTO timeline_marks VALUES('session',?1,?2,?3,NULL,NULL)",
            rusqlite::params![
                format!("input-{index}"),
                if index % 2 == 0 {
                    "visible"
                } else {
                    "offscreen"
                },
                1000 + index
            ],
        )?;
    }
    // Item IDs need only be unique within a session.
    tx.execute(
        "INSERT INTO timeline_marks VALUES('repair','input-0','offscreen',1000,NULL,'repair-run')",
        [],
    )?;
    tx.commit()?;
    let query = |start, end, since| Window {
        tracks: vec!["visible".into()],
        start,
        end,
        since,
    };
    let first = intercat_daemon::timeline::window(&db, query(0, 5000, None))?;
    assert_eq!(first["marks"].as_array().unwrap().len(), 601);
    assert_eq!(first["clips"], json!([]));
    assert_ne!(first["marks"][0]["id"], first["marks"][1]["id"]);
    let revision = first["revision"].as_i64().unwrap();
    let unchanged = intercat_daemon::timeline::window(&db, query(0, 5000, Some(revision)))?;
    assert_eq!(unchanged["marks"], json!([]));
    assert_eq!(unchanged["removedMarks"], json!([]));
    db.execute(
        "DELETE FROM timeline_marks WHERE session='session' AND item='input-0'",
        [],
    )?;
    db.execute(
        "UPDATE timeline_marks SET position=9000 WHERE item='input-1'",
        [],
    )?;
    db.execute(
        "INSERT INTO timeline_marks VALUES('session','new','offscreen',1200,NULL,NULL)",
        [],
    )?;
    let changed = intercat_daemon::timeline::window(&db, query(0, 5000, Some(revision)))?;
    assert_eq!(changed["marks"].as_array().unwrap().len(), 1);
    assert_eq!(changed["marks"][0]["trackId"], "offscreen");
    assert_eq!(changed["removedMarks"].as_array().unwrap().len(), 2);
    assert_eq!(
        intercat_daemon::timeline::window(&db, query(1000, 1000, None))?["marks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    drop(db);
    let j = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    assert!(j.snapshot().await?["timeline"].get("marks").is_none());
    assert_eq!(
        j.timeline_window(query(0, 5000, None)).await?["marks"]
            .as_array()
            .unwrap()
            .len(),
        600
    );
    Ok(())
}
