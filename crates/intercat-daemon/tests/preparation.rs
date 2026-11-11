use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    preparation::Preparation,
    project::{self, Folder},
    worktree,
};
use intercat_git::Repository;
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command, time::Duration};
use uuid::Uuid;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
async fn fixture() -> Result<(tempfile::TempDir, Journal, Folder, Value, String)> {
    let root = tempfile::tempdir()?;
    let source = root.path().join("source");
    fs::create_dir(&source)?;
    git(&source, &["init", "--quiet", "-b", "main"]);
    fs::write(source.join("file.txt"), "base\n")?;
    git(&source, &["add", "."]);
    git(
        &source,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "base",
        ],
    );
    let project =
        project::inspect(uuid::Uuid::now_v7(), "Dummy project".into(), vec![source]).await?;
    let journal = Journal::open(
        root.path().join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    journal
        .configure_project(Uuid::now_v7(), project.clone())
        .await?;
    let id = Uuid::now_v7();
    journal
        .stage_utterance(
            id,
            "Wait for my signal".into(),
            Target::New,
            None,
            Vec::new(),
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
                confidence: 1.0,
                inference_ms: 1.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let snapshot = journal.snapshot().await?;
    let track = snapshot["tracks"][0]["id"].as_str().unwrap().to_owned();
    Ok((
        root,
        journal,
        project.only_folder()?.clone(),
        snapshot["session"].clone(),
        track,
    ))
}
async fn ready(journal: &Journal) -> Result<Value> {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let state = journal.snapshot().await?;
            if state["workspaces"][0]["state"] == "failed" {
                panic!("{}", state["workspaces"][0]["error"]);
            }
            if state["workspaces"][0]["state"] == "ready" {
                return Ok(state);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?
}

#[tokio::test]
async fn reserves_a_distinct_slug_and_prepares_without_starting_a_waiting_request() -> Result<()> {
    let (root, journal, project, session, _) = fixture().await?;
    let repo = Repository::open(&project.root)?;
    let occupied = format!("refs/intercat/{}/old", session["slug"].as_str().unwrap());
    let source = project.source().await?;
    repo.compare_and_swap(&occupied, None, source.head)?;
    repo.sync_ref(&occupied)?;
    let index = fs::read(project.root.join(".git/index"))?;
    let preparation = Preparation::start(&root.path().join("workspaces"), journal.clone())?;
    let state = ready(&journal).await?;
    assert_eq!(
        state["sessionFolders"][0]["slug"],
        format!("{}-2", session["slug"].as_str().unwrap())
    );
    assert_eq!(state["session"]["bound"], true);
    assert_eq!(state["utterances"][0]["state"], "placed");
    assert_eq!(state["tracks"][0]["status"], "waiting");
    let path = state["workspaces"][0]["path"].as_str().unwrap();
    assert_eq!(
        fs::read_to_string(Path::new(path).join("file.txt"))?,
        "base\n"
    );
    assert_eq!(fs::read(project.root.join(".git/index"))?, index);
    assert_eq!(repo.reference_target(&occupied)?, Some(source.head));
    preparation.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn recovers_reserved_names_and_incomplete_checkout_using_the_same_operations() -> Result<()> {
    let (root, journal, project, session, track) = fixture().await?;
    let session_id = Uuid::parse_str(session["id"].as_str().unwrap())?;
    let slug = session["slug"].as_str().unwrap();
    let repo = Repository::open(&project.root)?;
    let marker = repo.write_blob(format!("Intercat session {session_id}\n").as_bytes())?;
    repo.sync_objects(&[marker])?;
    let marker_ref = format!("refs/intercat/{slug}/session");
    let reservation = Uuid::new_v5(
        &session_id,
        format!("reserve:{}:{slug}", project.id()).as_bytes(),
    );
    journal
        .intent(
            &reservation.to_string(),
            "session.reserve",
            json!({"repository":project.root,"ref":marker_ref,"next":marker.to_string()}),
        )
        .await?;
    repo.compare_and_swap(&marker_ref, None, marker)?;
    repo.sync_ref(&marker_ref)?;
    let workspaces = root.path().join("workspaces");
    fs::create_dir(&workspaces)?;
    let workspaces = workspaces.canonicalize()?;
    let destination = workspaces.join(session_id.to_string()).join(&track);
    let source = project.source().await?;
    let reference = format!("refs/intercat/{slug}/tracks/A");
    let operation = Uuid::new_v5(&Uuid::parse_str(&track)?, b"worktree.prepare");
    journal.intent(&operation.to_string(),"worktree.prepare",json!({"source":source.root,"path":destination,"head":source.head.to_string(),"ref":reference})).await?;
    worktree::prepare_detached(&source, &destination).await?;
    fs::remove_file(destination.join("file.txt"))?;
    let preparation = Preparation::start(&workspaces, journal.clone())?;
    let state = ready(&journal).await?;
    assert_eq!(state["sessionFolders"][0]["slug"], slug);
    assert_eq!(fs::read_to_string(destination.join("file.txt"))?, "base\n");
    assert_eq!(
        journal.operation(reservation).await?.unwrap()["state"],
        "complete"
    );
    assert_eq!(
        journal.operation(operation).await?.unwrap()["state"],
        "complete"
    );
    preparation.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn changed_namespace_ownership_holds_new_work_instead_of_reusing_foreign_refs() -> Result<()>
{
    let (root, journal, project, _, _) = fixture().await?;
    let workspaces = root.path().join("workspaces");
    let preparation = Preparation::start(&workspaces, journal.clone())?;
    let first = ready(&journal).await?;
    preparation.shutdown().await;
    let slug = first["session"]["slug"].as_str().unwrap();
    let reference = format!("refs/intercat/{slug}/session");
    let repo = Repository::open(&project.root)?;
    let old = repo.reference_target(&reference)?.unwrap();
    let foreign = repo.write_blob(b"another owner\n")?;
    repo.sync_objects(&[foreign])?;
    repo.compare_and_swap(&reference, Some(old), foreign)?;
    repo.sync_ref(&reference)?;
    journal
        .stage_utterance(
            Uuid::now_v7(),
            "Another request".into(),
            Target::New,
            None,
            Vec::new(),
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
                confidence: 1.0,
                inference_ms: 1.0,
                timed_out: false,
                scores: json!({}),
            }),
        )
        .await?;
    let preparation = Preparation::start(&workspaces, journal.clone())?;
    let mut status = preparation.status.clone();
    tokio::time::timeout(Duration::from_secs(10), async {
        while status.borrow_and_update().phase != "failed" {
            status.changed().await.unwrap();
        }
    })
    .await?;
    let state = journal.snapshot().await?;
    assert_eq!(state["workspaces"].as_array().unwrap().len(), 1);
    assert_eq!(
        repo.reference_target(&format!("refs/intercat/{slug}/tracks/B"))?,
        None
    );
    assert_eq!(repo.reference_target(&reference)?, Some(foreign));
    preparation.shutdown().await;
    Ok(())
}
