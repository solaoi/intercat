use intercat_daemon::{
    decision::{Candidate, Decision, Kind, Outcome, Request, Resolution, Target},
    journal::Journal,
};
use serde_json::{Value, json};

fn tracks() -> Vec<Candidate> {
    vec![Candidate {
        id: "track-a".into(),
        label: "A".into(),
        title: "Tests".into(),
        status: "working".into(),
        activity: None,
        running: None,
        planned: Vec::new(),
    }]
}
fn response(request: &Request, choices: &[(&str, &str)], elapsed: f64) -> Value {
    let mut stages = serde_json::Map::new();
    for (name, label) in choices {
        let definition = request.wire["stages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["name"] == *name)
            .unwrap();
        let labels = definition["candidates"].as_array().unwrap();
        let mut logprobs = serde_json::Map::new();
        let mut probabilities = serde_json::Map::new();
        for value in labels {
            let key = value.as_str().unwrap();
            logprobs.insert(key.into(), json!(if key == *label { -0.1 } else { -2.0 }));
            probabilities.insert(
                key.into(),
                json!(if labels.len() == 1 {
                    1.0
                } else if key == *label {
                    0.9
                } else {
                    0.1 / (labels.len() - 1) as f64
                }),
            );
        }
        stages.insert(
            name.to_string(),
            json!({"selected":label,"logprobs":logprobs,"probabilities":probabilities}),
        );
    }
    json!({"stages":stages,"inferenceMs":elapsed,"cachePrefix":false})
}
fn new_decision() -> Decision {
    let request = Request::new("Add a test", vec![], Target::New, None).unwrap();
    request
        .resolve(response(&request, &[("boundary", " D")], 12.0))
        .unwrap()
}

#[tokio::test]
async fn calibration_keeps_the_original_model_and_candidate_identity_in_the_journal() {
    use sha2::{Digest, Sha256};
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("events.sqlite");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32]))
        .await
        .unwrap();
    let id = uuid::Uuid::now_v7();
    journal
        .stage_utterance(id, "Wait for me".into(), Target::New, None, vec![])
        .await
        .unwrap();
    let pending = journal.claim_input().await.unwrap().unwrap();
    let request = Request::new("Wait for me", tracks(), Target::New, None).unwrap();
    let raw = response(&request, &[("boundary", " D")], 12.0);
    let resolved = request.resolve(raw.clone()).unwrap();
    let recorded = resolved.scores["calibration"].clone();
    assert_eq!(recorded["backend"], "GlimmerLogits(local)");
    assert_eq!(recorded["cachePrefix"], false);
    assert_eq!(
        recorded["modelRevision"],
        "3e7677d7a40d348a3daba263a2b1c0aa41910710"
    );
    assert_eq!(
        recorded["promptVersion"],
        Sha256::digest(request.wire["prefix"].as_str().unwrap().as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    assert_eq!(recorded["labelTracks"]["1"], "track-a");
    assert_eq!(recorded["temperature"], 1.0);
    assert_eq!(
        recorded["requestHash"],
        Sha256::digest(b"wait for me")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    let equivalent = Request::new("  ＷＡＩＴ\nFOR Me ", tracks(), Target::New, None).unwrap();
    assert_eq!(
        equivalent
            .resolve(response(&equivalent, &[("boundary", " D")], 1.0))
            .unwrap()
            .scores["calibration"]["requestHash"],
        recorded["requestHash"]
    );
    let mut masked =
        Request::new("The original private text", tracks(), Target::New, None).unwrap();
    let mut tail: Value = serde_json::from_str(masked.wire["tail"].as_str().unwrap()).unwrap();
    tail["request"] = json!("<redacted>");
    masked.wire["tail"] = json!(tail.to_string());
    assert_eq!(
        masked
            .resolve(response(&masked, &[("boundary", " D")], 1.0))
            .unwrap()
            .scores["calibration"]["requestHash"],
        Sha256::digest(b"<redacted>")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    assert_eq!(resolved.scores["stages"], raw["stages"]);
    assert_ne!(
        recorded["id"],
        request.resolve(raw).unwrap().scores["calibration"]["id"]
    );
    journal
        .apply_input(pending.id, pending.revision, Outcome::Classified(resolved))
        .await
        .unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    let saved: String = db
        .query_row(
            "SELECT decision FROM utterances WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let saved: Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved["scores"]["calibration"], recorded);
    assert!(journal.read_utterance(id.to_string()).await.unwrap()["decision"]["scores"].is_null());
}

#[test]
fn explicit_new_and_lock_preserve_the_users_target_and_read_timing() {
    let new = Request::new("Wait for me", tracks(), Target::New, Some("track-a")).unwrap();
    assert_eq!(new.wire["stages"].as_array().unwrap().len(), 1);
    let resolved = new
        .resolve(response(&new, &[("boundary", " D")], 12.0))
        .unwrap();
    assert_eq!(resolved.kind, Kind::Fork);
    assert_eq!(resolved.boundary.as_deref(), Some("when-told"));
    let lock = Request::new("Stop", tracks(), Target::Lock("track-a".into()), None).unwrap();
    assert!(
        !lock.wire["stages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "lane")
    );
    assert!(
        !lock.wire["stages"][0]["candidates"]
            .as_array()
            .unwrap()
            .contains(&json!(" B"))
    );
    let resolved = lock
        .resolve(response(&lock, &[("decision", " D")], 12.0))
        .unwrap();
    assert_eq!(resolved.kind, Kind::Trim);
    assert_eq!(resolved.track_id.as_deref(), Some("track-a"));
    // The local service rejects a condition that refers to an excluded label.
    // Validate the wire contract for every destination mode, independently of
    // mocked decision scores.
    for target in [Target::None, Target::New, Target::Lock("track-a".into())] {
        let request = Request::new("Continue", tracks(), target, None).unwrap();
        let mut prior: std::collections::HashMap<&str, &Vec<Value>> =
            std::collections::HashMap::new();
        for stage in request.wire["stages"].as_array().unwrap() {
            if let Some(conditions) = stage["when"].as_object() {
                for (name, labels) in conditions {
                    let candidates: &Vec<Value> = prior.get(name.as_str()).unwrap();
                    assert!(
                        labels
                            .as_array()
                            .unwrap()
                            .iter()
                            .all(|label| candidates.contains(label))
                    );
                }
            }
            prior.insert(
                stage["name"].as_str().unwrap(),
                stage["candidates"].as_array().unwrap(),
            );
        }
    }
    let many: Vec<_> = (0..12)
        .map(|n| Candidate {
            id: format!("track-{n}"),
            label: "A".into(),
            title: format!("Task {n}"),
            status: "waiting".into(),
            activity: None,
            running: None,
            planned: Vec::new(),
        })
        .collect();
    let lock = Request::new(
        "Continue",
        vec![many[11].clone()],
        Target::Lock("track-11".into()),
        None,
    )
    .unwrap();
    assert_eq!(lock.wire["stages"][0]["name"], "decision");
    let selected = Request::new(
        "Continue",
        vec![many[11].clone()],
        Target::None,
        Some("track-11"),
    )
    .unwrap();
    let tail: Value = serde_json::from_str(selected.wire["tail"].as_str().unwrap()).unwrap();
    assert_eq!(tail["tracks"][0]["title"], "Task 11");
    assert_eq!(tail["selected"], 1);
}
#[test]
fn malformed_model_scores_and_late_results_cannot_be_adopted_silently() {
    let request = Request::new("More tests", tracks(), Target::None, Some("track-a")).unwrap();
    let valid = response(&request, &[("decision", " A"), ("lane", "1")], 1201.0);
    assert!(request.resolve(valid.clone()).unwrap().timed_out);
    for mode in [serde_json::Value::Null, json!(true)] {
        let mut wrong_mode = valid.clone();
        wrong_mode["cachePrefix"] = mode;
        assert!(request.resolve(wrong_mode).is_err());
    }
    let mut missing = valid.clone();
    missing["stages"]["decision"]["logprobs"]
        .as_object_mut()
        .unwrap()
        .remove(" H");
    assert!(request.resolve(missing).is_err());
    let mut invalid = valid.clone();
    invalid["stages"]["lane"]["selected"] = json!("unlisted");
    assert!(request.resolve(invalid).is_err());
    let mut unnormalized = valid;
    unnormalized["stages"]["lane"]["probabilities"]["1"] = json!(0.0);
    assert!(request.resolve(unnormalized).is_err());
}
#[tokio::test]
async fn cut_during_inference_wins_and_recovery_reclaims_only_unfinished_reading() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("events.sqlite");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32]))
        .await
        .unwrap();
    let id = uuid::Uuid::now_v7();
    journal
        .stage_utterance(id, "Add a test".into(), Target::New, None, Vec::new())
        .await
        .unwrap();
    let input = journal.claim_input().await.unwrap().unwrap();
    journal
        .cut_input(uuid::Uuid::now_v7(), id.to_string())
        .await
        .unwrap();
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(new_decision()),
        )
        .await
        .unwrap();
    let state = journal.snapshot().await.unwrap();
    assert_eq!(state["utterances"][0]["state"], "cut");
    assert_eq!(state["tracks"], json!([]));
    let second = uuid::Uuid::now_v7();
    journal
        .stage_utterance(second, "Second".into(), Target::New, None, Vec::new())
        .await
        .unwrap();
    journal.claim_input().await.unwrap();
    drop(journal);
    let journal = Journal::open(path, intercat_daemon::secrets::Key::new([7; 32]))
        .await
        .unwrap();
    journal.recover_inputs().await.unwrap();
    let input = journal.claim_input().await.unwrap().unwrap();
    assert_eq!(input.id, second.to_string());
    assert_eq!(input.revision, 2);
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(new_decision()),
        )
        .await
        .unwrap();
    let state = journal.snapshot().await.unwrap();
    assert_eq!(state["tracks"].as_array().unwrap().len(), 1);
    assert_eq!(state["utterances"][1]["state"], "placed");
}

#[tokio::test]
async fn holds_survive_recovery_and_only_the_current_answer_can_requeue_a_request() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("events.sqlite");
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32]))
        .await
        .unwrap();
    let id = uuid::Uuid::now_v7();
    journal
        .stage_utterance(id, "Wait for a step".into(), Target::New, None, Vec::new())
        .await
        .unwrap();
    let input = journal.claim_input().await.unwrap().unwrap();
    let mut decision = new_decision();
    decision.boundary = Some("after-clip".into());
    journal
        .apply_input(input.id, input.revision, Outcome::Classified(decision))
        .await
        .unwrap();
    assert!(
        journal.snapshot().await.unwrap()["tracks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let hold_op = uuid::Uuid::now_v7();
    let held = journal
        .resolve_input(hold_op, id.to_string(), 1, Resolution::Hold)
        .await
        .unwrap();
    assert_eq!(held["state"], "held");
    assert_eq!(held["revision"], 2);
    journal
        .resolve_input(hold_op, id.to_string(), 1, Resolution::Hold)
        .await
        .unwrap();
    assert!(
        journal
            .resolve_input(
                uuid::Uuid::now_v7(),
                id.to_string(),
                1,
                Resolution::Retry {
                    target: Target::New
                }
            )
            .await
            .is_err()
    );
    drop(journal);
    let journal = Journal::open(path, intercat_daemon::secrets::Key::new([7; 32]))
        .await
        .unwrap();
    journal.recover_inputs().await.unwrap();
    assert!(journal.claim_input().await.unwrap().is_none());
    journal
        .resolve_input(
            uuid::Uuid::now_v7(),
            id.to_string(),
            2,
            Resolution::Retry {
                target: Target::New,
            },
        )
        .await
        .unwrap();
    let retried = journal.claim_input().await.unwrap().unwrap();
    assert_eq!(retried.revision, 3);
    journal
        .apply_input(id.to_string(), 1, Outcome::Classified(new_decision()))
        .await
        .unwrap();
    assert!(
        journal.snapshot().await.unwrap()["tracks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    journal
        .apply_input(
            retried.id,
            retried.revision,
            Outcome::Classified(new_decision()),
        )
        .await
        .unwrap();
    let state = journal.snapshot().await.unwrap();
    let track = state["tracks"][0]["id"].as_str().unwrap().to_string();
    let stop = uuid::Uuid::now_v7();
    journal
        .stage_utterance(
            stop,
            "Stop".into(),
            Target::Lock(track.clone()),
            None,
            Vec::new(),
        )
        .await
        .unwrap();
    let input = journal.claim_input().await.unwrap().unwrap();
    let mut decision = new_decision();
    decision.kind = Kind::Trim;
    decision.track_id = Some(track);
    decision.boundary = Some("now".into());
    journal
        .apply_input(input.id, input.revision, Outcome::Classified(decision))
        .await
        .unwrap();
    assert_eq!(
        journal.snapshot().await.unwrap()["utterances"][1]["state"],
        "asked"
    );
}
