use crate::{
    Result,
    clock::Stamp,
    decision::{Decision, Kind, Outcome, Target},
    edit_hold::{Action, Boundary, Edit, Request},
    journal::{Admission, Journal},
    plan_store::{Action as PlanAction, Command, Generated},
    plans::{Draft, DraftItem, ItemState, Plan},
    request_sources::Submission,
    runtime::WorkerState,
    secrets::Key,
    start_condition::Target as WaitTarget,
};
use rusqlite::params;
use serde_json::{Value, json};
use uuid::Uuid;

async fn word(journal: &Journal, id: &str) -> Result<Value> {
    Ok(journal.snapshot().await?["utterances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["id"] == id)
        .unwrap()
        .clone())
}
async fn request(journal: &Journal) -> Result<Value> {
    let id = Uuid::now_v7();
    journal
        .stage_utterance(
            id,
            "Dummy waiting request".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    let pending = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            pending.id.clone(),
            pending.revision,
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
    word(journal, &id.to_string()).await
}
async fn lease(journal: &Journal, word: &Value) -> Result<(Uuid, Uuid, i64)> {
    let owner = Uuid::now_v7();
    let token = Uuid::now_v7();
    let held = journal
        .edit_input(Request {
            owner,
            action: Action::Begin {
                token,
                input: word["id"].as_str().unwrap().into(),
                revision: word["revision"].as_i64().unwrap(),
            },
        })
        .await?;
    Ok((owner, token, held["revision"].as_i64().unwrap()))
}
async fn edit(journal: &Journal, word: &Value, edit: Edit) -> Result<Value> {
    let (owner, token, revision) = lease(journal, word).await?;
    journal
        .edit_input(Request {
            owner,
            action: Action::End {
                token,
                revision,
                edit,
            },
        })
        .await
}
async fn frame(
    journal: &Journal,
    track: &str,
    session: &str,
    turn: Uuid,
    method: &str,
) -> Result<()> {
    journal.try_msp(track.into(),session.into(),Stamp::now(),json!({"method":method,"params":{"sessionId":session,"turnId":turn,"terminal":"completed"}}),None,None)?.await??;
    Ok(())
}
async fn empty_secrets(journal: &Journal) -> Result<()> {
    let project = journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let catalog = journal
        .prepare_project_secrets(crate::project_secrets::Prepare {
            op_id: Uuid::now_v7(),
            project_id: project.clone(),
            revision: 0,
            fingerprints: vec![],
            key_check: Key::new([7; 32])
                .check()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    journal
        .activate_project_secrets(crate::project_secrets::Load {
            project_id: project,
            revision: 0,
            entries: vec![],
        })
        .await?;
    Ok(())
}
async fn reserve(journal: &Journal, word: &Value) -> Result<Admission> {
    empty_secrets(journal).await?;
    journal
        .worker_state(WorkerState {
            track_id: word["decision"]["trackId"].as_str().unwrap().into(),
            phase: "ready".into(),
            session_id: Some("dummy-destination".into()),
            active_turn: None,
            secret_revision: Some(0),
            identity: None,
            detail: None,
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        })
        .await?;

    let op = Uuid::now_v7();
    journal
        .reserve_turn(
            op,
            word["id"].as_str().unwrap().into(),
            word["revision"].as_i64().unwrap(),
            word["decision"]["trackId"].as_str().unwrap().into(),
            json!({"commandId":op,"sessionId":"dummy-destination","input":[]}),
        )
        .await
}
async fn fixture() -> Result<(tempfile::TempDir, Journal)> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("project");
    gix::init(&path)?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    journal
        .configure_project(
            Uuid::now_v7(),
            crate::project::inspect(Uuid::now_v7(), "Dummy project".into(), vec![path]).await?,
        )
        .await?;
    Ok((root, journal))
}
async fn create_plan(journal: &Journal, after: Option<String>) -> Result<Plan> {
    journal
        .stage_document(Submission {
            op_id: Uuid::now_v7(),
            text: "Plan dummy work".into(),
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
        assumptions: vec![],
        conflicts: vec![],
        summary: "Dummy plan".into(),
        items: (0..2)
            .map(|i| DraftItem {
                id: format!("item-{i}"),
                track_ref: "new:1".into(),
                folder_id: folder.clone(),
                title: format!("Dummy item {i}"),
                instruction: "Use dummy data only".into(),
                constraints: vec![],
                done_when: "Dummy result exists".into(),
                after: after
                    .iter()
                    .map(|id| crate::plans::Dependency {
                        id: id.clone(),
                        inherit: false,
                    })
                    .collect(),
            })
            .collect(),
    };
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
    Ok(journal.plans().await?.pop().unwrap())
}
fn target(plan: &Plan, index: usize) -> WaitTarget {
    WaitTarget::PlanItem {
        plan_id: plan.id,
        item_id: plan.items[index].plan_item_id,
        revision: plan.rev,
    }
}
fn set_state(root: &tempfile::TempDir, plan: &mut Plan, state: ItemState) -> Result<()> {
    plan.items[0].state = state;
    rusqlite::Connection::open(root.path().join("db"))?.execute(
        "UPDATE plans SET record=?2 WHERE id=?1",
        params![plan.id.to_string(), serde_json::to_string(plan)?],
    )?;
    Ok(())
}

#[tokio::test]
async fn clip_wait_is_for_its_exact_turn_and_completed_clips_are_rejected() -> Result<()> {
    let (root, journal) = fixture().await?;
    let source = request(&journal).await?;
    let track = source["decision"]["trackId"].as_str().unwrap();
    let turn = Uuid::now_v7();
    journal
        .worker_state(WorkerState {
            track_id: track.into(),
            phase: "working".into(),
            session_id: Some("dummy-source".into()),
            active_turn: Some(turn),
            secret_revision: None,
            identity: None,
            detail: None,
            approvals: vec![],
            output: None,
            checkpoint_pending: false,
            checkpoint: None,
            prompts: vec![],
        })
        .await?;
    frame(&journal, track, "dummy-source", turn, "turn/started").await?;
    journal.try_msp(track.into(),"dummy-source".into(),Stamp::now(),json!({"method":"item/updated","params":{"sessionId":"dummy-source","item":{"itemId":"dummy-message","turnId":turn,"kind":"assistantMessage","revision":1,"status":"inProgress"}}}),None,None)?.await??;

    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let clip: String = db.query_row(
        "SELECT id FROM timeline_clips WHERE json_extract(record,'$.turnId')=?1",
        [turn.to_string()],
        |r| r.get(0),
    )?;
    let pending = request(&journal).await?;
    let saved = edit(
        &journal,
        &pending,
        Edit::WaitFor {
            source: WaitTarget::Clip {
                clip_id: clip.clone(),
            },
        },
    )
    .await?;
    assert_eq!(saved["startCondition"]["source"]["kind"], "clip");
    assert_eq!(saved["startCondition"]["ready"], false);
    assert!(reserve(&journal, &saved).await.is_err());
    // A clip can finish while its enclosing turn is still running.
    db.execute(
        "UPDATE timeline_clips SET record=json_set(record,'$.end',json(?2)) WHERE id=?1",
        params![clip, serde_json::to_string(&Stamp::now())?],
    )?;
    let next = request(&journal).await?;
    assert!(
        edit(
            &journal,
            &next,
            Edit::WaitFor {
                source: WaitTarget::Clip {
                    clip_id: clip.clone()
                }
            }
        )
        .await
        .is_err()
    );
    let next_turn = Uuid::now_v7();
    frame(&journal, track, "dummy-source", next_turn, "turn/started").await?;
    frame(&journal, track, "dummy-source", next_turn, "turn/completed").await?;
    assert_eq!(
        word(&journal, saved["id"].as_str().unwrap()).await?["startCondition"]["ready"],
        false
    );
    frame(&journal, track, "dummy-source", turn, "turn/completed").await?;
    let ready = word(&journal, saved["id"].as_str().unwrap()).await?;
    assert_eq!(ready["startCondition"]["ready"], true);
    assert!(matches!(reserve(&journal, &ready).await?, Admission::New));
    Ok(())
}

#[tokio::test]
async fn plan_wait_replays_atomically_survives_restart_and_holds_failed_or_cut_items() -> Result<()>
{
    let (root, journal) = fixture().await?;
    let mut plan = create_plan(&journal, None).await?;
    let pending = request(&journal).await?;
    let (owner, token, revision) = lease(&journal, &pending).await?;
    let op = Uuid::now_v7();
    let apply = Request {
        owner,
        action: Action::Apply {
            token,
            revision,
            op_id: op,
            edit: Edit::WaitFor {
                source: target(&plan, 0),
            },
        },
    };
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    db.execute_batch("CREATE TRIGGER fail_wait BEFORE UPDATE ON input_start_conditions BEGIN SELECT RAISE(ABORT,'dummy disk failure'); END;")?;
    assert!(journal.edit_input(apply.clone()).await.is_err());
    let unchanged = word(&journal, pending["id"].as_str().unwrap()).await?;
    assert!(unchanged["startCondition"].is_null());
    assert_eq!(unchanged["revision"], revision);
    db.execute_batch("DROP TRIGGER fail_wait;")?;
    let saved = journal.edit_input(apply.clone()).await?;
    let replay = journal.edit_input(apply).await?;
    assert_eq!(saved["revision"], replay["revision"]);
    journal
        .edit_input(Request {
            owner,
            action: Action::End {
                token,
                revision: saved["revision"].as_i64().unwrap(),
                edit: Edit::Cancel,
            },
        })
        .await?;
    let saved = word(&journal, pending["id"].as_str().unwrap()).await?;
    assert_eq!(saved["decision"]["trackId"], pending["decision"]["trackId"]);
    assert!(reserve(&journal, &saved).await.is_err());
    drop(db);
    drop(journal);
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    let restored = word(&journal, pending["id"].as_str().unwrap()).await?;
    assert_eq!(restored["startCondition"], saved["startCondition"]);
    for (state, detail) in [
        (ItemState::Failed, "Dependency did not finish"),
        (ItemState::Cut, "Dependency was cut"),
    ] {
        set_state(&root, &mut plan, state)?;
        let held = word(&journal, pending["id"].as_str().unwrap()).await?;
        assert_eq!(held["startCondition"]["ready"], false);
        assert_eq!(held["startCondition"]["detail"], detail);
        assert!(reserve(&journal, &held).await.is_err());
    }
    set_state(&root, &mut plan, ItemState::Succeeded)?;
    let ready = word(&journal, pending["id"].as_str().unwrap()).await?;
    assert_eq!(ready["startCondition"]["ready"], true);
    assert!(matches!(reserve(&journal, &ready).await?, Admission::New));
    assert_eq!(
        rusqlite::Connection::open(root.path().join("db"))?.query_row(
            "SELECT count(*) FROM input_inheritance WHERE input=?1",
            [pending["id"].as_str().unwrap()],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

#[tokio::test]
async fn plan_and_word_edges_reject_self_same_track_and_cross_plan_cycles() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let initial = create_plan(&journal, None).await?;
    journal
        .plan_command(Command {
            op_id: Uuid::now_v7(),
            plan_id: initial.id,
            revision: initial.rev,
            action: PlanAction::Review,
        })
        .await?;
    let started = journal
        .plan_command(Command {
            op_id: Uuid::now_v7(),
            plan_id: initial.id,
            revision: initial.rev,
            action: PlanAction::Start,
        })
        .await?;
    journal.advance_plans().await?;
    let plan = journal
        .plans()
        .await?
        .into_iter()
        .find(|p| p.id == started.id)
        .unwrap();
    let input = plan.items[0].utterance_id.unwrap().to_string();
    let pending = word(&journal, &input).await?;
    for index in [0, 1] {
        let (owner, token, revision) = lease(&journal, &word(&journal, &input).await?).await?;
        let error = journal
            .edit_input(Request {
                owner,
                action: Action::Apply {
                    token,
                    revision,
                    op_id: Uuid::now_v7(),
                    edit: Edit::WaitFor {
                        source: target(&plan, index),
                    },
                },
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("dependency cycle"), "{error}");
        journal
            .edit_input(Request {
                owner,
                action: Action::End {
                    token,
                    revision,
                    edit: Edit::Cancel,
                },
            })
            .await?;
    }
    let second = create_plan(&journal, Some(plan.items[0].plan_item_id.to_string())).await?;
    let current = word(&journal, &input).await?;
    let (owner, token, revision) = lease(&journal, &current).await?;
    let error = journal
        .edit_input(Request {
            owner,
            action: Action::Apply {
                token,
                revision,
                op_id: Uuid::now_v7(),
                edit: Edit::WaitFor {
                    source: target(&second, 0),
                },
            },
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("dependency cycle"), "{error}");
    assert_eq!(
        word(&journal, &input).await?["startCondition"],
        pending["startCondition"]
    );
    Ok(())
}

#[tokio::test]
async fn read_as_binds_a_displayed_turn_and_now_clears_a_plan_wait() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let plan = create_plan(&journal, None).await?;
    let pending = request(&journal).await?;
    let waiting = edit(
        &journal,
        &pending,
        Edit::WaitFor {
            source: target(&plan, 0),
        },
    )
    .await?;
    let track = waiting["decision"]["trackId"].as_str().unwrap().to_owned();
    let now = edit(
        &journal,
        &waiting,
        Edit::ReadAs {
            track: track.clone(),
            boundary: Boundary::Now,
            turn: None,
        },
    )
    .await?;
    assert!(now["startCondition"].is_null());
    let turn = Uuid::now_v7();
    frame(&journal, &track, "dummy-source", turn, "turn/started").await?;
    let after = edit(
        &journal,
        &now,
        Edit::ReadAs {
            track: track.clone(),
            boundary: Boundary::AfterTurn,
            turn: Some(crate::decision_review::Turn {
                session_id: "dummy-source".into(),
                turn_id: turn,
            }),
        },
    )
    .await?;
    assert_eq!(
        after["startCondition"]["source"]["value"]["turnId"],
        turn.to_string()
    );
    assert_eq!(after["startCondition"]["ready"], false);
    journal
        .cut_input(Uuid::now_v7(), after["id"].as_str().unwrap().into())
        .await?;
    let history = journal.snapshot().await?;
    journal
        .edit_history(crate::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: crate::timeline_history::Action::Restore {
                input: after["id"].as_str().unwrap().into(),
                revision: word(&journal, after["id"].as_str().unwrap()).await?["revision"]
                    .as_i64()
                    .unwrap(),
            },
        })
        .await?;
    let restored = word(&journal, after["id"].as_str().unwrap()).await?;
    assert_eq!(restored["startCondition"], after["startCondition"]);
    assert!(
        journal
            .start_held_input(
                Uuid::now_v7(),
                restored["id"].as_str().unwrap().into(),
                restored["revision"].as_i64().unwrap()
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("has not finished")
    );
    assert!(
        !history["editHistory"]["takenBack"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn a_recorded_plan_candidate_can_be_confirmed_but_a_stale_edit_cannot() -> Result<()> {
    let (_root, journal) = fixture().await?;
    let plan = create_plan(&journal, None).await?;
    let destination = request(&journal).await?;
    let track = destination["decision"]["trackId"]
        .as_str()
        .unwrap()
        .to_owned();
    let id = Uuid::now_v7();
    journal
        .stage_utterance(
            id,
            "After the chosen dummy step".into(),
            Target::Lock(track.clone()),
            Some(track.clone()),
            vec![],
        )
        .await?;
    let pending = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            pending.id.clone(),
            pending.revision,
            Outcome::Classified(Decision {
                kind: Kind::Schedule,
                track_id: Some(track),
                boundary: Some("after-clip".into()),
                command: None,
                confidence: 1.0,
                inference_ms: 0.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let asked = word(&journal, &id.to_string()).await?;
    assert_eq!(asked["review"]["stage"], "start-turn");
    let source = serde_json::from_value(asked["startCondition"]["candidates"][0].clone())?;
    let accepted = journal
        .resolve_input(
            Uuid::now_v7(),
            id.to_string(),
            asked["revision"].as_i64().unwrap(),
            crate::decision::Resolution::Confirm {
                choice: crate::decision_review::Choice::After { source },
            },
        )
        .await?;
    assert_eq!(accepted["startCondition"]["source"]["kind"], "plan-item");
    assert_eq!(accepted["startCondition"]["ready"], false);
    assert_eq!(
        accepted["decision"]["trackId"],
        destination["decision"]["trackId"]
    );
    let (owner, token, revision) = lease(&journal, &accepted).await?;
    let mut stale = target(&plan, 1);
    if let WaitTarget::PlanItem { revision, .. } = &mut stale {
        *revision += 1;
    }
    assert!(
        journal
            .edit_input(Request {
                owner,
                action: Action::Apply {
                    token,
                    revision,
                    op_id: Uuid::now_v7(),
                    edit: Edit::WaitFor { source: stale }
                }
            })
            .await
            .is_err()
    );
    let unchanged = word(&journal, &id.to_string()).await?;
    assert_eq!(unchanged["revision"], revision);
    assert_eq!(unchanged["startCondition"], accepted["startCondition"]);
    Ok(())
}

#[tokio::test]
async fn exact_turn_alias_is_checked_again_at_dispatch() -> Result<()> {
    let (root, journal) = fixture().await?;
    let pending = request(&journal).await?;
    assert!(matches!(reserve(&journal, &pending).await?, Admission::New));
    let db = rusqlite::Connection::open(root.path().join("db"))?;
    let turn = Uuid::now_v7();
    db.execute(
        "UPDATE attachment_turns SET turn=?1 WHERE session='dummy-destination'",
        [turn.to_string()],
    )?;
    db.execute("UPDATE utterances SET state='placed',decision=json_set(decision,'$.boundary','after-turn') WHERE id=?1",[pending["id"].as_str().unwrap()])?;
    let condition = crate::start_condition::Context {
        source: Some(crate::start_condition::Source::Turn(
            crate::start_condition::Turn {
                track_id: pending["decision"]["trackId"].as_str().unwrap().into(),
                label: "A".into(),
                session_id: "dummy-destination".into(),
                turn_id: turn,
            },
        )),
        candidates: vec![],
    };
    crate::start_condition::save(&db, pending["id"].as_str().unwrap(), &condition)?;
    // A terminal turn would pass the readiness gate, but cannot wait on its own input.
    db.execute("INSERT INTO timeline_turns VALUES('dummy-destination',?1,?2)",params![turn.to_string(),json!({"track":pending["decision"]["trackId"],"repair_run_id":null,"current":null,"index":0,"terminal":true}).to_string()])?;
    let current = word(&journal, pending["id"].as_str().unwrap()).await?;
    assert_eq!(current["startCondition"]["ready"], true);
    let error = reserve(&journal, &current).await.unwrap_err();
    assert!(error.to_string().contains("dependency cycle"), "{error}");
    Ok(())
}
