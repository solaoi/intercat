//! Received turn identities and real Git checkpoints across continuation.
//! The MSP envelopes are modeled locally; native transport is verified separately.
use super::plans::{current, draft, plan};
use super::*;
use crate::{
    plan_store::{Action as PlanAction, Command as PlanCommand, Generated},
    plans::{ItemState, State},
};

async fn boot(f: &Fixture, track: &str, session: String) -> Result<WorkerState> {
    let value = WorkerState {
        track_id: track.into(),
        phase: "ready".into(),
        session_id: Some(session),
        secret_revision: Some(0),
        identity: None,
        detail: None,
        active_turn: None,
        approvals: vec![],
        output: None,
        checkpoint_pending: false,
        checkpoint: None,
        prompts: vec![],
    };
    f.journal.worker_state(value.clone()).await?;
    Ok(value)
}
async fn receive(
    f: &Fixture,
    mut worker: WorkerState,
    input: &str,
    result: &str,
) -> Result<WorkerState> {
    let word = f
        .journal
        .read_utterance(input.into())
        .await
        .map_err(|error| format!("Receive input {input}: {error}"))?;
    let revision = word["revision"].as_i64().unwrap();
    let command = Uuid::now_v7();
    let turn = Uuid::now_v7();
    let session = worker.session_id.clone().unwrap();
    f.journal.reserve_turn(command,input.into(),revision,worker.track_id.clone(),json!({"commandId":command,"sessionId":session,"input":[{"type":"text","text":word["text"]}]}), None).await?;
    worker.phase = "working".into();
    worker.active_turn = Some(turn);
    f.journal.worker_state(worker.clone()).await?;
    for frame in [
        json!({"method":"turn/started","params":{"sessionId":session,"turnId":turn,"commandId":command}}),
        json!({"method":"item/completed","params":{"sessionId":session,"turnId":turn,"item":{"itemId":Uuid::now_v7(),"turnId":turn,"commandId":command,"kind":"userMessage","revision":1,"status":"completed","text":word["text"]}}}),
        json!({"method":"item/completed","params":{"sessionId":session,"turnId":turn,"item":{"itemId":Uuid::now_v7(),"turnId":turn,"commandId":command,"kind":"agentMessage","revision":1,"status":"completed","messagePhase":"finalAnswer","text":result}}}),
        json!({"method":"turn/completed","params":{"sessionId":session,"turnId":turn,"terminal":"completed"}}),
    ] {
        f.journal
            .try_msp(
                worker.track_id.clone(),
                session.clone(),
                crate::clock::Stamp::now(),
                frame,
                None,
                None,
            )?
            .await??;
    }
    f.journal
        .input_execution(input.into(), revision, "landed")
        .await?;
    f.journal.finish(&command.to_string(), "dispatched").await?;
    f.journal.finish(&command.to_string(), "complete").await?;
    worker.phase = "checkpoint".into();
    worker.active_turn = None;
    worker.checkpoint_pending = true;
    f.journal.worker_state(worker.clone()).await?;
    let workspace = f.journal.workspace(worker.track_id.clone()).await?;
    let point = checkpoint::save(
        workspace,
        turn,
        worker.checkpoint.clone(),
        false,
        f.journal.clone(),
    )
    .await
    .map_err(|error| format!("Receive checkpoint for {input}: {error}"))?;
    worker.phase = "ready".into();
    worker.checkpoint_pending = false;
    worker.checkpoint = Some(point);
    f.journal.worker_state(worker.clone()).await?;
    Ok(worker)
}
async fn fresh_from(f: &Fixture, worker: WorkerState) -> Result<crate::fresh::Job> {
    let released = release_source(f, worker)
        .await
        .map_err(|error| format!("Fresh release: {error}"))?;
    let files = snapshot_released(f, released)
        .await
        .map_err(|error| format!("Fresh capture: {error}"))?;
    let job = summarize_snapshot(f, files)
        .await
        .map_err(|error| format!("Fresh summary: {error}"))?;
    transfer(f, &job)
        .await
        .map_err(|error| format!("Fresh transfer: {error}"))?;
    Ok(job)
}
fn stored_input(db: &rusqlite::Connection, input: &str) -> Result<String> {
    Ok(db.query_row("SELECT json_object('text',text,'target',target,'selected',selected,'state',state,'revision',revision,'decision',decision,'provisional',provisional,'command',dispatch_command) FROM utterances WHERE id=?1",[input],|row|row.get(0))?)
}
fn stored_clip(db: &rusqlite::Connection, origin: &crate::input_origin::Origin) -> Result<String> {
    Ok(db.query_row(
        "SELECT c.record FROM timeline_turns t JOIN timeline_clips c ON c.id=json_extract(t.record,'$.current') WHERE t.session=?1 AND t.turn=?2",
        params![origin.identity.session_id,origin.identity.turn_id.to_string()],|row|row.get(0),
    )?)
}
async fn complete_first_summary(f: &Fixture, job: &crate::fresh::Job) -> Result<WorkerState> {
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let owner = crate::continuation::start(&db, &job.successor_track)?.unwrap();
    let worker = boot(f, &job.successor_track, owner.session_id.clone()).await?;
    f.journal
        .session_start_opened(&job.successor_track, &owner.session_id)
        .await?;
    let done = Box::pin(receive(
        f,
        worker,
        &owner.first_input,
        "Result: done — Own handoff inspected",
    ))
    .await?;
    assert!(
        crate::continuation::start(&db, &job.successor_track)?
            .unwrap()
            .ready
    );
    Ok(done)
}

#[tokio::test]
async fn a_completed_single_job_keeps_its_received_turn_and_plan_record_after_fresh() -> Result<()>
{
    let f = Fixture::new(1).await?;
    let track = f.workspaces[0].track_id.clone();
    let worker = boot(&f, &track, Uuid::now_v7().to_string()).await?;
    let initial = plan(&f, Some(&track)).await?;
    assert!(
        f.journal
            .begin_plan(initial.id, initial.generation.id, initial.rev)
            .await?
    );
    assert!(
        f.journal
            .generated_plan(
                initial.id,
                initial.generation.id,
                initial.rev,
                Generated::TimedOut,
                vec![]
            )
            .await?
    );
    let failed = current(&f, initial.id).await?;
    f.journal
        .plan_command(PlanCommand {
            op_id: Uuid::now_v7(),
            plan_id: failed.id,
            revision: failed.rev,
            action: PlanAction::RunAsOne,
        })
        .await?;
    let input = f.journal.claim_input().await?.unwrap();
    f.journal
        .apply_input(
            input.id.clone(),
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(track),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let worker = Box::pin(receive(
        &f,
        worker,
        &input.id,
        "Result: done — Own single job complete",
    ))
    .await?;
    let origin = f
        .journal
        .read_input_origin(input.id.parse()?)
        .await?
        .unwrap();
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let before = stored_input(&db, &input.id)?;
    let before_clip = stored_clip(&db, &origin)?;
    let before_plan: String = db.query_row(
        "SELECT record FROM plans WHERE id=?1",
        [initial.id.to_string()],
        |row| row.get(0),
    )?;
    let before_turn: String = db.query_row(
        "SELECT record FROM timeline_turns WHERE session=?1 AND turn=?2",
        params![
            origin.identity.session_id,
            origin.identity.turn_id.to_string()
        ],
        |row| row.get(0),
    )?;
    let job = Box::pin(fresh_from(&f, worker)).await?;
    assert_eq!(stored_input(&db, &input.id)?, before);
    assert_eq!(stored_clip(&db, &origin)?, before_clip);
    assert_eq!(
        db.query_row(
            "SELECT record FROM plans WHERE id=?1",
            [initial.id.to_string()],
            |row| row.get::<_, String>(0)
        )?,
        before_plan
    );
    assert_eq!(
        db.query_row(
            "SELECT record FROM timeline_turns WHERE session=?1 AND turn=?2",
            params![
                origin.identity.session_id,
                origin.identity.turn_id.to_string()
            ],
            |row| row.get::<_, String>(0)
        )?,
        before_turn
    );
    let after = f
        .journal
        .read_input_origin(input.id.parse()?)
        .await?
        .unwrap();
    assert_eq!(after.identity, origin.identity);
    assert_eq!(after.checkpoint, origin.checkpoint);
    assert_eq!(current(&f, initial.id).await?.state, State::SingleJob);
    assert!(crate::continuation::closed(&db, &job.request.track_id)?);
    assert!(f.workspaces[0].path.join("own-final.txt").is_file());
    Ok(())
}

#[tokio::test]
async fn retry_of_a_saved_failed_item_uses_the_new_session_without_rewriting_its_old_clip()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let track = f.workspaces[0].track_id.clone();
    let worker = boot(&f, &track, Uuid::now_v7().to_string()).await?;
    let initial = plan(&f, Some(&track)).await?;
    let folder = f
        .journal
        .plan_context(initial.id)
        .await?
        .folders
        .into_iter()
        .next()
        .unwrap();
    assert!(
        f.journal
            .begin_plan(initial.id, initial.generation.id, initial.rev)
            .await?
    );
    assert!(
        f.journal
            .generated_plan(
                initial.id,
                initial.generation.id,
                initial.rev,
                Generated::Draft(draft(&folder, &track, "Own two steps with a failure")),
                vec![]
            )
            .await?
    );
    let ready = current(&f, initial.id).await?;
    f.journal
        .plan_command(PlanCommand {
            op_id: Uuid::now_v7(),
            plan_id: ready.id,
            revision: ready.rev,
            action: PlanAction::Review,
        })
        .await?;
    f.journal
        .plan_command(PlanCommand {
            op_id: Uuid::now_v7(),
            plan_id: ready.id,
            revision: ready.rev,
            action: PlanAction::Start,
        })
        .await?;
    f.journal.advance_plans().await?;
    let running = current(&f, initial.id).await?;
    let old = running.items[0].utterance_id.unwrap();
    let worker = Box::pin(receive(
        &f,
        worker,
        &old.to_string(),
        "Result: failed — Own dummy file still missing",
    ))
    .await?;
    f.journal.advance_plans().await?;
    let failed = current(&f, initial.id).await?;
    assert_eq!(failed.items[0].state, ItemState::Failed);
    f.journal
        .plan_command(PlanCommand {
            op_id: Uuid::now_v7(),
            plan_id: failed.id,
            revision: failed.rev,
            action: PlanAction::Hold,
        })
        .await?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let before = stored_input(&db, &old.to_string())?;
    let origin = f.journal.read_input_origin(old).await?.unwrap();
    let before_clip = stored_clip(&db, &origin)?;
    let job = Box::pin(fresh_from(&f, worker)).await?;
    let moved = current(&f, initial.id).await?;
    assert_eq!(moved.items[0].utterance_id, Some(old));
    assert_eq!(moved.items[0].track_ref, job.successor_track);
    let request = PlanCommand {
        op_id: Uuid::now_v7(),
        plan_id: moved.id,
        revision: moved.rev,
        action: PlanAction::RetryItem {
            item_id: moved.items[0].plan_item_id,
        },
    };
    let retry = Uuid::new_v5(&request.op_id, b"retry-plan-item");
    let queued = f.journal.plan_command(request.clone()).await?;
    assert_eq!(f.journal.plan_command(request).await?.rev, queued.rev);
    assert_eq!(queued.items[0].utterance_id, Some(old));
    let word = f.journal.read_utterance(retry.to_string()).await?;
    assert_eq!(word["decision"]["trackId"], job.successor_track);
    assert_eq!(word["target"]["trackId"], job.successor_track);
    assert_eq!(word["freshRunnable"], false);
    let owner = crate::continuation::start(&db, &job.successor_track)?.unwrap();
    let command = Uuid::now_v7();
    let error=f.journal.reserve_turn(command,retry.to_string(),word["revision"].as_i64().unwrap(),job.successor_track.clone(),json!({"commandId":command,"sessionId":owner.session_id,"input":[{"type":"text","text":word["text"]}]}), None).await.unwrap_err();
    assert!(error.is::<crate::journal::StartSuperseded>());
    let worker = Box::pin(complete_first_summary(&f, &job)).await?;
    let worker = Box::pin(receive(
        &f,
        worker,
        &retry.to_string(),
        "Result: done — Own retry completed",
    ))
    .await?;
    assert_eq!(
        worker.session_id.as_deref(),
        Some(job.successor_session.as_str())
    );
    f.journal.advance_plans().await?;
    let done = current(&f, initial.id).await?;
    assert_eq!(done.items[0].utterance_id, Some(retry));
    assert_eq!(done.items[0].state, ItemState::Succeeded);
    assert_eq!(done.items[1].state, ItemState::Planned);
    assert!(!done.items[1].start_requested);
    assert_eq!(done.state, State::Held);
    assert_eq!(stored_input(&db, &old.to_string())?, before);
    assert_eq!(stored_clip(&db, &origin)?, before_clip);
    let old_origin = f.journal.read_input_origin(old).await?.unwrap();
    assert_eq!(old_origin.identity, origin.identity);
    assert_eq!(old_origin.checkpoint, origin.checkpoint);
    assert_eq!(
        crate::plan_execution::owner(&db, &retry.to_string())?,
        Some((done.id, done.items[0].plan_item_id))
    );
    Ok(())
}

#[tokio::test]
async fn a_cancelled_hold_follows_a_later_fresh_and_inputs_materialized_after_its_summary()
-> Result<()> {
    let stage = std::cell::Cell::new("fixture setup");
    let result: Result<()> = async {
        let f = Fixture::new(1).await?;
        stage.set("source release");
        let original = release_without_a_host(&f).await?;
        let track = original.request.track_id.clone();
        stage.set("plan generation and start");
        let initial = plan(&f, Some(&track)).await?;
        assert!(
            f.journal
                .begin_plan(initial.id, initial.generation.id, initial.rev)
                .await?
        );
        assert!(
            f.journal
                .generated_plan(
                    initial.id,
                    initial.generation.id,
                    initial.rev,
                    Generated::Draft(draft(
                        &original.folder.id,
                        &track,
                        "Own plan before cancellation"
                    )),
                    vec![],
                )
                .await?
        );
        let ready = current(&f, initial.id).await?;
        f.journal
            .plan_command(PlanCommand {
                op_id: Uuid::now_v7(),
                plan_id: ready.id,
                revision: ready.rev,
                action: PlanAction::Review,
            })
            .await?;
        f.journal
            .plan_command(PlanCommand {
                op_id: Uuid::now_v7(),
                plan_id: ready.id,
                revision: ready.rev,
                action: PlanAction::Start,
            })
            .await?;
        assert!(
            current(&f, initial.id)
                .await?
                .items
                .iter()
                .all(|item| item.utterance_id.is_none())
        );
        stage.set("pending words and cancellation");
        let old_word = future(&f).await?;
        let cancel = Uuid::now_v7();
        f.journal
            .fresh(Change::Cancel {
                op_id: cancel,
                id: original.request.op_id,
                revision: original.revision,
            })
            .await?;
        let between = future(&f).await?;
        let hold = Uuid::new_v5(&cancel, b"fresh-cancel-hold");
        let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
        stage.set("source worker before first transfer");
        let source: WorkerState = serde_json::from_str(&db.query_row(
            "SELECT record FROM workers WHERE track_id=?1",
            [&track],
            |row| row.get::<_, String>(0),
        )?)?;
        stage.set("first transfer");
        let job = Box::pin(fresh_from(&f, source)).await?;
        f.journal.advance_plans().await?;
        stage.set("plan after first transfer");
        let moved = current(&f, initial.id).await?;
        assert_eq!(moved.state, State::Running);
        assert_eq!(moved.items[0].track_ref, job.successor_track);
        let first = moved.items[0].utterance_id.unwrap();
        stage.set("first materialized held input");
        let held = f.journal.read_utterance(first.to_string()).await?;
        assert_eq!(held["state"], "held");
        assert_eq!(held["dispatchRunnable"], false);
        assert_eq!(
            crate::worker_hold::current(&db, &job.successor_track)?,
            Some(hold)
        );
        assert_eq!(crate::worker_hold::current(&db, &track)?, Some(hold));
        stage.set("first summary completion");
        let worker = Box::pin(complete_first_summary(&f, &job)).await?;
        f.journal.advance_plans().await?;
        assert_eq!(
            f.journal.read_utterance(first.to_string()).await?["state"],
            "held"
        );
        assert_eq!(
            f.journal
                .read_utterance(between["id"].as_str().unwrap().into())
                .await?["dispatchRunnable"],
            true
        );
        // A later new word is allowed without authorizing older generated work.
        stage.set("pending input after first summary");
        let newer = future(&f).await?;
        assert_eq!(newer["decision"]["trackId"], job.successor_track);
        assert_eq!(newer["dispatchRunnable"], true);
        stage.set("explicit item start");
        let admitted = f
            .journal
            .start_held_input(
                Uuid::now_v7(),
                first.to_string(),
                held["revision"].as_i64().unwrap(),
            )
            .await?;
        assert_eq!(admitted["state"], "placed");
        stage.set("explicitly started item receipt");
        let worker = Box::pin(receive(
            &f,
            worker,
            &first.to_string(),
            "Result: done — Own explicitly started item",
        ))
        .await?;
        f.journal.advance_plans().await?;
        stage.set("second materialized input");
        let next = current(&f, initial.id).await?;
        assert_eq!(next.items[0].state, ItemState::Succeeded);
        let second = next.items[1].utterance_id.unwrap();
        assert_eq!(
            f.journal.read_utterance(second.to_string()).await?["state"],
            "held"
        );
        assert_eq!(
            f.journal
                .read_utterance(old_word["id"].as_str().unwrap().into())
                .await?["state"],
            "held"
        );
        assert_eq!(
            crate::worker_hold::current(&db, &job.successor_track)?,
            Some(hold)
        );
        stage.set("second transfer");
        let again = Box::pin(fresh_from(&f, worker)).await?;
        assert_eq!(
            crate::worker_hold::current(&db, &again.successor_track)?,
            Some(hold)
        );
        stage.set("second summary completion");
        Box::pin(complete_first_summary(&f, &again)).await?;
        f.journal.advance_plans().await?;
        stage.set("held input after second summary");
        let later = f.journal.read_utterance(second.to_string()).await?;
        assert_eq!(later["decision"]["trackId"], again.successor_track);
        assert_eq!(later["state"], "held");
        assert_eq!(later["dispatchRunnable"], false);
        stage.set("database reopen");
        let Fixture { root, journal, .. } = f;
        drop(journal);
        let reopened = Journal::open(
            root.path().join("events.sqlite"),
            crate::secrets::Key::new([7; 32]),
        )
        .await?;
        stage.set("reopened plan input and hold");
        reopened.advance_plans().await?;
        assert_eq!(
            reopened.read_utterance(second.to_string()).await?["state"],
            "held"
        );
        assert_eq!(
            reopened.snapshot().await?["workerHolds"][&again.successor_track],
            hold.to_string()
        );
        Ok(())
    }
    .await;
    result.map_err(|error| {
        format!(
            "Fresh cancellation/materialization at {}: {error}",
            stage.get()
        )
        .into()
    })
}

#[tokio::test]
async fn a_later_hold_blocks_the_fresh_summary_until_its_own_explicit_start() -> Result<()> {
    for before_transfer in [true, false] {
        let f = Fixture::new(1).await?;
        let job = summarized(&f).await?;
        let hold = Uuid::now_v7();
        if before_transfer {
            f.journal
                .hold_inputs(hold, job.request.track_id.clone())
                .await?;
        }
        transfer(&f, &job).await?;
        let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
        let owner = crate::continuation::start(&db, &job.successor_track)?.unwrap();
        if !before_transfer {
            assert!(crate::worker_hold::allowed(
                &db,
                &job.successor_track,
                &owner.first_input
            )?);
            f.journal
                .hold_inputs(hold, job.successor_track.clone())
                .await?;
        }
        assert!(!crate::worker_hold::allowed(
            &db,
            &job.successor_track,
            &owner.first_input
        )?);
        let error = Box::pin(complete_first_summary(&f, &job))
            .await
            .unwrap_err();
        assert!(error.is::<crate::journal::StartSuperseded>());
        let word = f.journal.read_utterance(owner.first_input.clone()).await?;
        assert_eq!(word["state"], "held");
        f.journal
            .start_held_input(
                Uuid::now_v7(),
                owner.first_input,
                word["revision"].as_i64().unwrap(),
            )
            .await?;
        Box::pin(complete_first_summary(&f, &job)).await?;
        assert_eq!(
            crate::worker_hold::current(&db, &job.successor_track)?,
            Some(hold)
        );
    }
    Ok(())
}

#[tokio::test]
async fn answers_after_the_summary_freeze_are_received_as_later_words_in_the_new_conversation()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let files = snapshot(&f).await?;
    let report = crate::reports::Report::new(
        files.request.track_id.clone(),
        files.request.session_id.clone(),
        files.boundary.as_ref().unwrap().checkpoint.turn_id,
        "own-frozen-questions".into(),
        "Own two unresolved choices".into(),
    );
    let report = f.journal.observe_report(report).await?.report;
    f.journal
        .analyze_report(
            report.id,
            report.revision,
            crate::reports::Analysis::Questions(vec![
                "Own original color choice?".into(),
                "Own original format choice?".into(),
            ]),
        )
        .await?;
    let questions = f.journal.snapshot().await?["reportQuestions"]
        .as_array()
        .unwrap()
        .clone();
    let preparing = f
        .journal
        .fresh(Change::PrepareSummary {
            id: files.request.op_id,
            revision: files.revision,
            changes: crate::fresh_summary::changes(&files).await?,
        })
        .await?;
    assert_eq!(
        preparing.summary_input.as_ref().unwrap()["unresolvedQuestions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let before = Uuid::now_v7();
    f.journal
        .respond_report(
            before,
            questions[0]["id"].as_str().unwrap().into(),
            questions[0]["revision"].as_i64().unwrap(),
            crate::reports::Answer::Reply {
                text: "Own latest correction: GREEN_ONLY".into(),
            },
        )
        .await?;
    let mut summary = document(&preparing);
    summary.fields.unresolved = questions
        .iter()
        .map(|question| question["text"].as_str().unwrap().into())
        .collect();
    let ready = f
        .journal
        .fresh(Change::Summarized {
            id: preparing.request.op_id,
            revision: preparing.revision,
            document: summary,
        })
        .await?;
    assert_eq!(ready.summary_input, preparing.summary_input);
    transfer(&f, &ready).await?;
    let after = Uuid::now_v7();
    f.journal
        .respond_report(
            after,
            questions[1]["id"].as_str().unwrap().into(),
            questions[1]["revision"].as_i64().unwrap(),
            crate::reports::Answer::Reply {
                text: "Own latest format: PLAIN_TEXT".into(),
            },
        )
        .await?;
    for (id, question) in [(before, &questions[0]), (after, &questions[1])] {
        let reply = f.journal.read_utterance(id.to_string()).await?;
        assert_eq!(reply["decision"]["trackId"], ready.successor_track);
        assert_eq!(reply["freshRunnable"], false);
        assert!(reply["replyTo"].is_null());
        assert!(
            reply["text"]
                .as_str()
                .unwrap()
                .contains(question["id"].as_str().unwrap())
        );
        assert!(
            reply["text"]
                .as_str()
                .unwrap()
                .contains(question["text"].as_str().unwrap())
        );
    }
    let mut worker = Box::pin(complete_first_summary(&f, &ready)).await?;
    for id in [before, after] {
        worker = Box::pin(receive(
            &f,
            worker,
            &id.to_string(),
            "Result: done — Own later answer received",
        ))
        .await?;
        let origin = f.journal.read_input_origin(id).await?.unwrap();
        assert_eq!(origin.identity.session_id, ready.successor_session);
        assert!(origin.checkpoint.is_some());
    }
    assert_eq!(
        worker.session_id.as_deref(),
        Some(ready.successor_session.as_str())
    );
    assert!(
        f.journal.read_utterance(before.to_string()).await?["text"]
            .as_str()
            .unwrap()
            .contains("GREEN_ONLY")
    );
    assert!(
        f.journal.read_utterance(after.to_string()).await?["text"]
            .as_str()
            .unwrap()
            .contains("PLAIN_TEXT")
    );
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM report_questions WHERE state='answered-elsewhere'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        2
    );
    assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind IN ('turn.start','input/answer') AND json_extract(payload,'$.sessionId')=?1",[&ready.request.session_id],|row|row.get::<_, i64>(0))?, 0);
    assert_eq!(
        f.journal
            .fresh_job(ready.request.op_id)
            .await?
            .unwrap()
            .summary_input,
        preparing.summary_input
    );
    Ok(())
}
