use super::*;

async fn prepare_plan(fixture: &Fixture) -> Result<crate::plans::Plan> {
    fixture
        .journal
        .configure_project(
            Uuid::now_v7(),
            crate::project::inspect(
                Uuid::now_v7(),
                "Dummy capacity project".into(),
                vec![fixture.workspace.path.clone()],
            )
            .await?,
        )
        .await?;
    fixture
        .journal
        .confirm_fingerprints(fixture.journal.fingerprint_catalog().await?.current)
        .await?;
    let project: crate::project::Project =
        serde_json::from_value(fixture.journal.snapshot().await?["project"].clone())?;
    let db = rusqlite::Connection::open(fixture.root.path().join("db"))?;
    crate::folders::bind(&db, &fixture.workspace.track_id, &project.folders[0].id())?;
    fixture.journal.stage_document(crate::request_sources::Submission {
        op_id:Uuid::now_v7(),text:"Plan exactly two independent dummy jobs: correct the double function in math.rs, and add a test for its result. Use separate new tracks. Do not implement the jobs.".into(),
        target:crate::decision::Target::New,selected_track_id:None,attachment_ids:vec![],sources:None,execute_allowed:false,plan_item:None,
    }).await?;
    let plan = fixture
        .journal
        .plans()
        .await?
        .pop()
        .ok_or("Missing dummy plan")?;
    Ok(plan)
}

#[tokio::test]
async fn planner_context_keeps_worker_clips_and_excludes_the_reviewer_lane() -> Result<()> {
    let fixture = Fixture::new().await?;
    let job = fixture.begin().await?;
    let plan = prepare_plan(&fixture).await?;
    let context = fixture.journal.plan_context(plan.id).await?;
    assert!(context.external.contains_key(&job.target.clips[0].id));
    assert!(
        context
            .external
            .values()
            .all(|clip| clip.track_id == fixture.workspace.track_id)
    );
    let db = rusqlite::Connection::open(fixture.root.path().join("db"))?;
    let review_clip: String =
        db.query_row("SELECT clip FROM review_clips", [], |row| row.get(0))?;
    assert!(!context.external.contains_key(&review_clip));
    Ok(())
}

#[tokio::test]
#[ignore = "three paid dummy reads: one Planner and two Reviewers share a limit reduced during execution"]
async fn native_planner_and_reviewers_share_the_limit_and_keep_the_third_read_queued() -> Result<()>
{
    let first = Fixture::new().await?;
    let second = Fixture::new().await?;
    let plan = prepare_plan(&first).await?;
    let binary = PathBuf::from(std::env::var("INTERCAT_MUSE_BINARY")?);
    let config = Config {
        source: binary.clone(),
        binary,
        credentials: std::env::var("INTERCAT_MUSE_AUTH")?.into(),
        path: std::env::var("PATH")?,
    };
    for fixture in [&first, &second] {
        fixture
            .journal
            .set_worker_config(Uuid::now_v7(), config.clone())
            .await?;
        std::fs::create_dir(fixture.root.path().join("rpc"))?;
    }
    let pool = std::sync::Arc::new(crate::scheduler::Capacity::new(2));
    let paused = std::sync::Arc::new(crate::dispatch_control::Gate::new(false));
    let first_reviews = crate::reviewing::Service::start(crate::host_memory::Pool::new(),
        first.journal.clone(),
        first.root.path().into(),
        first.root.path().join("rpc"),
        pool.clone(),
        paused.clone(),
    );
    let second_reviews = crate::reviewing::Service::start(crate::host_memory::Pool::new(),
        second.journal.clone(),
        second.root.path().into(),
        second.root.path().join("rpc"),
        pool.clone(),
        paused.clone(),
    );
    let plans = crate::planning::Service::start(crate::host_memory::Pool::new(),
        first.journal.clone(),
        first.root.path().into(),
        first.root.path().join("rpc"),
        pool.clone(),
        paused,
    );
    let result=tokio::time::timeout(Duration::from_secs(180),async {
        let mut queue_seen=false;
        let mut queued_id=None;
        let mut peak=0;
        let mut previous=String::new();
        loop {
            let capacity=pool.snapshot();
            peak=peak.max(capacity.active);
            assert!(capacity.active<=2);
            let left=first.journal.reviews().await?.pop().unwrap();
            let right=second.journal.reviews().await?.pop().unwrap();
            let current=first.journal.plans().await?.into_iter().find(|p|p.id==plan.id).unwrap();
            let progress=format!("active={}, waiting={}, left={:?}, right={:?}, planner={:?}",capacity.active,capacity.waiting.len(),left.state,right.state,current.generation.state);
            if previous!=progress {eprintln!("{progress}");previous=progress;}
            let states=[(left.generation.to_string(),left.state==State::Running),(right.generation.to_string(),right.state==State::Running),(current.source_id.clone(),current.generation.state==crate::plans::GenerationState::Running)];
            if !queue_seen && capacity.active==2 && capacity.waiting.len()==1 {
                let waiting=capacity.waiting[0].id.clone();
                assert!(!states.iter().find(|(id,_)|id==&waiting).ok_or("Unknown queued read")?.1);
                let host_path=if waiting==left.generation.to_string() {first.root.path().join("review-hosts").join(&waiting)} else if waiting==right.generation.to_string() {second.root.path().join("review-hosts").join(&waiting)} else {first.root.path().join("planner-hosts").join(current.generation.id.to_string())};
                assert!(!host_path.exists(),"The third read spawned a host before receiving a slot");
                queue_seen=true;queued_id=Some(waiting);
                pool.set_limit(1,1);
            }
            if let Some(waiting)=&queued_id && states.iter().any(|(id,running)|id==waiting && *running) {
                assert!(states.iter().filter(|(id,_)|id!=waiting).all(|(_,running)|!*running),"Shrinking admitted a third read before the original two finished");
            }
            if matches!(left.state,State::Failed|State::Held)||matches!(right.state,State::Failed|State::Held)||matches!(current.generation.state,crate::plans::GenerationState::Failed|crate::plans::GenerationState::Invalid|crate::plans::GenerationState::TimedOut|crate::plans::GenerationState::UnlockRequired) {
                return Err(format!("A dummy reader failed: {:?}, {:?}, {:?}",left.detail,right.detail,current.generation.detail).into());
            }
            if left.state==State::Complete && right.state==State::Complete && current.generation.state==crate::plans::GenerationState::Ready {
                assert!(queue_seen);assert_eq!(peak,2);
                return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(json!({"schema":crate::journal::SCHEMA_VERSION,"peakActive":peak,"queuedBeforeHost":true,"shrankWithoutInterrupting":true,"completedReads":3,"paidTurns":3,"source":"agent-created dummy code only"}));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }).await;
    plans.shutdown().await;
    first_reviews.shutdown().await;
    second_reviews.shutdown().await;
    assert_eq!(pool.snapshot().active, 0);
    assert!(pool.snapshot().waiting.is_empty());
    let value = result??;
    let output = PathBuf::from(std::env::var("INTERCAT_CAPACITY_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    std::fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(&value)?,
    )?;
    Ok(())
}
