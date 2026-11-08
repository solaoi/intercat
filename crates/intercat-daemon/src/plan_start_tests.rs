use super::*;
use crate::{
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    plan_store::{self, Generated},
    plans::{Draft, DraftItem},
    secrets::Key,
};

async fn fixture(allowed: bool) -> Result<(tempfile::TempDir, Journal, Connection, Plan)> {
    let root = tempfile::tempdir()?;
    let source = root.path().join("dummy");
    std::fs::create_dir(&source)?;
    gix::init(&source)?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    journal
        .configure_project(
            Uuid::now_v7(),
            crate::project::inspect(Uuid::now_v7(), "Dummy".into(), vec![source]).await?,
        )
        .await?;
    journal
        .change_plans_start(Change {
            op_id: Uuid::now_v7(),
            revision: 0,
            mode: Mode::AfterDelay,
        })
        .await?;
    let id = Uuid::now_v7();
    journal
        .stage_document(crate::request_sources::Submission {
            op_id: id,
            text: "Split these dummy jobs.".into(),
            target: Target::New,
            selected_track_id: None,
            attachment_ids: vec![],
            sources: None,
            execute_allowed: allowed,
            plan_item: None,
        })
        .await?;
    if allowed {
        let input = journal.claim_input().await?.unwrap();
        journal
            .apply_input(
                input.id,
                input.revision,
                Outcome::Classified(Decision {
                    kind: Kind::Plan,
                    track_id: None,
                    boundary: None,
                    command: None,
                    confidence: 1.,
                    inference_ms: 0.,
                    timed_out: false,
                    scores: serde_json::json!({}),
                }),
            )
            .await?;
    }
    let plan = journal.plans().await?.pop().unwrap();
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
                summary: "Dummy plan".into(),
                items: vec![DraftItem {
                    id: "one".into(),
                    track_ref: "new:1".into(),
                    folder_id: folder,
                    title: "Dummy job".into(),
                    instruction: "Use dummy data.".into(),
                    constraints: vec![],
                    done_when: "Done.".into(),
                    after: vec![],
                }],
            }),
            vec![],
        )
        .await?;
    let plan = journal.plans().await?.pop().unwrap();
    let db = Connection::open(root.path().join("db"))?;
    crate::clock::register(&db)?;
    install(&db)?;
    Ok((root, journal, db, plan))
}
fn at(ms: i64) -> Stamp {
    Stamp {
        epoch_ms: ms,
        mono_ms: ms,
        boot_id: "test-boot".into(),
        timeline_ms: ms,
    }
}
fn view(owner: Uuid, sequence: u64, plan: &Plan, idle: bool) -> Observation {
    Observation {
        owner,
        sequence,
        idle,
        plans: vec![Seen {
            plan_id: plan.id,
            revision: plan.rev,
        }],
    }
}

#[tokio::test]
async fn starts_once_after_five_seconds_of_the_same_visible_version() -> Result<()> {
    let (_root, _journal, mut db, plan) = fixture(true).await?;
    let owner = Uuid::now_v7();
    for tick in 0..=20 {
        let result = observe(
            &mut db,
            view(owner, tick + 1, &plan, true),
            &at(tick as i64 * 250),
        )?;
        if tick < 20 {
            assert_eq!(result.countdowns[0].remaining_ms, 5_000 - tick as i64 * 250);
            assert!(result.started.is_empty());
        } else {
            assert_eq!(result.started.len(), 1);
            assert_eq!(result.started[0].state, State::Running);
        }
    }
    assert!(
        observe(&mut db, view(owner, 22, &plan, true), &at(5_250))?
            .started
            .is_empty()
    );
    assert!(observe(&mut db, view(owner, 21, &plan, true), &at(5_250)).is_err());
    assert!(
        observe(&mut db, view(Uuid::now_v7(), 1, &plan, true), &at(5_250))?
            .started
            .is_empty()
    );
    let tracks: i64 = db.query_row("SELECT count(*) FROM tracks", [], |row| row.get(0))?;
    assert_eq!(tracks, 1);
    assert_eq!(plan_store::read(&db, plan.id)?.rev, plan.rev + 1);
    Ok(())
}

#[tokio::test]
async fn interaction_missing_observations_and_window_changes_restart_the_delay() -> Result<()> {
    let (_root, _journal, mut db, plan) = fixture(true).await?;
    let owner = Uuid::now_v7();
    for tick in 0..5 {
        observe(
            &mut db,
            view(owner, tick + 1, &plan, true),
            &at(tick as i64 * 1_000),
        )?;
    }
    assert!(
        observe(&mut db, view(owner, 6, &plan, false), &at(4_100))?
            .countdowns
            .is_empty()
    );
    assert_eq!(
        observe(&mut db, view(owner, 7, &plan, true), &at(4_200))?.countdowns[0].remaining_ms,
        5_000
    );
    assert_eq!(
        observe(&mut db, view(owner, 8, &plan, true), &at(6_000))?.countdowns[0].remaining_ms,
        5_000
    );
    assert_eq!(
        observe(&mut db, view(Uuid::now_v7(), 1, &plan, true), &at(6_500))?.countdowns[0]
            .remaining_ms,
        5_000
    );
    assert!(
        observe(
            &mut db,
            Observation {
                owner,
                sequence: 9,
                idle: true,
                plans: vec![]
            },
            &at(6_750)
        )?
        .countdowns
        .is_empty()
    );
    assert_eq!(
        observe(&mut db, view(owner, 10, &plan, true), &at(7_000))?.countdowns[0].remaining_ms,
        5_000
    );
    let mut reboot = at(7_500);
    reboot.boot_id = "next-boot".into();
    assert_eq!(
        observe(&mut db, view(owner, 11, &plan, true), &reboot)?.countdowns[0].remaining_ms,
        5_000
    );
    assert_eq!(plan_store::read(&db, plan.id)?.state, State::Draft);
    Ok(())
}

#[tokio::test]
async fn plan_only_hold_confirmation_stop_and_old_revisions_cannot_auto_start() -> Result<()> {
    let (_root, journal, mut db, mut plan) = fixture(false).await?;
    let owner = Uuid::now_v7();
    assert!(
        observe(&mut db, view(owner, 1, &plan, true), &at(0))?
            .countdowns
            .is_empty()
    );
    plan.execute_allowed = true;
    plan_store::save(&db, &plan, "fixture")?;
    assert_eq!(
        observe(&mut db, view(owner, 2, &plan, true), &at(1_000))?.countdowns[0].remaining_ms,
        5_000
    );
    let old = plan.clone();
    plan.next_revision()?;
    plan_store::save(&db, &plan, "fixture")?;
    assert!(
        observe(&mut db, view(owner, 3, &old, true), &at(1_500))?
            .countdowns
            .is_empty()
    );
    assert_eq!(
        observe(&mut db, view(owner, 4, &plan, true), &at(2_000))?.countdowns[0].remaining_ms,
        5_000
    );
    plan.state = State::Held;
    plan_store::save(&db, &plan, "fixture")?;
    assert!(
        observe(&mut db, view(owner, 5, &plan, true), &at(2_500))?
            .countdowns
            .is_empty()
    );
    plan.state = State::Draft;
    plan_store::save(&db, &plan, "fixture")?;
    db.execute("INSERT INTO settings VALUES('confirmation',?1)",[serde_json::json!({"active":true,"revision":1,"policy":"always","dismissedThrough":0,"suggested":false,"suggestedFor":null,"notice":""}).to_string()])?;
    assert!(
        observe(&mut db, view(owner, 6, &plan, true), &at(3_000))?
            .countdowns
            .is_empty()
    );
    db.execute("DELETE FROM settings WHERE key='confirmation'", [])?;
    journal.pause_dispatch(Uuid::now_v7(), true).await?;
    assert!(
        observe(&mut db, view(owner, 7, &plan, true), &at(3_500))?
            .countdowns
            .is_empty()
    );
    assert_eq!(plan_store::read(&db, plan.id)?.state, State::Held);
    Ok(())
}

#[tokio::test]
async fn setting_changes_apply_to_new_plans_and_survive_lost_acknowledgement() -> Result<()> {
    let (root, journal, mut db, plan) = fixture(true).await?;
    assert_eq!(plan.start_mode, Mode::AfterDelay);
    let request = Change {
        op_id: Uuid::now_v7(),
        revision: 1,
        mode: Mode::Manual,
    };
    let wire = serde_json::to_string(&request)?;
    assert_eq!(change(&mut db, request)?.revision, 2);
    assert_eq!(change(&mut db, serde_json::from_str(&wire)?)?.revision, 2);
    assert!(
        change(
            &mut db,
            Change {
                op_id: Uuid::now_v7(),
                revision: 1,
                mode: Mode::AfterDelay
            }
        )
        .is_err()
    );
    assert_eq!(plan_store::read(&db, plan.id)?.start_mode, Mode::AfterDelay);
    journal
        .stage_document(crate::request_sources::Submission {
            op_id: Uuid::now_v7(),
            text: "Plan a new dummy job.".into(),
            target: Target::New,
            selected_track_id: None,
            attachment_ids: vec![],
            sources: None,
            execute_allowed: false,
            plan_item: None,
        })
        .await?;
    assert_eq!(
        journal.plans().await?.last().unwrap().start_mode,
        Mode::Manual
    );
    let reopened = Connection::open(root.path().join("db"))?;
    assert_eq!(read(&reopened)?.mode, Mode::Manual);
    assert_eq!(Setting::default().mode, Mode::Manual);
    Ok(())
}

#[tokio::test]
async fn journal_uses_real_elapsed_time_without_a_background_start_timer() -> Result<()> {
    let (_root, journal, _db, plan) = fixture(true).await?;
    let owner = Uuid::now_v7();
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        for sequence in 1.. {
            let update = journal
                .observe_plans(view(owner, sequence, &plan, true))
                .await?;
            if !update.started.is_empty() {
                return Result::<Update>::Ok(update);
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
        unreachable!()
    })
    .await??;
    assert!(started.elapsed() >= std::time::Duration::from_secs(5));
    assert_eq!(result.started[0].state, State::Running);
    assert_eq!(
        journal.snapshot().await?["workers"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    Ok(())
}

#[tokio::test]
async fn failed_save_rolls_back_the_start_and_restart_loses_the_countdown() -> Result<()> {
    let (root, _journal, mut db, plan) = fixture(true).await?;
    let owner = Uuid::now_v7();
    for tick in 0..5 {
        observe(
            &mut db,
            view(owner, tick + 1, &plan, true),
            &at(tick as i64 * 1_000),
        )?;
    }
    db.execute_batch("CREATE TRIGGER fail_auto BEFORE INSERT ON events WHEN NEW.kind='plan.started-after-delay' BEGIN SELECT RAISE(ABORT,'dummy full disk'); END;")?;
    assert!(observe(&mut db, view(owner, 6, &plan, true), &at(5_000)).is_err());
    assert_eq!(plan_store::read(&db, plan.id)?.state, State::Draft);
    assert_eq!(
        db.query_row("SELECT count(*) FROM tracks", [], |row| row
            .get::<_, i64>(0))?,
        0
    );
    db.execute_batch("DROP TRIGGER fail_auto;")?;
    drop(db);
    let mut reopened = Connection::open(root.path().join("db"))?;
    crate::clock::register(&reopened)?;
    install(&reopened)?;
    assert_eq!(
        observe(&mut reopened, view(owner, 7, &plan, true), &at(5_250))?.countdowns[0].remaining_ms,
        5_000
    );
    assert_eq!(plan_store::read(&reopened, plan.id)?.state, State::Draft);
    Ok(())
}

fn add_parent(plan: &mut Plan, state: crate::plans::ItemState) -> Uuid {
    let mut parent = plan.items[0].clone();
    parent.plan_item_id = Uuid::now_v7();
    parent.track_ref = "new:2".into();
    parent.title = "Dummy prerequisite".into();
    parent.state = state;
    let id = parent.plan_item_id;
    plan.items[0].order = 1;
    plan.items[0].after = vec![crate::plans::Dependency {
        id: id.to_string(),
        inherit: false,
    }];
    plan.items.insert(0, parent);
    id
}

#[tokio::test]
async fn an_unseen_plan_stays_unstarted_even_after_its_dependency_succeeds() -> Result<()> {
    let (_root, journal, mut db, mut plan) = fixture(true).await?;
    add_parent(&mut plan, crate::plans::ItemState::Succeeded);
    plan_store::save(&db, &plan, "fixture.ready-parent")?;
    journal.advance_plans().await?;
    let unseen = plan_store::read(&db, plan.id)?;
    assert_eq!(unseen.state, State::Draft);
    assert_eq!(unseen.seen_revision, None);
    assert!(unseen.items.iter().all(|item| item.utterance_id.is_none()));
    let owner = Uuid::now_v7();
    for tick in 0..=20 {
        let result = observe(
            &mut db,
            view(owner, tick + 1, &plan, true),
            &at(tick as i64 * 250),
        )?;
        if tick < 20 {
            assert!(result.started.is_empty());
        } else {
            assert_eq!(result.started.len(), 1);
        }
    }
    journal.advance_plans().await?;
    let started = plan_store::read(&db, plan.id)?;
    assert_eq!(started.state, State::Running);
    assert!(started.items[1].utterance_id.is_some());
    Ok(())
}

#[tokio::test]
async fn manual_start_keeps_a_failed_dependency_closed_until_it_succeeds() -> Result<()> {
    let (_root, journal, db, mut plan) = fixture(true).await?;
    add_parent(&mut plan, crate::plans::ItemState::Failed);
    plan.seen_revision = Some(plan.rev);
    plan_store::save(&db, &plan, "fixture.failed-parent")?;
    let mut running = journal
        .plan_command(crate::plan_store::Command {
            op_id: Uuid::now_v7(),
            plan_id: plan.id,
            revision: plan.rev,
            action: crate::plan_store::Action::Start,
        })
        .await?;
    journal.advance_plans().await?;
    assert_eq!(plan_store::read(&db, plan.id)?.state, State::Running);
    assert!(
        plan_store::read(&db, plan.id)?.items[1]
            .utterance_id
            .is_none()
    );
    running.items[0].state = crate::plans::ItemState::Succeeded;
    plan_store::save(&db, &running, "fixture.parent-completed")?;
    journal.advance_plans().await?;
    assert!(
        plan_store::read(&db, plan.id)?.items[1]
            .utterance_id
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn a_dependency_failure_discards_elapsed_auto_start_time() -> Result<()> {
    let (_root, journal, mut db, mut plan) = fixture(true).await?;
    add_parent(&mut plan, crate::plans::ItemState::Succeeded);
    plan_store::save(&db, &plan, "fixture.ready-parent")?;
    let owner = Uuid::now_v7();
    for tick in 0..5 {
        let update = observe(
            &mut db,
            view(owner, tick + 1, &plan, true),
            &at(tick as i64 * 1_000),
        )?;
        assert_eq!(update.countdowns.len(), 1);
    }
    plan.items[0].state = crate::plans::ItemState::Failed;
    plan_store::save(&db, &plan, "fixture.failed-parent")?;
    let blocked = observe(&mut db, view(owner, 6, &plan, true), &at(4_750))?;
    assert!(blocked.countdowns.is_empty());
    assert!(blocked.started.is_empty());
    journal.advance_plans().await?;
    assert!(
        plan_store::read(&db, plan.id)?.items[1]
            .utterance_id
            .is_none()
    );
    plan.items[0].state = crate::plans::ItemState::Succeeded;
    plan_store::save(&db, &plan, "fixture.ready-parent")?;
    let resumed = observe(&mut db, view(owner, 7, &plan, true), &at(5_000))?;
    assert_eq!(resumed.countdowns[0].remaining_ms, 5_000);
    for tick in 1..=20 {
        let update = observe(
            &mut db,
            view(owner, 7 + tick, &plan, true),
            &at(5_000 + tick as i64 * 250),
        )?;
        assert_eq!(update.started.len(), usize::from(tick == 20));
    }
    Ok(())
}
