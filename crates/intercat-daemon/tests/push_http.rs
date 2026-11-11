use intercat_daemon::{
    Result,
    git_tool::GitTool,
    project::Folder,
    push_auth::Authentication,
    push_http,
    push_inspect::Inspector,
    push_lfs::Binding,
    push_review::{Choice, Member, Request, Review},
    push_target::{Destination, Target},
    push_transfer::Snapshot,
};
use intercat_git::Repository;
use std::{
    fs, net::TcpListener, os::unix::fs::PermissionsExt, path::Path, process::Stdio,
    sync::atomic::AtomicBool, time::Duration,
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
async fn https_uses_dummy_credentials_and_only_the_approved_port_without_source_hooks() -> Result<()>
{
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/support/push-https-fixture.py");
    let mut server = Command::new("/usr/bin/python3")
        .arg(fixture)
        .arg(root.join("server"))
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut lines = BufReader::new(server.stdout.take().unwrap()).lines();
    let ready = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
        .await??
        .ok_or("The dummy server did not start.")?;
    let ready: serde_json::Value = serde_json::from_str(&ready)?;
    let url = ready["url"].as_str().unwrap();
    let home = root.join("client-home");
    let source_root = root.join("source");
    fs::create_dir(&home)?;
    fs::create_dir(&source_root)?;
    git(&source_root, &["init", "-q", "-b", "intercat/dummy"]);
    git(&source_root, &["config", "user.name", "Dummy"]);
    git(
        &source_root,
        &["config", "user.email", "dummy@example.invalid"],
    );
    fs::write(source_root.join("dummy.txt"), "dummy HTTPS payload\n")?;
    git(&source_root, &["add", "."]);
    git(&source_root, &["commit", "-qm", "dummy HTTPS"]);
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
    let trap = TcpListener::bind("127.0.0.1:0")?;
    trap.set_nonblocking(true)?;
    let helper = root.join("dummy helper");
    fs::write(
        &helper,
        format!(
            "#!/bin/sh\nif [ \"$1\" = get ]; then\n if IFS= read -r blocked < '{}'; then exit 81; fi\n if /bin/bash -c 'printf bad > /dev/tcp/127.0.0.1/{}' >/dev/null 2>&1; then exit 82; fi\n printf 'username=dummy\\npassword=dummy-local-only\\n'\nfi\n",
            source.root.join("dummy.txt").display(),
            trap.local_addr()?.port()
        ),
    )?;
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))?;
    let prepared_helper = format!("'{}'", helper.display());
    fs::write(
        home.join(".gitconfig"),
        format!(
            "[credential]\n helper = \"{prepared_helper}\"\n[http]\n sslCAInfo = {}\n",
            ready["ca"].as_str().unwrap()
        ),
    )?;
    let folder = Folder {
        id: Uuid::now_v7().to_string(),
        name: "Dummy".into(),
        path: source.root.clone(),
        root: source.root.clone(),
        common_dir: source.common_dir.clone(),
        base: source.head.to_string(),
        base_branch: Some("intercat/dummy".into()),
    };
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
            ssh: Vec::new(),
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
        }],
    };
    let snapshot = Snapshot::prepare(
        &review,
        &folder.id,
        &root.join("transport"),
        &AtomicBool::new(false),
    )?;
    let tool = GitTool::system().await?;
    let inspector = Inspector::new(tool.clone(), home.clone(), home.join(".config"), false)?;
    let auth = Authentication::capture(
        &inspector,
        &folder,
        &tool,
        url,
        "/usr/bin:/bin".as_ref(),
        None,
        no_cancel(),
    )
    .await?;
    let client = push_http::Client::new(&snapshot, tool, url, auth).await?;
    assert!(client.advertise(no_cancel()).await?.is_empty());
    let scanner = intercat_daemon::secret_scan::Engine::capture()?.start(&[])?;
    let repo = Repository::open(&snapshot.git_directory)?;
    for sha in repo.push_commits(source.head, &[], &AtomicBool::new(false))? {
        assert!(
            scanner
                .push_commit(&repo, sha, &snapshot.git_directory.join("lfs"), no_cancel())
                .await?
                .findings
                .is_empty()
        );
    }
    let receipt = client.send(no_cancel()).await?;
    assert!(receipt.changed);
    assert_eq!(receipt.sha, source.head.to_string());
    let refs = client.advertise(no_cancel()).await?;
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].sha, source.head);
    assert!(!client.send(no_cancel()).await?.changed);
    assert_eq!(
        trap.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(fs::read(source.common_dir.join("index"))?, index);
    assert_eq!(
        git(&source.root, &["rev-parse", "HEAD"]),
        source.head.to_string()
    );
    assert_eq!(
        fs::read_to_string(source.root.join("dummy.txt"))?,
        "dummy HTTPS payload\n"
    );
    let credential_url = url.replacen("https://", "https://dummy:dummy-local-only@", 1);
    let mut credential_review = review.clone();
    credential_review.request.op_id = Uuid::now_v7();
    let hidden = Destination::capture(&credential_url)?;
    credential_review.members[0].target.destinations = vec![hidden.clone()];
    credential_review.members[0].lfs = vec![Binding {
        git: hidden.clone(),
        upload: hidden.clone(),
        api: hidden,
    }];
    let credential_snapshot = Snapshot::prepare(
        &credential_review,
        &folder.id,
        &root.join("credential-transport"),
        &AtomicBool::new(false),
    )?;
    let tool = GitTool::system().await?;
    let auth = Authentication::capture(
        &inspector,
        &folder,
        &tool,
        &credential_url,
        "/usr/bin:/bin".as_ref(),
        None,
        no_cancel(),
    )
    .await?;
    let credential_client =
        push_http::Client::new(&credential_snapshot, tool.clone(), &credential_url, auth).await?;
    let receipt = credential_client.send(no_cancel()).await?;
    assert!(!receipt.changed);
    assert!(!serde_json::to_string(&receipt)?.contains("dummy-local-only"));
    git(
        &source.root,
        &["config", "credential.helper", &prepared_helper],
    );
    assert!(
        Authentication::capture(
            &inspector,
            &folder,
            &tool,
            url,
            "/usr/bin:/bin".as_ref(),
            None,
            no_cancel()
        )
        .await
        .is_err()
    );
    server.stdin.take().unwrap().write_all(b"quit\n").await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), server.wait())
            .await??
            .success()
    );
    Ok(())
}
