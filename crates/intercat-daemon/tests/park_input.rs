use intercat_daemon::{
    Result,
    decision::Target,
    editor::{Config, Manager},
    input::Processor,
    journal::Journal,
    secrets::Key,
};
use serde_json::Value;
use std::{fs, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
use tokio::sync::Notify;
use uuid::Uuid;

async fn wait_for(
    journal: &Journal,
    id: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let value = journal.snapshot().await?;
            if let Some(word) = value["utterances"]
                .as_array()
                .unwrap()
                .iter()
                .find(|word| word["id"] == id)
                && predicate(word)
            {
                return Ok(word.clone());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

#[tokio::test]
async fn a_new_input_preempts_a_park_reading_and_only_the_latest_context_can_place_it() -> Result<()>
{
    let root = tempfile::tempdir()?;
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let python = root.path().join("python");
    fs::write(
        &python,
        format!(
            "#!/usr/bin/python3\n{}",
            fs::read_to_string(project.join("tests/fixtures/park_editor.py"))?
        ),
    )?;
    fs::set_permissions(&python, fs::Permissions::from_mode(0o700))?;
    let model = root.path().join("model");
    fs::create_dir(&model)?;
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    let editor = Manager::new(
        &project.join("services/editor/src"),
        &root.path().join("editor.log"),
    )?;
    editor.configure(Config { python, model }).await?;
    let wake = Arc::new(Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone()).await?;
    let parked = Uuid::now_v7();
    journal
        .stage_utterance(parked, "park this idea".into(), Target::None, None, vec![])
        .await?;
    wait_for(&journal, &parked.to_string(), |word| {
        word["decision"]["kind"] == "park"
    })
    .await?;
    let slow = Uuid::now_v7();
    journal
        .stage_utterance(slow, "slow context".into(), Target::New, None, vec![])
        .await?;
    wait_for(&journal, &slow.to_string(), |word| {
        word["state"] == "placed"
    })
    .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !root.path().join("park-running").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let old = journal.snapshot().await?["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|word| word["id"] == parked.to_string())
        .unwrap()["reading"]["index"]
        .as_i64()
        .unwrap();
    let latest = Uuid::now_v7();
    let started = std::time::Instant::now();
    journal
        .stage_utterance(
            latest,
            "unlock the parked idea on Worker A".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    wait_for(&journal, &latest.to_string(), |word| {
        word["state"] == "placed"
    })
    .await?;
    assert!(started.elapsed() < Duration::from_secs(5));
    let placed = wait_for(&journal, &parked.to_string(), |word| {
        word["decision"]["kind"] == "extend"
    })
    .await?;
    assert!(placed["reading"]["index"].as_i64().unwrap() > old);
    assert_eq!(placed["reading"]["state"], "complete");
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    processor.shutdown().await?;
    editor.stop().await;
    Ok(())
}

#[tokio::test]
#[ignore = "local Glimmer weights and Python; no paid model turns"]
async fn a_real_local_model_uses_later_context_to_place_the_original_parked_request() -> Result<()>
{
    use intercat_daemon::decision::{Decision, Kind, Outcome};
    use serde_json::json;
    let root = tempfile::tempdir()?;
    let project = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let journal = Journal::open(root.path().join("events.sqlite"), Key::new([7; 32])).await?;
    let stage = |text: &str, target| {
        let journal = journal.clone();
        let text = text.to_owned();
        async move {
            journal
                .stage_utterance(Uuid::now_v7(), text, target, None, vec![])
                .await?;
            let pending = journal.claim_input().await?.ok_or("Missing input")?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(pending)
        }
    };
    let classified = |kind, track| {
        Outcome::Classified(Decision {
            kind,
            track_id: track,
            boundary: Some("now".into()),
            command: None,
            confidence: 1.0,
            inference_ms: 0.0,
            timed_out: false,
            scores: json!({}),
        })
    };
    let first = stage("Arithmetic tests", Target::New).await?;
    journal
        .apply_input(first.id, first.revision, classified(Kind::Fork, None))
        .await?;
    let track = journal.snapshot().await?["tracks"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let parked = stage("Add one more edge-case test to those tests.", Target::None).await?;
    journal
        .apply_input(
            parked.id.clone(),
            parked.revision,
            classified(Kind::Park, None),
        )
        .await?;
    let context = stage("By those tests I mean the Arithmetic tests on Worker A. The parked request should add the zero-input case to Worker A now.",Target::Lock(track.clone())).await?;
    journal
        .apply_input(
            context.id,
            context.revision,
            classified(Kind::Extend, Some(track.clone())),
        )
        .await?;
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
    let processor =
        Processor::start(journal.clone(), editor.clone(), Arc::new(Notify::new())).await?;
    let result = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let snapshot = journal.snapshot().await?;
            let word = snapshot["utterances"]
                .as_array()
                .unwrap()
                .iter()
                .find(|word| word["id"] == parked.id)
                .unwrap();
            if word["reading"]["kind"] == "park" && word["reading"]["state"] == "complete" {
                break Ok::<_, Box<dyn std::error::Error + Send + Sync>>(word.clone());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    processor.shutdown().await?;
    editor.stop().await;
    let value = result?;
    if let Ok(output) = std::env::var("INTERCAT_PARK_OUTPUT") {
        fs::write(output, serde_json::to_vec_pretty(&value)?)?;
    }
    assert_eq!(value["state"], "placed");
    assert_eq!(value["decision"]["kind"], "extend");
    assert_eq!(value["decision"]["trackId"], track);
    assert_eq!(value["text"], "Add one more edge-case test to those tests.");
    Ok(())
}
