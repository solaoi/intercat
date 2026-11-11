//! Pinned MSP acceptance with offline, metadata-only and opt-in provider cases.
#[path = "two_ways_paid.rs"]
mod paid;

use super::*;
use crate::two_ways::{Change, Phase, Request};

async fn start_at_worker_boundary(service: &Service, track: &str) -> Result<()> {
    let id = Uuid::now_v7();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match service.start_turn(id, track).await {
                Err(error) if error.to_string() == "Worker is not at a start boundary." => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                result => return result,
            }
        }
    })
    .await?
}

#[tokio::test]
#[ignore = "actual pinned SDK empty branch; modeled budget/catalog, offline echo, no model turns"]
async fn native_initial_files_use_an_empty_sdk_fork_without_a_completed_turn() -> Result<()> {
    let output = PathBuf::from(std::env::var("INTERCAT_M4_INITIAL_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let mut fixture = QueueFixture::new().await?;
    let result=async {
        fixture.launch(&[0]).await?;
        let track=&fixture.words[0].1;
        let snapshot=fixture.journal.snapshot().await?;
        let worker=snapshot["workers"].as_array().unwrap().iter().find(|w|w["trackId"]==*track).unwrap();
        let session=worker["sessionId"].as_str().unwrap();
        let word=fixture.journal.read_utterance(fixture.words[0].0.clone()).await?;
        assert!(worker["checkpoint"].is_null());
        let workspace=fixture.journal.workspace(track.clone()).await?;
        std::fs::write(workspace.path.join("own-initial.txt"),"Own files shared before any turn\n")?;
        let models:muse_code_sdk::msp::ModelListResult=serde_json::from_value(json!({"providerId":"own-modeled-provider","profileId":null,"source":"fakeCatalog","models":[{"modelId":"own-modeled-model","displayLabel":"Own modeled catalogue","providerId":"own-modeled-provider","profileId":null,"releaseDate":null,"description":null,"contextLimit":null,"outputLimit":null,"cost":null,"variants":["minimal","xhigh"],"isActive":true,"isDefault":true}]}))?;
        let pending=fixture.journal.two_ways(Change::Prepare(Request{op_id:Uuid::now_v7(),input_id:fixture.words[0].0.clone(),revision:word["revision"].as_i64().unwrap(),selection:crate::two_ways::Selection{track_id:track.clone(),session_id:session.into(),variation:crate::two_ways::Variation::from_catalog(&models)?}})).await?;
        let capturing=fixture.journal.two_ways(Change::Capture{id:pending.request.op_id,revision:pending.revision}).await?;
        let captured=fixture.service.advance_two_ways(capturing).await?;
        assert_eq!(captured.phase,Phase::Inspecting);
        assert!(captured.point.as_ref().unwrap().checkpoint().is_none());
        let now=crate::journal::timestamp();
        let usage=serde_json::from_value(json!({"usage":{"window":{"usedPercent":10,"windowDurationMins":300,"resetsAtMs":now+3600000},"weekly":{"usedPercent":10,"resetsAtMs":now+86400000},"tier":"own-modeled-tier","observedAtMs":now-1}}))?;
        let forking=fixture.journal.two_ways(Change::Inspect{id:pending.request.op_id,revision:captured.revision,usage,models}).await?;
        host::OFFLINE_ECHO.scope((),crate::branch_run::prepare(fixture.memory.clone(),&fixture.journal,fixture.config.clone(),fixture.root.path(),&fixture.root.path().join("rpc"),fixture.stop.subscribe(),forking.branch_request()?)).await?;
        let awaiting=fixture.journal.two_ways(Change::BranchReady{id:forking.request.op_id,revision:forking.revision}).await?;
        assert_eq!(awaiting.phase,Phase::AwaitingReceipt);
        let state=fixture.journal.snapshot().await?;
        let branch:crate::branch::Branch=serde_json::from_value(state["branches"][0].clone())?;
        assert_eq!(branch.origin.turn_number,0);
        assert_eq!(std::fs::read_to_string(branch.workspace.path.join("own-initial.txt"))?,"Own files shared before any turn\n");
        fixture.service.port(track)?.verify_empty_session(session).await?;
        let db=rusqlite::Connection::open(fixture.root.path().join("db"))?;
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind IN ('checkpoint.save','turn.start')",[],|row|row.get::<_,i64>(0))?,0);
        let operation=fixture.journal.operation(branch.request.op_id).await?.unwrap();
        assert!(operation["payload"].get("cutPoint").is_none());
        fixture.journal.confirm_fingerprints(fixture.journal.fingerprint_catalog().await?.current).await?;
        let activated=fixture.service.activate_branch(branch.request.op_id).await?;
        assert!(activated.point.checkpoint().is_none());
        crate::two_ways_files::before_publish(&fixture.journal,&awaiting,&activated.track_id).await?;
        let state=fixture.journal.snapshot().await?;
        assert!(state["workers"].as_array().unwrap().iter().all(|w|w["checkpoint"].is_null()));
        std::fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"provider":"echo","paidTurns":0,"modeledBudgetAndCatalogue":true,"actualEmptySessionReadAndFork":true,"noFabricatedTurnOrCheckpoint":true,"sourceStillEmpty":true,"initialFilesInChild":true,"omittedCutPoint":true,"job":awaiting,"branch":activated}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let joined = fixture.stop_hosts().await;
    fixture.editor.stop().await;
    result?;
    joined?;
    assert_eq!(
        native_git(&fixture.root.path().join("source"), &["rev-parse", "HEAD"])?,
        fixture.source_head
    );
    Ok(())
}
#[tokio::test]
#[ignore = "explicit pinned Muse offline echo; actual usage/model queries and boundary fence"]
async fn native_two_ways_reads_real_sdk_usage_and_waits_for_confirmation_on_its_own_request()
-> Result<()> {
    let mut fixture = QueueFixture::new().await?;
    let output = PathBuf::from(std::env::var("INTERCAT_M4_TWO_WAYS_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let reports =
        crate::report_analysis::Service::start(fixture.journal.clone(), fixture.editor.clone());
    let mut driver = None;
    let result=async{
        fixture.launch(&[0,1]).await?;fixture.start(0).await?;fixture.start(1).await?;fixture.wait_workers(&[0,1],true).await?;
        fixture.wait_workers(&[0,1],false).await?;
        let source=fixture.words[0].1.clone();let other=fixture.words[1].1.clone();
        let original=fixture.journal.snapshot().await?["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==source).unwrap().clone();
        let workspace=fixture.journal.workspace(source.clone()).await?;let bytes=std::fs::read(workspace.path.join("README.txt"))?;let head=native_git(&workspace.path,&["rev-parse","HEAD"])?;
        async fn word(journal:&Journal,track:&str,text:&str)->Result<Value>{
            let id=Uuid::now_v7();journal.stage_utterance(id,text.into(),Target::Lock(track.into()),None,vec![]).await?;
            let input=journal.claim_input().await?.unwrap();assert_eq!(input.id,id.to_string());
            journal.apply_input(input.id,input.revision,Outcome::Classified(Decision{kind:Kind::Extend,track_id:Some(track.into()),boundary:Some("when-told".into()),command:None,confidence:1.,inference_ms:0.,timed_out:false,scores:json!({})})).await?;
            journal.read_utterance(id.to_string()).await
        }
        let pending=word(&fixture.journal,&source,"OWN_UNMEASURED_PAIRED_REQUEST").await?;
        let job=fixture.journal.two_ways(Change::Prepare(Request{op_id:Uuid::now_v7(),input_id:pending["id"].as_str().unwrap().into(),revision:pending["revision"].as_i64().unwrap(),selection:crate::two_ways::Selection{track_id:source.clone(),session_id:original["sessionId"].as_str().unwrap().into(),variation:crate::two_ways::Variation{model_id:"unused-dummy-model-before-quota-decision".into(),provider_id:"echo".into(),primary:crate::two_ways::PRIMARY_EFFORT,alternate:muse_code_sdk::msp::ReasoningEffort::Xhigh}}})).await?;
        driver=Some(crate::two_ways_driver::Driver::start(fixture.service.clone(),fixture.journal.clone(),fixture.stop.subscribe()));
        start_at_worker_boundary(&fixture.service,&source).await?;
        let awaiting=tokio::time::timeout(Duration::from_secs(20),async{
            loop{
                let current=fixture.journal.two_ways_job(job.request.op_id).await?.unwrap();
                if current.phase==Phase::AwaitingUsageConfirmation{return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(current);}
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await??;
        assert!(awaiting.error.is_none());
        assert!(awaiting.variation.is_none());assert_eq!(serde_json::to_value(awaiting.point.as_ref().and_then(|point| point.checkpoint()))?,original["checkpoint"]);
        let db=rusqlite::Connection::open(fixture.root.path().join("db"))?;
        assert!(db.query_row("SELECT dispatch_command IS NULL FROM utterances WHERE id=?1",[&job.request.input_id],|row|row.get::<_,bool>(0))?);
        assert!(fixture.journal.snapshot().await?["branches"].as_array().unwrap().is_empty());
        let follow=word(&fixture.journal,&other,"OWN_OTHER_PATH_CAN_CONTINUE").await?;
        start_at_worker_boundary(&fixture.service,&other).await?;
        tokio::time::timeout(Duration::from_secs(20),async{
            loop{if fixture.journal.read_utterance(follow["id"].as_str().unwrap().into()).await?["state"]=="landed"{return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(());}tokio::time::sleep(Duration::from_millis(20)).await;}
        }).await??;
        assert!(!fixture.journal.snapshot().await?["dispatchPaused"].as_bool().unwrap());
        assert_eq!(fixture.journal.read_utterance(job.request.input_id.clone()).await?["state"],"placed");
        assert_eq!(std::fs::read(workspace.path.join("README.txt"))?,bytes);assert_eq!(native_git(&workspace.path,&["rev-parse","HEAD"])?,head);
        std::fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"provider":"echo","paidTurns":0,"actualUsageAndModelQueries":true,"capturedAtActorSelection":true,"unknownNotTreatedAsPermission":true,"noBranchOrOriginalDispatch":true,"otherPathContinued":true,"sourceFilesAndHeadUnchanged":true,"awaiting":awaiting}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    std::fs::write(
        output.join("last-state.json"),
        serde_json::to_vec_pretty(&fixture.journal.snapshot().await?)?,
    )?;
    if let Some(driver) = driver {
        driver.shutdown().await;
    }
    let joined = fixture.stop_hosts().await;
    reports.shutdown().await;
    fixture.editor.stop().await;
    assert_eq!(
        native_git(&fixture.root.path().join("source"), &["rev-parse", "HEAD"])?,
        fixture.source_head
    );
    result?;
    joined?;
    Ok(())
}

#[tokio::test]
#[ignore = "actual pinned SDK empty-session fork and model metadata; offline echo only"]
async fn native_empty_session_fork_and_model_metadata_before_the_first_turn() -> Result<()> {
    let output = PathBuf::from(std::env::var("INTERCAT_M4_FIRST_POINT_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let mut fixture = QueueFixture::new().await?;
    let result=async{
        fixture.launch(&[0]).await?;
        let source=fixture.words[0].1.clone();
        let state=fixture.journal.snapshot().await?["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==source).unwrap().clone();
        assert!(state["activeTurn"].is_null());assert!(state["checkpoint"].is_null());
        let session=state["sessionId"].as_str().unwrap();
        let (_,models)=fixture.service.port(&source)?.two_ways_observation(session).await?;
        let workspace=fixture.journal.workspace(source.clone()).await?;
        let bytes=std::fs::read(workspace.path.join("README.txt"))?;
        let directory=fixture.root.path().join("own-empty-fork-probe");
        let mut probe=host::OFFLINE_ECHO.scope((),host::Host::spawn(&fixture.config.binary,&directory,&workspace.path,fixture.config.path.as_ref(),Some(&fixture.config.credentials),host::Boundary{steps:None,profile:host::Profile::Worker,secrets:&[],rpc:&fixture.root.path().join("rpc"),denied:&[]})).await?;
        let inspect=async{
            probe.connection.client.initialize().await?;
            let before=probe.connection.client.request("session/read",json!({"sessionId":session,"excludeItems":false})).await?;
            let operation=Uuid::now_v7();
            let fork=probe.connection.client.request("session/fork",json!({"commandId":operation,"sessionId":session,"excludeItems":false})).await?;
            let child=fork["session"]["sessionId"].as_str().ok_or("No empty child session")?.to_owned();
            assert_ne!(child,session);
            assert_eq!(fork["session"]["forkedFrom"]["sessionId"],session);
            assert_eq!(fork["session"]["forkedFrom"]["commandId"],operation.to_string());
            assert_eq!(fork["session"]["forkedFrom"]["cutExplicit"],false);
            let replay=probe.connection.client.request("session/fork",json!({"commandId":operation,"sessionId":session,"excludeItems":false})).await?;
            assert_eq!(replay["session"]["sessionId"],child);
            probe.release_session(&child, true).await?;
            let after=probe.connection.client.request("session/read",json!({"sessionId":session,"excludeItems":false})).await?;
            assert_eq!(before["history"],after["history"]);
            std::fs::write(output.join("sdk-empty-fork.json"),serde_json::to_vec_pretty(&json!({"provider":"echo","paidTurns":0,"models":models,"before":before,"fork":fork,"after":after,"parentUnchanged":true,"forkReplaySameSession":true}))?)?;
            Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
        }.await;
        let joined=probe.shutdown().await;
        inspect?;joined?;
        assert_eq!(std::fs::read(workspace.path.join("README.txt"))?,bytes);
        assert!(fixture.journal.snapshot().await?["workers"][0]["checkpoint"].is_null());
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let joined = fixture.stop_hosts().await;
    fixture.editor.stop().await;
    result?;
    joined?;
    assert_eq!(
        native_git(&fixture.root.path().join("source"), &["rev-parse", "HEAD"])?,
        fixture.source_head
    );
    Ok(())
}

#[tokio::test]
#[ignore = "saved-auth metadata query only; no turn/start, project files or paid model input"]
async fn native_real_model_variants_for_the_two_way_entry() -> Result<()> {
    let output = PathBuf::from(std::env::var("INTERCAT_M4_MODEL_OPTIONS_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let root = tempfile::tempdir_in("/private/tmp")?;
    let workspace = root.path().join("own-workspace");
    std::fs::create_dir(&workspace)?;
    std::fs::write(
        workspace.join("README.txt"),
        "Own disposable model-metadata fixture. No model turns.\n",
    )?;
    let binary = PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_BINARY")?).canonicalize()?;
    assert_eq!(host::hash(&binary)?, host::BINARY_SHA256);
    let auth = PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?);
    let rpc = root.path().join("rpc");
    let denied = [std::env::current_dir()?];
    let mut probe = host::Host::spawn(
        &binary,
        &root.path().join("host"),
        &workspace,
        &std::env::var_os("PATH").ok_or("PATH")?,
        Some(&auth),
        host::Boundary {
            steps: None,
            profile: host::Profile::Worker,
            secrets: &[],
            rpc: &rpc,
            denied: &denied,
        },
    )
    .await?;
    let result=async{
        probe.connection.client.initialize().await?;
        let started=probe.connection.client.request("session/start",json!({"commandId":Uuid::now_v7(),"workspaceRoot":workspace,"approvalMode":"promptUnmatched","providerId":"meta","modelId":"muse-spark-1.3-contributor","recoveryMode":"on"})).await?;
        let session=started["session"]["sessionId"].as_str().ok_or("No metadata session")?.to_owned();
        assert_eq!(started["session"]["turnCount"],0);
        let mut observations=vec![];
        for _ in 0..3 {
            let models=probe.connection.client.request("model/list",json!({"sessionId":session})).await?;
            let typed:muse_code_sdk::msp::ModelListResult=serde_json::from_value(models.clone())?;
            let variation=match crate::two_ways::Variation::from_catalog(&typed){Ok(value)=>json!({"selected":value}),Err(error)=>json!({"unavailable":error.to_string()})};
            observations.push(json!({"models":models,"variation":variation}));
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let read=probe.connection.client.request("session/read",json!({"sessionId":session,"excludeItems":true})).await?;
        assert_eq!(read["session"]["turnCount"],0);
        probe.release_session(&session, true).await?;
        std::fs::write(output.join("model-options.json"),serde_json::to_vec_pretty(&json!({"paidTurns":0,"savedAuthUsedForMetadata":true,"turnCount":0,"observations":observations}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let joined = probe.shutdown().await;
    result?;
    joined?;
    Ok(())
}

#[tokio::test]
#[ignore = "saved-auth warm/cold two-way option query; no model turns or project input"]
async fn native_two_way_options_use_live_and_closed_workers_without_starting_a_turn() -> Result<()>
{
    let output = PathBuf::from(std::env::var("INTERCAT_M4_LIVE_OPTIONS_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let mut fixture = QueueFixture::new().await?;
    fixture.config.credentials = PathBuf::from(std::env::var("INTERCAT_TEST_MUSE_AUTH")?);
    fixture.service.host_memory = fixture.memory.clone();
    fixture
        .journal
        .set_worker_config(Uuid::now_v7(), fixture.config.clone())
        .await?;
    let source = fixture.words[0].1.clone();
    let (port, commands) =
        crate::execution::Port::new(Arc::new(crate::dispatch_control::Gate::new(false)));
    fixture
        .service
        .ports
        .lock()
        .unwrap()
        .insert(source.clone(), port.clone());
    let launch = Launch {
        host_memory: fixture.memory.clone(),
        config: fixture.config.clone(),
        data: fixture.root.path().into(),
        rpc: fixture.root.path().join("rpc"),
        journal: fixture.journal.clone(),
        workspace: serde_json::to_value(fixture.journal.workspace(source.clone()).await?)?,
        previous: None,
        stopping: fixture.stop.subscribe(),
        execution: crate::execution::Context {
            project_secrets: fixture.service.project_secrets(),
            port,
            commands,
            editor: fixture.editor.clone(),
            capacity: fixture.capacity.clone(),
        },
    };
    fixture.tasks.push(tokio::spawn(run_host(launch)));
    let result=async{
        fixture.wait_workers(&[0],false).await?;
        let snapshot=fixture.journal.snapshot().await?;let worker=snapshot["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==source).unwrap().clone();
        assert!(worker["checkpoint"].is_null());assert!(worker["activeTurn"].is_null());
        let workspace=fixture.journal.workspace(source.clone()).await?;let bytes=std::fs::read(workspace.path.join("README.txt"))?;
        let input=Uuid::now_v7();fixture.journal.stage_utterance(input,"Own optional second path after the first turn".into(),Target::Lock(source.clone()),None,vec![]).await?;
        let word=fixture.journal.claim_input().await?.unwrap();fixture.journal.apply_input(word.id,word.revision,Outcome::Classified(Decision{kind:Kind::Extend,track_id:Some(source.clone()),boundary:Some("when-told".into()),command:None,confidence:1.,inference_ms:0.,timed_out:false,scores:json!({})})).await?;
        let word=fixture.journal.read_utterance(input.to_string()).await?;
        let query=crate::two_ways::OptionsQuery{input_id:input.to_string(),revision:word["revision"].as_i64().unwrap()};
        let warm=fixture.service.two_ways_options(query.clone()).await?;
        assert_eq!(warm.selection.variation.primary,crate::two_ways::PRIMARY_EFFORT);
        assert_ne!(warm.selection.variation.primary,warm.selection.variation.alternate);
        fixture.stop_hosts().await?;
        let cold=fixture.service.two_ways_options(query).await?;
        assert_eq!(warm,cold);
        assert_eq!(fixture.memory.snapshot().hosts,0);
        let after=fixture.journal.snapshot().await?;
        let current=after["workers"].as_array().unwrap().iter().find(|worker|worker["trackId"]==source).unwrap();
        assert_eq!(current["sessionId"],worker["sessionId"]);assert!(current["activeTurn"].is_null());assert!(current["checkpoint"].is_null());
        let db=rusqlite::Connection::open(fixture.root.path().join("db"))?;
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'",[],|row|row.get::<_,i64>(0))?,0);
        assert_eq!(std::fs::read(workspace.path.join("README.txt"))?,bytes);
        std::fs::write(output.join("options.json"),serde_json::to_vec_pretty(&json!({"warm":warm,"cold":cold,"sameSourceSession":true,"turnStarts":0,"paidTurns":0,"metadataOnlyResumeFromColdQuery":true,"noTurnStartFromColdQuery":true,"metadataHostJoined":true,"sourceFilesUnchanged":true}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    let joined = fixture.stop_hosts().await;
    fixture.editor.stop().await;
    result?;
    joined?;
    assert_eq!(
        native_git(&fixture.root.path().join("source"), &["rev-parse", "HEAD"])?,
        fixture.source_head
    );
    Ok(())
}

#[tokio::test]
#[ignore = "real offline first turn and paired boundary selection; no paid API"]
async fn native_the_first_real_turn_finishes_before_the_next_paired_instruction_is_captured()
-> Result<()> {
    let output = PathBuf::from(std::env::var("INTERCAT_M4_FIRST_BOUNDARY_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let mut fixture = QueueFixture::new().await?;
    let reports =
        crate::report_analysis::Service::start(fixture.journal.clone(), fixture.editor.clone());
    let mut driver = None;
    let result=async{
        fixture.launch(&[0]).await?;let source=fixture.words[0].1.clone();
        fixture.start(0).await?;
        let state=fixture.journal.snapshot().await?["workers"][0].clone();assert!(state["checkpoint"].is_null());
        let id=Uuid::now_v7();fixture.journal.stage_utterance(id,"OWN_SECOND_INSTRUCTION_AFTER_FIRST_TURN".into(),Target::Lock(source.clone()),None,vec![]).await?;
        let word=fixture.journal.claim_input().await?.unwrap();fixture.journal.apply_input(word.id,word.revision,Outcome::Classified(Decision{kind:Kind::Extend,track_id:Some(source.clone()),boundary:Some("after-turn".into()),command:None,confidence:1.,inference_ms:0.,timed_out:false,scores:json!({})})).await?;
        let word=fixture.journal.read_utterance(id.to_string()).await?;
        let job=fixture.journal.two_ways(Change::Prepare(Request{op_id:Uuid::now_v7(),input_id:id.to_string(),revision:word["revision"].as_i64().unwrap(),selection:crate::two_ways::Selection{track_id:source.clone(),session_id:state["sessionId"].as_str().unwrap().into(),variation:crate::two_ways::Variation{model_id:"unused-model-before-quota-decision".into(),provider_id:"echo".into(),primary:crate::two_ways::PRIMARY_EFFORT,alternate:muse_code_sdk::msp::ReasoningEffort::Xhigh}}})).await?;
        driver=Some(crate::two_ways_driver::Driver::start(fixture.service.clone(),fixture.journal.clone(),fixture.stop.subscribe()));
        assert!(fixture.journal.snapshot().await?["workers"][0]["checkpoint"].is_null());
        let awaiting=tokio::time::timeout(Duration::from_secs(30),async{loop{let current=fixture.journal.two_ways_job(job.request.op_id).await?.unwrap();if current.phase==Phase::AwaitingUsageConfirmation{return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(current);}tokio::time::sleep(Duration::from_millis(20)).await;}}).await??;
        assert!(awaiting.error.is_none());
        assert_eq!(fixture.journal.read_utterance(fixture.words[0].0.clone()).await?["state"],"landed");
        assert_eq!(fixture.journal.read_utterance(id.to_string()).await?["state"],"placed");
        let snapshot=fixture.journal.snapshot().await?;
        assert_eq!(serde_json::to_value(awaiting.point.as_ref().and_then(|point| point.checkpoint()))?,snapshot["workers"][0]["checkpoint"]);
        let db=rusqlite::Connection::open(fixture.root.path().join("db"))?;
        assert_eq!(db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'",[],|r|r.get::<_,i64>(0))?,1);
        let turn:String=db.query_row("SELECT id FROM operations WHERE kind='turn.start'",[],|r|r.get(0))?;
        assert_eq!(awaiting.point.as_ref().unwrap().checkpoint().unwrap().turn_id.to_string(),turn);
        assert!(snapshot["branches"].as_array().unwrap().is_empty());
        std::fs::write(output.join("first-boundary.json"),serde_json::to_vec_pretty(&json!({"provider":"echo","paidTurns":0,"initialCheckpoint":null,"actualFirstTurnSaved":true,"pairedCheckpointIsFirstTurn":true,"originalFirstInstructionLanded":true,"nextInstructionNotSent":true,"catalogSelectionModeledOnlyBeforeUnknownQuotaGate":true,"awaiting":awaiting}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    if let Some(driver) = driver {
        driver.shutdown().await;
    }
    let joined = fixture.stop_hosts().await;
    reports.shutdown().await;
    fixture.editor.stop().await;
    result?;
    joined?;
    assert_eq!(
        native_git(&fixture.root.path().join("source"), &["rev-parse", "HEAD"])?,
        fixture.source_head
    );
    Ok(())
}
