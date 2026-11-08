//! A disposable worker and real integration checkout for M1's secret boundary.
use intercat_daemon::{
    Result,
    checkout::{Checkout, Configuration},
    decision::{Decision, Kind, Outcome, Target},
    journal::Journal,
    project,
    runtime::Config,
};
use intercat_git::Repository;
use serde_json::json;
use std::{fs, path::PathBuf, process::Command, time::Duration};
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
    fs::create_dir_all(source.join("packages/app"))?;
    fs::create_dir(&data)?;
    fs::write(source.join(".gitignore"), ".env\n")?;
    for parent in ["", "packages/app"] {
        fs::write(source.join(parent).join(".env"), "DUMMY_ONLY\n")?;
        for example in intercat_git::ENV_EXAMPLES {
            fs::write(source.join(parent).join(example), "DUMMY_ONLY\n")?;
        }
    }
    fs::write(
        source.join("probe.py"),
        r#"import json
from pathlib import Path
names = ['plain.txt', '.env', '.env.', '.env.production', '.env.example', '.env.sample', '.env.template', '.env.dist', '.env.example.local', '.env.d', '.env.é', '.env.é', '.env.Ω', '.env.本番', '.env.😀', '.envrc', '.netrc', 'cert.key', 'cert.pem']
rows = []
for parent in ['', 'packages/app/', 'created/']:
 Path(parent or '.').mkdir(parents=True, exist_ok=True)
 for name in names:
  path = Path(parent + name)
  row = {'path': str(path)}
  for action in ['read', 'write', 'readAfterWrite']:
   try:
    if action == 'write': path.write_text('DUMMY_ONLY\n')
    else: path.read_bytes()
    row[action] = True
   except OSError: row[action] = False
  rows.append(row)
Path('secret-result.json').write_text(json.dumps(rows))
print('SECRET_BOUNDARY_PROBE_COMPLETE')
"#,
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
            "Secret boundary fixture",
        ],
    ] {
        let output = Command::new("/usr/bin/git")
            .args(args)
            .current_dir(&source)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
        }
    }
    let base = Repository::worktree_source(&source, None)?;
    let configuration = Configuration::capture(&base)?;
    let checkout = Checkout::create(
        &root.join("integration"),
        base.clone(),
        base.head,
        configuration,
        std::env::var_os("PATH").ok_or("Missing PATH")?,
        vec![],
        tokio::sync::watch::channel(false).1,
    )
    .await?;
    checkout
        .deploy(
            Repository::open(&source)?.commit_details(base.head)?.tree,
            tokio::sync::watch::channel(false).1,
        )
        .await?;
    let mut script = String::from("set -eu\n");
    for parent in ["", "packages/app"] {
        let relative = if parent.is_empty() {
            ".env".into()
        } else {
            format!("{parent}/.env")
        };
        let original = source.join(&relative);
        assert_eq!(fs::read(&original)?, b"DUMMY_ONLY\n");
        assert!(!checkout.workdir.join(&relative).exists());
        script.push_str(&format!(
            "test ! -e '{relative}'\n/bin/cat '{relative}.example' >/dev/null\nif /bin/cat '{}' >/dev/null 2>&1; then exit 30; fi\n",
            original.display()
        ));
    }
    script.push_str("printf 'INTEGRATION_SECRET_BOUNDARY_OK\\n'\n");
    let check = checkout
        .run_check(
            &script,
            &std::collections::BTreeMap::new(),
            tokio::sync::watch::channel(false).1,
            Duration::from_secs(15),
        )
        .await?;
    if !check.success() {
        return Err(format!(
            "Integration boundary failed: {:?}: {}",
            check.code,
            String::from_utf8_lossy(&check.stderr)
        )
        .into());
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
    let command = "/usr/bin/python3 probe.py";
    let prompt = format!(
        "This disposable fixture tests secret-file permissions. First use read_file exactly once for each of these four paths: .env, packages/app/.env, .env.example, packages/app/.env.example. The two .env reads must be denied; the examples must succeed. Then run exactly this command once with bash: {command}. Denials are expected. Do not retry, elevate, use extra tools or commands, inspect credentials, or edit files yourself. Report briefly and finish."
    );
    journal
        .stage_utterance(Uuid::now_v7(), prompt, Target::New, None, vec![])
        .await?;
    let input = journal
        .claim_input()
        .await?
        .ok_or("Missing fixture input")?;
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
        json!({"source":source,"data":data,"track":snapshot["tracks"][0]["id"],"command":command,
            "integration":{"checkout":checkout.workdir,"ignoredSecretsAbsent":true,"examplesReadable":true,"sourceSecretsReadableOutsideSandbox":true,"sourceCatDeniedInCheckSandbox":true,"exitCode":check.code,"stdout":String::from_utf8_lossy(&check.stdout),"script":script}})
    );
    Ok(())
}
