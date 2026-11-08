use intercat_daemon::{
    Result,
    app_commands::WaitingRequest,
    decision::{Decision, Kind, Outcome, Resolution, Target},
    journal::Journal,
    plan_store::{Action, Command, Generated},
    plans::{Draft, DraftItem, Plan, State},
    request_sources::Submission,
    secrets::Key,
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let source = root.path().join("dummy");
    std::fs::create_dir(&source)?;
    gix::init(&source)?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    journal
        .configure_project(
            Uuid::now_v7(),
            intercat_daemon::project::inspect(Uuid::now_v7(), "Dummy".into(), vec![source]).await?,
        )
        .await?;
    Ok((root, journal))
}
async fn queued(journal: &Journal, title: &str) -> Result<Plan> {
    journal
        .stage_document(Submission {
            op_id: Uuid::now_v7(),
            text: title.into(),
            target: Target::New,
            selected_track_id: None,
            attachment_ids: vec![],
            sources: None,
            execute_allowed: false,
            plan_item: None,
        })
        .await?;
    Ok(journal.plans().await?.pop().unwrap())
}
async fn generate(journal: &Journal, plan: &Plan) -> Result<Plan> {
    let folder = journal
        .plan_context(plan.id)
        .await?
        .folders
        .into_iter()
        .next()
        .unwrap();
    journal
        .begin_plan(plan.id, plan.generation.id, plan.rev)
        .await?;
    journal
        .generated_plan(
            plan.id,
            plan.generation.id,
            plan.rev,
            Generated::Draft(Draft {
                assumptions: vec![],
                conflicts: vec![],
                summary: plan.source_id.clone(),
                items: (0..2)
                    .map(|i| DraftItem {
                        id: format!("job-{i}"),
                        track_ref: "new:1".into(),
                        folder_id: folder.clone(),
                        title: format!("Dummy {i}"),
                        instruction: format!("Write dummy-{i}.txt"),
                        constraints: vec![],
                        done_when: "Dummy exists".into(),
                        after: vec![],
                    })
                    .collect(),
            }),
            vec![],
        )
        .await?;
    current(journal, plan.id).await
}
async fn ready(journal: &Journal) -> Result<Plan> {
    let plan = queued(journal, "Plan the dummy tasks").await?;
    generate(journal, &plan).await
}
async fn current(journal: &Journal, id: Uuid) -> Result<Plan> {
    Ok(journal
        .plans()
        .await?
        .into_iter()
        .find(|plan| plan.id == id)
        .unwrap())
}
fn action(plan: &Plan, action: Action) -> Command {
    Command {
        op_id: Uuid::now_v7(),
        plan_id: plan.id,
        revision: plan.rev,
        action,
    }
}
fn go_decision() -> Decision {
    Decision {
        kind: Kind::Command,
        track_id: None,
        boundary: None,
        command: Some("start-waiting".into()),
        confidence: 1.,
        inference_ms: 0.,
        timed_out: false,
        scores: json!({}),
    }
}
async fn say(journal: &Journal) -> Result<Uuid> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(id, "それで進めて".into(), Target::None, None, vec![])
        .await?;
    Ok(id)
}
async fn read_go(journal: &Journal, id: Uuid) -> Result<Value> {
    let input = journal.claim_input().await?.unwrap();
    assert_eq!(input.id, id.to_string());
    journal
        .apply_input(input.id, input.revision, Outcome::Classified(go_decision()))
        .await?;
    journal.read_utterance(id.to_string()).await
}
async fn choose(journal: &Journal, word: &Value, plan: &Plan, op: Uuid) -> Result<Value> {
    journal
        .resolve_input(
            op,
            word["id"].as_str().unwrap().into(),
            word["revision"].as_i64().unwrap(),
            Resolution::StartWaiting {
                choice: WaitingRequest::Plan {
                    plan_id: plan.id,
                    revision: plan.rev,
                },
            },
        )
        .await
}

#[tokio::test]
async fn an_unseen_plan_waits_for_review_and_an_exact_explicit_choice() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let plan = ready(&journal).await?;
    let go = say(&journal).await?;
    let word = read_go(&journal, go).await?;
    assert_eq!(word["state"], "asked");
    assert_eq!(word["startWaiting"]["candidates"][0]["kind"], "plan");
    assert_eq!(word["startWaiting"]["candidates"][0]["available"], false);
    assert!(
        choose(&journal, &word, &plan, Uuid::now_v7())
            .await
            .is_err()
    );
    assert_eq!(current(&journal, plan.id).await?.state, State::Draft);
    journal.plan_command(action(&plan, Action::Review)).await?;
    assert_eq!(
        journal.read_utterance(go.to_string()).await?["startWaiting"]["candidates"][0]["available"],
        true
    );
    let op = Uuid::now_v7();
    let result = choose(&journal, &word, &plan, op).await?;
    assert_eq!(choose(&journal, &word, &plan, op).await?, result);
    assert_eq!(result["state"], "landed");
    assert_eq!(
        result["startWaiting"]["started"]["planId"],
        plan.id.to_string()
    );
    let started = current(&journal, plan.id).await?;
    assert_eq!(started.state, State::Running);
    assert_eq!(started.rev, plan.rev + 1);
    assert!(!started.execute_allowed);
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn a_sole_reviewed_plan_starts_from_words_without_starting_the_words_as_a_job() -> Result<()>
{
    let (_root, journal) = fixture().await?;
    let plan = ready(&journal).await?;
    journal.plan_command(action(&plan, Action::Review)).await?;
    let go = say(&journal).await?;
    let word = read_go(&journal, go).await?;
    assert_eq!(word["state"], "landed");
    assert_eq!(current(&journal, plan.id).await?.state, State::Running);
    assert_eq!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(word["decision"]["trackId"], Value::Null);
    Ok(())
}

#[tokio::test]
async fn plan_revisions_and_new_plans_cannot_change_a_captured_go_ahead() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let plan = ready(&journal).await?;
    journal.plan_command(action(&plan, Action::Review)).await?;
    let go = say(&journal).await?;
    let changed = journal
        .plan_command(action(
            &plan,
            Action::Move {
                item_id: plan.items[0].plan_item_id,
                direction: intercat_daemon::plan_store::Direction::Later,
            },
        ))
        .await?;
    journal
        .plan_command(action(&changed, Action::Review))
        .await?;
    let word = read_go(&journal, go).await?;
    assert_eq!(word["state"], "asked");
    assert!(
        choose(&journal, &word, &changed, Uuid::now_v7())
            .await
            .is_err()
    );
    assert_eq!(current(&journal, plan.id).await?.state, State::Draft);
    let another = ready(&journal).await?;
    assert!(
        choose(&journal, &word, &another, Uuid::now_v7())
            .await
            .is_err()
    );
    assert_eq!(
        word["startWaiting"]["candidates"].as_array().unwrap().len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn a_plan_that_arrives_after_the_words_waits_for_a_new_go_ahead() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let queued = queued(&journal, "Late dummy plan").await?;
    let go = say(&journal).await?;
    let plan = generate(&journal, &queued).await?;
    journal.plan_command(action(&plan, Action::Review)).await?;
    let word = read_go(&journal, go).await?;
    assert_eq!(word["state"], "asked");
    assert_eq!(word["startWaiting"]["candidates"], json!([]));
    assert_eq!(current(&journal, plan.id).await?.state, State::Draft);
    Ok(())
}

#[tokio::test]
async fn multiple_plans_need_a_choice_and_disk_failure_rolls_back_both_sides() -> Result<()> {
    let (root, journal) = fixture().await?;
    let a = ready(&journal).await?;
    let b = ready(&journal).await?;
    for plan in [&a, &b] {
        journal.plan_command(action(plan, Action::Review)).await?;
    }
    let go = say(&journal).await?;
    let word = read_go(&journal, go).await?;
    assert_eq!(word["state"], "asked");
    assert_eq!(
        word["startWaiting"]["candidates"].as_array().unwrap().len(),
        2
    );
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER refuse_go BEFORE UPDATE ON utterances WHEN NEW.state='landed' AND json_extract(NEW.decision,'$.command')='start-waiting' BEGIN SELECT RAISE(ABORT,'dummy disk failure'); END;")?;
    let op = Uuid::now_v7();
    assert!(choose(&journal, &word, &b, op).await.is_err());
    assert_eq!(current(&journal, b.id).await?.rev, b.rev);
    assert_eq!(current(&journal, b.id).await?.state, State::Draft);
    assert!(
        journal
            .operation(Uuid::new_v5(&go, b"start-waiting"))
            .await?
            .is_none()
    );
    db.execute_batch("DROP TRIGGER refuse_go")?;
    choose(&journal, &word, &b, op).await?;
    assert_eq!(current(&journal, a.id).await?.state, State::Draft);
    assert_eq!(current(&journal, b.id).await?.state, State::Running);
    Ok(())
}

#[tokio::test]
async fn a_plan_and_an_ordinary_request_require_an_explicit_choice() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let plan = ready(&journal).await?;
    journal.plan_command(action(&plan, Action::Review)).await?;
    let ordinary = Uuid::now_v7();
    journal
        .stage_utterance(
            ordinary,
            "Wait for my go on this dummy task".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    let mut decision = go_decision();
    decision.kind = Kind::Fork;
    decision.command = None;
    decision.boundary = Some("when-told".into());
    journal
        .apply_input(input.id, input.revision, Outcome::Classified(decision))
        .await?;
    let go = say(&journal).await?;
    let word = read_go(&journal, go).await?;
    assert_eq!(word["state"], "asked");
    assert_eq!(
        word["startWaiting"]["candidates"].as_array().unwrap().len(),
        2
    );
    choose(&journal, &word, &plan, Uuid::now_v7()).await?;
    assert_eq!(current(&journal, plan.id).await?.state, State::Running);
    assert_eq!(
        journal.read_utterance(ordinary.to_string()).await?["decision"]["boundary"],
        "when-told"
    );
    Ok(())
}

#[tokio::test]
async fn a_request_switched_to_one_job_can_still_wait_for_a_spoken_go() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let source = Uuid::now_v7();
    journal
        .stage_utterance(
            source,
            "Plan a dummy job, then wait for my go".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    let mut decision = go_decision();
    decision.kind = Kind::Plan;
    decision.command = None;
    journal
        .apply_input(input.id, input.revision, Outcome::Classified(decision))
        .await?;
    let plan = journal.plans().await?.pop().unwrap();
    journal
        .begin_plan(plan.id, plan.generation.id, plan.rev)
        .await?;
    journal
        .generated_plan(
            plan.id,
            plan.generation.id,
            plan.rev,
            Generated::TimedOut,
            vec![],
        )
        .await?;
    let failed = current(&journal, plan.id).await?;
    journal
        .plan_command(action(&failed, Action::RunAsOne))
        .await?;
    let input = journal.claim_input().await?.unwrap();
    let mut decision = go_decision();
    decision.kind = Kind::Fork;
    decision.command = None;
    decision.boundary = Some("when-told".into());
    journal
        .apply_input(input.id, input.revision, Outcome::Classified(decision))
        .await?;
    let go = say(&journal).await?;
    let word = read_go(&journal, go).await?;
    assert_eq!(word["state"], "landed");
    assert_eq!(word["startWaiting"]["started"]["kind"], "input");
    assert_eq!(
        word["startWaiting"]["started"]["inputId"],
        source.to_string()
    );
    assert_eq!(
        journal.read_utterance(source.to_string()).await?["decision"]["boundary"],
        "now"
    );
    assert_eq!(current(&journal, plan.id).await?.state, State::SingleJob);
    Ok(())
}

#[tokio::test]
async fn a_named_track_cannot_start_a_plan_that_also_runs_another_track() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let mut tracks = Vec::new();
    for name in ["Dummy A", "Dummy B"] {
        let id = Uuid::now_v7();
        journal
            .stage_utterance(id, name.into(), Target::New, None, vec![])
            .await?;
        let input = journal.claim_input().await?.unwrap();
        let mut decision = go_decision();
        decision.kind = Kind::Fork;
        decision.command = None;
        decision.boundary = Some("now".into());
        journal
            .apply_input(input.id, input.revision, Outcome::Classified(decision))
            .await?;
        tracks.push(
            journal.read_utterance(id.to_string()).await?["decision"]["trackId"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    let mut plans = Vec::new();
    for destinations in [
        vec![tracks[0].clone(), tracks[1].clone()],
        vec![tracks[0].clone()],
    ] {
        let plan = queued(&journal, "Scoped dummy plan").await?;
        let folder = journal
            .plan_context(plan.id)
            .await?
            .folders
            .into_iter()
            .next()
            .unwrap();
        journal
            .begin_plan(plan.id, plan.generation.id, plan.rev)
            .await?;
        journal
            .generated_plan(
                plan.id,
                plan.generation.id,
                plan.rev,
                Generated::Draft(Draft {
                    assumptions: vec![],
                    conflicts: vec![],
                    summary: "Scoped dummy plan".into(),
                    items: destinations
                        .into_iter()
                        .enumerate()
                        .map(|(i, track_ref)| DraftItem {
                            id: format!("job-{i}"),
                            track_ref,
                            folder_id: folder.clone(),
                            title: format!("Dummy {i}"),
                            instruction: "Use only dummy data".into(),
                            constraints: vec![],
                            done_when: "Done".into(),
                            after: vec![],
                        })
                        .collect(),
                }),
                vec![],
            )
            .await?;
        let ready = current(&journal, plan.id).await?;
        journal.plan_command(action(&ready, Action::Review)).await?;
        plans.push(ready);
    }
    let go = Uuid::now_v7();
    journal
        .stage_utterance(
            go,
            "Start only A's waiting work".into(),
            Target::Lock(tracks[0].clone()),
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    let mut decision = go_decision();
    decision.track_id = Some(tracks[0].clone());
    journal
        .apply_input(input.id, input.revision, Outcome::Classified(decision))
        .await?;
    assert_eq!(current(&journal, plans[0].id).await?.state, State::Draft);
    assert_eq!(current(&journal, plans[1].id).await?.state, State::Running);
    assert_eq!(
        journal.read_utterance(go.to_string()).await?["startWaiting"]["candidates"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    Ok(())
}
