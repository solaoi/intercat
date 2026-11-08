use crate::{
    Result, checkpoint,
    decision::{Decision, Kind, Outcome, Target},
    folders::Assignment,
    integration::{Action as CommitAction, Command as CommitCommand, Service},
    journal::Journal,
    plan_store::{Action, Command, Generated},
    plans::{Draft, DraftItem},
    preparation::{Preparation, Workspace},
    project,
    request_sources::Submission,
    secrets::Key,
};
use intercat_git::Repository;
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

fn git(path: &Path, args: &[&str]) -> Result<String> {
    let result = std::process::Command::new("/usr/bin/git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()?;
    if !result.status.success() {
        return Err(String::from_utf8_lossy(&result.stderr).to_string().into());
    }
    Ok(String::from_utf8(result.stdout)?.trim().into())
}
fn source(root: &Path, name: &str) -> Result<PathBuf> {
    let path = root.join(name);
    std::fs::create_dir(&path)?;
    git(&path, &["init", "-q", "-b", "main"])?;
    std::fs::write(path.join(format!("{name}.txt")), format!("dummy {name}\n"))?;
    git(&path, &["add", "."])?;
    git(
        &path,
        &[
            "-c",
            "user.name=Dummy",
            "-c",
            "user.email=dummy@localhost",
            "commit",
            "-qm",
            "Dummy base",
        ],
    )?;
    Ok(path)
}
async fn workspaces(journal: &Journal, count: usize) -> Result<Vec<Workspace>> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let state = journal.snapshot().await?;
            let values: Vec<Workspace> = serde_json::from_value(state["workspaces"].clone())?;
            if let Some(failed) = values.iter().find(|workspace| workspace.state == "failed") {
                return Err(failed.error.clone().unwrap_or_default().into());
            }
            if values.len() == count && values.iter().all(|workspace| workspace.state == "ready") {
                return Ok(values);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

#[tokio::test]
async fn planned_tracks_keep_separate_roots_session_refs_and_commit_reviews() -> Result<()> {
    let root = tempfile::tempdir()?;
    let a = source(root.path(), "a")?;
    let b = source(root.path(), "b")?;
    let originals = [&a, &b]
        .into_iter()
        .map(|path| {
            Ok((
                std::fs::read(path.join(".git/index"))?,
                git(path, &["rev-parse", "HEAD"])?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let project = project::inspect(
        Uuid::now_v7(),
        "Two dummy folders".into(),
        vec![a.clone(), b.clone()],
    )
    .await?;
    let database = root.path().join("db");
    let journal = Journal::open(&database, Key::new([7; 32])).await?;
    journal
        .configure_project(Uuid::now_v7(), project.clone())
        .await?;
    journal
        .stage_document(Submission {
            op_id: Uuid::now_v7(),
            text: "Prepare independent dummy files in both folders.".into(),
            target: Target::New,
            selected_track_id: None,
            attachment_ids: vec![],
            sources: None,
            execute_allowed: false,
            plan_item: None,
        })
        .await?;
    let pending = journal.plans().await?.pop().unwrap();
    journal
        .begin_plan(pending.id, pending.generation.id, pending.rev)
        .await?;
    journal
        .generated_plan(
            pending.id,
            pending.generation.id,
            pending.rev,
            Generated::Draft(Draft {
                assumptions: vec![],
                conflicts: vec![],
                summary: "Two independent dummy jobs.".into(),
                items: project
                    .folders
                    .iter()
                    .enumerate()
                    .map(|(i, folder)| DraftItem {
                        id: format!("item-{i}"),
                        track_ref: format!("new:{}", i + 1),
                        folder_id: folder.id(),
                        title: format!("Dummy {i}"),
                        instruction: "Write one dummy file.".into(),
                        constraints: vec![],
                        done_when: "The dummy file exists.".into(),
                        after: vec![],
                    })
                    .collect(),
            }),
            vec![],
        )
        .await?;
    let plan = journal.plans().await?.pop().unwrap();
    for action in [Action::Review, Action::Start] {
        journal
            .plan_command(Command {
                op_id: Uuid::now_v7(),
                plan_id: plan.id,
                revision: plan.rev,
                action,
            })
            .await?;
    }
    journal.advance_plans().await?;
    let state = journal.snapshot().await?;
    let slug = state["session"]["slug"].as_str().unwrap();
    let repo = Repository::open(&b)?;
    let foreign = repo.write_blob(b"dummy foreign owner")?;
    repo.compare_and_swap(&format!("refs/intercat/{slug}/foreign"), None, foreign)?;
    let preparation = Preparation::start(&root.path().join("workspaces"), journal.clone())?;
    let result = async {
        let work = workspaces(&journal, 2).await?;
        let state = journal.snapshot().await?;
        assert_eq!(state["sessionFolders"].as_array().unwrap().len(), 2);
        assert_eq!(state["sessionFolders"][0]["slug"], slug);
        assert_eq!(state["sessionFolders"][1]["slug"], format!("{slug}-2"));
        let mut tracks = vec![];
        for (i, folder) in project.folders.iter().enumerate() {
            let track = state["plans"][0]["items"][i]["trackRef"].as_str().unwrap();
            assert_eq!(state["trackFolders"][track], folder.id());
            let workspace = work
                .iter()
                .find(|workspace| workspace.track_id == track)
                .unwrap();
            let own = if i == 0 { "a.txt" } else { "b.txt" };
            let other = if i == 0 { "b.txt" } else { "a.txt" };
            assert!(workspace.path.join(own).exists());
            assert!(!workspace.path.join(other).exists());
            assert_eq!(
                Repository::clean_project(&workspace.path)?.common_dir,
                folder.common_dir
            );
            std::fs::write(
                workspace.path.join("result.txt"),
                format!("dummy result {i}\n"),
            )?;
            checkpoint::save(
                workspace.clone(),
                Uuid::now_v7(),
                None,
                false,
                journal.clone(),
            )
            .await?;
            tracks.push(track.to_string());
        }
        let service = Service::new(journal.clone(), root.path().join("integration"));
        assert!(
            service
                .command(CommitCommand {
                    op_id: Uuid::now_v7(),
                    action: CommitAction::Open {
                        tracks: Some(tracks.clone())
                    }
                })
                .await
                .is_err()
        );
        for (i, track) in tracks.iter().enumerate() {
            let review = service
                .command(CommitCommand {
                    op_id: Uuid::now_v7(),
                    action: CommitAction::Open {
                        tracks: Some(vec![track.clone()]),
                    },
                })
                .await?;
            assert_eq!(review.project.id(), project.folders[i].id());
            assert_eq!(review.tracks.len(), 1);
            assert_eq!(review.tracks[0].id, *track);
            service
                .command(CommitCommand {
                    op_id: Uuid::now_v7(),
                    action: CommitAction::Discard {
                        id: review.id,
                        revision: review.revision,
                    },
                })
                .await?;
        }
        for (path, (index, head)) in [&a, &b].into_iter().zip(&originals) {
            assert_eq!(std::fs::read(path.join(".git/index"))?, *index);
            assert_eq!(git(path, &["rev-parse", "HEAD"])?, *head);
        }
        let reopened = Journal::open(&database, Key::new([7; 32]))
            .await?
            .snapshot()
            .await?;
        assert_eq!(reopened["trackFolders"], state["trackFolders"]);
        assert_eq!(reopened["sessionFolders"], state["sessionFolders"]);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    preparation.shutdown().await;
    result
}

#[tokio::test]
async fn an_unassigned_track_waits_for_a_durable_explicit_folder_choice() -> Result<()> {
    let root = tempfile::tempdir()?;
    let a = source(root.path(), "a")?;
    let b = source(root.path(), "b")?;
    let project = project::inspect(Uuid::now_v7(), "Dummy choices".into(), vec![a, b]).await?;
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    journal
        .configure_project(Uuid::now_v7(), project.clone())
        .await?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Dummy job".into(),
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
                boundary: Some("now".into()),
                command: None,
                confidence: 1.,
                inference_ms: 0.,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let state = journal.snapshot().await?;
    let track = state["tracks"][0]["id"].as_str().unwrap().to_string();
    assert!(state["trackFolders"][&track].is_null());
    let preparation = Preparation::start(&root.path().join("workspaces"), journal.clone())?;
    let result = async {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if journal.snapshot().await?["session"]["bound"] == true {
                    return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await??;
        assert!(
            journal.snapshot().await?["workspaces"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let choice = Assignment {
            op_id: Uuid::now_v7(),
            track_id: track.clone(),
            folder_id: project.folders[1].id(),
        };
        journal.assign_folder(choice.clone()).await?;
        journal.assign_folder(choice.clone()).await?;
        let work = workspaces(&journal, 1).await?;
        assert!(work[0].path.join("b.txt").exists());
        assert!(!work[0].path.join("a.txt").exists());
        assert!(
            journal
                .assign_folder(Assignment {
                    op_id: Uuid::now_v7(),
                    track_id: track,
                    folder_id: project.folders[0].id()
                })
                .await
                .is_err()
        );
        journal.assign_folder(choice).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    preparation.shutdown().await;
    result
}

#[tokio::test]
async fn duplicate_repository_paths_and_a_ninth_folder_are_rejected() -> Result<()> {
    let root = tempfile::tempdir()?;
    let a = source(root.path(), "a")?;
    assert!(
        project::inspect(Uuid::now_v7(), "No folder".into(), vec![])
            .await
            .is_err()
    );
    let error = project::inspect(
        Uuid::now_v7(),
        "Duplicate".into(),
        vec![a.clone(), a.clone()],
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("Same repository"));
    assert!(
        project::inspect(Uuid::now_v7(), "Too many".into(), vec![a; 9])
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
#[ignore = "one paid dummy Planner turn; requires saved auth and INTERCAT_M3_MULTI_FOLDER_OUTPUT"]
async fn real_planner_reads_each_frozen_folder_and_keeps_the_destinations() -> Result<()> {
    let output = PathBuf::from(std::env::var("INTERCAT_M3_MULTI_FOLDER_OUTPUT")?);
    std::fs::create_dir_all(&output)?;
    let root = tempfile::tempdir()?;
    let a = source(root.path(), "a")?;
    let b = source(root.path(), "b")?;
    for (path, instruction) in [
        (
            &a,
            "Plan one job in this folder: build a blue robot badge. Mention the color and object in the job. Do not implement it.",
        ),
        (
            &b,
            "Plan one job in this folder: write a green kite guide. Mention the color and object in the job. Do not implement it.",
        ),
    ] {
        std::fs::write(path.join("WORK.txt"), instruction)?;
        std::fs::write(
            path.join(".env"),
            "DUMMY_PRIVATE_ONLY=excluded-test-value\n",
        )?;
        git(path, &["add", "WORK.txt", ".env"])?;
        git(
            path,
            &[
                "-c",
                "user.name=Dummy",
                "-c",
                "user.email=dummy@localhost",
                "commit",
                "-qm",
                "Dummy task",
            ],
        )?;
    }
    let project = project::inspect(
        Uuid::now_v7(),
        "Two native dummy folders".into(),
        vec![a.clone(), b.clone()],
    )
    .await?;
    for path in [&a, &b] {
        std::fs::write(
            path.join("WORK.txt"),
            "UNSAVED: orange submarine. Do not use this.",
        )?;
    }
    let journal = Journal::open(root.path().join("db"), Key::new([7; 32])).await?;
    journal
        .configure_project(Uuid::now_v7(), project.clone())
        .await?;
    journal.stage_document(Submission {
        op_id: Uuid::now_v7(), text: "Read WORK.txt in each of the two saved folder roots. Plan exactly two independent jobs, one in each folder, as those files describe. Do not implement them.".into(),
        target: Target::New, selected_track_id: None, attachment_ids: vec![], sources: None, execute_allowed: false, plan_item: None,
    }).await?;
    let plan = journal.plans().await?.pop().unwrap();
    journal
        .begin_plan(plan.id, plan.generation.id, plan.rev)
        .await?;
    let binary = PathBuf::from(std::env::var("INTERCAT_MUSE_BINARY")?);
    let config = crate::runtime::Config {
        source: binary.clone(),
        binary,
        credentials: PathBuf::from(std::env::var("INTERCAT_MUSE_AUTH")?),
        path: "/usr/bin:/bin:/usr/sbin:/sbin".into(),
    };
    let rpc = root.path().join("rpc");
    std::fs::create_dir(&rpc)?;
    let (_stop, stopping) = tokio::sync::watch::channel(false);
    let (draft, based_on) = crate::planner::run(
        crate::host_memory::Pool::new(),
        &journal,
        &config,
        root.path(),
        &rpc,
        &plan,
        crate::msp::StartGuard::Stopped(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        ))),
        stopping,
    )
    .await?;
    std::fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(
            &json!({"verified":false,"draft":draft,"basedOn":based_on,"paidTurns":1}),
        )?,
    )?;
    let (items, _) =
        crate::plans::materialize(plan.id, None, &draft, &journal.plan_context(plan.id).await?)
            .map_err(|errors| errors.join("; "))?;
    assert_eq!(items.len(), 2);
    assert_eq!(based_on.len(), 2);
    for (i, (color, object)) in [("blue", "robot"), ("green", "kite")].iter().enumerate() {
        let folder = &project.folders[i];
        let item = items
            .iter()
            .find(|item| item.folder_id == folder.id)
            .ok_or("Planner omitted a folder")?;
        let text = format!("{} {} {}", item.title, item.instruction, item.done_when).to_lowercase();
        assert!(text.contains(*color) && text.contains(*object));
        assert_eq!(based_on[i].folder_id, folder.id);
        assert_eq!(based_on[i].sha, folder.base);
    }
    let text = serde_json::to_string(&draft)?.to_lowercase();
    assert!(
        !text.contains("orange")
            && !text.contains("submarine")
            && !text.contains("excluded-test-value")
    );
    assert!(
        journal.snapshot().await?["tracks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        std::fs::read_dir(root.path().join("planner-hosts"))?.count(),
        0
    );
    for path in [&a, &b] {
        assert!(std::fs::read_to_string(path.join("WORK.txt"))?.contains("orange submarine"));
    }
    std::fs::write(
        output.join("result.json"),
        serde_json::to_vec_pretty(
            &json!({"draft":draft,"basedOn":based_on,"folderIds":project.folders.iter().map(|folder|&folder.id).collect::<Vec<_>>(),"workersStarted":0,"hostCleaned":true,"savedTreesOnly":true,"verified":true,"paidTurns":1}),
        )?,
    )?;
    Ok(())
}
