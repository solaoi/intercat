//! Disposable, model-free project for exercising the desktop Commit flow.
use intercat_daemon::{
    Result, checkpoint,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    preparation::{Preparation, Workspace},
    project,
};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use uuid::Uuid;

fn git(root: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("/usr/bin/git")
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let failed_check = std::env::args().nth(2).as_deref() == Some("--failed-check");
    let unborn = std::env::args().nth(2).as_deref() == Some("--unborn");
    let root = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("Expected a new fixture directory.")?,
    );
    fs::create_dir(&root)?;
    let source = root.join("source");
    let data = root.join("data");
    fs::create_dir(&source)?;
    fs::create_dir(&data)?;
    git(&source, &["init", "-q", "-b", "main"])?;
    if !unborn {
        fs::write(source.join("index.js"), "exports.value = 1;\n")?;
        fs::write(source.join("README.md"), "# Commit fixture\n")?;
        fs::write(
            source.join("package.json"),
            r#"{"name":"commit-fixture","version":"1.0.0","scripts":{"test":"node --check index.js"}}"#,
        )?;
        git(&source, &["add", "."])?;
        git(&source, &["commit", "-qm", "base"])?;
    }
    let journal = Journal::open(
        data.join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    journal
        .configure_project(Uuid::now_v7(), project::inspect(source.clone()).await?)
        .await?;
    for title in ["Update the exported value", "Document the change"] {
        journal
            .stage_utterance(Uuid::now_v7(), title.into(), Target::New, None, Vec::new())
            .await?;
        let input = journal
            .claim_input()
            .await?
            .ok_or("Missing fixture input.")?;
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
                    inference_ms: 0.,
                    timed_out: false,
                    scores: json!({}),
                }),
            )
            .await?;
    }
    let preparation = Preparation::start(&data.join("workspaces"), journal.clone())?;
    let workspaces = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let value = journal.snapshot().await?;
            let workspaces: Vec<Workspace> = serde_json::from_value(value["workspaces"].clone())?;
            if workspaces
                .iter()
                .any(|workspace| workspace.state == "failed")
            {
                return Err::<Vec<Workspace>, Box<dyn std::error::Error + Send + Sync>>(
                    "Fixture workspace failed.".into(),
                );
            }
            if workspaces.len() == 2
                && workspaces
                    .iter()
                    .all(|workspace| workspace.state == "ready")
            {
                return Ok(workspaces);
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await??;
    preparation.shutdown().await;
    for (index, workspace) in workspaces.into_iter().enumerate() {
        if index == 0 {
            if unborn {
                fs::write(
                    workspace.path.join("package.json"),
                    r#"{"name":"commit-fixture","version":"1.0.0","scripts":{"test":"node --check index.js"}}"#,
                )?;
            }
            fs::write(
                workspace.path.join("index.js"),
                if failed_check {
                    "exports.value = (2;\n"
                } else {
                    "exports.value = 2;\n"
                },
            )?;
        } else {
            fs::write(
                workspace.path.join("README.md"),
                "# Commit fixture\n\nThe exported value is now 2.\n",
            )?;
        }
        checkpoint::save(workspace, Uuid::now_v7(), None, false, journal.clone()).await?;
    }
    println!("{}", json!({"source":source,"data":data}));
    Ok(())
}
