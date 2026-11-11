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
async fn sparse_project_selection_and_worker_preparation_preserve_the_human_checkout()
-> intercat_daemon::Result<()> {
    use intercat_daemon::{
        checkpoint,
        decision::{Decision, Kind, Outcome},
        preparation::{Preparation, Workspace},
    };
    use std::{fs, time::Duration};
    for mode in ["cone", "non-cone", "sparse-index", "split-index"] {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        fs::create_dir(&source)?;
        git(&source, &["init", "-q", "-b", "main"]);
        for name in [
            "inside/file.txt",
            "outside/changed.txt",
            "outside/retained.txt",
        ] {
            let path = source.join(name);
            fs::create_dir_all(path.parent().unwrap())?;
            fs::write(path, "before\n")?;
        }
        git(&source, &["add", "-A"]);
        git(
            &source,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "Sparse project baseline",
            ],
        );
        if mode == "non-cone" {
            git(&source, &["sparse-checkout", "init", "--no-cone"]);
            git(
                &source,
                &["sparse-checkout", "set", "--no-cone", "/inside/"],
            );
        } else {
            git(
                &source,
                &[
                    "sparse-checkout",
                    "init",
                    "--cone",
                    if mode == "sparse-index" {
                        "--sparse-index"
                    } else {
                        "--no-sparse-index"
                    },
                ],
            );
            git(&source, &["sparse-checkout", "set", "inside"]);
        }
        if mode == "split-index" {
            git(&source, &["config", "core.splitIndex", "true"]);
            git(&source, &["update-index", "--split-index"]);
        }
        let human_index = fs::read(source.join(".git/index"))?;
        let human_head = git(&source, &["rev-parse", "HEAD"]);
        assert!(!source.join("outside/retained.txt").exists());
        let journal = Journal::open(
            root.path().join("events.sqlite"),
            intercat_daemon::secrets::Key::new([7; 32]),
        )
        .await?;
        let project = intercat_daemon::project::inspect(
            uuid::Uuid::now_v7(),
            "Generated sparse project".into(),
            vec![source.clone()],
        )
        .await?;
        journal
            .configure_project(uuid::Uuid::now_v7(), project)
            .await?;
        journal
            .stage_utterance(
                uuid::Uuid::now_v7(),
                "Dummy sparse request".into(),
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
        let prepared = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let status = journal.snapshot().await?;
                if let Some(workspace) = status["workspaces"].as_array().unwrap().first() {
                    if workspace["state"] == "failed" {
                        return Err(
                            format!("{mode}: worker preparation failed: {workspace}").into()
                        );
                    }
                    if workspace["state"] == "ready" {
                        return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                            serde_json::from_value::<Workspace>(workspace.clone())?,
                        );
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        preparation.shutdown().await;
        let workspace = prepared??;
        fs::create_dir_all(workspace.path.join("outside"))?;
        fs::write(workspace.path.join("outside/changed.txt"), "changed\n")?;
        let point = checkpoint::save(
            workspace.clone(),
            uuid::Uuid::now_v7(),
            None,
            false,
            journal,
        )
        .await?;
        assert_eq!(
            git(
                &workspace.path,
                &["show", &format!("{}:outside/changed.txt", point.sha)]
            ),
            "changed"
        );
        assert_eq!(
            git(
                &workspace.path,
                &["show", &format!("{}:outside/retained.txt", point.sha)]
            ),
            "before"
        );
        assert_eq!(
            fs::read(source.join(".git/index"))?,
            human_index,
            "{mode}: human index"
        );
        assert_eq!(
            git(&source, &["rev-parse", "HEAD"]),
            human_head,
            "{mode}: human HEAD"
        );
        assert!(
            !source.join("outside/retained.txt").exists(),
            "{mode}: human sparse state"
        );
        assert_eq!(
            fs::read_to_string(source.join("inside/file.txt"))?,
            "before\n"
        );
    }
    Ok(())
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
    let project =
        intercat_daemon::project::inspect(uuid::Uuid::now_v7(), "Dummy project".into(), vec![path])
            .await
            .unwrap();
    journal
        .configure_project(op, project.clone())
        .await
        .unwrap();
    journal
        .configure_project(op, project.clone())
        .await
        .unwrap();
    let mut other = project.clone();
    other.folders[0].base = "2".repeat(40);
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
    assert_eq!(
        restored["project"]["folders"][0]["base"],
        project.folders[0].base
    );
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
    let project = intercat_daemon::project::inspect(
        uuid::Uuid::now_v7(),
        "Dummy project".into(),
        vec![source.clone()],
    )
    .await?;
    assert!(project.folders[0].base_branch.is_none());
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
            .commit_details(project.folders[0].base.parse()?)
            .is_err()
    );
    project.pin().await?;
    let reference = format!("refs/intercat/starting-points/{}", project.folders[0].base);
    let repo = intercat_git::Repository::open(&source)?;
    repo.delete_ref(&reference, project.folders[0].base.parse()?)?;
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
    assert_eq!(
        state["project"]["folders"][0]["base"],
        project.folders[0].base
    );
    assert_eq!(journal.operation(id).await?.unwrap()["state"], "complete");
    assert_eq!(repo.reference_target("refs/heads/main")?, None);
    assert_eq!(
        repo.reference_target(&reference)?.unwrap().to_string(),
        project.folders[0].base
    );
    git(&source, &["gc", "--prune=now"]);
    assert!(
        repo.commit_details(project.folders[0].base.parse()?)
            .is_ok()
    );
    let workspace = state["workspaces"][0]["path"].as_str().unwrap();
    assert_eq!(
        git(std::path::Path::new(workspace), &["rev-parse", "HEAD"]),
        project.folders[0].base
    );
    assert!(git(std::path::Path::new(workspace), &["status", "--porcelain"]).is_empty());
    assert!(!source.join(".git/index").exists());
    Ok(())
}
