use super::*;
use crate::review_fixes::{Action, Command, State as FixState};
fn database(f: &Fixture) -> Result<rusqlite::Connection> {
    let db = rusqlite::Connection::open(f.root.path().join("db"))?;
    crate::clock::register(&db)?;
    Ok(db)
}
async fn reviewed(count: usize) -> Result<(Fixture, Job, Vec<crate::review_fixes::Fix>)> {
    let f = Fixture::new().await?;
    let job = f.begin().await?;
    let notes = (0..count)
        .map(|i| {
            let mut note = Fixture::note(&job);
            note.text = format!("Division issue {i}");
            note
        })
        .collect();
    crate::reviewing::publish(
        &f.journal,
        &job,
        Draft {
            resolutions: vec![],
            summary: "Findings.".into(),
            notes,
        },
    )
    .await?;
    let db = database(&f)?;
    let fixes = crate::review_fix_store::list(&db)?;
    Ok((f, job, fixes))
}
async fn reserve(
    f: &Fixture,
    fix: &crate::review_fixes::Fix,
) -> Result<(Uuid, Result<crate::journal::Admission>)> {
    let id = fix.input_id.unwrap().to_string();
    let word = f.journal.read_utterance(id.clone()).await?;
    let mut db = database(f)?;
    let check = crate::review_fix_store::prepare(&db, &id)?.unwrap();
    assert!(matches!(
        crate::review_fix_store::checked(&mut db, &check, false)?,
        crate::review_fixes::Checked::Ready
    ));
    let operation = Uuid::now_v7();
    let outcome=f.journal.reserve_turn(operation,id,word["revision"].as_i64().unwrap(),f.workspace.track_id.clone(),
        json!({"commandId":operation,"sessionId":f.state.session_id,"input":[{"type":"text","text":"dummy fix"}]}), None).await;
    Ok((operation, outcome))
}
#[tokio::test]
async fn findings_create_bound_inputs_but_optional_and_outdated_notes_do_not() -> Result<()> {
    let f = Fixture::new().await?;
    let job = f.begin().await?;
    let required = Fixture::note(&job);
    let mut optional = required.clone();
    optional.blocking = false;
    crate::reviewing::publish(
        &f.journal,
        &job,
        Draft {
            resolutions: vec![],
            summary: "Review".into(),
            notes: vec![required, optional],
        },
    )
    .await?;
    let state = f.journal.snapshot().await?;
    let fixes = state["reviewFixes"].as_array().unwrap();
    assert_eq!(fixes.len(), 1);
    let input = fixes[0]["inputId"].as_str().unwrap();
    let word = f.journal.read_utterance(input.into()).await?;
    assert_eq!(word["reviewFix"]["clipId"], job.target.clips[0].id);
    assert_eq!(word["decision"]["trackId"], f.workspace.track_id);
    assert!(
        word["text"]
            .as_str()
            .unwrap()
            .contains("Review verdict: disagree")
    );
    let db = database(&f)?;
    let window = crate::input_timeline::window(&db, 0, i64::MAX, None)?;
    assert!(window.0.iter().all(|item| item["inputId"] != input));
    Ok(())
}
#[tokio::test]
async fn a_changed_line_cuts_the_fix_at_preflight_without_charging_a_turn() -> Result<()> {
    let (mut f, _, fixes) = reviewed(1).await?;
    let fix = &fixes[0];
    std::fs::write(
        f.workspace.path.join("math.rs"),
        "pub fn double(value: i64) -> i64 {\n    value * 2\n}\n",
    )?;
    git(&f.workspace.path, &["add", "math.rs"]);
    git(
        &f.workspace.path,
        &[
            "-c",
            "user.name=Dummy",
            "-c",
            "user.email=dummy@example.invalid",
            "commit",
            "-qm",
            "Changed dummy lines",
        ],
    );
    f.state.checkpoint.as_mut().unwrap().sha = git(&f.workspace.path, &["rev-parse", "HEAD"]);
    f.journal.worker_state(f.state.clone()).await?;
    let result = f
        .journal
        .prepare_turn(
            fix.input_id.unwrap().to_string(),
            f.workspace.track_id.clone(),
            f.state.session_id.clone().unwrap(),
            f.workspace.path.clone(),
            json!({"input":[]}),
        )
        .await;
    assert!(
        result
            .err()
            .unwrap()
            .downcast_ref::<crate::journal::StartSuperseded>()
            .is_some()
    );
    let db = database(&f)?;
    assert_eq!(
        crate::review_fix_store::read(&db, fix.id)?.state,
        FixState::Outdated
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM review_fix_budget", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        f.journal
            .read_utterance(fix.input_id.unwrap().to_string())
            .await?["state"],
        "cut"
    );
    Ok(())
}
#[tokio::test]
async fn two_fixes_per_file_then_one_explicit_extra_keeps_the_counter_and_replays_once()
-> Result<()> {
    let (f, _, fixes) = reviewed(4).await?;
    for fix in &fixes[..2] {
        assert!(reserve(&f, fix).await?.1.is_ok());
    }
    assert!(reserve(&f, &fixes[2]).await?.1.is_err());
    let db = database(&f)?;
    let blocked = crate::review_fix_store::read(&db, fixes[2].id)?;
    assert_eq!(blocked.state, FixState::Blocked);
    let command = Command {
        op_id: Uuid::now_v7(),
        fix_id: blocked.id,
        revision: blocked.revision,
        action: Action::FixOnceMore,
    };
    let extra = f.journal.review_fix_command(command.clone()).await?;
    assert_eq!(
        f.journal.review_fix_command(command).await?.revision,
        extra.revision
    );
    assert!(reserve(&f, &extra).await?.1.is_ok());
    assert_eq!(
        db.query_row("SELECT total FROM review_fix_file_budget", [], |r| r
            .get::<_, i64>(0))?,
        3
    );
    assert!(reserve(&f, &fixes[3]).await?.1.is_err());
    let blocked = crate::review_fix_store::read(&db, fixes[3].id)?;
    f.journal
        .review_fix_command(Command {
            op_id: Uuid::now_v7(),
            fix_id: blocked.id,
            revision: blocked.revision,
            action: Action::KeepIt,
        })
        .await?;
    assert_eq!(
        db.query_row("SELECT total FROM review_fix_budget", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    Ok(())
}
#[tokio::test]
async fn an_unsent_start_refunds_once_and_an_old_epoch_cannot_refund_new_work() -> Result<()> {
    let (f, _, fixes) = reviewed(2).await?;
    let (op, result) = reserve(&f, &fixes[0]).await?;
    result?;
    f.journal.finish(&op.to_string(), "superseded").await?;
    f.journal.finish(&op.to_string(), "superseded").await?;
    let db = database(&f)?;
    assert_eq!(
        db.query_row("SELECT total FROM review_fix_budget", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    let (op, result) = reserve(&f, &fixes[1]).await?;
    result?;
    crate::review_fix_store::human_input(&db, "dummy-user", 1, &f.workspace.track_id)?;
    crate::review_fix_store::human_input(&db, "dummy-user", 1, &f.workspace.track_id)?;
    assert_eq!(
        db.query_row("SELECT epoch FROM review_fix_budget", [], |r| r
            .get::<_, i64>(0))?,
        1
    );
    f.journal.finish(&op.to_string(), "superseded").await?;
    assert_eq!(
        db.query_row("SELECT total FROM review_fix_budget", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    Ok(())
}
#[tokio::test]
async fn cutting_and_undoing_a_fix_preserves_its_identity_and_checks_again() -> Result<()> {
    let (f, _, fixes) = reviewed(1).await?;
    let fix = &fixes[0];
    let id = fix.input_id.unwrap().to_string();
    f.journal.cut_input(Uuid::now_v7(), id.clone()).await?;
    let db = database(&f)?;
    assert_eq!(
        crate::review_fix_store::read(&db, fix.id)?.state,
        FixState::Cut
    );
    let snapshot = f.journal.snapshot().await?;
    let entry = snapshot["editHistory"]["undo"]["entryId"]
        .as_str()
        .unwrap()
        .to_owned();
    f.journal
        .edit_history(crate::timeline_history::Request {
            op_id: Uuid::now_v7(),
            action: crate::timeline_history::Action::Undo { entry },
        })
        .await?;
    let restored = crate::review_fix_store::read(&db, fix.id)?;
    assert_eq!(restored.id, fix.id);
    assert_eq!(restored.state, FixState::Planned);
    assert!(restored.checked_sha.is_none());
    Ok(())
}

// Exercise actual saved frames, input receipts, checkpoints, and the automatic review queue.
async fn complete_fix(
    f: &mut Fixture,
    fix: &crate::review_fixes::Fix,
    operation: Uuid,
    body: &str,
    content: Option<&str>,
) -> Result<()> {
    complete_fix_with_pending_source(f, fix, operation, body, content, false).await
}
async fn complete_fix_with_pending_source(
    f: &mut Fixture,
    fix: &crate::review_fixes::Fix,
    operation: Uuid,
    body: &str,
    content: Option<&str>,
    pending_source: bool,
) -> Result<()> {
    let turn = Uuid::now_v7();
    let session = f.state.session_id.clone().unwrap();
    if let Some(content) = content {
        std::fs::write(f.workspace.path.join("math.rs"), content)?;
        git(&f.workspace.path, &["add", "math.rs"]);
    }
    git(
        &f.workspace.path,
        &[
            "-c",
            "user.name=Dummy",
            "-c",
            "user.email=dummy@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "Dummy fix checkpoint",
        ],
    );
    let sha = git(&f.workspace.path, &["rev-parse", "HEAD"]);
    let reference = format!("refs/intercat/dummy/checkpoints/{turn}");
    git(&f.workspace.path, &["update-ref", &reference, &sha]);
    let db = database(f)?;
    db.execute(
        "UPDATE utterances SET state='landed' WHERE id=?1",
        [fix.input_id.unwrap().to_string()],
    )?;
    if pending_source {
        f.journal.try_msp(f.workspace.track_id.clone(),session.clone(),Stamp::now(),json!({"method":"item/completed","params":{"sessionId":session,"item":{"itemId":format!("tool-{turn}"),"callId":format!("call-{turn}"),"turnId":turn,"kind":"toolCall","tool":"edit_file","status":"completed","revision":1,"args":"{}"}}}),None,None)?.await??;
    }
    for frame in [
        json!({"method":"turn/started","params":{"sessionId":session,"turnId":turn,"commandId":operation}}),
        json!({"method":"item/completed","params":{"sessionId":session,"item":{"itemId":format!("report-{turn}"),"turnId":turn,"kind":"agentMessage","messagePhase":"finalAnswer","status":"completed","revision":1,"text":body}}}),
        json!({"method":"turn/completed","params":{"sessionId":session,"turnId":turn,"terminal":"completed"}}),
    ] {
        let started = frame["method"] == "turn/started";
        f.journal
            .try_msp(
                f.workspace.track_id.clone(),
                session.clone(),
                Stamp::now(),
                frame,
                None,
                None,
            )?
            .await??;
        if started {
            f.state.active_turn = Some(turn);
            f.state.phase = "working".into();
            f.journal.worker_state(f.state.clone()).await?;
            assert_eq!(
                crate::review_fix_store::read(&db, fix.id)?.state,
                FixState::Running
            );
        }
    }
    f.state.active_turn = None;
    f.state.phase = "ready".into();
    f.state.checkpoint = Some(crate::checkpoint::Checkpoint {
        turn_id: turn,
        sha,
        reference,
        unmerged_paths: vec![],
        live_processes: false,
    });
    f.journal.worker_state(f.state.clone()).await?;
    Ok(())
}
async fn next_review(f: &Fixture) -> Result<Job> {
    let job = f
        .journal
        .reviews()
        .await?
        .into_iter()
        .find(|j| j.state == State::Queued)
        .ok_or("Expected queued re-review")?;
    Ok(f.journal
        .begin_review(job.id, job.generation, job.revision)
        .await?
        .unwrap())
}
#[tokio::test]
async fn rereview_requires_an_explicit_resolution_even_when_the_fix_has_no_file_changes()
-> Result<()> {
    let (mut f, _, fixes) = reviewed(1).await?;
    let fix = &fixes[0];
    let (op, result) = reserve(&f, fix).await?;
    result?;
    complete_fix(&mut f,fix,op,"The existing helper already handles it.\nReview verdict: agree — checked the helper\nResult: done — helper verified",None).await?;
    let job = next_review(&f).await?;
    assert_eq!(job.target.pending_fixes.len(), 1);
    let prepared = prepare(&f.journal, f.root.path(), &job).await?;
    assert!(prepared.changed.is_empty());
    let missing = Draft {
        summary: "Checked".into(),
        notes: vec![],
        resolutions: vec![],
    };
    assert!(
        crate::reviewing::publish(&f.journal, &job, missing)
            .await
            .is_err()
    );
    assert_eq!(
        crate::review_fix_store::read(&database(&f)?, fix.id)?.state,
        FixState::AwaitingReview
    );
    let done = Draft {
        summary: "Resolved".into(),
        notes: vec![],
        resolutions: vec![crate::reviews::Resolution {
            note_id: fix.note_id,
            explanation: "The saved helper handles this case.".into(),
            remaining: None,
        }],
    };
    assert!(crate::reviewing::publish(&f.journal, &job, done).await?);
    assert_eq!(
        crate::review_fix_store::read(&database(&f)?, fix.id)?.state,
        FixState::Resolved
    );
    Ok(())
}
#[tokio::test]
async fn two_unsuccessful_rereviews_stop_and_an_explicit_extra_stops_again() -> Result<()> {
    let (mut f, _, mut fixes) = reviewed(1).await?;
    for count in 1..=3 {
        let fix = fixes.last().unwrap().clone();
        let (op, result) = reserve(&f, &fix).await?;
        result?;
        complete_fix(&mut f,&fix,op,"Checked.\nReview verdict: agree — attempted correction\nResult: done — change complete",None).await?;
        let job = next_review(&f).await?;
        let remaining = Fixture::note(&job);
        let draft = Draft {
            summary: "Still present".into(),
            notes: vec![],
            resolutions: vec![crate::reviews::Resolution {
                note_id: fix.note_id,
                explanation: "Division by zero remains.".into(),
                remaining: Some(remaining),
            }],
        };
        crate::reviewing::publish(&f.journal, &job, draft).await?;
        fixes = crate::review_fix_store::list(&database(&f)?)?;
        let next = fixes.last().unwrap();
        assert_eq!(
            next.state,
            if count == 1 {
                FixState::Planned
            } else {
                FixState::Blocked
            }
        );
        if count == 2 {
            let extra = f
                .journal
                .review_fix_command(Command {
                    op_id: Uuid::now_v7(),
                    fix_id: next.id,
                    revision: next.revision,
                    action: Action::FixOnceMore,
                })
                .await?;
            *fixes.last_mut().unwrap() = extra;
        }
    }
    assert_eq!(
        database(&f)?.query_row("SELECT total FROM review_fix_budget", [], |r| r
            .get::<_, i64>(0))?,
        3
    );
    Ok(())
}
#[tokio::test]
async fn disagreement_retries_with_a_new_input_and_preserves_the_previous_attempt() -> Result<()> {
    let (mut f, _, fixes) = reviewed(1).await?;
    let fix = &fixes[0];
    let (op, result) = reserve(&f, fix).await?;
    result?;
    complete_fix(&mut f,fix,op,"Review verdict: disagree — the requested behavior is intentional\nResult: done — reviewed the request",None).await?;
    let db = database(&f)?;
    let blocked = crate::review_fix_store::read(&db, fix.id)?;
    assert_eq!(blocked.state, FixState::Blocked);
    assert!(
        f.journal
            .reviews()
            .await?
            .iter()
            .all(|j| j.state != State::Queued)
    );
    let extra = f
        .journal
        .review_fix_command(Command {
            op_id: Uuid::now_v7(),
            fix_id: fix.id,
            revision: blocked.revision,
            action: Action::FixOnceMore,
        })
        .await?;
    assert_ne!(extra.input_id, fix.input_id);
    assert_eq!(
        f.journal
            .read_utterance(fix.input_id.unwrap().to_string())
            .await?["reviewFixRunnable"],
        false
    );
    assert_eq!(
        crate::review_fix_store::projection(&db)?[0]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(reserve(&f, &extra).await?.1.is_ok());
    Ok(())
}
#[tokio::test]
async fn a_late_review_cannot_resume_a_stopped_worker() -> Result<()> {
    let f = Fixture::new().await?;
    let job = f.begin().await?;
    f.journal
        .hold_inputs(Uuid::now_v7(), f.workspace.track_id.clone())
        .await?;
    crate::reviewing::publish(
        &f.journal,
        &job,
        Draft {
            summary: "Issue".into(),
            notes: vec![Fixture::note(&job)],
            resolutions: vec![],
        },
    )
    .await?;
    let db = database(&f)?;
    let fix = crate::review_fix_store::list(&db)?.pop().unwrap();
    assert_eq!(fix.state, FixState::Held);
    assert_eq!(
        f.journal
            .read_utterance(fix.input_id.unwrap().to_string())
            .await?["state"],
        "held"
    );
    assert!(
        crate::dispatch_control::local_resumes(&db)?
            .as_object()
            .unwrap()
            .is_empty()
    );
    Ok(())
}
#[test]
fn only_a_standalone_final_verdict_is_a_disagreement() {
    use crate::review_fix_store::disagreement;
    assert_eq!(
        disagreement("Review verdict: disagree — intentional\nResult: done — checked"),
        Some("intentional")
    );
    for text in [
        "```\nReview verdict: disagree — example\n```\nResult: done — checked",
        "Review verdict: disagree — old\nActually fixed it.\nResult: done — checked",
        "> Review verdict: disagree — quoted\nResult: done — checked",
        "Review verdict: disagree — invalid\nResult: unknown — checked",
    ] {
        assert_eq!(disagreement(text), None);
    }
}

#[tokio::test]
#[ignore = "one paid Wonder re-review of agent-created dummy code"]
async fn native_rereview_resolves_the_original_finding_from_the_saved_fix() -> Result<()> {
    let (mut f, _, fixes) = reviewed(1).await?;
    let fix = &fixes[0];
    let (op, result) = reserve(&f, fix).await?;
    result?;
    complete_fix(
        &mut f,
        fix,
        op,
        "Fixed doubling.\nReview verdict: agree — multiply by two\nResult: done — doubling fixed",
        Some("pub fn double(value: i64) -> i64 {\n    value * 2\n}\n"),
    )
    .await?;
    let queued = f
        .journal
        .reviews()
        .await?
        .into_iter()
        .find(|j| j.state == State::Queued)
        .ok_or("Missing re-review")?;
    assert_eq!(queued.target.pending_fixes.len(), 1);
    std::fs::write(
        f.workspace.path.join("math.rs"),
        "UNSAVED_CHECKOUT_MARKER\n",
    )?;
    let config = Config {
        source: "/dummy".into(),
        binary: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
        credentials: std::env::var("INTERCAT_MUSE_AUTH")?.into(),
        path: std::env::var("PATH")?,
    };
    let rpc = f.root.path().join("rpc");
    std::fs::create_dir(&rpc)?;
    f.journal.set_worker_config(Uuid::now_v7(), config).await?;
    let capacity = std::sync::Arc::new(crate::scheduler::Capacity::new(1));
    let service = crate::reviewing::Service::start(
        crate::host_memory::Pool::new(),
        f.journal.clone(),
        f.root.path().into(),
        rpc,
        capacity.clone(),
        std::sync::Arc::new(crate::dispatch_control::Gate::new(false)),
    );
    let result = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let job = f
                .journal
                .reviews()
                .await?
                .into_iter()
                .find(|j| j.id == queued.id)
                .unwrap();
            match job.state {
                State::Complete => return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(job),
                State::Failed | State::Held => {
                    return Err(format!("Re-review did not complete: {:?}", job.detail).into());
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    service.shutdown().await;
    let job = result??;
    let db = database(&f)?;
    let resolved = crate::review_fix_store::read(&db, fix.id)?;
    assert_eq!(resolved.state, FixState::Resolved);
    assert_eq!(capacity.snapshot().active, 0);
    assert_eq!(crate::review_fix_store::list(&db)?.len(), 1);
    if let Ok(output) = std::env::var("INTERCAT_REVIEWER_OUTPUT") {
        std::fs::create_dir_all(&output)?;
        std::fs::write(
            Path::new(&output).join("result.json"),
            serde_json::to_vec_pretty(
                &json!({"schema":crate::journal::SCHEMA_VERSION,"review":job,"fix":resolved,"paidTurns":1,"source":"agent-created dummy code only","unsavedCheckoutRead":false,"capacityReturned":true}),
            )?,
        )?;
    }
    Ok(())
}

#[tokio::test]
async fn retiring_history_removes_review_jobs_and_internal_inputs() -> Result<()> {
    let (f, job, fixes) = reviewed(1).await?;
    let id = fixes[0].input_id.unwrap().to_string();
    let mut db = database(&f)?;
    db.pragma_update(None, "foreign_keys", true)?;
    let tx = db.transaction()?;
    crate::session_retirement::retire(
        &tx,
        &job.target.session_id,
        &job.target.track_id,
        Uuid::now_v7(),
    )?;
    tx.commit()?;
    assert!(f.journal.reviews().await?.is_empty());
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM timeline_clips WHERE track='reviewer'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    assert!(crate::review_fix_store::list(&db)?.is_empty());
    assert!(f.journal.read_utterance(id).await.is_err());
    assert!(f.journal.review_context(job).await.is_err());
    Ok(())
}
#[tokio::test]
async fn fixes_reject_editing_the_target_and_human_work_resets_blocked_attempts() -> Result<()> {
    let (f, _, fixes) = reviewed(3).await?;
    let input = fixes[0].input_id.unwrap().to_string();
    assert!(
        f.journal
            .edit_input(crate::edit_hold::Request {
                owner: Uuid::now_v7(),
                action: crate::edit_hold::Action::Begin {
                    token: Uuid::now_v7(),
                    input: input.clone(),
                    revision: 1
                }
            })
            .await
            .is_err()
    );
    for fix in &fixes[..2] {
        reserve(&f, fix).await?.1?;
    }
    assert!(reserve(&f, &fixes[2]).await?.1.is_err());
    let db = database(&f)?;
    crate::review_fix_store::human_input(&db, "new-human-work", 1, &f.workspace.track_id)?;
    assert_eq!(
        crate::review_fix_store::read(&db, fixes[2].id)?.state,
        FixState::Superseded
    );
    assert_eq!(
        db.query_row("SELECT total FROM review_fix_budget", [], |r| r
            .get::<_, i64>(0))?,
        0
    );
    Ok(())
}
#[tokio::test]
async fn track_budget_stops_a_fifth_fix_in_a_different_file() -> Result<()> {
    let (f, job, fixes) = reviewed(1).await?;
    let mut db = database(&f)?;
    for i in 1..5 {
        let mut note = crate::review_fix_store::note(&db, fixes[0].note_id)?;
        note.id = Uuid::now_v7();
        note.draft.path = format!("dummy-{i}.rs");
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO review_notes VALUES(?1,?2,?3,?4)",
            rusqlite::params![
                note.id.to_string(),
                job.id.to_string(),
                note.draft.clip_id,
                serde_json::to_string(&note)?
            ],
        )?;
        crate::review_fix_store::enqueue_with_reason(&tx, &note, &f.workspace.track_id, None)?;
        tx.commit()?;
    }
    let fixes = crate::review_fix_store::list(&db)?;
    for fix in &fixes[..4] {
        reserve(&f, fix).await?.1?;
    }
    assert!(reserve(&f, &fixes[4]).await?.1.is_err());
    let blocked = crate::review_fix_store::read(&db, fixes[4].id)?;
    assert_eq!(blocked.state, FixState::Blocked);
    assert!(blocked.reason.unwrap().contains("4 fixes on this track"));
    Ok(())
}

async fn bind_reviewed_plan(f: &Fixture) -> Result<crate::plans::Plan> {
    let mut db = database(f)?;
    let input = Uuid::now_v7();
    let command = Uuid::now_v7();
    let item = Uuid::now_v7();
    let session = f.state.session_id.as_ref().unwrap();
    let turn = f.state.checkpoint.as_ref().unwrap().turn_id;
    let tx = db.transaction()?;
    crate::journal::insert_utterance(
        &tx,
        &input.to_string(),
        "Original dummy job",
        &crate::decision::Target::Lock(f.workspace.track_id.clone()),
        Some(&f.workspace.track_id),
        &[],
        None,
        true,
        None,
    )?;
    tx.execute("UPDATE utterances SET state='landed',dispatch_command=?2,decision=?3 WHERE id=?1",rusqlite::params![input.to_string(),command.to_string(),json!({"kind":"extend","trackId":f.workspace.track_id,"boundary":"now","command":null,"confidence":1.,"inferenceMs":0.,"timedOut":false,"scores":{}}).to_string()])?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'turn.start',?2,'complete')",
        rusqlite::params![
            command.to_string(),
            json!({"sessionId":session}).to_string()
        ],
    )?;
    crate::attachment_history::reserve(&tx, &command.to_string(), session, &input.to_string())?;
    crate::attachment_history::observe(
        &tx,
        session,
        &json!({"method":"turn/started","params":{"commandId":command,"turnId":turn}}),
    )?;
    let mut plan = crate::plans::Plan::new(input, 1, true, None);
    plan.state = crate::plans::State::Held;
    plan.generation.state = crate::plans::GenerationState::Ready;
    plan.items.push(crate::plans::Item {
        plan_item_id: item,
        track_ref: f.workspace.track_id.clone(),
        folder_id: "dummy".into(),
        order: 0,
        title: "Double".into(),
        instruction: "Double the input".into(),
        constraints: vec![],
        done_when: "Doubled".into(),
        after: vec![],
        skip_order: vec![],
        start_requested: true,
        state: crate::plans::ItemState::Succeeded,
        utterance_id: Some(input),
        detail: None,
    });
    let mut child = plan.items[0].clone();
    child.plan_item_id = Uuid::now_v7();
    child.order = 1;
    child.title = "Use the reviewed result".into();
    child.instruction = "Use the doubled value".into();
    child.state = crate::plans::ItemState::Planned;
    child.utterance_id = None;
    child.start_requested = false;
    child.after = vec![crate::plans::Dependency {
        id: item.to_string(),
        inherit: false,
    }];
    plan.items.push(child);
    crate::plan_store::save(&tx, &plan, "dummy-plan")?;
    tx.execute(
        "INSERT INTO plan_inputs VALUES(?1,?2,?3,'initial')",
        rusqlite::params![input.to_string(), plan.id.to_string(), item.to_string()],
    )?;
    tx.commit()?;
    Ok(plan)
}

fn assert_reviewed_dependency(
    db: &rusqlite::Connection,
    plan: &crate::plans::Plan,
    waiting: bool,
) -> Result<()> {
    assert_eq!(
        crate::plan_execution::waiting(db, &plan.items[1], plan, std::slice::from_ref(plan))?
            .is_some(),
        waiting
    );
    assert!(
        plan.items[1].utterance_id.is_none(),
        "A held dependent must not start implicitly"
    );
    Ok(())
}

#[tokio::test]
async fn a_fix_continues_the_original_plan_item_instead_of_creating_a_new_plan() -> Result<()> {
    let mut f = Fixture::new().await?;
    let plan = bind_reviewed_plan(&f).await?;
    let item = plan.items[0].plan_item_id;
    let db = database(&f)?;
    let job = f.begin().await?;
    crate::reviewing::publish(
        &f.journal,
        &job,
        Draft {
            summary: "Issue".into(),
            notes: vec![Fixture::note(&job)],
            resolutions: vec![],
        },
    )
    .await?;
    let fix = crate::review_fix_store::list(&db)?.pop().unwrap();
    let correction = fix.input_id.unwrap().to_string();
    assert_eq!(
        crate::plan_execution::owner(&db, &correction)?,
        Some((plan.id, item))
    );
    assert!(crate::plan_continuation::has_pending(
        &db,
        &item.to_string()
    )?);
    assert_eq!(f.journal.plans().await?.len(), 1);
    assert_eq!(
        f.journal.read_utterance(correction).await?["planItemId"],
        item.to_string()
    );
    assert_reviewed_dependency(&db, &f.journal.plans().await?[0], true)?;
    let (op, result) = reserve(&f, &fix).await?;
    result?;
    complete_fix(
        &mut f,
        &fix,
        op,
        "Fixed.\nReview verdict: agree — attempted\nResult: done — completed",
        None,
    )
    .await?;
    assert!(crate::plan_continuation::has_pending(
        &db,
        &item.to_string()
    )?);
    assert_reviewed_dependency(&db, &f.journal.plans().await?[0], true)?;
    let checkpoint = f.state.checkpoint.as_ref().unwrap();
    let operation = Uuid::new_v5(&checkpoint.turn_id, b"checkpoint").to_string();
    f.journal
        .intent(
            &operation,
            "checkpoint.save",
            json!({"trackId":f.workspace.track_id,"checkpoint":checkpoint}),
        )
        .await?;
    f.journal.finish(&operation, "dispatched").await?;
    f.journal.finish(&operation, "complete").await?;
    let rereview = next_review(&f).await?;
    crate::reviewing::publish(
        &f.journal,
        &rereview,
        Draft {
            summary: "Resolved".into(),
            notes: vec![],
            resolutions: vec![crate::reviews::Resolution {
                note_id: fix.note_id,
                explanation: "The saved correction resolves the finding.".into(),
                remaining: None,
            }],
        },
    )
    .await?;
    f.journal.advance_plans().await?;
    let current = f.journal.plans().await?.pop().unwrap();
    assert_eq!(current.state, crate::plans::State::Held);
    assert_eq!(current.items[0].state, crate::plans::ItemState::Succeeded);
    assert_eq!(current.items[0].utterance_id, fix.input_id);
    assert_eq!(
        crate::plan_execution::owner(&db, &fix.input_id.unwrap().to_string())?,
        Some((plan.id, item))
    );
    assert_reviewed_dependency(&db, &current, false)?;
    Ok(())
}

#[tokio::test]
async fn delayed_file_sources_queue_the_fix_review_after_the_actor_starts_waiting() -> Result<()> {
    let (mut f, _, fixes) = reviewed(1).await?;
    let fix = &fixes[0];
    let (op, result) = reserve(&f, fix).await?;
    result?;
    complete_fix_with_pending_source(
        &mut f,
        fix,
        op,
        "Fixed.\nReview verdict: agree — changed\nResult: done — complete",
        None,
        true,
    )
    .await?;
    assert!(
        f.journal
            .reviews()
            .await?
            .iter()
            .all(|j| j.state != State::Queued)
    );
    f.state.phase = "reviewing-fix".into();
    f.journal.worker_state(f.state.clone()).await?;
    let turn = f.state.checkpoint.as_ref().unwrap().turn_id.to_string();
    let session = f.state.session_id.clone().unwrap();
    let calls = vec![format!("call-{turn}")];
    f.journal
        .save_step_source(crate::step_sources::Source {
            id: crate::step_sources::identity(&session, &turn, &calls),
            track: f.workspace.track_id.clone(),
            session,
            turn,
            calls,
            paths: Some(vec!["math.rs".into()]),
            workspace: f.workspace.clone(),
            at: Stamp::now(),
        })
        .await?;
    assert_eq!(
        f.journal
            .reviews()
            .await?
            .iter()
            .filter(|j| j.state == State::Queued)
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn reviewer_lane_records_attempts_and_opens_their_findings_without_creating_workers()
-> Result<()> {
    let f = Fixture::new().await?;
    let queued = f.journal.snapshot().await?;
    assert_eq!(queued["reviewerLane"]["status"], "waiting");
    let job = f.begin().await?;
    let window = f
        .journal
        .timeline_window(crate::timeline::Window {
            tracks: vec![crate::review_timeline::LANE.into()],
            start: 0,
            end: i64::MAX,
            since: None,
        })
        .await?;
    let clip = window["clips"][0].clone();
    assert_eq!(clip["category"], "review");
    assert_eq!(clip["sessionId"], job.generation.to_string());
    assert_eq!(clip["running"], true);
    assert_eq!(
        f.journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    crate::reviewing::publish(
        &f.journal,
        &job,
        Draft {
            summary: "One required fix.".into(),
            notes: vec![Fixture::note(&job)],
            resolutions: vec![],
        },
    )
    .await?;
    let detail = f
        .journal
        .clip_detail(clip["id"].as_str().unwrap().into())
        .await?;
    assert_eq!(detail["clip"]["running"], false);
    assert_eq!(detail["reviewNotes"].as_array().unwrap().len(), 1);
    assert_eq!(detail["reviewedClips"][0]["id"], job.target.clips[0].id);
    assert_eq!(detail["reviews"][0]["summary"], "One required fix.");
    assert_eq!(detail["reviews"][0]["retryable"], false);
    assert!(detail["items"].as_array().unwrap().is_empty());
    Ok(())
}
#[tokio::test]
async fn retry_creates_a_second_review_clip_and_the_interrupted_attempt_stays_closed() -> Result<()>
{
    let f = Fixture::new().await?;
    let job = f.begin().await?;
    f.journal
        .finish_review(job.clone(), crate::reviews::Finished::Held)
        .await?;
    let next = f
        .journal
        .retry_review(crate::reviews::Retry {
            op_id: Uuid::now_v7(),
            review_id: job.id,
            revision: job.revision,
        })
        .await?;
    let next = f
        .journal
        .begin_review(next.id, next.generation, next.revision)
        .await?
        .unwrap();
    let window = f
        .journal
        .timeline_window(crate::timeline::Window {
            tracks: vec![crate::review_timeline::LANE.into()],
            start: 0,
            end: i64::MAX,
            since: None,
        })
        .await?;
    assert_eq!(window["clips"].as_array().unwrap().len(), 2);
    assert_eq!(window["clips"][0]["running"], false);
    assert_eq!(window["clips"][1]["running"], true);
    let old = f
        .journal
        .clip_detail(window["clips"][0]["id"].as_str().unwrap().into())
        .await?;
    assert_eq!(old["reviews"][0]["retryable"], false);
    f.journal
        .finish_review(next, crate::reviews::Finished::Held)
        .await?;
    Ok(())
}

#[tokio::test]
#[ignore = "three paid dummy turns: initial review, real worker correction, and re-review"]
async fn native_required_fix_runs_once_then_reviewer_confirms_the_saved_result() -> Result<()> {
    let mut f = Fixture::new().await?;
    let plan = bind_reviewed_plan(&f).await?;
    std::fs::write(
        f.workspace.path.join("AGENTS.md"),
        "This is an agent-created test fixture. Correct only math.rs. The harness compiles and tests the saved function after your final answer. Do not create extra drivers or temporary files, and do not run commands that write outside this workspace.\n",
    )?;
    git(
        &f.workspace.path,
        &["update-ref", &f.workspace.reference, &f.workspace.base],
    );
    git(
        &f.workspace.path,
        &[
            "update-ref",
            &format!("{}-checkpoint", f.workspace.reference),
            &f.state.checkpoint.as_ref().unwrap().sha,
        ],
    );
    f.state.phase = "stopped".into();
    f.state.session_id = None;
    f.state.output = None;
    f.journal.worker_state(f.state.clone()).await?;
    let binary = PathBuf::from(std::env::var("INTERCAT_MUSE_BINARY")?);
    let config = Config {
        source: binary.clone(),
        binary,
        credentials: std::env::var("INTERCAT_MUSE_AUTH")?.into(),
        path: std::env::var("PATH")?,
    };
    f.journal.set_worker_config(Uuid::now_v7(), config).await?;
    let rpc = f.root.path().join("rpc");
    std::fs::create_dir(&rpc)?;
    let editor = crate::runtime::tests::native_editor(f.root.path()).await?;
    let paused = std::sync::Arc::new(crate::dispatch_control::Gate::new(false));
    let runtime = crate::runtime::Runtime::start(
        crate::host_memory::Pool::new(),
        f.root.path().into(),
        rpc.clone(),
        f.journal.clone(),
        editor.clone(),
        std::sync::Arc::new(crate::scheduler::Capacity::new(1)),
        paused.clone(),
    );
    let reviewer = crate::reviewing::Service::start(
        crate::host_memory::Pool::new(),
        f.journal.clone(),
        f.root.path().into(),
        rpc,
        runtime.service.assistant_pool(),
        paused,
    );
    let result = tokio::time::timeout(Duration::from_secs(150), async {
        let mut previous = String::new();
        let mut denied=std::collections::HashSet::new();
        loop {
            let db = database(&f)?;
            let fixes = crate::review_fix_store::list(&db)?;
            let status = f.journal.snapshot().await?;
            for worker in status["workers"].as_array().into_iter().flatten() {
                for value in worker["approvals"].as_array().into_iter().flatten() {
                    let approval:crate::execution::Approval=serde_json::from_value(value.clone())?;
                    if denied.insert((approval.id.clone(),approval.requirement.to_string())) {
                        if denied.len()>4 || approval.reply_mode!=Some(crate::approval::ReplyMode::Feedback) {return Err("The dummy test cannot approve or restart this command.".into())}
                        runtime.service.answer(Uuid::now_v7(),&f.workspace.track_id,approval.id,approval.requirement,crate::approval::Response::Reply{text:"Do not run this command. The test harness compiles and checks math.rs after you finish. Complete only the requested correction; do not create other files or write to shared temporary paths.".into(),mode:crate::approval::ReplyMode::Feedback}).await?;
                    }
                }
            }
            let progress = format!(
                "fix={:?}, workers={:?}, reviews={:?}",
                fixes.first().map(|fix| fix.state),
                status["workers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|w| w["phase"].as_str().unwrap_or("unknown"))
                    .collect::<Vec<_>>(),
                f.journal
                    .reviews()
                    .await?
                    .iter()
                    .map(|j| j.state)
                    .collect::<Vec<_>>()
            );
            if progress != previous {
                eprintln!("{progress}");
                previous = progress;
            }
            let Some(current) = fixes.first().cloned() else {
                if f.journal
                    .reviews()
                    .await?
                    .iter()
                    .any(|j| matches!(j.state, State::Failed | State::Held))
                {
                    return Err("Initial native review did not finish.".into());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let current_plan = f.journal.plans().await?.pop().unwrap();
            assert_eq!(current_plan.id, plan.id);
            assert_eq!(crate::plan_execution::owner(&db, &current.input_id.unwrap().to_string())?, Some((plan.id,plan.items[0].plan_item_id)));
            if current.state != FixState::Resolved {
                assert_reviewed_dependency(&db, &current_plan, true)?;
            }
            if current.state == FixState::Resolved {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(current);
            }
            if matches!(
                current.state,
                FixState::Blocked | FixState::Outdated | FixState::Unresolved | FixState::Cut
            ) {
                return Err(format!(
                    "Fix stopped before resolution: {:?} {:?}",
                    current.state, current.reason
                )
                .into());
            }
            let status = f.journal.snapshot().await?;
            if status["workers"].as_array().into_iter().flatten().any(|w| {
                matches!(
                    w["phase"].as_str(),
                    Some("failed" | "lost" | "needs-recovery" | "after-connection-loss")
                )
            }) {
                return Err(format!("Dummy worker stopped: {}", status["workers"]).into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    reviewer.shutdown().await;
    runtime.shutdown().await;
    editor.stop().await;
    let status = f.journal.snapshot().await?;
    if let Ok(output) = std::env::var("INTERCAT_REVIEWER_OUTPUT") {
        std::fs::create_dir_all(&output)?;
        std::fs::write(
            Path::new(&output).join("status.json"),
            serde_json::to_vec_pretty(&status)?,
        )?;
    }
    let resolved = result??;
    let db = database(&f)?;
    let attempts: i64 = db.query_row("SELECT count(*) FROM review_fix_inputs", [], |r| r.get(0))?;
    assert_eq!(attempts, 1);
    f.journal.advance_plans().await?;
    let current_plan = f.journal.plans().await?.pop().unwrap();
    assert_eq!(
        current_plan.items[0].plan_item_id,
        plan.items[0].plan_item_id
    );
    assert_eq!(current_plan.items[0].utterance_id, resolved.input_id);
    assert_eq!(
        current_plan.items[0].state,
        crate::plans::ItemState::Succeeded
    );
    assert_eq!(current_plan.state, crate::plans::State::Held);
    assert_reviewed_dependency(&db, &current_plan, false)?;
    let origin = f
        .journal
        .read_input_origin(resolved.input_id.unwrap())
        .await?
        .ok_or("The correction lost its saved turn")?;
    assert_eq!(origin.checkpoint, resolved.completed_sha);
    assert_eq!(
        crate::plan_execution::turn_owner(
            &db,
            &origin.identity.session_id,
            &origin.identity.turn_id.to_string()
        )?,
        Some((plan.id, plan.items[0].plan_item_id))
    );
    let clips = f
        .journal
        .timeline_window(crate::timeline::Window {
            start: 0,
            end: i64::MAX,
            tracks: vec![f.workspace.track_id.clone()],
            since: None,
        })
        .await?;
    assert!(
        clips["clips"]
            .as_array()
            .unwrap()
            .iter()
            .any(
                |clip| clip["planItemId"] == plan.items[0].plan_item_id.to_string()
                    && clip["turnId"] == origin.identity.turn_id.to_string()
            )
    );
    let mut harness = String::from("include!(");
    harness.push_str(&serde_json::to_string(&f.workspace.path.join("math.rs"))?);
    harness.push_str(");\n#[test] fn doubles_values(){assert_eq!(double(4),8);assert_eq!(double(-3),-6);assert_eq!(double(0),0);}\n");
    let source = f.root.path().join("verify.rs");
    let executable = f.root.path().join("verify");
    std::fs::write(&source, harness)?;
    let built = std::process::Command::new("rustc")
        .args(["--test"])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()?;
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(std::process::Command::new(executable).status()?.success());
    if let Ok(output) = std::env::var("INTERCAT_REVIEWER_OUTPUT") {
        std::fs::write(
            Path::new(&output).join("result.json"),
            serde_json::to_vec_pretty(
                &json!({"schema":crate::journal::SCHEMA_VERSION,"fix":resolved,"attempts":attempts,"workerOutputExecuted":true,"source":"agent-created dummy repository only","paidTurnsExpected":3,"planItemId":plan.items[0].plan_item_id,"correctionKeptOriginalItem":true,"dependentWaitedForRereview":true,"heldPlanRemainedHeld":true}),
            )?,
        )?;
    }
    Ok(())
}

#[tokio::test]
async fn reopening_closes_an_interrupted_review_without_reopening_its_worker_history() -> Result<()>
{
    let f = Fixture::new().await?;
    let job = f.begin().await?;
    let db = database(&f)?;
    crate::review_store::install(&db)?;
    crate::review_timeline::install(&db)?;
    assert_eq!(
        crate::review_store::read(&db, job.id)?.unwrap().state,
        State::Held
    );
    let window = f
        .journal
        .timeline_window(crate::timeline::Window {
            tracks: vec![crate::review_timeline::LANE.into()],
            start: 0,
            end: i64::MAX,
            since: None,
        })
        .await?;
    assert_eq!(window["clips"][0]["running"], false);
    assert_eq!(window["clips"][0]["approximate"], true);
    assert_eq!(window["clips"][0]["sessionId"], job.generation.to_string());
    Ok(())
}

#[tokio::test]
#[ignore = "at most two paid dummy reviews; interrupt an owned real host and retry after reopening"]
async fn native_interrupted_reviewer_closes_its_host_and_retries_after_reopening() -> Result<()> {
    let f = Fixture::new().await?;
    let config = Config {
        source: "/dummy".into(),
        binary: std::env::var("INTERCAT_MUSE_BINARY")?.into(),
        credentials: std::env::var("INTERCAT_MUSE_AUTH")?.into(),
        path: std::env::var("PATH")?,
    };
    f.journal.set_worker_config(Uuid::now_v7(), config).await?;
    let job = f.journal.reviews().await?.pop().unwrap();
    let rpc = f.root.path().join("rpc");
    std::fs::create_dir(&rpc)?;
    let gate = std::sync::Arc::new(crate::dispatch_control::Gate::new(false));
    let capacity = std::sync::Arc::new(crate::scheduler::Capacity::new(1));
    let service = crate::reviewing::Service::start(
        crate::host_memory::Pool::new(),
        f.journal.clone(),
        f.root.path().into(),
        rpc.clone(),
        capacity.clone(),
        gate.clone(),
    );
    let host_dir = f
        .root
        .path()
        .join("review-hosts")
        .join(job.generation.to_string());
    let process = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(record) =
                crate::process_ledger::read(&host_dir.join(crate::process_ledger::NAME))
                && let Some(root) = record.root
                && crate::process::is_alive(root)
            {
                return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(root);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    f.journal
        .stop_review(crate::reviews::Stop {
            op_id: Uuid::now_v7(),
            review_id: job.id,
            revision: job.revision,
        })
        .await?;
    service.shutdown().await;
    assert!(!crate::process::is_alive(process));
    assert_eq!(capacity.snapshot().active, 0);
    assert!(!host_dir.exists());
    drop(f.journal);
    let journal = Journal::open(f.root.path().join("db"), Key::new([7; 32])).await?;
    let held = journal.reviews().await?.pop().unwrap();
    assert_eq!(held.state, State::Held);
    let window = journal
        .timeline_window(crate::timeline::Window {
            tracks: vec![crate::review_timeline::LANE.into()],
            start: 0,
            end: i64::MAX,
            since: None,
        })
        .await?;
    assert_eq!(window["clips"][0]["running"], false);
    journal.pause_dispatch(Uuid::now_v7(), false).await?;
    let next = journal
        .retry_review(crate::reviews::Retry {
            op_id: Uuid::now_v7(),
            review_id: held.id,
            revision: held.revision,
        })
        .await?;
    assert_ne!(next.generation, held.generation);
    let service = crate::reviewing::Service::start(
        crate::host_memory::Pool::new(),
        journal.clone(),
        f.root.path().into(),
        rpc,
        capacity.clone(),
        std::sync::Arc::new(crate::dispatch_control::Gate::new(false)),
    );
    let result = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            let current = journal.reviews().await?.pop().unwrap();
            match current.state {
                State::Complete => {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(current);
                }
                State::Failed | State::Held => {
                    return Err(format!("Retry failed: {:?}", current.detail).into());
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    service.shutdown().await;
    let completed = result??;
    assert_eq!(capacity.snapshot().active, 0);
    if let Ok(output) = std::env::var("INTERCAT_REVIEWER_OUTPUT") {
        std::fs::create_dir_all(&output)?;
        std::fs::write(
            Path::new(&output).join("result.json"),
            serde_json::to_vec_pretty(
                &json!({"schema":crate::journal::SCHEMA_VERSION,"completed":completed,"interruptedProcessStopped":true,"reopenedHeld":true,"newGeneration":true,"capacityReturned":true,"paidTurnsMaximum":2,"source":"agent-created dummy only"}),
            )?,
        )?;
    }
    Ok(())
}

#[tokio::test]
async fn stopping_a_review_is_idempotent_and_cannot_stop_a_newer_attempt() -> Result<()> {
    let f = Fixture::new().await?;
    let job = f.begin().await?;
    let request = crate::reviews::Stop {
        op_id: Uuid::now_v7(),
        review_id: job.id,
        revision: job.revision,
    };
    let stopped = f.journal.stop_review(request.clone()).await?;
    assert_eq!(stopped.state, State::Held);
    assert_eq!(
        f.journal.stop_review(request).await?.revision,
        stopped.revision
    );
    assert!(
        !f.journal
            .finish_review(
                job.clone(),
                crate::reviews::Finished::Complete {
                    draft: Draft {
                        summary: "Too late".into(),
                        notes: vec![],
                        resolutions: vec![]
                    },
                    checked_sha: job.target.checkpoint.sha.clone(),
                    outdated: vec![]
                }
            )
            .await?
    );
    let next = f
        .journal
        .retry_review(crate::reviews::Retry {
            op_id: Uuid::now_v7(),
            review_id: job.id,
            revision: stopped.revision,
        })
        .await?;
    assert!(
        f.journal
            .stop_review(crate::reviews::Stop {
                op_id: Uuid::now_v7(),
                review_id: job.id,
                revision: job.revision
            })
            .await
            .is_err()
    );
    assert_eq!(f.journal.reviews().await?[0].generation, next.generation);
    assert_eq!(f.journal.reviews().await?[0].state, State::Queued);
    let window = f
        .journal
        .timeline_window(crate::timeline::Window {
            tracks: vec![crate::review_timeline::LANE.into()],
            start: 0,
            end: i64::MAX,
            since: None,
        })
        .await?;
    assert_eq!(window["clips"][0]["running"], false);
    assert_eq!(window["clips"][0]["approximate"], true);
    Ok(())
}

#[tokio::test]
async fn corrections_follow_the_reviewed_requests_hold_boundary_instead_of_their_creation_time()
-> Result<()> {
    use crate::decision::{Decision, Kind as DecisionKind, Outcome, Target as InputTarget};
    for request_after_hold in [true, false] {
        let mut f = Fixture::new().await?;
        if request_after_hold {
            f.journal
                .hold_inputs(Uuid::now_v7(), f.workspace.track_id.clone())
                .await?;
        }
        let input = Uuid::now_v7();
        f.journal
            .stage_utterance(
                input,
                "Own request around a local hold".into(),
                InputTarget::Lock(f.workspace.track_id.clone()),
                None,
                vec![],
            )
            .await?;
        let claimed = f.journal.claim_input().await?.unwrap();
        f.journal
            .apply_input(
                claimed.id,
                claimed.revision,
                Outcome::Classified(Decision {
                    kind: DecisionKind::Extend,
                    track_id: Some(f.workspace.track_id.clone()),
                    boundary: Some("now".into()),
                    command: None,
                    confidence: 1.,
                    inference_ms: 0.,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await?;
        let word = f.journal.read_utterance(input.to_string()).await?;
        let operation = Uuid::now_v7();
        let session = f.state.session_id.clone().unwrap();
        let turn = Uuid::now_v7();
        f.journal.reserve_turn(operation,input.to_string(),word["revision"].as_i64().unwrap(),f.workspace.track_id.clone(),json!({"commandId":operation,"sessionId":session,"input":[{"type":"text","text":"Own new request"}]}), None).await?;
        git(
            &f.workspace.path,
            &[
                "-c",
                "user.name=Dummy",
                "-c",
                "user.email=dummy@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "Own post-hold checkpoint",
            ],
        );
        let sha = git(&f.workspace.path, &["rev-parse", "HEAD"]);
        let reference = format!("refs/intercat/dummy/checkpoints/{turn}");
        git(&f.workspace.path, &["update-ref", &reference, &sha]);
        for frame in [
            json!({"method":"turn/started","params":{"sessionId":session,"turnId":turn,"commandId":operation}}),
            json!({"method":"item/completed","params":{"sessionId":session,"turnId":turn,"item":{"itemId":format!("own-user-{turn}"),"turnId":turn,"kind":"userMessage","commandId":operation,"revision":1,"text":"Own new request"}}}),
            json!({"method":"item/completed","params":{"sessionId":session,"turnId":turn,"item":{"itemId":format!("own-tool-{turn}"),"turnId":turn,"callId":format!("own-call-{turn}"),"kind":"toolCall","tool":"edit_file","status":"completed","revision":1,"args":"{}"}}}),
            json!({"method":"item/completed","params":{"sessionId":session,"turnId":turn,"item":{"itemId":format!("own-report-{turn}"),"turnId":turn,"kind":"agentMessage","messagePhase":"finalAnswer","status":"completed","revision":1,"text":"Own report\nResult: done — own dummy work"}}}),
            json!({"method":"turn/completed","params":{"sessionId":session,"turnId":turn,"terminal":"completed"}}),
        ] {
            f.journal
                .try_msp(
                    f.workspace.track_id.clone(),
                    session.clone(),
                    Stamp::now(),
                    frame,
                    None,
                    None,
                )?
                .await??;
        }
        let db = database(&f)?;
        db.execute(
            "UPDATE utterances SET state='landed' WHERE id=?1",
            [input.to_string()],
        )?;
        let calls = vec![format!("own-call-{turn}")];
        f.journal
            .save_step_source(crate::step_sources::Source {
                id: crate::step_sources::identity(&session, &turn.to_string(), &calls),
                track: f.workspace.track_id.clone(),
                session: session.clone(),
                turn: turn.to_string(),
                calls,
                paths: Some(vec!["math.rs".into()]),
                workspace: f.workspace.clone(),
                at: Stamp::now(),
            })
            .await?;
        f.state.checkpoint = Some(crate::checkpoint::Checkpoint {
            turn_id: turn,
            sha,
            reference,
            unmerged_paths: vec![],
            live_processes: false,
        });
        f.journal.worker_state(f.state.clone()).await?;
        if !request_after_hold {
            f.journal
                .hold_inputs(Uuid::now_v7(), f.workspace.track_id.clone())
                .await?;
        }
        let queued = f
            .journal
            .reviews()
            .await?
            .into_iter()
            .find(|job| job.target.turn_id == turn.to_string())
            .ok_or("Missing own post-hold review")?;
        let job = f
            .journal
            .begin_review(queued.id, queued.generation, queued.revision)
            .await?
            .unwrap();
        crate::reviewing::publish(
            &f.journal,
            &job,
            Draft {
                summary: "Own required correction".into(),
                notes: vec![Fixture::note(&job)],
                resolutions: vec![],
            },
        )
        .await?;
        let fix = crate::review_fix_store::list(&db)?
            .into_iter()
            .find(|fix| {
                fix.note_id == crate::review_store::notes_for_job(&db, job.id).unwrap()[0].id
            })
            .unwrap();
        let correction = f
            .journal
            .read_utterance(fix.input_id.unwrap().to_string())
            .await?;
        assert_eq!(
            fix.state,
            if request_after_hold {
                FixState::Planned
            } else {
                FixState::Held
            }
        );
        assert_eq!(
            correction["state"],
            if request_after_hold { "placed" } else { "held" }
        );
        assert_eq!(correction["dispatchRunnable"], request_after_hold);
        assert_eq!(reserve(&f, &fix).await?.1.is_ok(), request_after_hold);
        assert!(crate::worker_hold::current(&db, &f.workspace.track_id)?.is_some());
        assert_eq!(
            crate::worker_hold::turn_input(&db, &session, &turn.to_string())?,
            Some(input.to_string())
        );
    }
    Ok(())
}

#[tokio::test]
async fn explicit_fix_once_more_after_a_hold_authorizes_only_that_finding() -> Result<()> {
    let (f, _, fixes) = reviewed(3).await?;
    for fix in &fixes[..2] {
        assert!(reserve(&f, fix).await?.1.is_ok());
    }
    assert!(reserve(&f, &fixes[2]).await?.1.is_err());
    f.journal
        .hold_inputs(Uuid::now_v7(), f.workspace.track_id.clone())
        .await?;
    let db = database(&f)?;
    let blocked = crate::review_fix_store::read(&db, fixes[2].id)?;
    assert_eq!(blocked.state, FixState::Blocked);
    let extra = f
        .journal
        .review_fix_command(Command {
            op_id: Uuid::now_v7(),
            fix_id: blocked.id,
            revision: blocked.revision,
            action: Action::FixOnceMore,
        })
        .await?;
    assert!(crate::worker_hold::allowed(
        &db,
        &f.workspace.track_id,
        &extra.input_id.unwrap().to_string()
    )?);
    assert!(reserve(&f, &extra).await?.1.is_ok());
    assert!(crate::worker_hold::current(&db, &f.workspace.track_id)?.is_some());
    assert_eq!(
        db.query_row("SELECT total FROM review_fix_file_budget", [], |row| row
            .get::<_, i64>(0))?,
        3
    );
    Ok(())
}
