use intercat_daemon::{
    Result,
    git_tool::GitTool,
    project::Folder,
    push_auth::Authentication,
    push_inspect::Inspector,
    push_lfs::Tool,
    push_review::{Choice, Member, Request, Review},
    push_target::{Destination, Target},
    push_transfer::Snapshot,
};
use intercat_git::Repository;
use sha2::{Digest, Sha256};
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
async fn pinned_lfs_client_uploads_scanned_payloads_over_https_before_git_refs() -> Result<()> {
    upload_before_refs(false).await
}
#[tokio::test]
async fn lfs_ssh_authentication_uses_reviewed_agent_peer_and_delegated_https_storage() -> Result<()>
{
    upload_before_refs(true).await
}
async fn upload_before_refs(ssh: bool) -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let root = temporary.path().canonicalize()?;
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/support/push-lfs-fixture.py");
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
        .ok_or("The dummy LFS service did not start.")?;
    let ready: serde_json::Value = serde_json::from_str(&ready)?;
    let url = ready["url"].as_str().unwrap();
    let mut ssh_server = None;
    let mut ssh_ready = None;
    if ssh {
        let response = root.join("lfs-auth.json");
        fs::write(
            &response,
            serde_json::to_vec(
                &serde_json::json!({"href":url,"header":{"Authorization":"Basic ZHVtbXk6ZHVtbXktbG9jYWwtb25seQ=="},"expires_in":3600}),
            )?,
        )?;
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/support/push-ssh-fixture.py");
        let mut child = Command::new("/usr/bin/python3")
            .arg(fixture)
            .arg(root.join("ssh"))
            .arg(response)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false)
            .spawn()?;
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let ready = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
            .await??
            .ok_or("Dummy SSH did not start.")?;
        ssh_ready = Some(serde_json::from_str::<serde_json::Value>(&ready)?);
        ssh_server = Some(child);
    }
    let home = ssh_ready
        .as_ref()
        .map(|ready| Path::new(ready["home"].as_str().unwrap()).to_path_buf())
        .unwrap_or_else(|| root.join("home"));
    let source_root = root.join("source");
    fs::create_dir_all(&home)?;
    fs::create_dir(&source_root)?;
    let helper = root.join("dummy-helper");
    fs::write(
        &helper,
        "#!/bin/sh\nif [ \"$1\" = get ]; then printf 'username=dummy\\npassword=dummy-local-only\\n'; fi\n",
    )?;
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700))?;
    fs::write(
        home.join(".gitconfig"),
        format!(
            "[credential]\n helper = {}\n[http]\n sslCAInfo = {}\n",
            helper.display(),
            ready["ca"].as_str().unwrap()
        ),
    )?;
    git(&source_root, &["init", "-q", "-b", "intercat/dummy"]);
    git(&source_root, &["config", "user.name", "Dummy"]);
    git(
        &source_root,
        &["config", "user.email", "dummy@example.invalid"],
    );
    fs::write(source_root.join("safe.txt"), "dummy base\n")?;
    git(&source_root, &["add", "."]);
    git(&source_root, &["commit", "-qm", "dummy base"]);
    let base = git(&source_root, &["rev-parse", "HEAD"]);
    let remote = root.join("remote.git");
    git(&root, &["init", "--bare", "-q", remote.to_str().unwrap()]);
    let git_url = reqwest::Url::from_file_path(&remote).unwrap().to_string();
    git(
        &source_root,
        &[
            "push",
            "--no-verify",
            &git_url,
            "HEAD:refs/heads/intercat/dummy",
        ],
    );
    let data = b"OWN GENERATED HTTPS LFS DATA\0\n";
    let oid = Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
        data.len()
    );
    fs::write(source_root.join("dummy.dat"), &pointer)?;
    fs::write(
        source_root.join(".lfsconfig"),
        format!(
            "[lfs]\n url = {}\n",
            ssh_ready
                .as_ref()
                .map(|ready| ready["url"].as_str().unwrap())
                .unwrap_or(url)
        ),
    )?;
    fs::write(
        source_root.join(".gitattributes"),
        "*.dat filter=lfs -text\n",
    )?;
    let object = source_root
        .join(".git/lfs/objects")
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(&oid);
    fs::create_dir_all(object.parent().unwrap())?;
    fs::write(object, data)?;
    git(&source_root, &["add", "."]);
    git(&source_root, &["commit", "-qm", "dummy LFS HTTPS"]);
    let source = Repository::worktree_source(&source_root, None)?;
    let index = fs::read(source.common_dir.join("index"))?;
    let folder = Folder {
        id: Uuid::now_v7().to_string(),
        name: "Dummy".into(),
        path: source.root.clone(),
        root: source.root.clone(),
        common_dir: source.common_dir.clone(),
        base: base.clone(),
        base_branch: Some("intercat/dummy".into()),
    };
    let git_tool = GitTool::system().await?;
    let inspector = Inspector::new(git_tool.clone(), home.clone(), home.join(".config"), false)?;
    let own_tool = source_root.join("own-dummy-lfs-tool");
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tools/git-lfs-3.8.1/git-lfs"),
        &own_tool,
    )?;
    let lfs = Tool::open(git_tool.clone(), own_tool.clone())?;
    fs::set_permissions(&own_tool, fs::Permissions::from_mode(0o700))?;
    fs::write(&own_tool, "#!/bin/sh\nexit 91\n")?;
    let reference = "refs/heads/intercat/dummy";
    let destination = Destination::capture(&git_url)?;
    let target = Target {
        source_ref: reference.into(),
        sha: source.head.to_string(),
        remote: "origin".into(),
        target_ref: reference.into(),
        destinations: vec![destination],
    };
    let endpoints = lfs
        .resolve(
            &inspector,
            &folder,
            &target,
            std::slice::from_ref(&git_url),
            no_cancel(),
        )
        .await?;
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
            target,
            lfs: endpoints
                .iter()
                .map(intercat_daemon::push_lfs::Binding::capture)
                .collect::<Result<Vec<_>>>()?,
            ssh: if let Some(ready) = &ssh_ready {
                vec![
                    intercat_daemon::push_ssh_config::Configuration::inspect(
                        &inspector,
                        &folder,
                        ready["url"].as_str().unwrap(),
                        no_cancel(),
                    )
                    .await?
                    .binding(ready["url"].as_str().unwrap())?,
                ]
            } else {
                Vec::new()
            },
        }],
    };
    let snapshot = Snapshot::prepare(
        &review,
        &folder.id,
        &root.join("transport"),
        &AtomicBool::new(false),
    )?;
    let repo = Repository::open(&snapshot.git_directory)?;
    let range = repo.push_commits(source.head, &[base.parse()?], &AtomicBool::new(false))?;
    let payloads = snapshot.copy_lfs_range(
        &range,
        &source.common_dir.join("lfs"),
        &AtomicBool::new(false),
    )?;
    assert_eq!(payloads.entries().len(), 1);
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
    assert_eq!(scan.unscanned_lfs, vec!["dummy.dat"]);
    let auth = Authentication::capture(
        &inspector,
        &folder,
        &git_tool,
        &endpoints[0].api_url,
        "/usr/bin:/bin".as_ref(),
        ssh_ready
            .as_ref()
            .map(|ready| Path::new(ready["agent"].as_str().unwrap())),
        no_cancel(),
    )
    .await?;
    lfs.push_network(&snapshot, &endpoints[0], &payloads, auth, no_cancel())
        .await?;
    assert_eq!(
        fs::read(Path::new(ready["objects"].as_str().unwrap()).join(&oid))?,
        data
    );
    assert_eq!(git(&remote, &["rev-parse", reference]), base);
    let client = intercat_daemon::push_file::Client::new(&snapshot, git_tool.clone(), &git_url)?;
    assert!(client.send(no_cancel()).await?.changed);
    let auth = Authentication::capture(
        &inspector,
        &folder,
        &git_tool,
        &endpoints[0].api_url,
        "/usr/bin:/bin".as_ref(),
        ssh_ready
            .as_ref()
            .map(|ready| Path::new(ready["agent"].as_str().unwrap())),
        no_cancel(),
    )
    .await?;
    lfs.push_network(&snapshot, &endpoints[0], &payloads, auth, no_cancel())
        .await?;
    assert_eq!(fs::read(source.common_dir.join("index"))?, index);
    assert_eq!(
        git(&source.root, &["rev-parse", "HEAD"]),
        source.head.to_string()
    );
    assert_eq!(fs::read_to_string(source.root.join("dummy.dat"))?, pointer);
    if let Some(mut child) = ssh_server {
        child.stdin.take().unwrap().write_all(b"quit\n").await?;
        assert!(
            tokio::time::timeout(Duration::from_secs(5), child.wait())
                .await??
                .success()
        );
    }
    server.stdin.take().unwrap().write_all(b"quit\n").await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), server.wait())
            .await??
            .success()
    );
    let events: Vec<String> =
        serde_json::from_slice(&fs::read(ready["events"].as_str().unwrap())?)?;
    assert!(events.contains(&"upload".into()));
    assert!(events.contains(&"verify".into()));
    Ok(())
}
