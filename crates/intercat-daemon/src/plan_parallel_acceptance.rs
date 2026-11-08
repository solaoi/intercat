use super::*;

#[tokio::test]
#[ignore = "three paid dummy workers: two independent parents and one child reading both saved trees"]
async fn real_two_parent_plan_waits_for_both_saved_results_before_starting_its_child() -> Result<()>
{
    let QueuedFixture {
        root,
        journal,
        plan,
        folder,
        ..
    } = queued_fixture(
        None,
        false,
        "Three dummy jobs for parallel inheritance acceptance.",
    )
    .await?;
    let item = |id: &str, track: &str, instruction: &str, after: Vec<crate::plans::Dependency>| {
        DraftItem {
        id:id.into(),track_ref:track.into(),folder_id:folder.clone(),title:format!("Dummy {id}"),instruction:instruction.into(),
        constraints:vec!["Use only file read/write tools inside the primary workspace. Do not run commands, use network, create commits, spawn agents, or ask questions.".into()],
        done_when:format!("{id}.txt exists with the specified dummy text. End with Result: done when verified."),after,
    }
    };
    let draft = Draft {
        summary: "Two independent dummy parents and their joined result".into(),
        assumptions: vec![],
        conflicts: vec![],
        items: vec![
            item(
                "a",
                "new:1",
                "Create a.txt containing exactly DUMMY_A followed by a newline.",
                vec![],
            ),
            item(
                "b",
                "new:2",
                "Create b.txt containing exactly DUMMY_B followed by a newline.",
                vec![],
            ),
            item(
                "c",
                "new:3",
                "Read a.txt and b.txt from your primary workspace. Verify that their contents are DUMMY_A and DUMMY_B respectively. If either file is missing or different, report Result: failed and do not create c.txt. Otherwise preserve both files and create c.txt containing exactly DUMMY_A+DUMMY_B followed by a newline.",
                vec![
                    crate::plans::Dependency {
                        id: "a".into(),
                        inherit: true,
                    },
                    crate::plans::Dependency {
                        id: "b".into(),
                        inherit: true,
                    },
                ],
            ),
        ],
    };
    journal
        .begin_plan(plan.id, plan.generation.id, plan.rev)
        .await?;
    assert!(
        journal
            .generated_plan(
                plan.id,
                plan.generation.id,
                plan.rev,
                Generated::Draft(draft),
                vec![],
            )
            .await?
    );
    let plan = journal
        .plans()
        .await?
        .into_iter()
        .find(|current| current.id == plan.id)
        .unwrap();
    let binary = std::path::PathBuf::from(std::env::var("INTERCAT_MUSE_BINARY")?);
    journal
        .set_worker_config(
            Uuid::now_v7(),
            crate::runtime::Config {
                source: binary.clone(),
                binary,
                credentials: std::env::var("INTERCAT_MUSE_AUTH")?.into(),
                path: std::env::var("PATH")?,
            },
        )
        .await?;
    let rpc = root.path().join("rpc");
    std::fs::create_dir(&rpc)?;
    let editor = ready_editor(root.path()).await?;
    let preparation =
        crate::preparation::Preparation::start(&root.path().join("workspaces"), journal.clone())?;
    let gate = std::sync::Arc::new(crate::dispatch_control::Gate::new(false));
    let runtime = crate::runtime::Runtime::start(crate::host_memory::Pool::new(),
        root.path().into(),
        rpc.clone(),
        journal.clone(),
        editor.clone(),
        std::sync::Arc::new(crate::scheduler::Capacity::new(2)),
        gate.clone(),
    );
    let project_id = journal.snapshot().await?["projectSecrets"]["projectId"]
        .as_str()
        .unwrap()
        .to_owned();
    journal
        .confirm_fingerprints(journal.fingerprint_catalog().await?.current)
        .await?;
    let key = Key::new([7; 32]);
    let catalog = journal
        .prepare_project_secrets(crate::project_secrets::Prepare {
            op_id: Uuid::now_v7(),
            project_id: project_id.clone(),
            revision: 0,
            fingerprints: vec![],
            key_check: key.check().iter().map(|b| format!("{b:02x}")).collect(),
        })
        .await?;
    journal.confirm_fingerprints(catalog.current).await?;
    runtime
        .service
        .activate_project_secrets(crate::project_secrets::Load {
            project_id,
            revision: 0,
            entries: vec![],
        })
        .await?;
    let planning = crate::planning::Service::start(crate::host_memory::Pool::new(),
        journal.clone(),
        root.path().into(),
        rpc,
        runtime.service.assistant_pool(),
        gate,
    );
    let result=async {
        journal.plan_command(command(&plan,Action::Review)).await?;
        journal.plan_command(command(&plan,Action::Start)).await?;
        let mut dependency_at_start=None;
        let mut prior=String::new();
        let completed=tokio::time::timeout(std::time::Duration::from_secs(180),async {
            loop {
                let snapshot=journal.snapshot().await?;
                for worker in snapshot["workers"].as_array().into_iter().flatten() {
                    if worker["approvals"].as_array().is_some_and(|a|!a.is_empty()) {return Err("The dummy file-only worker unexpectedly requested command approval.".into());}
                    if matches!(worker["phase"].as_str(),Some("lost"|"failed"|"needs-recovery")) {return Err(format!("The dummy worker stopped: {}",worker["detail"]).into());}
                }
                for report in snapshot["reports"].as_array().into_iter().flatten() {
                    journal.analyze_report(report["id"].as_str().unwrap().into(),report["revision"].as_i64().unwrap(),crate::reports::Analysis::Clear).await?;
                }
                let current=journal.plans().await?.into_iter().find(|p|p.id==plan.id).unwrap();
                let state=format!("{:?}",current.items.iter().map(|i|&i.state).collect::<Vec<_>>());
                if state!=prior {eprintln!("{state}");prior=state;}
                if current.items[..2].iter().any(|i|i.state!=ItemState::Succeeded) {assert!(current.items[2].utterance_id.is_none());}
                if current.items[2].utterance_id.is_some() && dependency_at_start.is_none() {
                    let mut parents=Vec::new();
                    for item in &current.items[..2] {
                        let origin=journal.read_input_origin(item.utterance_id.unwrap()).await?.ok_or("Missing parent result")?;
                        assert!(origin.checkpoint.is_some());
                        parents.push(json!({"itemId":item.plan_item_id,"turn":origin.identity.turn_id,"checkpoint":origin.checkpoint}));
                    }
                    dependency_at_start=Some(parents);
                }
                if current.items.iter().any(|i|i.state==ItemState::Failed) {return Err(format!("A dummy plan item failed: {current:?}").into());}
                if current.state==State::Done {return Ok::<_,Box<dyn std::error::Error+Send+Sync>>(current);}
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }).await??;
        let child=journal.workspace(completed.items[2].track_ref.clone()).await?;
        for (path,expected) in [("a.txt","DUMMY_A"),("b.txt","DUMMY_B"),("c.txt","DUMMY_A+DUMMY_B")] {
            assert_eq!(std::fs::read_to_string(child.path.join(path))?.trim(),expected);
        }
        let db=Connection::open(root.path().join("db"))?;
        let basis=crate::lineage_store::basis(&db,&child.track_id)?;
        for parent in &completed.items[..2] {
            let origin=journal.read_input_origin(parent.utterance_id.unwrap()).await?.unwrap();
            let point=crate::lineage_store::point(&db,&parent.track_ref,origin.checkpoint.as_ref().unwrap())?;
            assert_eq!(basis.inherits[&parent.track_ref],point.inherited());
        }
        let starts:i64=db.query_row("SELECT count(*) FROM operations WHERE kind='turn.start'",[],|r|r.get(0))?;
        assert_eq!(starts,3);
        let output=std::path::PathBuf::from(std::env::var("INTERCAT_PARALLEL_OUTPUT")?);
        std::fs::create_dir_all(&output)?;
        std::fs::write(output.join("result.json"),serde_json::to_vec_pretty(&json!({"schema":crate::journal::SCHEMA_VERSION,"plan":completed,"parentsAtChildStart":dependency_at_start,"childBasis":basis,"paidTurns":starts,"bothSavedParentsRead":true,"source":"agent-created dummy only","editor":"local deterministic question extractor"}))?)?;
        Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
    }.await;
    planning.shutdown().await;
    runtime.shutdown().await;
    preparation.shutdown().await;
    editor.stop().await;
    result
}
