//! Real provider admission and Stop for the Fresh caller, using only own data.
use super::*;
use crate::{
    handoff,
    host::{Host, Profile},
    runtime::Config,
};
use tokio::{sync::watch, task::JoinHandle};

#[tokio::test]
#[ignore = "one paid dummy Wonder turn; saved auth and INTERCAT_FRESH_SUMMARY_OUTPUT"]
async fn real_fresh_cancel_stops_the_active_summary_without_pausing_other_work() -> Result<()> {
    let f = Fixture::new(1).await?;
    let output = PathBuf::from(std::env::var("INTERCAT_FRESH_SUMMARY_OUTPUT")?);
    fs::create_dir_all(&output)?;
    let files = snapshot(&f).await?;
    let job = f
        .journal
        .fresh(Change::PrepareSummary {
            id: files.request.op_id,
            revision: files.revision,
            changes: crate::fresh_summary::changes(&files).await?,
        })
        .await?;
    let queued = future(&f).await?;
    let config = Config {
        source: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
        binary: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
        credentials: std::env::var("INTERCAT_TEST_MUSE_AUTH")?.into(),
        path: std::env::var("PATH")?,
    };
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let workspace = f.root.path().join("own-cancel-observer-workspace");
    fs::create_dir(&workspace)?;
    fs::create_dir(f.root.path().join("rpc"))?;
    let memory = crate::host_memory::Pool::fixed(crate::host_memory::Sample {
        free_bytes: 8 * 1024 * 1024 * 1024,
        pressured: false,
    });
    let capacity = Arc::new(crate::scheduler::Capacity::new(2));
    let mut ticket = capacity.request(
        "own-unrelated-work".into(),
        crate::scheduler::Priority::User,
    );
    let unrelated = ticket.try_acquire().unwrap();
    let gate = Arc::new(crate::dispatch_control::Gate::new(false));
    let (_shutdown, stopping) = watch::channel(false);
    let mut observer = None;
    let mut observer_identity = None;
    let mut running = None;
    let result = async {
        let host = Host::spawn(&config.binary,&f.root.path().join("own-cancel-observer-host"),
            &workspace,config.path.as_ref(),Some(&config.credentials),crate::host::Boundary {
                steps:None,profile:Profile::Summary,secrets:&[],rpc:&f.root.path().join("rpc"),denied:&[],
            }).await?;
        observer_identity = Some(host.identity);
        observer = Some(host);
        observer.as_ref().unwrap().connection.client.initialize().await?;
        running = Some(start(&f,job.clone(),config.clone(),memory.clone(),capacity.clone(),gate.clone(),stopping));
        let (task,before) = live_turn(&db,observer.as_ref().unwrap(),running.as_ref().unwrap(),job.summary_id.unwrap()).await?;
        if capacity.snapshot().active!=2 || memory.snapshot().hosts!=1 {
            return Err("Own summary did not hold its slot and host beside unrelated work".into());
        }
        fs::write(output.join("before-cancel.json"),serde_json::to_vec_pretty(&before)?)?;
        let cancel = Uuid::now_v7();
        let pending = f.journal.fresh(Change::Cancel {op_id:cancel,id:job.request.op_id,revision:job.revision}).await?;
        let cancelled = tokio::time::timeout(Duration::from_secs(20),running.as_mut().unwrap()).await?;
        running.take();
        let error = cancelled?.unwrap_err();
        assert!(error.is::<crate::msp::StartCancelled>());
        assert_eq!(pending.phase,Phase::Cancelling);
        assert!(!gate.paused());
        assert_eq!(capacity.snapshot().active,1);
        assert_eq!(memory.snapshot().hosts,0);
        assert!(crate::fresh_store::holds(&db,&job.request.track_id)?);
        assert_eq!(f.journal.operation(cancel).await?.unwrap()["state"],"prepared");
        assert!(f.journal.fresh(Change::Summarized {id:job.request.op_id,revision:job.revision,document:document(&job)}).await.is_err());
        let stale = f.journal.fresh(Change::Failed {id:job.request.op_id,revision:job.revision,error:error.to_string()}).await?;
        assert_eq!(stale.phase,Phase::Cancelling);
        let after = observer.as_ref().unwrap().connection.client.request("view/page",
            json!({"sessionId":task.session_id,"direction":"forward","limit":200,"maxBytes":512*1024})).await?;
        let events = after["events"].as_array().unwrap();
        assert_eq!(events.iter().filter(|frame|frame["method"]=="turn/started").count(),1);
        assert!(events.iter().any(|frame|frame["method"]=="turn/completed" && frame["params"]["terminal"]=="cancelled"));
        fs::write(output.join("after-cancel.json"),serde_json::to_vec_pretty(&after)?)?;
        let done = crate::fresh_cancel::complete(f.root.path(),&f.journal,&pending).await?;
        assert_eq!(done.phase,Phase::Cancelled);
        assert_eq!(f.journal.operation(cancel).await?.unwrap()["state"],"complete");
        assert!(!crate::fresh_store::holds(&db,&job.request.track_id)?);
        assert!(!crate::continuation::closed(&db,&job.request.track_id)?);
        assert_eq!(f.journal.read_utterance(queued["id"].as_str().unwrap().into()).await?["state"],"held");
        let later = future(&f).await?;
        assert_eq!(later["dispatchRunnable"],true);
        assert_eq!(db.query_row("SELECT count(*) FROM tracks WHERE id=?1",[&job.successor_track],|row|row.get::<_,i64>(0))?,0);
        fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({
            "paidTurns":1,"receivedTurnCancelled":true,"globalPauseUnchanged":true,
            "unrelatedSlotStillActive":true,"memoryReturned":true,"oldWordHeld":true,
            "laterWordAllowed":true,"noWriterTransferred":true,"cancelOperationCompleted":true,
            "job":done.status(),
        }))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    gate.begin(true);
    if let Some(mut task) = running
        && tokio::time::timeout(Duration::from_secs(20), &mut task)
            .await
            .is_err()
    {
        task.abort();
        let _ = task.await;
    }
    let stopped = if let Some(host) = observer {
        host.shutdown().await.map(|_| ())
    } else {
        Ok(())
    };
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    if let Err(error) = &result {
        fs::write(
            output.join("failure.json"),
            serde_json::to_vec_pretty(
                &json!({"error":error.to_string(),"capacity":capacity.snapshot(),"memory":memory.snapshot()}),
            )?,
        )?;
    }
    drop(unrelated);
    stopped?;
    assert!(observer_identity.is_none_or(|identity| !crate::process::is_alive(identity)));
    assert_eq!(capacity.snapshot().active, 0);
    result
}

fn start(
    f: &Fixture,
    job: crate::fresh::Job,
    config: Config,
    memory: Arc<crate::host_memory::Pool>,
    capacity: Arc<crate::scheduler::Capacity>,
    gate: Arc<crate::dispatch_control::Gate>,
    stopping: watch::Receiver<bool>,
) -> JoinHandle<Result<handoff::Document>> {
    let journal = f.journal.clone();
    let data = f.root.path().to_owned();
    tokio::spawn(async move {
        crate::fresh_summary::run(
            &journal,
            &job,
            &config,
            &data,
            &data.join("rpc"),
            memory,
            capacity,
            gate,
            stopping,
        )
        .await
    })
}

async fn live_turn(
    db: &rusqlite::Connection,
    observer: &Host,
    running: &JoinHandle<Result<handoff::Document>>,
    summary: Uuid,
) -> Result<(handoff::Task, serde_json::Value)> {
    tokio::time::timeout(Duration::from_secs(40), async {
            loop {
                if running.is_finished() {
                    return Err("Own summary finished before a live received turn was observed".into());
                }
                let tasks = db.prepare("SELECT payload FROM operations WHERE kind='handoff.summary' AND json_extract(payload,'$.handoffId')=?1 ORDER BY rowid")?
                    .query_map([summary.to_string()], |row|row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for raw in tasks {
                    let task: handoff::Task = serde_json::from_str(&raw)?;
                    if task.state != handoff::State::Running { continue; }
                    let request = json!({"sessionId":task.session_id,"direction":"forward","limit":200,"maxBytes":512*1024});
                    if let Ok(page) = observer.connection.client.request("view/page", request).await {
                        let events = page["events"].as_array().ok_or("Own summary page has no events")?;
                        let started = events.iter().find(|frame| frame["method"] == "turn/started");
                        if let Some(started) = started
                            && !events.iter().any(|frame| frame["method"] == "turn/completed" && frame["params"]["turnId"] == started["params"]["turnId"])
                        {
                            return Ok::<_, Box<dyn std::error::Error + Send + Sync>>((task, page));
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await?
}

#[tokio::test]
#[ignore = "bounded paid dummy Wonder turns; saved auth, Gitleaks and INTERCAT_FRESH_SUMMARY_OUTPUT"]
async fn real_fresh_summary_stop_observes_a_received_turn_and_retry_preserves_the_frozen_input()
-> Result<()> {
    let f = Fixture::new(1).await?;
    let output = PathBuf::from(std::env::var("INTERCAT_FRESH_SUMMARY_OUTPUT")?);
    fs::create_dir_all(&output)?;
    let files = snapshot(&f).await?;
    let db = rusqlite::Connection::open(f.root.path().join("events.sqlite"))?;
    let mut source: WorkerState = serde_json::from_str(&db.query_row(
        "SELECT record FROM workers WHERE track_id=?1",
        [&files.request.track_id],
        |row| row.get::<_, String>(0),
    )?)?;
    source.output = Some(format!(
        "Own dummy file inspection. No checks have run. An earlier BLUE_OLD suggestion was withdrawn.\n{}\nLATEST USER CORRECTION: GREEN_ONLY. Preserve this correction. No file edits are requested.",
        "Own historical observation: an empty dummy folder was inspected; no tests were run.\n"
            .repeat(250)
    ));
    f.journal.worker_state(source).await?;
    let job = f
        .journal
        .fresh(Change::PrepareSummary {
            id: files.request.op_id,
            revision: files.revision,
            changes: crate::fresh_summary::changes(&files).await?,
        })
        .await?;
    assert_eq!(job.summary_id.unwrap().get_version_num(), 5);
    let config = Config {
        source: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
        binary: std::env::var("INTERCAT_TEST_MUSE_BINARY")?.into(),
        credentials: std::env::var("INTERCAT_TEST_MUSE_AUTH")?.into(),
        path: std::env::var("PATH")?,
    };
    let observer_workspace = f.root.path().join("own-observer-workspace");
    fs::create_dir(&observer_workspace)?;
    fs::create_dir(f.root.path().join("rpc"))?;
    let memory = crate::host_memory::Pool::fixed(crate::host_memory::Sample {
        free_bytes: 8 * 1024 * 1024 * 1024,
        pressured: false,
    });
    let capacity = Arc::new(crate::scheduler::Capacity::new(1));
    let gate = Arc::new(crate::dispatch_control::Gate::new(false));
    let (_shutdown, stopping) = watch::channel(false);
    let mut observer: Option<Host> = None;
    let mut running = None;
    let mut before_stop = None;
    let mut observer_identity = None;
    let final_file = fs::read(job.workspace.path.join("own-final.txt"))?;
    let result = async {
        let host = Host::spawn(&config.binary, &f.root.path().join("own-observer-host"),
            &observer_workspace, config.path.as_ref(), Some(&config.credentials), crate::host::Boundary {
                steps: None, profile: Profile::Summary, secrets: &[],
                rpc: &f.root.path().join("rpc"), denied: &[],
            }).await?;
        observer_identity = Some(host.identity);
        observer = Some(host);
        observer.as_ref().unwrap().connection.client.initialize().await?;
        running = Some(start(&f, job.clone(), config.clone(), memory.clone(), capacity.clone(), gate.clone(), stopping.clone()));
        let (task, page) = live_turn(&db, observer.as_ref().unwrap(), running.as_ref().unwrap(), job.summary_id.unwrap()).await?;
        before_stop = Some(page.clone());
        if capacity.snapshot().active != 1 || memory.snapshot().hosts != 1 {
            return Err("The live summary did not own exactly one slot and host".into());
        }
        fs::write(output.join("received-before-stop.json"), serde_json::to_vec_pretty(&page)?)?;
        gate.begin(true);
        f.journal.pause_dispatch(Uuid::now_v7(), true).await?;
        let stopped = tokio::time::timeout(Duration::from_secs(20), running.as_mut().unwrap()).await?;
        running.take();
        let stopped = stopped?;
        let error = stopped.unwrap_err();
        assert!(error.is::<crate::msp::StartCancelled>());
        assert_eq!(capacity.snapshot().active, 0);
        assert!(capacity.snapshot().waiting.is_empty());
        assert_eq!(memory.snapshot().hosts, 0);
        let held = f.journal.fresh(Change::Failed {
            id: job.request.op_id, revision: job.revision, error: error.to_string(),
        }).await?;
        assert_eq!(held.phase, Phase::SummaryHeld);
        assert!(held.summary.is_none());
        assert!(f.journal.fresh(Change::Summarized {
            id: job.request.op_id, revision: job.revision, document: document(&job),
        }).await.is_err());
        let after = observer.as_ref().unwrap().connection.client.request("view/page",
            json!({"sessionId":task.session_id,"direction":"forward","limit":200,"maxBytes":512*1024})).await?;
        fs::write(output.join("received-after-stop.json"), serde_json::to_vec_pretty(&after)?)?;
        let resume = gate.begin(false);
        assert!(gate.resume(resume));
        f.journal.pause_dispatch(Uuid::now_v7(), false).await?;
        let retry = f.journal.fresh(Change::Retry {
            op_id: Uuid::now_v7(), id: held.request.op_id, revision: held.revision,
        }).await?;
        assert_eq!(retry.summary_id, job.summary_id);
        assert_eq!(retry.summary_input, job.summary_input);
        running = Some(start(&f, retry.clone(), config.clone(), memory.clone(), capacity.clone(), gate.clone(), stopping.clone()));
        let retried = tokio::time::timeout(Duration::from_secs(90), running.as_mut().unwrap()).await?;
        running.take();
        let retried = retried?;
        let summary = retried?;
        let saved = f.journal.fresh(Change::Summarized {
            id: retry.request.op_id, revision: retry.revision, document: summary,
        }).await?;
        assert_eq!(saved.phase, Phase::SummaryReady);
        assert_eq!(capacity.snapshot().active, 0);
        assert_eq!(memory.snapshot().hosts, 0);
        assert_eq!(fs::read(job.workspace.path.join("own-final.txt"))?, final_file);
        let tasks = db.prepare("SELECT payload FROM operations WHERE kind='handoff.summary' AND json_extract(payload,'$.handoffId')=?1 ORDER BY rowid")?
            .query_map([job.summary_id.unwrap().to_string()], |row|row.get::<_, String>(0))?
            .map(|row| Ok(serde_json::from_str::<handoff::Task>(&row?)?))
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].state, handoff::State::Failed);
        assert_eq!(tasks[1].state, handoff::State::Complete);
        assert_eq!(tasks[0].input, tasks[1].input);
        assert_ne!(tasks[0].session_id, tasks[1].session_id);
        let mut started = 0;
        for task in &tasks {
            let page = observer.as_ref().unwrap().connection.client.request("view/page",
                json!({"sessionId":task.session_id,"direction":"forward","limit":200,"maxBytes":512*1024})).await?;
            let count = page["events"].as_array().unwrap().iter()
                .filter(|frame|frame["method"] == "turn/started").count();
            assert_eq!(count, 1);
            started += count;
            fs::write(output.join(format!("task-{}-events.json",task.attempt)), serde_json::to_vec_pretty(&page)?)?;
        }
        fs::write(output.join("result.json"), serde_json::to_vec_pretty(&json!({
            "liveTurnObservedBeforeStop":true,"stopReturnedCancelled":true,
            "capacityReturned":true,"memoryReturned":true,"staleResultRejected":true,
            "sameFrozenInputOnRetry":true,"summaryReadyAfterRetry":true,
            "noFileChanges":true,"job":saved.status(),"firstTask":task,
            "paidTurns":started,"oldAttemptRetained":true,"retryUsesNewSession":true,
        }))?)?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }.await;
    // Stop and join every owned caller even when an observation or assertion fails.
    gate.begin(true);
    if let Some(mut task) = running
        && tokio::time::timeout(Duration::from_secs(20), &mut task)
            .await
            .is_err()
    {
        task.abort();
        let _ = task.await;
    }
    let observer_stopped = if let Some(host) = observer {
        host.shutdown().await.map(|_| ())
    } else {
        Ok(())
    };
    db.execute(
        "VACUUM INTO ?1",
        [output.join("events.sqlite").to_str().unwrap()],
    )?;
    if let Err(error) = &result {
        fs::write(
            output.join("failure.json"),
            serde_json::to_vec_pretty(&json!({
                "error":error.to_string(),"beforeStop":before_stop,
                "capacity":capacity.snapshot(),"memory":memory.snapshot(),
                "job":f.journal.fresh_job(job.request.op_id).await?.map(|job|job.status()),
            }))?,
        )?;
    }
    observer_stopped?;
    assert!(observer_identity.is_none_or(|identity| !crate::process::is_alive(identity)));
    result
}
