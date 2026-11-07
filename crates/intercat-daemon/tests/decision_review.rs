use intercat_daemon::{
    decision::{Candidate, Decision, Kind, Outcome, Request, Resolution, Target},
    decision_review::{self, Choice, Stage},
    journal::Journal,
    runtime::WorkerState,
};
use serde_json::{Value, json};
use uuid::Uuid;

fn tracks() -> Vec<Candidate> {
    ["a", "b"]
        .map(|id| Candidate {
            id: id.into(),
            label: "A".into(),
            title: id.into(),
            status: "working".into(),
            activity: None,
            running: None,
            planned: Vec::new(),
        })
        .into()
}
fn resolve(
    text: &str,
    tracks: Vec<Candidate>,
    target: Target,
    choices: &[(&str, &str, f64)],
) -> Decision {
    let request = Request::new(text, tracks, target, None).unwrap();
    let mut selected = serde_json::Map::new();
    let mut stages = serde_json::Map::new();
    for definition in request.wire["stages"].as_array().unwrap() {
        if definition["when"].as_object().is_some_and(|conditions| {
            conditions.iter().any(|(key, allowed)| {
                !allowed
                    .as_array()
                    .unwrap()
                    .contains(selected.get(key).unwrap())
            })
        }) {
            continue;
        }
        let name = definition["name"].as_str().unwrap();
        let labels = definition["candidates"].as_array().unwrap();
        let (_, label, confidence) = choices
            .iter()
            .find(|(stage, _, _)| *stage == name)
            .copied()
            .unwrap_or((name, labels[0].as_str().unwrap(), 0.95));
        let mut probabilities = serde_json::Map::new();
        let mut logprobs = serde_json::Map::new();
        for candidate in labels {
            let candidate = candidate.as_str().unwrap();
            let p = if candidate == label {
                confidence
            } else {
                (1.0 - confidence) / (labels.len() - 1) as f64
            };
            probabilities.insert(candidate.into(), json!(p));
            logprobs.insert(candidate.into(), json!(p.ln()));
        }
        selected.insert(name.into(), json!(label));
        stages.insert(
            name.into(),
            json!({"selected":label,"probabilities":probabilities,"logprobs":logprobs}),
        );
    }
    request
        .resolve(json!({"stages":stages,"cachePrefix":false,"inferenceMs":10.0}))
        .unwrap()
}

#[test]
fn uncertainty_is_checked_per_stage_and_trim_requires_point_eight() {
    let lane = resolve(
        "Continue",
        tracks(),
        Target::None,
        &[("decision", " A", 0.95), ("lane", "1", 0.4)],
    );
    assert_eq!(
        decision_review::read(&lane).unwrap().unwrap().stage,
        Stage::Lane
    );
    let operation = resolve(
        "Continue",
        tracks(),
        Target::Lock("a".into()),
        &[("decision", " A", 0.4)],
    );
    assert_eq!(
        decision_review::read(&operation).unwrap().unwrap().stage,
        Stage::Operation
    );
    for (confidence, expected) in [(0.55, true), (0.799, true), (0.8, false)] {
        let decision = resolve(
            "Stop now",
            tracks(),
            Target::Lock("a".into()),
            &[("decision", " D", confidence)],
        );
        assert_eq!(
            decision_review::read(&decision).unwrap().is_some(),
            expected
        );
    }
    let parked = resolve(
        "Mmm",
        tracks(),
        Target::None,
        &[("decision", " A", 0.2), ("lane", "1", 0.9)],
    );
    assert_eq!(parked.kind, Kind::Park);
    assert!(decision_review::read(&parked).unwrap().is_none());
}

#[test]
fn timing_commands_and_explicit_new_have_their_own_questions() {
    let boundary = resolve(
        "Do this later",
        tracks(),
        Target::New,
        &[("boundary", " D", 0.4)],
    );
    assert_eq!(
        decision_review::read(&boundary).unwrap().unwrap().stage,
        Stage::Boundary
    );
    let command = resolve(
        "Open a panel",
        tracks(),
        Target::None,
        &[("decision", " H", 0.95), ("command", "1", 0.4)],
    );
    assert_eq!(
        decision_review::read(&command).unwrap().unwrap().stage,
        Stage::Command
    );
    let timing = resolve(
        "Wait until I say go",
        tracks(),
        Target::Lock("a".into()),
        &[("decision", " A", 0.95)],
    );
    assert_eq!(
        decision_review::read(&timing).unwrap().unwrap().stage,
        Stage::Operation
    );
    let other = resolve(
        "Continue",
        tracks(),
        Target::None,
        &[("decision", " D", 0.7), ("lane", "9", 0.9)],
    );
    assert!(decision_review::read(&other).unwrap().is_none());
}

async fn fixture() -> (tempfile::TempDir, Journal, WorkerState) {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    let new = resolve("Start", vec![], Target::New, &[("boundary", " D", 0.95)]);
    let id = place(&journal, new).await;
    let state = journal.snapshot().await.unwrap();
    let track = word(&state, &id)["decision"]["trackId"]
        .as_str()
        .unwrap()
        .to_owned();
    let worker = WorkerState {
        track_id: track,
        phase: "working".into(),
        session_id: Some("session".into()),
        secret_revision: None,
        identity: None,
        detail: None,
        active_turn: Some(Uuid::now_v7()),
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    journal.worker_state(worker.clone()).await.unwrap();
    (root, journal, worker)
}
async fn place(journal: &Journal, decision: Decision) -> String {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, "Dummy request".into(), Target::None, None, vec![])
        .await
        .unwrap();
    let input = journal.claim_input().await.unwrap().unwrap();
    journal
        .apply_input(input.id, input.revision, Outcome::Classified(decision))
        .await
        .unwrap();
    id.to_string()
}
fn word(state: &Value, id: &str) -> Value {
    state["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|word| word["id"] == id)
        .unwrap()
        .clone()
}
fn medium_trim(worker: &WorkerState) -> Decision {
    resolve(
        "Stop now",
        vec![Candidate {
            id: worker.track_id.clone(),
            label: "A".into(),
            title: "Work".into(),
            status: "working".into(),
            activity: None,
            running: None,
            planned: Vec::new(),
        }],
        Target::Lock(worker.track_id.clone()),
        &[("decision", " D", 0.7)],
    )
}

#[tokio::test]
async fn confirming_trim_is_atomic_idempotent_and_bound_to_the_original_turn() {
    let (_root, journal, worker) = fixture().await;
    let id = place(&journal, medium_trim(&worker)).await;
    let snapshot = journal.snapshot().await.unwrap();
    assert_eq!(word(&snapshot, &id)["state"], "asked");
    assert_eq!(
        word(&snapshot, &id)["review"]["turn"]["turnId"],
        worker.active_turn.unwrap().to_string()
    );
    assert!(snapshot["redirects"].as_array().unwrap().is_empty());
    let op = Uuid::now_v7();
    let result = journal
        .resolve_input(
            op,
            id.clone(),
            1,
            Resolution::Confirm {
                choice: Choice::StopAndSend,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["state"], "redirecting");
    assert_eq!(result["revision"], 2);
    let replay = journal
        .resolve_input(
            op,
            id.clone(),
            1,
            Resolution::Confirm {
                choice: Choice::StopAndSend,
            },
        )
        .await
        .unwrap();
    assert_eq!(result, replay);
    let snapshot = journal.snapshot().await.unwrap();
    assert_eq!(snapshot["redirects"].as_array().unwrap().len(), 1);
    assert_eq!(
        snapshot["redirects"][0]["turnId"],
        worker.active_turn.unwrap().to_string()
    );
    assert!(
        journal
            .resolve_input(
                Uuid::now_v7(),
                id,
                1,
                Resolution::Confirm {
                    choice: Choice::StopAndSend
                }
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_new_turn_is_never_interrupted_by_an_old_confirmation() {
    let (_root, journal, mut worker) = fixture().await;
    let id = place(&journal, medium_trim(&worker)).await;
    worker.active_turn = Some(Uuid::now_v7());
    journal.worker_state(worker.clone()).await.unwrap();
    let result = journal
        .resolve_input(
            Uuid::now_v7(),
            id.clone(),
            1,
            Resolution::Confirm {
                choice: Choice::StopAndSend,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["state"], "asked");
    assert_eq!(result["review"]["stage"], "stale-turn");
    assert!(
        journal.snapshot().await.unwrap()["redirects"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let result = journal
        .resolve_input(
            Uuid::now_v7(),
            id,
            2,
            Resolution::Confirm {
                choice: Choice::Instruction,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["state"], "placed");
    assert_eq!(result["decision"]["kind"], "extend");
    assert_eq!(result["decision"]["boundary"], "after-turn");
}

#[tokio::test]
async fn choosing_after_turn_or_a_finished_turn_does_not_create_an_interrupt() {
    let (_root, journal, mut worker) = fixture().await;
    let id = place(&journal, medium_trim(&worker)).await;
    let result = journal
        .resolve_input(
            Uuid::now_v7(),
            id,
            1,
            Resolution::Confirm {
                choice: Choice::AfterTurn,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["decision"]["kind"], "schedule");
    assert_eq!(result["decision"]["boundary"], "after-turn");
    let id = place(&journal, medium_trim(&worker)).await;
    worker.active_turn = None;
    worker.phase = "ready".into();
    journal.worker_state(worker).await.unwrap();
    let result = journal
        .resolve_input(
            Uuid::now_v7(),
            id,
            1,
            Resolution::Confirm {
                choice: Choice::StopAndSend,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["state"], "placed");
    assert_eq!(result["decision"]["kind"], "extend");
    assert_eq!(result["decision"]["boundary"], "now");
    assert!(
        journal.snapshot().await.unwrap()["redirects"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn held_questions_cannot_start_and_unrelated_choices_are_rejected() {
    let (_root, journal, worker) = fixture().await;
    let mut decision = medium_trim(&worker);
    decision.kind = Kind::Extend;
    let id = place(&journal, decision).await;
    assert!(
        journal
            .resolve_input(
                Uuid::now_v7(),
                id.clone(),
                1,
                Resolution::Confirm {
                    choice: Choice::Command {
                        command: "push".into()
                    }
                }
            )
            .await
            .is_err()
    );
    let held = journal
        .resolve_input(Uuid::now_v7(), id.clone(), 1, Resolution::Hold)
        .await
        .unwrap();
    assert_eq!(held["revision"], 2);
    assert!(
        journal
            .start_held_input(Uuid::now_v7(), id.clone(), 2)
            .await
            .is_err()
    );
    assert!(
        journal
            .resolve_input(
                Uuid::now_v7(),
                id,
                2,
                Resolution::Confirm {
                    choice: Choice::StopAndSend
                }
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn human_choices_finish_a_new_track_and_prepare_only_the_selected_command() {
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(
        root.path().join("db"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await
    .unwrap();
    let id = place(
        &journal,
        resolve(
            "Do it later",
            vec![],
            Target::New,
            &[("boundary", " D", 0.4)],
        ),
    )
    .await;
    assert!(
        journal.snapshot().await.unwrap()["tracks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let result = journal
        .resolve_input(
            Uuid::now_v7(),
            id,
            1,
            Resolution::Confirm {
                choice: Choice::WhenTold,
            },
        )
        .await
        .unwrap();
    assert_eq!(result["state"], "placed");
    assert_eq!(result["decision"]["kind"], "fork");
    assert_eq!(result["decision"]["boundary"], "when-told");
    assert_eq!(
        journal.snapshot().await.unwrap()["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let id = place(
        &journal,
        resolve(
            "Open a panel",
            tracks(),
            Target::None,
            &[("decision", " H", 0.4), ("command", "1", 0.4)],
        ),
    )
    .await;
    assert_eq!(
        word(&journal.snapshot().await.unwrap(), &id)["review"]["stage"],
        "command"
    );
    let result = journal
        .resolve_input(
            Uuid::now_v7(),
            id,
            1,
            Resolution::Confirm {
                choice: Choice::Command {
                    command: "export".into(),
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(result["state"], "placed");
    assert_eq!(result["decision"]["command"], "export");
}

#[test]
fn rereading_checks_timing_in_the_latest_context_without_retaining_old_context() {
    let mut request = Request::new(
        "A build report might help.",
        tracks(),
        Target::Lock("a".into()),
        None,
    )
    .unwrap();
    let labels = request.wire["stages"][0]["candidates"].as_array().unwrap();
    let mut probabilities = serde_json::Map::new();
    let mut logprobs = serde_json::Map::new();
    for value in labels {
        let label = value.as_str().unwrap();
        let probability: f64 = if label == " A" {
            0.95
        } else {
            0.05 / (labels.len() - 1) as f64
        };
        probabilities.insert(label.into(), json!(probability));
        logprobs.insert(label.into(), json!(probability.ln()));
    }
    let response = json!({"cachePrefix":false,"inferenceMs":1,
        "stages":{"decision":{"selected":" A","probabilities":probabilities,"logprobs":logprobs}}});
    request
        .reread("After the current turn, write that build report.")
        .unwrap();
    assert_eq!(
        decision_review::read(&request.resolve(response.clone()).unwrap())
            .unwrap()
            .unwrap()
            .stage,
        Stage::Operation
    );
    request.reread("Write that build report.").unwrap();
    assert!(
        decision_review::read(&request.resolve(response).unwrap())
            .unwrap()
            .is_none()
    );
}
