//! A disposable, held worker for checking the real daemon's RPC boundary.
use intercat_daemon::{
    Result,
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    project,
    runtime::Config,
};
use serde_json::json;
use std::{fs, path::PathBuf, process::Command};
use uuid::Uuid;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 3 {
        return Err("Expected a new fixture directory, pinned Muse, and its auth file.".into());
    }
    let root = PathBuf::from(&args[0]);
    fs::create_dir(&root)?;
    let root = root.canonicalize()?;
    let source = root.join("source");
    let data = root.join("data");
    fs::create_dir(&source)?;
    fs::create_dir(&data)?;
    fs::write(
        source.join("probe.py"),
        "import json, socket, sys\nfrom pathlib import Path\ns = socket.socket(socket.AF_UNIX)\ntry:\n s.connect(sys.argv[1]); result = {'connected': True}\nexcept OSError as e:\n result = {'connected': False, 'errno': e.errno}\nfinally:\n s.close()\nPath('rpc-result.json').write_text(json.dumps(result))\nprint(json.dumps(result))\n",
    )?;
    for args in [
        vec!["init", "-q", "-b", "main"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "RPC fixture",
        ],
    ] {
        let result = Command::new("/usr/bin/git")
            .args(args)
            .current_dir(&source)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()?;
        if !result.status.success() {
            return Err(String::from_utf8_lossy(&result.stderr).into_owned().into());
        }
    }
    let journal = Journal::open(
        data.join("events.sqlite"),
        intercat_daemon::secrets::Key::new([7; 32]),
    )
    .await?;
    journal
        .configure_project(
            Uuid::now_v7(),
            project::inspect(
                uuid::Uuid::now_v7(),
                "Dummy project".into(),
                vec![source.clone()],
            )
            .await?,
        )
        .await?;
    let binary = PathBuf::from(&args[1]).canonicalize()?;
    journal
        .set_worker_config(
            Uuid::now_v7(),
            Config {
                source: binary.clone(),
                binary,
                credentials: PathBuf::from(&args[2]).canonicalize()?,
                path: std::env::var("PATH")?,
            },
        )
        .await?;
    let socket = data.join("run/rpc.sock");
    let command = format!("/usr/bin/python3 probe.py '{}'", socket.display());
    let prompt = format!(
        "This disposable fixture tests a private RPC boundary. Run exactly this command once with bash: {command}. A denied connection is expected. Do not edit, retry, elevate, read credentials, or run other tools. Report briefly and finish."
    );
    journal
        .stage_utterance(Uuid::now_v7(), prompt, Target::New, None, vec![])
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
    let snapshot = journal.snapshot().await?;
    println!(
        "{}",
        json!({"source":source,"data":data,"track":snapshot["tracks"][0]["id"],"command":command})
    );
    Ok(())
}
