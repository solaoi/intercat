use intercat_daemon::{
    Result,
    clip_summaries::{Finish, Summary, View},
    clock::Stamp,
    decision::{Candidate, Decision, Kind, Outcome, PendingInput, Target},
    journal::Journal,
    routing::{Selection, Track},
    secrets::Key,
};
use serde_json::json;
use uuid::Uuid;

fn select(
    text: &str,
    tracks: Vec<Track>,
    selected: Option<&str>,
    locked: Option<&str>,
    needs_you: &[String],
) -> Result<Selection> {
    let db = rusqlite::Connection::open_in_memory()?;
    db.execute_batch(
        "CREATE TABLE tracks(id TEXT PRIMARY KEY,label TEXT,title TEXT,status TEXT);
        CREATE TABLE timeline_clips(id TEXT PRIMARY KEY,track TEXT,ordinal INTEGER,record TEXT);
        CREATE TABLE timeline_versions(id TEXT PRIMARY KEY,version INTEGER,deleted INTEGER);
        CREATE TABLE clip_summaries(clip TEXT,state TEXT,version INTEGER,result TEXT);",
    )?;
    for track in &tracks {
        db.execute(
            "INSERT INTO tracks VALUES(?1,?2,?3,?4)",
            rusqlite::params![
                track.candidate.id,
                track.candidate.label,
                track.candidate.title,
                track.candidate.status
            ],
        )?;
    }
    intercat_daemon::routing_index::install(&db)?;
    intercat_daemon::routing::select(&db, text, tracks, selected, locked, needs_you)
}

fn tracks(count: usize) -> Vec<Track> {
    (0..count)
        .map(|index| Track {
            candidate: Candidate {
                id: format!("track-{index}"),
                label: char::from(b'A' + index as u8 % 26).to_string(),
                title: format!("Work {index}"),
                status: "waiting".into(),
                activity: None,
                running: None,
                planned: Vec::new(),
            },
            spoken_at: None,
        })
        .collect()
}

fn ids(candidates: &[Candidate]) -> Vec<&str> {
    candidates
        .iter()
        .map(|candidate| candidate.id.as_str())
        .collect()
}

#[test]
fn names_browsing_needs_you_and_recent_tracks_fill_eight_slots_in_order() {
    let mut all = tracks(20);
    for (index, track) in all.iter_mut().take(8).enumerate() {
        track.spoken_at = Some(100 - index as i64);
    }
    let picked = select(
        "Continue S",
        all,
        Some("track-19"),
        None,
        &["track-17".into(), "track-16".into(), "track-15".into()],
    )
    .unwrap();
    assert_eq!(
        ids(&picked.first),
        [
            "track-18", "track-19", "track-17", "track-16", "track-0", "track-1", "track-2",
            "track-3"
        ]
    );
    assert_eq!(picked.expanded.len(), 8);
    assert!(
        picked
            .expanded
            .iter()
            .all(|track| !ids(&picked.first).contains(&track.id.as_str()))
    );
}

#[test]
fn duplicates_leave_room_for_search_and_only_five_recent_tracks_are_promoted() {
    let mut all = tracks(16);
    for (index, track) in all.iter_mut().take(8).enumerate() {
        track.spoken_at = Some(100 - index as i64);
    }
    all[12].candidate.title = "Mercury latency".into();
    let picked = select(
        "Continue A and improve mercury",
        all,
        Some("track-0"),
        None,
        &["track-0".into(), "track-1".into()],
    )
    .unwrap();
    assert_eq!(
        ids(&picked.first),
        [
            "track-0", "track-1", "track-2", "track-3", "track-4", "track-12"
        ]
    );
    assert_eq!(ids(&picked.expanded)[0], "track-5");
}

#[test]
fn title_search_matches_japanese_and_treats_query_operators_as_text() {
    let mut all = tracks(20);
    all[14].candidate.title = "認証処理の改善".into();
    all[15].candidate.title = "Mercury pipeline".into();
    let picked = select(
        "認証処理を修正。NEAR(mercury) OR xyz*",
        all,
        None,
        None,
        &[],
    )
    .unwrap();
    assert!(ids(&picked.first).contains(&"track-14"));
    assert!(ids(&picked.first).contains(&"track-15"));
    assert_eq!(picked.first.len(), 2);
}

#[test]
fn locked_track_cannot_fall_out_and_a_label_is_not_part_of_another_word() {
    let picked = select(
        "API change",
        tracks(20),
        Some("track-19"),
        Some("track-18"),
        &[],
    )
    .unwrap();
    assert_eq!(ids(&picked.first), ["track-18", "track-19"]);
    let picked = select("add a test", tracks(20), None, None, &[]).unwrap();
    assert!(picked.first.is_empty());
}

#[test]
fn second_pass_search_promotes_matches_outside_the_first_eight() {
    let mut all = tracks(20);
    all[19].candidate.title = "Mercury batching".into();
    for (index, track) in all.iter_mut().take(5).enumerate() {
        track.spoken_at = Some(100 - index as i64);
    }
    let picked = select(
        "Continue R; improve mercury",
        all,
        Some("track-16"),
        None,
        &["track-15".into(), "track-14".into()],
    )
    .unwrap();
    assert_eq!(picked.first.len(), 8);
    assert!(!ids(&picked.first).contains(&"track-19"));
    assert_eq!(ids(&picked.expanded)[0], "track-19");
}

#[test]
fn three_hundred_tracks_keep_the_named_destination_and_bounded_candidate_count() {
    let mut all = tracks(300);
    all[277].candidate.title = "Pinpoint destination".into();
    let started = std::time::Instant::now();
    let picked = select("Continue Pinpoint destination", all, None, None, &[]).unwrap();
    println!(
        "300-track selection: {:.3} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    assert_eq!(picked.first[0].id, "track-277");
    assert!(picked.first.len() <= 8 && picked.expanded.len() <= 8);
}

async fn journal_tracks(count: usize) -> Result<(tempfile::TempDir, Journal, Vec<String>)> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let mut tracks = Vec::new();
    for index in 0..count {
        let id = Uuid::now_v7();
        journal
            .stage_utterance(id, format!("Work item {index}"), Target::New, None, vec![])
            .await?;
        let input = journal.claim_input().await?.unwrap();
        journal
            .apply_input(
                input.id.clone(),
                input.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Fork,
                    track_id: None,
                    boundary: Some("when-told".into()),
                    command: None,
                    confidence: 1.0,
                    inference_ms: 0.0,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await?;
        let snapshot = journal.snapshot().await?;
        let word = snapshot["utterances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|word| word["id"] == input.id)
            .unwrap();
        tracks.push(word["decision"]["trackId"].as_str().unwrap().to_owned());
    }
    Ok((root, journal, tracks))
}

async fn message(
    journal: &Journal,
    track: &str,
    turn: &str,
    text: &str,
    revision: i64,
    closed: bool,
) -> Result<()> {
    let session = format!("session-{track}");
    let stamp = Stamp {
        epoch_ms: revision + 1000,
        mono_ms: revision,
        boot_id: "test".into(),
        timeline_ms: revision + 1000,
    };
    journal.try_msp(track.into(), session.clone(), stamp.clone(), json!({"method":"item/updated","params":{
        "sessionId":session,"item":{"itemId":format!("answer-{turn}"),"turnId":turn,"kind":"agentMessage",
        "revision":revision,"status":if closed {"completed"} else {"inProgress"},"text":text}}}), None, None)?.await??;
    if closed {
        journal
            .try_msp(
                track.into(),
                session.clone(),
                stamp,
                json!({"method":"turn/completed","params":{
            "sessionId":session,"turnId":turn,"terminal":"completed"}}),
                None,
                None,
            )?
            .await??;
    }
    Ok(())
}

async fn summarize(journal: &Journal, title: &str, text: &str) -> Result<String> {
    let ticket = journal.claim_clip_summary().await?.unwrap();
    journal
        .finish_clip_summary(
            ticket.clone(),
            Finish::Complete(Summary {
                title: title.into(),
                summary: text.into(),
            }),
        )
        .await?;
    Ok(ticket.clip)
}

async fn input(journal: &Journal, text: &str) -> Result<PendingInput> {
    journal
        .stage_utterance(Uuid::now_v7(), text.into(), Target::None, None, vec![])
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal.cut_input(Uuid::now_v7(), input.id.clone()).await?;
    Ok(input)
}

#[tokio::test]
async fn normal_input_finds_historical_summaries_and_rebuilds_the_index_on_restart() -> Result<()> {
    let (root, journal, tracks) = journal_tracks(12).await?;
    let target = &tracks[0];
    message(&journal, target, "old", "raw-worker-sentinel", 1, true).await?;
    let before = input(&journal, "raw-worker-sentinel").await?;
    assert!(!before.tracks.iter().any(|track| &track.id == target));
    let old_clip = summarize(
        &journal,
        "Retry limits",
        "Investigated exponential backoff for network retries.",
    )
    .await?;
    message(&journal, target, "new", "unrelated latest step", 2, true).await?;
    summarize(
        &journal,
        "Button styling",
        "Adjusted toolbar button spacing.",
    )
    .await?;
    let picked = input(&journal, "Continue exponential backoff").await?;
    let found = picked
        .tracks
        .iter()
        .find(|track| &track.id == target)
        .unwrap();
    assert_eq!(found.activity.as_ref().unwrap().title, "Retry limits");
    let request =
        intercat_daemon::decision::Request::new(&picked.text, picked.tracks, Target::None, None)?;
    let wire = request.wire.to_string();
    assert!(wire.contains("exponential backoff"));
    assert!(!wire.contains("raw-worker-sentinel"));
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let picked = input(&journal, "Continue exponential backoff").await?;
    assert_eq!(
        picked
            .tracks
            .iter()
            .find(|track| &track.id == target)
            .unwrap()
            .activity
            .as_ref()
            .unwrap()
            .title,
        "Retry limits"
    );
    message(
        &journal,
        target,
        "old",
        "Replaced the previous result",
        3,
        true,
    )
    .await?;
    let stale = input(&journal, "Continue exponential backoff").await?;
    assert!(!stale.tracks.iter().any(|track| &track.id == target));
    let ticket = journal.claim_clip_summary().await?.unwrap();
    assert_eq!(ticket.clip, old_clip);
    Ok(())
}

#[tokio::test]
async fn thinking_search_keeps_append_only_results_and_drops_replaced_or_closed_text() -> Result<()>
{
    let (_root, journal, tracks) = journal_tracks(12).await?;
    let target = &tracks[0];
    journal
        .observe_timeline(
            Uuid::now_v7(),
            1,
            Some(View {
                track_id: target.clone(),
                repair_run_id: None,
            }),
        )
        .await?;
    message(
        &journal,
        target,
        "live",
        "Investigating race conditions",
        1,
        false,
    )
    .await?;
    summarize(
        &journal,
        "Queue investigation",
        "Investigating semaphore ownership.",
    )
    .await?;
    assert!(
        input(&journal, "Continue semaphore ownership")
            .await?
            .tracks
            .iter()
            .any(|track| &track.id == target)
    );
    message(
        &journal,
        target,
        "live",
        "Investigating race conditions in the queue",
        2,
        false,
    )
    .await?;
    assert!(
        input(&journal, "Continue semaphore ownership")
            .await?
            .tracks
            .iter()
            .any(|track| &track.id == target)
    );
    message(
        &journal,
        target,
        "live",
        "Investigating a different issue",
        3,
        false,
    )
    .await?;
    assert!(
        !input(&journal, "Continue semaphore ownership")
            .await?
            .tracks
            .iter()
            .any(|track| &track.id == target)
    );
    message(
        &journal,
        target,
        "live",
        "Investigating a different issue",
        4,
        true,
    )
    .await?;
    assert!(
        !input(&journal, "Continue semaphore ownership")
            .await?
            .tracks
            .iter()
            .any(|track| &track.id == target)
    );
    Ok(())
}

#[tokio::test]
#[ignore = "local Glimmer model and Python, no paid API"]
async fn real_glimmer_routes_requests_using_generated_activity_context() -> Result<()> {
    use intercat_daemon::{
        editor::{Config, Manager},
        input::Processor,
    };
    use std::{sync::Arc, time::Duration};
    use tokio::sync::{Notify, watch};
    let (root, journal, tracks) = journal_tracks(12).await?;
    let cases = [
        (
            "Investigated session expiration and refresh token handling.",
            "Add a regression test for session expiration.",
        ),
        (
            "Added arithmetic boundary tests and investigated zero-case failures.",
            "Continue the arithmetic boundary tests with a zero case.",
        ),
        (
            "Implemented notification stream reconnection and unread counter updates.",
            "Add a regression test for notification stream reconnection.",
        ),
        (
            "Adjusted search form spacing and search button alignment.",
            "Continue adjusting the search form spacing.",
        ),
    ];
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let editor = Manager::new(
        &project.join("services/editor/src"),
        &root.path().join("editor.log"),
    )?;
    editor
        .configure(Config {
            python: std::env::var("INTERCAT_TEST_EDITOR_PYTHON")?.into(),
            model: std::env::var("INTERCAT_TEST_EDITOR_MODEL")?.into(),
        })
        .await?;
    tokio::time::timeout(Duration::from_secs(60), async {
        while !editor.status().model_loaded {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let (_stop, mut stopping) = watch::channel(false);
    let mut generated = Vec::new();
    for (index, (recorded, _)) in cases.iter().enumerate() {
        let snapshot = journal.snapshot().await?;
        let original = snapshot["utterances"]
            .as_array()
            .unwrap()
            .iter()
            .find(|word| word["decision"]["trackId"] == tracks[index])
            .unwrap();
        let started = journal
            .start_held_input(
                Uuid::now_v7(),
                original["id"].as_str().unwrap().into(),
                original["revision"].as_i64().unwrap(),
            )
            .await?;
        let command = Uuid::now_v7();
        journal
            .reserve_turn(
                command,
                started["id"].as_str().unwrap().into(),
                started["revision"].as_i64().unwrap(),
                tracks[index].clone(),
                json!({"sessionId":format!("session-{}",tracks[index]),"commandId":command}),
                None,
            )
            .await?;
        journal.finish(&command.to_string(), "dispatched").await?;
        journal
            .input_execution(
                started["id"].as_str().unwrap().into(),
                started["revision"].as_i64().unwrap(),
                "landed",
            )
            .await?;
        journal.finish(&command.to_string(), "complete").await?;
        message(&journal, &tracks[index], "work", recorded, 1, true).await?;
        let mut window = json!({});
        for _ in 0..2 {
            intercat_daemon::clip_summaries::process_next(&journal, &editor, &mut stopping).await?;
            window = journal
                .timeline_window(intercat_daemon::timeline::Window {
                    tracks: vec![tracks[index].clone()],
                    start: 0,
                    end: i64::MAX,
                    since: None,
                })
                .await?;
            if window["clips"][0]["summary"].is_string() {
                break;
            }
        }
        if let Ok(path) = std::env::var("INTERCAT_ROUTING_OUTPUT") {
            std::fs::copy(root.path().join("editor.log"), format!("{path}.editor.log"))?;
        }
        let clip = &window["clips"][0];
        assert!(clip["summary"].is_string(), "{window}");
        generated.push(clip.clone());
    }
    let wake = Arc::new(Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone()).await?;
    let mut results = Vec::new();
    for (index, (_, request)) in cases.iter().enumerate() {
        let id = Uuid::now_v7();
        journal
            .stage_utterance(id, request.to_string(), Target::None, None, vec![])
            .await?;
        wake.notify_one();
        let word = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let snapshot = journal.snapshot().await?;
                let word = snapshot["utterances"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|word| word["id"] == id.to_string())
                    .unwrap();
                if word["state"] == "placed" || word["state"] == "asked" {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(word.clone());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        let stored: Option<String> = rusqlite::Connection::open(root.path().join("db"))?
            .query_row(
                "SELECT decision FROM utterances WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )?;
        let decision = stored
            .map(|text| serde_json::from_str::<serde_json::Value>(&text))
            .transpose()?;
        results.push(json!({"request":request,"expectedTrack":tracks[index],"generatedClip":generated[index],"result":word,"decision":decision}));
    }
    processor.shutdown().await?;
    editor.stop().await;
    if let Ok(path) = std::env::var("INTERCAT_ROUTING_OUTPUT") {
        std::fs::write(path, serde_json::to_string_pretty(&results)?)?;
    }
    for result in results {
        assert_eq!(result["result"]["state"], "placed", "{result}");
        assert_eq!(result["result"]["decision"]["kind"], "extend", "{result}");
        assert_eq!(
            result["result"]["decision"]["trackId"], result["expectedTrack"],
            "{result}"
        );
    }
    Ok(())
}
