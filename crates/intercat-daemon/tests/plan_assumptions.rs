use intercat_daemon::{
    Result,
    decision::Target,
    journal::Journal,
    plan_feedback::Reply,
    plan_store::{Action, Command, Generated},
    plans::{Draft, DraftAssumption, DraftItem, Plan},
    request_sources::Submission,
    secrets::Key,
};
use serde_json::{Value, json};
use uuid::Uuid;

async fn fixture() -> Result<(tempfile::TempDir, Journal, Plan)> {
    let root = tempfile::tempdir()?;
    let project = root.path().join("dummy");
    std::fs::create_dir(&project)?;
    assert!(
        std::process::Command::new("/usr/bin/git")
            .args(["init", "-q", "-b", "main"])
            .arg(&project)
            .status()?
            .success()
    );
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    journal
        .configure_project(
            Uuid::now_v7(),
            intercat_daemon::project::inspect(Uuid::now_v7(), "Dummy".into(), vec![project])
                .await?,
        )
        .await?;
    journal
        .stage_document(Submission {
            op_id: Uuid::now_v7(),
            text: "Plan two dummy reports.".into(),
            target: Target::New,
            selected_track_id: None,
            attachment_ids: vec![],
            sources: None,
            execute_allowed: false,
            plan_item: None,
        })
        .await?;
    let plan = journal.plans().await?.pop().unwrap();
    let folder = journal
        .plan_context(plan.id)
        .await?
        .folders
        .into_iter()
        .next()
        .unwrap();
    let draft = Draft {
        summary: "Two dummy reports".into(),
        conflicts: vec![],
        assumptions: vec![
            DraftAssumption {
                what: "Report format".into(),
                default: "JSON".into(),
            },
            DraftAssumption {
                what: "Report language".into(),
                default: "English".into(),
            },
        ],
        items: vec![DraftItem {
            id: "report".into(),
            track_ref: "new:1".into(),
            folder_id: folder,
            title: "Dummy report".into(),
            instruction: "Create a dummy report.".into(),
            constraints: vec![],
            done_when: "Report exists".into(),
            after: vec![],
        }],
    };
    finish(&journal, &plan, draft).await?;
    let ready = journal.plans().await?.pop().unwrap();
    Ok((root, journal, ready))
}
async fn finish(journal: &Journal, plan: &Plan, draft: Draft) -> Result<()> {
    journal
        .begin_plan(plan.id, plan.generation.id, plan.rev)
        .await?;
    journal
        .generated_plan(
            plan.id,
            plan.generation.id,
            plan.rev,
            Generated::Draft(draft),
            vec![],
        )
        .await?;
    Ok(())
}
fn keep(plan: &Plan, id: Uuid) -> Command {
    Command {
        op_id: Uuid::now_v7(),
        plan_id: plan.id,
        revision: plan.rev,
        action: Action::KeepAssumption { assumption_id: id },
    }
}
fn reply(card: &Value) -> Reply {
    Reply {
        op_id: Uuid::now_v7(),
        plan_id: card["planId"].as_str().unwrap().parse().unwrap(),
        revision: card["planRevision"].as_u64().unwrap(),
        question_id: card["id"].as_str().unwrap().parse().unwrap(),
        question: format!(
            "{}\n{}",
            card["text"].as_str().unwrap(),
            card["detail"].as_str().unwrap()
        ),
        text: "Use CSV for the dummy report.".into(),
    }
}
#[tokio::test]
async fn planner_assumptions_keep_is_durable_replayable_and_never_starts_work() -> Result<()> {
    let (root, journal, plan) = fixture().await?;
    let card = journal.snapshot().await?["planAssumptions"][0].clone();
    let command = keep(&plan, plan.assumptions[0].id);
    let kept = journal.plan_command(command.clone()).await?;
    assert_eq!(kept.rev, plan.rev);
    assert!(kept.assumptions[0].kept_at.is_some());
    journal.plan_command(command.clone()).await?;
    assert_eq!(
        journal.snapshot().await?["planAssumptions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        journal.reply_to_plan(reply(&card)).await?["state"],
        "orphaned"
    );
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    assert_eq!(
        journal.plan_command(command).await?.assumptions[0].kept_at,
        kept.assumptions[0].kept_at
    );
    assert!(journal.claim_input().await?.is_none());
    Ok(())
}
#[tokio::test]
async fn assumption_change_replans_and_preserves_other_answers_without_reusing_the_old_question()
-> Result<()> {
    let (_root, journal, plan) = fixture().await?;
    journal
        .plan_command(keep(&plan, plan.assumptions[1].id))
        .await?;
    let card = journal.snapshot().await?["planAssumptions"][0].clone();
    let request = reply(&card);
    let receipt = journal.reply_to_plan(request.clone()).await?;
    assert_eq!(receipt["state"], "landed");
    assert_eq!(journal.reply_to_plan(request).await?, receipt);
    let queued = journal.plans().await?.pop().unwrap();
    assert_eq!(
        queued.generation.state,
        intercat_daemon::plans::GenerationState::Queued
    );
    assert!(
        journal.snapshot().await?["planAssumptions"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let old = &plan.items[0];
    finish(
        &journal,
        &queued,
        Draft {
            summary: plan.summary.clone(),
            conflicts: vec![],
            assumptions: vec![
                DraftAssumption {
                    what: "Report format".into(),
                    default: "CSV".into(),
                },
                DraftAssumption {
                    what: "Report language".into(),
                    default: "English".into(),
                },
            ],
            items: vec![DraftItem {
                id: old.plan_item_id.to_string(),
                track_ref: old.track_ref.clone(),
                folder_id: old.folder_id.clone(),
                title: old.title.clone(),
                instruction: "Write a dummy CSV report".into(),
                constraints: vec![],
                done_when: old.done_when.clone(),
                after: vec![],
            }],
        },
    )
    .await?;
    let revised = journal.plans().await?.pop().unwrap();
    assert_eq!(revised.items[0].plan_item_id, old.plan_item_id);
    assert_ne!(revised.assumptions[0].id, plan.assumptions[0].id);
    assert_eq!(revised.assumptions[1].id, plan.assumptions[1].id);
    assert!(revised.assumptions[1].kept_at.is_some());
    assert_eq!(
        journal.reply_to_plan(reply(&card)).await?["state"],
        "orphaned"
    );
    Ok(())
}
#[tokio::test]
async fn assumption_keep_failure_rolls_back_and_retries_the_same_operation() -> Result<()> {
    let (root, journal, plan) = fixture().await?;
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER dummy_assumption_failure BEFORE UPDATE ON plans BEGIN SELECT RAISE(ABORT,'dummy full disk'); END;")?;
    let command = keep(&plan, plan.assumptions[0].id);
    assert!(journal.plan_command(command.clone()).await.is_err());
    assert!(journal.plans().await?[0].assumptions[0].kept_at.is_none());
    db.execute_batch("DROP TRIGGER dummy_assumption_failure;")?;
    assert!(
        journal.plan_command(command).await?.assumptions[0]
            .kept_at
            .is_some()
    );
    Ok(())
}
#[test]
fn planner_requires_explicit_bounded_assumptions_with_unique_nonempty_questions() {
    let mut wire = json!({"summary":"Dummy","items":[],"conflicts":[]});
    assert!(serde_json::from_value::<Draft>(wire.clone()).is_err());
    wire["assumptions"] = json!([]);
    assert!(serde_json::from_value::<Draft>(wire).is_ok());
}

#[tokio::test]
async fn invalid_planner_assumptions_are_rejected_before_becoming_cards() -> Result<()> {
    let (_root, journal, plan) = fixture().await?;
    let item = &plan.items[0];
    let base = Draft {
        summary: "Dummy".into(),
        conflicts: vec![],
        assumptions: vec![],
        items: vec![DraftItem {
            id: "one".into(),
            track_ref: item.track_ref.clone(),
            folder_id: item.folder_id.clone(),
            title: item.title.clone(),
            instruction: item.instruction.clone(),
            constraints: vec![],
            done_when: item.done_when.clone(),
            after: vec![],
        }],
    };
    let context = journal.plan_context(plan.id).await?;
    for assumptions in [
        vec![DraftAssumption {
            what: "".into(),
            default: "JSON".into(),
        }],
        vec![DraftAssumption {
            what: "Format".into(),
            default: " ".into(),
        }],
        vec![DraftAssumption {
            what: "x".repeat(1025),
            default: "JSON".into(),
        }],
        vec![DraftAssumption {
            what: "Format".into(),
            default: "x".repeat(1025),
        }],
        vec![
            DraftAssumption {
                what: "Format".into(),
                default: "JSON".into()
            };
            2
        ],
        (0..33)
            .map(|n| DraftAssumption {
                what: format!("Choice {n}"),
                default: "JSON".into(),
            })
            .collect(),
    ] {
        let mut draft = base.clone();
        draft.assumptions = assumptions;
        assert!(intercat_daemon::plans::materialize(plan.id, None, &draft, &context).is_err());
    }
    Ok(())
}
