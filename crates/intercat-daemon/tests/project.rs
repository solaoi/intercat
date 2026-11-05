use intercat_daemon::{decision::Target, journal::Journal};

fn git(path: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("/usr/bin/git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[tokio::test]
async fn project_and_initial_session_survive_replay_without_changing_the_starting_point() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("events.sqlite");
    let journal = Journal::open(&database, intercat_daemon::secrets::Key::new([7; 32]))
        .await
        .unwrap();
    let op = uuid::Uuid::now_v7();
    let path = directory.path().join("project");
    std::fs::create_dir(&path).unwrap();
    assert!(
        std::process::Command::new("/usr/bin/git")
            .args(["init", "-q", "-b", "main"])
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    let project = intercat_daemon::project::inspect(path).await.unwrap();
    journal
        .configure_project(op, project.clone())
        .await
        .unwrap();
    journal
        .configure_project(op, project.clone())
        .await
        .unwrap();
    let mut other = project.clone();
    other.base = "2".repeat(40);
    assert!(journal.configure_project(op, other.clone()).await.is_err());
    assert!(
        journal
            .configure_project(uuid::Uuid::now_v7(), other)
            .await
            .is_err()
    );
    let first = uuid::Uuid::now_v7();
    journal
        .stage_utterance(
            first,
            "Keep the initial session name".into(),
            Target::New,
            None,
            Vec::new(),
        )
        .await
        .unwrap();
    let initial = journal.snapshot().await.unwrap();
    let session = initial["session"].clone();
    assert_eq!(session["name"], "Keep the initial session name");
    assert!(session["slug"].as_str().unwrap().starts_with("s-"));
    journal
        .stage_utterance(
            uuid::Uuid::now_v7(),
            "A later input".into(),
            Target::None,
            None,
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(journal.snapshot().await.unwrap()["session"], session);
    drop(journal);
    let restored = Journal::open(database, intercat_daemon::secrets::Key::new([7; 32]))
        .await
        .unwrap()
        .snapshot()
        .await
        .unwrap();
    assert_eq!(restored["session"], session);
    assert_eq!(restored["project"]["base"], project.base);
}

#[tokio::test]
async fn interrupted_project_selection_recovers_before_preparing_an_empty_worker()
-> intercat_daemon::Result<()> {
    use intercat_daemon::{
        decision::{Decision, Kind, Outcome},
        preparation::Preparation,
    };
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    std::fs::create_dir(&source)?;
    git(&source, &["init", "-q", "-b", "main"]);
    let project = intercat_daemon::project::inspect(source.clone()).await?;
    assert!(project.base_branch.is_none());
    let path = root.path().join("db");
    let id = uuid::Uuid::now_v7();
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    journal.prepare_project(id, project.clone()).await?;
    assert!(journal.snapshot().await?["project"].is_null());
    assert!(
        journal
            .prepare_project(uuid::Uuid::now_v7(), project.clone())
            .await
            .is_err()
    );
    // A restart can leave only the durable DB intent while Git has pruned the
    // unreferenced root. The saved root is deterministic and must be restored.
    git(&source, &["gc", "--prune=now"]);
    assert!(
        intercat_git::Repository::open(&source)?
            .commit_details(project.base.parse()?)
            .is_err()
    );
    project.pin().await?;
    let reference = format!("refs/intercat/starting-points/{}", project.base);
    let repo = intercat_git::Repository::open(&source)?;
    repo.delete_ref(&reference, project.base.parse()?)?;
    let wrong = repo.write_blob(b"foreign")?;
    repo.compare_and_swap(&reference, None, wrong)?;
    assert!(
        journal
            .configure_project(id, project.clone())
            .await
            .is_err()
    );
    assert!(journal.snapshot().await?["project"].is_null());
    repo.delete_ref(&reference, wrong)?;
    // Reproduce a crash after publishing the ref but before confirming the DB.
    project.pin().await?;
    drop(journal);
    let journal = Journal::open(&path, intercat_daemon::secrets::Key::new([7; 32])).await?;
    journal
        .stage_utterance(
            uuid::Uuid::now_v7(),
            "Start an empty project".into(),
            Target::New,
            None,
            vec![],
        )
        .await?;
    let input = journal.claim_input().await?.unwrap();
    journal
        .apply_input(
            input.id,
            input.revision,
            Outcome::Classified(Decision {
                kind: Kind::Fork,
                track_id: None,
                boundary: Some("when-told".into()),
                command: None,
                confidence: 1.,
                inference_ms: 1.,
                timed_out: false,
                scores: serde_json::json!({}),
            }),
        )
        .await?;
    let preparation = Preparation::start(&root.path().join("workspaces"), journal.clone())?;
    let state = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let state = journal.snapshot().await?;
            if state["workspaces"][0]["state"] == "failed" {
                return Err::<serde_json::Value, Box<dyn std::error::Error + Send + Sync>>(
                    state["workspaces"][0]["error"].to_string().into(),
                );
            }
            if state["workspaces"][0]["state"] == "ready" {
                return Ok(state);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await??;
    preparation.shutdown().await;
    assert_eq!(state["project"]["base"], project.base);
    assert_eq!(journal.operation(id).await?.unwrap()["state"], "complete");
    assert_eq!(repo.reference_target("refs/heads/main")?, None);
    assert_eq!(
        repo.reference_target(&reference)?.unwrap().to_string(),
        project.base
    );
    git(&source, &["gc", "--prune=now"]);
    assert!(repo.commit_details(project.base.parse()?).is_ok());
    let workspace = state["workspaces"][0]["path"].as_str().unwrap();
    assert_eq!(
        git(std::path::Path::new(workspace), &["rev-parse", "HEAD"]),
        project.base
    );
    assert!(git(std::path::Path::new(workspace), &["status", "--porcelain"]).is_empty());
    assert!(!source.join(".git/index").exists());
    Ok(())
}
