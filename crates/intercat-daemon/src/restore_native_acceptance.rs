//! Real worker retirement and Restore; every file, credential and turn is dummy.
use super::*;

async fn worker(journal: &Journal, track: &str) -> Result<WorkerState> {
    serde_json::from_value(
        journal.snapshot().await?["workers"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|worker| worker["trackId"] == track)
            .cloned()
            .ok_or("Missing own native worker")?,
    )
    .map_err(Into::into)
}
async fn wait_idle(journal: &Journal, track: &str) -> Result<WorkerState> {
    let mut prior = String::new();
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if let Ok(state) = worker(journal, track).await {
                if state.phase != prior {
                    eprintln!(
                        "own live Restore: waiting worker {} {:?}",
                        state.phase, state.detail
                    );
                    prior = state.phase.clone();
                }
                if state.session_id.is_some()
                    && matches!(state.phase.as_str(), "ready" | "held")
                    && !state.checkpoint_pending
                    && state.active_turn.is_none()
                {
                    break Ok(state);
                }
                if matches!(state.phase.as_str(), "failed" | "needs-recovery") {
                    return Err(
                        format!("Own native worker did not open: {:?}", state.detail).into(),
                    );
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}
async fn schedule(f: &Fixture, track: &str, text: &str) -> Result<serde_json::Value> {
    f.journal
        .stage_utterance(
            Uuid::now_v7(),
            text.into(),
            Target::Lock(track.into()),
            None,
            vec![],
        )
        .await?;
    let word = f
        .journal
        .claim_input()
        .await?
        .ok_or("Missing own running instruction")?;
    let id = word.id.clone();
    f.journal
        .apply_input(
            word.id,
            word.revision,
            Outcome::Classified(Decision {
                kind: Kind::Schedule,
                track_id: Some(track.into()),
                boundary: Some("when-told".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    f.journal.read_utterance(id).await
}

async fn next_checkpoint(
    fixture: &Fixture,
    track: &str,
    previous: Option<Uuid>,
) -> Result<WorkerState> {
    let journal = &fixture.journal;
    let db = rusqlite::Connection::open(fixture.root.path().join("events.sqlite"))?;
    let result = tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let state = worker(journal, track).await?;
            let snapshot=journal.snapshot().await?;
            // Echo does not produce the SDK's final-answer metadata. Resolve
            // only the fixture's resulting review cards through the same
            // explicit Continue action as a user; never waive real questions.
            for question in snapshot["reportQuestions"].as_array().into_iter().flatten().filter(|question|question["trackId"]==track&&question["state"]=="open") {
                if question["kind"]!="review" {return Err("The echo fixture received an unexpected question.".into());}
                journal.respond_report(Uuid::now_v7(),question["id"].as_str().unwrap().into(),question["revision"].as_i64().unwrap(),crate::reports::Answer::Continue).await?;
            }
            let report_saved=if let Some(cp)=&state.checkpoint {
                db.query_row("SELECT EXISTS(SELECT 1 FROM reports WHERE track_id=?1 AND json_extract(record,'$.turnId')=?2 AND pending=0)",params![track,cp.turn_id.to_string()],|row|row.get::<_,bool>(0))?
            }else {false};
            if !state.checkpoint_pending
                && state.active_turn.is_none()
                && matches!(state.phase.as_str(),"ready"|"held")
                && report_saved
                && state
                    .checkpoint
                    .as_ref()
                    .is_some_and(|cp| Some(cp.turn_id) != previous)
            {
                break Ok(state);
            }
            if matches!(
                state.phase.as_str(),
                "failed" | "needs-recovery" | "checkpoint-blocked"
            ) {
                return Err(format!("Own native checkpoint failed: {:?}", state.detail).into());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if result.is_err() {
        let state = worker(journal, track).await?;
        eprintln!(
            "own checkpoint wait expired: {}",
            json!({"phase":state.phase,"pending":state.checkpoint_pending,"active":state.active_turn,"checkpoint":state.checkpoint.as_ref().map(|cp|cp.turn_id),"detail":state.detail})
        );
    }
    result?
}
#[tokio::test]
#[ignore = "native Muse echo: running source retirement, same-track Restore, held words; dummy auth only"]
async fn running_retirement_stops_only_its_worker_and_restore_keeps_the_session_and_held_words()
-> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(3).await?;
    let binary = crate::host::prepare_binary(
        &PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?),
        &f.root.path().join("muse"),
    )?;
    let credentials = f.root.path().join("own-dummy-auth.json");
    fs::write(&credentials, r#"{"schema_version":1}"#)?;
    let config = crate::runtime::Config {
        source: binary.clone(),
        binary,
        credentials,
        path: std::env::var("PATH")?,
    };
    let rpc = f.root.path().join("rpc");
    fs::create_dir(&rpc)?;
    fs::set_permissions(&rpc, fs::Permissions::from_mode(0o700))?;
    let editor = crate::runtime::tests::native_editor(f.root.path()).await?;
    let reports = crate::report_analysis::Service::start(f.journal.clone(), editor.clone());
    let assumptions = crate::assumptions::Service::start(f.journal.clone(), editor.clone());
    let capacity = Arc::new(crate::scheduler::Capacity::new(2));
    let memory = crate::host_memory::Pool::fixed(crate::host_memory::Sample {
        free_bytes: 8 * 1024 * 1024 * 1024,
        pressured: false,
    });
    let runtime = crate::runtime::Runtime::start(
        memory.clone(),
        f.root.path().into(),
        rpc,
        f.journal.clone(),
        editor.clone(),
        capacity.clone(),
        Arc::new(crate::dispatch_control::Gate::new(false)),
    );
    let service = &runtime.service;
    let project = f.journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    service.project_secrets().publish(
        f.journal
            .activate_project_secrets(crate::project_secrets::Load {
                project_id: project,
                revision: 0,
                entries: vec![],
            })
            .await?,
    );
    let a = &f.workspaces[0].track_id;
    let b = &f.workspaces[2].track_id;
    let a_task = crate::runtime::tests::fixture_echo_worker(
        service,
        config.clone(),
        editor.clone(),
        f.workspaces[0].clone(),
    )
    .await?;
    let b_task = crate::runtime::tests::fixture_echo_worker(
        service,
        config,
        editor.clone(),
        f.workspaces[2].clone(),
    )
    .await?;
    let result=async {
        let original=wait_idle(&f.journal,a).await?;
        let other=wait_idle(&f.journal,b).await?;
        eprintln!("own live Restore: both echo workers ready");
        let db=rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
        let echo:i64=db.query_row("SELECT count(*) FROM events WHERE kind='worker.session' AND json_extract(payload,'$.result.session.providerId')='echo'",[],|row|row.get(0))?;
        assert_eq!(echo,2,"Both native workers must use the free, local echo provider");
        // Create the source checkpoints through the real actor. Seeding a new
        // SDK worker over synthetic checkpoints gives it no matching previous
        // checkpoint and cannot represent the lifecycle being accepted here.
        for word in f.journal.snapshot().await?["utterances"].as_array().unwrap().iter().filter(|word|word["decision"]["trackId"]==*a) {
            eprintln!("own initial input: {}",json!({"state":word["state"],"decision":word["decision"],"workspaceAvailable":word["workspaceAvailable"],"planRunnable":word["planRunnable"],"startCondition":word["startCondition"],"confirmation":word["confirmation"]}));
        }
        service.start_turn(Uuid::now_v7(),a).await?;
        let first=next_checkpoint(&f,a,None).await?;
        let fork=first.checkpoint.clone().unwrap();
        eprintln!("own live Restore: first real checkpoint saved");
        fs::write(f.workspaces[0].path.join("later-a.txt"),"Own published A change\n")?;
        let word=schedule(&f,a,"Own second native checkpoint").await?;
        service.start_held_input(Uuid::now_v7(),word["id"].as_str().unwrap().into(),word["revision"].as_i64().unwrap()).await?;
        next_checkpoint(&f,a,Some(fork.turn_id)).await?;
        eprintln!("own live Restore: second real checkpoint saved");
        let source_review=f.action(Action::Open {tracks:Some(vec![a.clone()])}).await?;
        f.action(Action::Prepare {id:source_review.id,revision:source_review.revision}).await?;
        f.settled(source_review.id).await?;
        let mut last_counts=None;
        let source_checked=tokio::time::timeout(Duration::from_secs(30),async {loop {
            let job=f.journal.integration_job(source_review.id).await?;
            let counts=(job.assumptions_pending,job.assumptions_failed);if last_counts!=Some(counts){eprintln!("own live Restore: assumption reading {counts:?}");last_counts=Some(counts);}
            if job.assumptions_pending==0&&job.assumptions_failed==0 {break Ok::<_,Box<dyn std::error::Error+Send+Sync>>(job);}
            tokio::time::sleep(Duration::from_millis(25)).await;
        }}).await??;
        assert_eq!(source_checked.status,Status::Checked);
        eprintln!("own live Restore: source assumptions complete");
        f.action(commit(&source_checked)).await?;
        let source_done=f.settled(source_checked.id).await?;assert_eq!(source_done.status,Status::Done,"{:?}",source_done.error);
        eprintln!("own live Restore: source published");
        f.branch(&fork)?;
        fs::write(f.workspaces[1].path.join("alternative.txt"),"Own chosen alternative\n")?;
        f.save(1,None).await?;
        let opened=f.action(Action::Open {tracks:Some(vec![f.workspaces[1].track_id.clone()])}).await?;
        let chosen=f.action(Action::ChoosePath {id:opened.id,revision:opened.revision,track:f.workspaces[1].track_id.clone()}).await?;
        f.action(Action::Prepare {id:chosen.id,revision:chosen.revision}).await?;
        let checked=f.settled(chosen.id).await?;
        assert_eq!(checked.status,Status::Checked,"{:?}",checked.error);
        eprintln!("own live Restore: replacement checked");
        let word=schedule(&f,a,&"INTERCAT_OWN_RETIRE_RUNNING_ECHO ".repeat(900)).await?;
        service.start_held_input(Uuid::now_v7(),word["id"].as_str().unwrap().into(),word["revision"].as_i64().unwrap()).await?;
        eprintln!("own live Restore: third start requested");
        let mut last_phase=String::new();
        let running=tokio::time::timeout(Duration::from_secs(35),async {loop {
            let state=worker(&f.journal,a).await?;
            if state.phase!=last_phase {eprintln!("own live Restore: third turn phase {} {:?}",state.phase,state.detail);last_phase=state.phase.clone();}
            if state.phase=="working"&&let Some(turn)=state.active_turn {
                let started:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM events WHERE kind='msp.applied' AND session_id=?1 AND json_extract(payload,'$.method')='turn/started' AND json_extract(payload,'$.params.turnId')=?2)",params![state.session_id,turn.to_string()],|row|row.get(0))?;
                if started {break Ok::<_,Box<dyn std::error::Error+Send+Sync>>(state);}
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }}).await??;
        let turn=running.active_turn.unwrap();
        eprintln!("own live Restore: actual SDK running turn observed");
        let future=schedule(&f,a,"Own instruction that must stay held after Restore").await?;
        assert!(crate::process::is_alive(original.identity.unwrap()));
        let committing=f.review.command(ReviewCommand {op_id:Uuid::now_v7(),action:commit(&checked)}).await?;
        assert_eq!(committing.status,Status::Committing);
        let published=f.settled(checked.id).await?;
        assert_eq!(published.status,Status::Done,"{:?}",published.error);
        let entry:crate::path_retirement::Entry=serde_json::from_value(f.journal.snapshot().await?["pathRetirements"][0].clone())?;
        assert_eq!(entry.track,*a);assert_eq!(entry.phase,crate::path_retirement::Phase::Aside);
        let saved=entry.saved.as_ref().unwrap();assert_eq!(saved.session_id,original.session_id);
        let cp=saved.checkpoint.as_ref().ok_or("Running retirement must save a checkpoint")?;assert_eq!(cp.turn_id,turn);
        let stopped=wait_idle(&f.journal,a).await?;assert_eq!(stopped.session_id,original.session_id);
        assert!(stopped.active_turn.is_none());assert!(!stopped.checkpoint_pending);
        let repo=intercat_git::Repository::open(&f.source)?;assert_eq!(repo.reference_target(&saved.reference)?,Some(cp.sha.parse()?));
        let snapshot=f.journal.snapshot().await?;
        let held:Vec<_>=snapshot["utterances"].as_array().unwrap().iter().filter(|word|word["decision"]["trackId"]==*a&&word["state"]=="held").map(|word|(word["id"].clone(),word["revision"].clone(),word["text"].clone())).collect();
        assert!(held.iter().any(|(id,_,_)|id==&future["id"]));
        let request=RestoreRequest {op_id:Uuid::now_v7(),retirement_id:entry.id};service.restore_path(request.clone()).await?;
        let done=tokio::time::timeout(Duration::from_secs(35),async {loop {
            let job=f.journal.restore_path(request.clone()).await?;
            if job.phase==RestorePhase::Done{break Ok::<_,Box<dyn std::error::Error+Send+Sync>>(job);}
            if job.phase==RestorePhase::WaitingForAccess {f.journal.approve_path_restore_access(crate::path_restore::AccessConsent {op_id:Uuid::now_v7(),request:request.clone(),fingerprint:job.inspection.as_ref().unwrap().fingerprint.clone()}).await?;}
            else if matches!(job.phase,RestorePhase::Failed|RestorePhase::NeedsMerge) {return Err(format!("Own live Restore did not finish: {:?}",job.error).into());}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }}).await??;
        assert_eq!(done.restored.as_ref().unwrap().track_id,*a);assert_eq!(done.restored.as_ref().unwrap().session_id,original.session_id);
        assert_eq!(fs::read_to_string(f.workspaces[0].path.join("later-a.txt"))?,"Own published A change\n");
        assert_eq!(fs::read_to_string(f.workspaces[0].path.join("alternative.txt"))?,"Own chosen alternative\n");
        assert!(!crate::path_retirement::holds(&db,a)?);
        for (id,revision,text) in held {let current=f.journal.read_utterance(id.as_str().unwrap().into()).await?;assert_eq!(current["state"],"held");assert_eq!(current["revision"],revision);assert_eq!(current["text"],text);}
        let restored=wait_idle(&f.journal,a).await?;assert_eq!(restored.session_id,original.session_id);assert_eq!(restored.identity,original.identity);assert!(crate::process::is_alive(restored.identity.unwrap()));
        assert_eq!(serde_json::to_value(worker(&f.journal,b).await?)?,serde_json::to_value(&other)?);assert!(crate::process::is_alive(other.identity.unwrap()));assert_eq!(capacity.snapshot().active,0);
        let starts:i64=db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'",[],|row|row.get(0))?;assert_eq!(starts,3,"Restore must not start held words");
        let future=f.journal.read_utterance(future["id"].as_str().unwrap().into()).await?;
        service.start_held_input(Uuid::now_v7(),future["id"].as_str().unwrap().into(),future["revision"].as_i64().unwrap()).await?;
        let resumed=next_checkpoint(&f,a,restored.checkpoint.as_ref().map(|cp|cp.turn_id)).await?;
        assert_eq!(resumed.session_id,original.session_id);assert_eq!(resumed.identity,original.identity);
        let starts:i64=db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'",[],|row|row.get(0))?;assert_eq!(starts,4,"Only the explicit Start may resume the held instruction");

        let stop_for_other:i64=db.query_row("SELECT count(*) FROM events WHERE kind='worker.stop' AND json_extract(payload,'$.trackId')=?1",[b],|row|row.get(0))?;assert_eq!(stop_for_other,0);
        assert_eq!(git(&f.source,&["rev-parse","HEAD"] )?,f.head);assert_eq!(fs::read(f.source.join(".git/index"))?,f.index);
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    eprintln!(
        "own live Restore: checks finished, shutting down: {}",
        result.is_ok()
    );
    runtime.shutdown().await;
    tokio::time::timeout(Duration::from_secs(30), a_task).await??;
    tokio::time::timeout(Duration::from_secs(30), b_task).await??;
    reports.shutdown().await;
    assumptions.shutdown().await;
    editor.stop().await;
    assert_eq!(memory.snapshot().hosts, 0);
    result
}
