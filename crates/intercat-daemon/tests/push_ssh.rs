use intercat_daemon::{
    Result,
    git_tool::GitTool,
    project::Folder,
    push_auth::Authentication,
    push_inspect::Inspector,
    push_lfs::Binding,
    push_review::{Choice, Member, Request, Review},
    push_ssh,
    push_ssh_config::Configuration,
    push_target::{Destination, Target},
    push_transfer::Snapshot,
};
use intercat_git::Repository;
use std::{
    fs, os::unix::fs::PermissionsExt, path::Path, process::Stdio, sync::atomic::AtomicBool,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::watch,
};
use uuid::Uuid;
fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("/usr/bin/git")
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
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
fn no_cancel() -> watch::Receiver<bool> {
    watch::channel(false).1
}
#[tokio::test]
async fn ssh_uses_the_generated_agent_and_reviewed_alias_through_a_private_tunnel() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/support/push-ssh-fixture.py");
    let mut server = Command::new("/usr/bin/python3")
        .arg(fixture)
        .arg(root.join("server"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false)
        .spawn()?;
    let mut lines = BufReader::new(server.stdout.take().unwrap()).lines();
    let ready = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
        .await??
        .ok_or("The generated SSH fixture did not start.")?;
    let ready: serde_json::Value = serde_json::from_str(&ready)?;
    let url = ready["url"].as_str().unwrap();
    let home = Path::new(ready["home"].as_str().unwrap()).to_path_buf();
    let agent = Path::new(ready["agent"].as_str().unwrap());
    let source_root = root.join("source");
    fs::create_dir(&source_root)?;
    git(&source_root, &["init", "-q", "-b", "intercat/dummy"]);
    git(&source_root, &["config", "user.name", "Dummy"]);
    git(
        &source_root,
        &["config", "user.email", "dummy@example.invalid"],
    );
    fs::write(source_root.join("dummy.txt"), "dummy SSH payload\n")?;
    git(&source_root, &["add", "."]);
    git(&source_root, &["commit", "-qm", "dummy SSH"]);
    let source = Repository::worktree_source(&source_root, None)?;
    let index = fs::read(source.common_dir.join("index"))?;
    fs::write(
        source.common_dir.join("hooks/pre-push"),
        "#!/bin/sh\nexit 71\n",
    )?;
    fs::set_permissions(
        source.common_dir.join("hooks/pre-push"),
        fs::Permissions::from_mode(0o700),
    )?;
    git(
        &source.root,
        &["config", "core.sshCommand", "/bin/sh -c 'exit 72'"],
    );
    let folder = Folder {
        id: Uuid::now_v7().to_string(),
        name: "Dummy".into(),
        path: source.root.clone(),
        root: source.root.clone(),
        common_dir: source.common_dir.clone(),
        base: source.head.to_string(),
        base_branch: Some("intercat/dummy".into()),
    };
    let tool = GitTool::system().await?;
    let inspector = Inspector::new(tool.clone(), home.clone(), home.join(".config"), false)?;
    let ssh = Configuration::inspect(&inspector, &folder, url, no_cancel())
        .await?
        .binding(url)?;
    assert_eq!(ssh.port, ready["port"].as_u64().unwrap() as u16);
    let destination = Destination::capture(url)?;
    let reference = "refs/heads/intercat/dummy";
    let review = Review {
        request: Request {
            op_id: Uuid::now_v7(),
            choices: vec![Choice {
                folder_id: folder.id.clone(),
                remote: "origin".into(),
                target_ref: None,
                through: None,
            }],
        },
        project_id: Uuid::now_v7(),
        session_id: Uuid::now_v7().to_string(),
        session_name: "Dummy".into(),
        members: vec![Member {
            folder: folder.clone(),
            slug: "dummy".into(),
            target: Target {
                source_ref: reference.into(),
                sha: source.head.to_string(),
                remote: "origin".into(),
                target_ref: reference.into(),
                destinations: vec![destination.clone()],
            },
            lfs: vec![Binding {
                git: destination.clone(),
                upload: destination.clone(),
                api: destination,
            }],
            ssh: vec![ssh],
        }],
    };
    let snapshot = Snapshot::prepare(
        &review,
        &folder.id,
        &root.join("transport"),
        &AtomicBool::new(false),
    )?;
    let config = home.join(".ssh/config");
    let original_config = fs::read_to_string(&config)?;
    fs::write(
        &config,
        original_config.replace(
            &format!("IdentityAgent {}", agent.display()),
            "IdentityAgent none",
        ),
    )?;
    let missing_agent = Authentication::capture(
        &inspector,
        &folder,
        &tool,
        url,
        "/usr/bin:/bin".as_ref(),
        None,
        no_cancel(),
    )
    .await?;
    let unavailable =
        push_ssh::Client::new(&snapshot, tool.clone(), url, missing_agent, no_cancel()).await?;
    assert!(unavailable.advertise(no_cancel()).await.is_err());
    drop(unavailable);
    fs::write(&config, &original_config)?;
    let known = home.join(".ssh/known_hosts");
    let original_known = fs::read_to_string(&known)?;
    let wrong_public = fs::read_to_string(home.join(".ssh/client.pub"))?;
    fs::write(
        &known,
        format!(
            "[127.0.0.1]:{} {}\n",
            ready["port"].as_u64().unwrap(),
            wrong_public.trim()
        ),
    )?;
    let wrong_host = Authentication::capture(
        &inspector,
        &folder,
        &tool,
        url,
        "/usr/bin:/bin".as_ref(),
        Some(agent),
        no_cancel(),
    )
    .await?;
    let untrusted =
        push_ssh::Client::new(&snapshot, tool.clone(), url, wrong_host, no_cancel()).await?;
    assert!(untrusted.advertise(no_cancel()).await.is_err());
    drop(untrusted);
    fs::write(&known, &original_known)?;
    let auth = Authentication::capture(
        &inspector,
        &folder,
        &tool,
        url,
        "/usr/bin:/bin".as_ref(),
        Some(agent),
        no_cancel(),
    )
    .await?;
    let client = push_ssh::Client::new(&snapshot, tool.clone(), url, auth, no_cancel()).await?;
    assert!(client.advertise(no_cancel()).await?.is_empty());
    let repo = Repository::open(&snapshot.git_directory)?;
    let scan = intercat_daemon::secret_scan::Engine::capture()?
        .start(&[])?
        .push_commit(
            &repo,
            source.head,
            &snapshot.git_directory.join("lfs"),
            no_cancel(),
        )
        .await?;
    assert!(scan.findings.is_empty());
    let receipt = client.send(no_cancel()).await?;
    assert!(receipt.changed);
    assert_eq!(receipt.sha, source.head.to_string());
    let refs = client.advertise(no_cancel()).await?;
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].sha, source.head);
    assert!(!client.send(no_cancel()).await?.changed);
    let config = home.join(".ssh/config");
    let original = fs::read_to_string(&config)?;
    fs::write(
        &config,
        original.replace("HostName 127.0.0.1", "HostName 127.0.0.2"),
    )?;
    let auth = Authentication::capture(
        &inspector,
        &folder,
        &tool,
        url,
        "/usr/bin:/bin".as_ref(),
        Some(agent),
        no_cancel(),
    )
    .await?;
    assert!(
        push_ssh::Client::new(&snapshot, tool, url, auth, no_cancel())
            .await
            .is_err()
    );
    assert_eq!(fs::read(source.common_dir.join("index"))?, index);
    assert_eq!(
        git(&source.root, &["rev-parse", "HEAD"]),
        source.head.to_string()
    );
    assert_eq!(
        fs::read_to_string(source.root.join("dummy.txt"))?,
        "dummy SSH payload\n"
    );
    server.stdin.take().unwrap().write_all(b"quit\n").await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(10), server.wait())
            .await??
            .success()
    );
    Ok(())
}
