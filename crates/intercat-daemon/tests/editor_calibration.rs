use intercat_daemon::{
    Result,
    decision::{Kind, Request, Target},
    editor_calibration::{Artifact, Change, Identity},
    journal::Journal,
    secrets::Key,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn artifact(prefix: &str, temperature: f64) -> Value {
    let identity = Identity::for_prefix(prefix, false);
    json!({"format":1,"datasetHash":"a".repeat(64),"profiles":{identity.id():{
        "identity":identity,"fit":{"temperature":temperature,"groups":8,"observations":12},
        "calibrationHash":"b".repeat(64),"evaluationHash":"c".repeat(64),
        "evaluationBefore":{"observations":4},"evaluationAfter":{"observations":4}
    }}})
}

async fn apply(journal: &Journal, revision: u64, value: Value) -> Result<Change> {
    let change = Change {
        op_id: Uuid::now_v7(),
        revision,
        artifact: serde_json::from_value(value)?,
    };
    journal.apply_editor_calibration(change.clone()).await?;
    Ok(change)
}

#[tokio::test]
async fn installation_is_atomic_idempotent_and_revision_checked_across_restart() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let request = Request::new("Start", vec![], Target::New, None)?;
    let prefix = request.wire["prefix"].as_str().unwrap().to_owned();
    let first = apply(&journal, 0, artifact(&prefix, 2.0)).await?;
    let mut frozen = request;
    frozen.calibrate(journal.editor_calibration(prefix.clone(), false).await?)?;
    let mut reused = first.clone();
    reused.artifact = serde_json::from_value(artifact(&prefix, 4.0))?;
    assert!(journal.apply_editor_calibration(reused).await.is_err());
    let second = apply(&journal, 1, artifact(&prefix, 4.0)).await?;
    assert_eq!(journal.apply_editor_calibration(first).await?.revision, 2);
    assert_eq!(journal.apply_editor_calibration(second).await?.revision, 2);
    assert!(apply(&journal, 1, artifact(&prefix, 6.0)).await.is_err());
    assert_eq!(frozen.wire["temperature"], 2.0);
    assert_eq!(
        journal
            .editor_calibration(prefix.clone(), false)
            .await?
            .temperature,
        4.0
    );
    drop(journal);
    let reopened = Journal::open(&path, Key::new([7; 32])).await?;
    assert_eq!(
        reopened
            .editor_calibration(prefix, false)
            .await?
            .temperature,
        4.0
    );
    assert!(
        reopened
            .editor_calibration("different prompt".into(), false)
            .await
            .is_err()
    );
    let db = rusqlite::Connection::open(path)?;
    let count: i64 = db.query_row(
        "SELECT count(*) FROM events WHERE kind='editor.calibrated'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(count, 2);
    Ok(())
}

#[tokio::test]
async fn invalid_fits_and_concurrent_installations_cannot_partially_update_state() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let prefix = "classification prompt";
    let id = Identity::for_prefix(prefix, false).id();
    let original = artifact(prefix, 2.0);
    for path in [
        "/fit/temperature",
        "/identity/modelRevision",
        "/calibrationHash",
        "/evaluationAfter/observations",
    ] {
        let mut value = original.clone();
        let profile = &mut value["profiles"][&id];
        *profile.pointer_mut(path).unwrap() = match path {
            "/fit/temperature" => json!(0),
            "/evaluationAfter/observations" => json!(0),
            _ => json!("invalid"),
        };
        assert!(apply(&journal, 0, value).await.is_err());
        assert_eq!(journal.editor_calibration_state().await?.revision, 0);
    }
    let (left, right) = tokio::join!(
        apply(&journal, 0, original.clone()),
        apply(&journal, 0, original)
    );
    assert_ne!(left.is_ok(), right.is_ok());
    assert_eq!(journal.editor_calibration_state().await?.revision, 1);
    Ok(())
}

#[tokio::test]
async fn calibrated_probabilities_drive_review_and_reject_wrong_temperature_or_scores() -> Result<()>
{
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let mut request = Request::new("Start", vec![], Target::New, None)?;
    let prefix = request.wire["prefix"].as_str().unwrap().to_owned();
    apply(&journal, 0, artifact(&prefix, 8.0)).await?;
    request.calibrate(journal.editor_calibration(prefix, false).await?)?;
    let low = (-1.0_f64).exp();
    let high = 1.0 / (1.0 + 3.0 * low);
    let response = json!({"cachePrefix":false,"temperature":8.0,"inferenceMs":10,
        "stages":{"boundary":{"selected":" A","logprobs":{" A":0," B":-8," C":-8," D":-8},
        "probabilities":{" A":high," B":low*high," C":low*high," D":low*high}}}});
    let decision = request.resolve(response.clone())?;
    assert_eq!(decision.kind, Kind::Fork);
    assert_eq!(decision.scores["review"]["stage"], "boundary");
    assert_eq!(decision.confidence, high);
    assert_eq!(decision.scores["calibration"]["fit"]["temperature"], 8.0);
    let mut wrong = response.clone();
    wrong["temperature"] = json!(1);
    assert!(request.resolve(wrong).is_err());
    let mut wrong = response;
    wrong["stages"]["boundary"]["probabilities"] = json!({" A":1," B":0," C":0," D":0});
    assert!(request.resolve(wrong).is_err());
    assert!(
        request
            .calibrate(intercat_daemon::editor_calibration::Applied {
                temperature: 1.0,
                profile: "0".repeat(64),
                fit_hash: "a".repeat(64),
                dataset_hash: "a".repeat(64)
            })
            .is_err()
    );
    Ok(())
}

#[test]
fn artifact_rejects_unknown_envelope_fields() {
    let mut value = artifact("test", 1.0);
    value["extra"] = json!(true);
    assert!(serde_json::from_value::<Artifact>(value).is_err());
}

#[tokio::test]
async fn cached_and_uncached_loading_require_distinct_fits() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let mut request = Request::new("Start", vec![], Target::New, None)?;
    let prefix = request.wire["prefix"].as_str().unwrap().to_owned();
    let uncached = Identity::for_prefix(&prefix, false).id();
    let cached = Identity::for_prefix(&prefix, true).id();
    assert_ne!(cached, uncached);
    apply(&journal, 0, artifact(&prefix, 2.0)).await?;
    let fit = journal.editor_calibration(prefix.clone(), false).await?;
    assert!(
        journal
            .editor_calibration(prefix.clone(), true)
            .await
            .is_err()
    );
    request.wire["cachePrefix"] = json!(true);
    assert!(request.calibrate(fit).is_err());

    let mut profile = artifact(&prefix, 3.0)["profiles"][&uncached].clone();
    profile["identity"]["cachePrefix"] = json!(true);
    let value = json!({"format":1,"datasetHash":"a".repeat(64),"profiles":{cached:profile}});
    apply(&journal, 1, value).await?;
    request.calibrate(journal.editor_calibration(prefix.clone(), true).await?)?;
    assert_eq!(request.wire["temperature"], 3.0);
    assert_eq!(
        journal.editor_calibration(prefix, false).await?.temperature,
        2.0
    );
    let high = 1.0 / (1.0 + 3.0 * (-1.0_f64).exp());
    let low = (1.0 - high) / 3.0;
    let response = json!({"cachePrefix":true,"temperature":3.0,"inferenceMs":10,
        "stages":{"boundary":{"selected":" A","logprobs":{" A":0," B":-3," C":-3," D":-3},
        "probabilities":{" A":high," B":low," C":low," D":low}}}});
    let result = request.resolve(response.clone())?;
    assert_eq!(result.scores["calibration"]["cachePrefix"], true);
    request.wire["cachePrefix"] = json!(false);
    assert!(request.resolve(response).is_err());
    Ok(())
}

#[tokio::test]
async fn an_artifact_without_a_loading_mode_is_rejected() -> Result<()> {
    let root = tempfile::tempdir()?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let prefix = "dummy prompt";
    let id = Identity::for_prefix(prefix, false).id();
    let mut value = artifact(prefix, 2.0);
    value["profiles"][&id]["identity"]
        .as_object_mut()
        .unwrap()
        .remove("cachePrefix");
    assert!(apply(&journal, 0, value).await.is_err());
    assert_eq!(journal.editor_calibration_state().await?.revision, 0);
    Ok(())
}

#[tokio::test]
#[ignore = "requires the pinned local Glimmer and Metal"]
async fn the_real_input_processor_uses_and_records_the_installed_temperature() -> Result<()> {
    use intercat_daemon::{
        editor::{Config, Manager},
        input::Processor,
    };
    use std::{sync::Arc, time::Duration};
    use tokio::sync::Notify;
    let root = tempfile::tempdir()?;
    let path = root.path().join("db");
    let journal = Journal::open(&path, Key::new([7; 32])).await?;
    let request = Request::new(
        "合図するまでは新しいメモ帳を作り始めないで。",
        vec![],
        Target::New,
        None,
    )?;
    let prefix = request.wire["prefix"].as_str().unwrap().to_owned();
    let mut artifact: Value = serde_json::from_str(include_str!(
        "../../../services/editor/src/intercat_editor/calibration_profiles.json"
    ))?;
    let profile = Identity::for_prefix(&prefix, false).id();
    artifact["profiles"][&profile]["fit"]["temperature"] = json!(2.0);
    apply(&journal, 0, artifact).await?;
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
    tokio::time::timeout(Duration::from_secs(90), async {
        while !editor.status().model_loaded {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await?;
    let wake = Arc::new(Notify::new());
    let processor = Processor::start(journal.clone(), editor.clone(), wake.clone()).await?;
    let id = Uuid::now_v7();
    journal
        .stage_utterance(
            id,
            "合図するまでは新しいメモ帳を作り始めないで。".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    wake.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let value = journal.read_utterance(id.to_string()).await?;
            if value["state"] != "staged" && value["state"] != "reading" {
                return Result::<Value>::Ok(value);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    processor.shutdown().await?;
    editor.stop().await;
    let value = result??;
    let db = rusqlite::Connection::open(&path)?;
    let decision: String = db.query_row(
        "SELECT decision FROM utterances WHERE id=?1",
        [id.to_string()],
        |row| row.get(0),
    )?;
    let decision: Value = serde_json::from_str(&decision)?;
    assert_eq!(decision["scores"]["temperature"], 2.0);
    assert_eq!(decision["scores"]["calibration"]["fit"]["temperature"], 2.0);
    assert_eq!(decision["scores"]["calibration"]["fit"]["profile"], profile);
    assert_eq!(decision["scores"]["calibration"]["cachePrefix"], false);
    assert_eq!(decision["scores"]["stages"]["boundary"]["selected"], " D");
    println!(
        "{}",
        json!({"state":value["state"],"boundary":decision["boundary"],"temperature":decision["scores"]["temperature"],"roundTripMs":decision["scores"]["roundTripMs"],"inferenceMs":decision["inferenceMs"]})
    );
    Ok(())
}
