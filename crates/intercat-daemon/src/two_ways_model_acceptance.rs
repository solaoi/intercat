//! Actual journal admission; model notifications and SDK completion are modeled.
use super::*;

fn observe_model(f: &Fixture, session: &str) -> Result<()> {
    admit_project_secrets(f, session)?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    crate::session_parameters::observe(
        &db,
        session,
        &json!({"method":"session/modelChanged","params":{"sessionId":session,"modelId":"own-model","providerId":"own-provider","viewCursor":Uuid::now_v7()}}),
    )
}

async fn queued(f: &Fixture) -> Result<(two_ways::Job, crate::branch::Branch)> {
    let (_, _, pending) = pending(f).await?;
    let (ready, branch) = ready(f, &pending).await?;
    let job = f
        .journal
        .two_ways(PairChange::Publish {
            id: ready.request.op_id,
            revision: ready.revision,
        })
        .await?;
    observe_model(f, branch.session_id.as_deref().unwrap())?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    crate::session_parameters::observe(
        &db,
        branch.session_id.as_deref().unwrap(),
        &json!({"method":"session/reasoningEffortChanged","params":{"sessionId":branch.session_id,"reasoningEffort":"minimal","viewCursor":"own-default"}}),
    )?;
    Ok((job, branch))
}
fn continuation(
    f: &Fixture,
    branch: &crate::branch::Branch,
    status: &Value,
) -> crate::continuation::Continuation {
    crate::continuation::Continuation {
        operation: Uuid::now_v7(),
        source: branch.track_id.clone(),
        successor: f.workspaces[1].track_id.clone(),
        source_session: branch.session_id.clone().unwrap(),
        session_id: Uuid::now_v7().to_string(),
        board_session: status["session"]["id"].as_str().unwrap().into(),
        folder_id: status["trackFolders"][&branch.track_id]
            .as_str()
            .unwrap()
            .into(),
        checkpoint: branch.point.checkpoint().unwrap().clone(),
        origin: branch.origin.clone(),
        first_input: Uuid::now_v7().to_string(),
        start_command: Uuid::now_v7(),
        opened: true,
        ready: false,
    }
}

fn shown(snapshot: &Value, track: &str) -> Value {
    snapshot["workers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|worker| worker["trackId"] == track)
        .unwrap()
        .clone()
}
async fn instruction(f: &Fixture, track: &str) -> Result<Value> {
    let id = Uuid::now_v7();
    f.journal
        .stage_utterance(
            id,
            "Own ordinary instruction after two ways".into(),
            Target::Lock(track.into()),
            None,
            vec![],
        )
        .await?;
    let input = f.journal.claim_input().await?.unwrap();
    f.journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Extend,
                track_id: Some(track.into()),
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    f.journal.read_utterance(id.to_string()).await
}

#[tokio::test]
async fn paired_header_preserves_the_exact_effort_before_during_and_after_the_first_instruction()
-> Result<()> {
    let f = Fixture::new(2).await?;
    let (job, branch) = queued(&f).await?;
    let before = f.journal.snapshot().await?;
    let display = shown(&before, &branch.track_id);
    assert_eq!(display["model"]["reasoningEffort"], "minimal");
    assert_eq!(
        display["pairedInstruction"],
        json!({"inputId":job.child_input,"effort":"xhigh","phase":"pending"})
    );
    assert!(shown(&before, &job.source_track)["pairedInstruction"].is_null());
    let params = turn_params(branch.session_id.as_deref().unwrap(), "xhigh");
    let turn: Uuid = serde_json::from_value(params["commandId"].clone())?;
    f.journal
        .reserve_turn(
            turn,
            job.child_input.to_string(),
            1,
            branch.track_id.clone(),
            params,
            job.variation.clone(),
        )
        .await?;
    f.journal
        .input_execution(job.child_input.to_string(), 1, "sent")
        .await?;
    let mut worker: WorkerState = serde_json::from_value(display)?;
    worker.active_turn = Some(turn);
    worker.phase = "working".into();
    f.journal.worker_state(worker.clone()).await?;
    let active = shown(&f.journal.snapshot().await?, &branch.track_id);
    assert_eq!(active["pairedInstruction"]["phase"], "active");
    assert_eq!(active["model"]["reasoningEffort"], "xhigh");
    f.journal
        .input_execution(job.child_input.to_string(), 1, "landed")
        .await?;
    worker.active_turn = None;
    worker.phase = "ready".into();
    f.journal.worker_state(worker.clone()).await?;
    let done = shown(&f.journal.snapshot().await?, &branch.track_id);
    assert_eq!(done["model"]["reasoningEffort"], "minimal");
    assert_eq!(done["pairedInstruction"]["phase"], "finished");
    assert_eq!(done["pairedInstruction"]["effort"], "xhigh");
    let next = instruction(&f, &branch.track_id).await?;
    assert_eq!(next["reasoningEffort"], "minimal");
    let params = turn_params(branch.session_id.as_deref().unwrap(), "minimal");
    let next_turn: Uuid = serde_json::from_value(params["commandId"].clone())?;
    f.journal
        .reserve_turn(
            next_turn,
            next["id"].as_str().unwrap().into(),
            next["revision"].as_i64().unwrap(),
            branch.track_id.clone(),
            params,
            None,
        )
        .await?;
    worker.active_turn = Some(next_turn);
    worker.phase = "working".into();
    f.journal.worker_state(worker.clone()).await?;
    let later = shown(&f.journal.snapshot().await?, &branch.track_id);
    assert_eq!(later["model"]["reasoningEffort"], "minimal");
    assert_eq!(later["pairedInstruction"]["phase"], "finished");
    assert_eq!(later["pairedInstruction"]["effort"], "xhigh");
    f.journal
        .input_execution(
            next["id"].as_str().unwrap().into(),
            next["revision"].as_i64().unwrap(),
            "landed",
        )
        .await?;
    worker.active_turn = None;
    worker.phase = "ready".into();
    f.journal.worker_state(worker).await?;
    let link = continuation(&f, &branch, &f.journal.snapshot().await?);
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    db.execute(
        "INSERT INTO continuations VALUES(?1,?2,?3)",
        params![link.source, link.successor, serde_json::to_string(&link)?],
    )?;
    assert_eq!(
        shown(&f.journal.snapshot().await?, &branch.track_id)["pairedInstruction"],
        later["pairedInstruction"],
        "The closed source still records its completed paired effort."
    );
    let reopened = Journal::open(
        f.root.path().join("events.sqlite"),
        crate::secrets::Key::new([7; 32]),
    )
    .await?;
    assert_eq!(
        shown(&reopened.snapshot().await?, &branch.track_id)["pairedInstruction"],
        later["pairedInstruction"]
    );
    Ok(())
}

#[tokio::test]
async fn paired_header_never_substitutes_a_matching_session_default_for_a_missing_or_foreign_turn()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let (job, branch) = queued(&f).await?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let session = branch.session_id.as_deref().unwrap();
    crate::session_parameters::observe(
        &db,
        session,
        &json!({"method":"session/reasoningEffortChanged","params":{"sessionId":session,"reasoningEffort":"xhigh","viewCursor":"own-matching-default"}}),
    )?;
    for (owner, effort) in [("own-foreign-session", "xhigh"), (session, "minimal")] {
        let command = Uuid::now_v7();
        db.execute(
            "INSERT INTO operations VALUES(?1,'turn.start',?2,'complete')",
            params![
                command.to_string(),
                json!({"sessionId":owner,"reasoningEffort":effort}).to_string()
            ],
        )?;
        db.execute(
            "UPDATE utterances SET state='landed',dispatch_command=?2 WHERE id=?1",
            params![job.child_input.to_string(), command.to_string()],
        )?;
        assert!(
            shown(&f.journal.snapshot().await?, &branch.track_id)["pairedInstruction"].is_null()
        );
    }
    db.execute(
        "UPDATE utterances SET dispatch_command=NULL WHERE id=?1",
        [job.child_input.to_string()],
    )?;
    assert!(shown(&f.journal.snapshot().await?, &branch.track_id)["pairedInstruction"].is_null());
    db.execute("UPDATE utterances SET state='placed',decision=json_set(decision,'$.trackId',?2) WHERE id=?1",params![job.child_input.to_string(),job.source_track])?;
    let moved = f.journal.snapshot().await?;
    assert!(shown(&moved, &branch.track_id)["pairedInstruction"].is_null());
    assert!(shown(&moved, &job.source_track)["pairedInstruction"].is_null());
    Ok(())
}

#[tokio::test]
async fn paired_header_follows_a_continuation_and_does_not_label_its_summary_as_the_paired_turn()
-> Result<()> {
    let f = Fixture::new(2).await?;
    let (job, branch) = queued(&f).await?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let status = f.journal.snapshot().await?;
    let continuation = continuation(&f, &branch, &status);
    // Model only the continuation's already accepted identity/word transfer.
    db.execute(
        "INSERT INTO continuations VALUES(?1,?2,?3)",
        params![
            continuation.source,
            continuation.successor,
            serde_json::to_string(&continuation)?
        ],
    )?;
    db.execute(
        "UPDATE utterances SET decision=json_set(decision,'$.trackId',?2) WHERE id=?1",
        params![job.child_input.to_string(), continuation.successor],
    )?;
    let mut successor: WorkerState = serde_json::from_value(shown(&status, &branch.track_id))?;
    successor.track_id = continuation.successor.clone();
    successor.session_id = Some(continuation.session_id.clone());
    successor.active_turn = Some(continuation.start_command);
    successor.phase = "working".into();
    f.journal.worker_state(successor).await?;
    observe_model(&f, &continuation.session_id)?;
    crate::session_parameters::observe(
        &db,
        &continuation.session_id,
        &json!({"method":"session/reasoningEffortChanged","params":{"sessionId":continuation.session_id,"reasoningEffort":"minimal","viewCursor":"own-summary-default"}}),
    )?;
    let next = f.journal.snapshot().await?;
    assert!(shown(&next, &branch.track_id)["pairedInstruction"].is_null());
    let display = shown(&next, &continuation.successor);
    assert_eq!(display["model"]["reasoningEffort"], "minimal");
    assert_eq!(
        display["pairedInstruction"],
        json!({"inputId":job.child_input,"effort":"xhigh","phase":"pending"})
    );
    Ok(())
}
