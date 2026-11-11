use intercat_daemon::{Result, push_target::Destination};

#[test]
fn native_git_pre_push_reports_literal_sha_and_each_resolved_push_url() -> Result<()> {
    use std::{fs, os::unix::fs::PermissionsExt};
    let root = tempfile::tempdir()?;
    let home = root.path().join("home");
    fs::create_dir(&home)?;
    let source = root.path().join("source");
    fs::create_dir(&source)?;
    fixture_git(&home, &source, &["init", "-q", "-b", "main"])?;
    fixture_git(&home, &source, &["config", "user.name", "Dummy Hook"])?;
    fixture_git(
        &home,
        &source,
        &["config", "user.email", "dummy@example.invalid"],
    )?;
    fs::write(source.join("dummy.txt"), "Dummy pre-push content\n")?;
    fixture_git(&home, &source, &["add", "dummy.txt"])?;
    fixture_git(&home, &source, &["commit", "-qm", "Dummy pre-push"])?;
    let sha = fixture_git(&home, &source, &["rev-parse", "HEAD"])?;
    let mut urls = vec![];
    for name in ["a.git", "b.git"] {
        let remote = root.path().join(name);
        fixture_git(
            &home,
            root.path(),
            &["init", "--bare", "-q", remote.to_str().unwrap()],
        )?;
        urls.push(
            reqwest::Url::from_file_path(remote)
                .map_err(|_| "Invalid dummy remote URL")?
                .to_string(),
        );
    }
    fixture_git(&home, &source, &["remote", "add", "origin", &urls[0]])?;
    for url in &urls {
        fixture_git(
            &home,
            &source,
            &["config", "--add", "remote.origin.pushurl", url],
        )?;
    }
    let hook = source.join(".git/hooks/pre-push");
    fs::write(
        &hook,
        "#!/bin/sh\nprintf '%s\\n%s\\n' \"$1\" \"$2\" >> \"$INTERCAT_DUMMY_HOOK_ARGUMENTS\"\n/bin/cat >> \"$INTERCAT_DUMMY_HOOK_INPUT\"\n",
    )?;
    fs::set_permissions(hook, fs::Permissions::from_mode(0o700))?;
    let arguments = root.path().join("hook-arguments");
    let input = root.path().join("hook-input");
    let output = std::process::Command::new("/usr/bin/git")
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("INTERCAT_DUMMY_HOOK_ARGUMENTS", &arguments)
        .env("INTERCAT_DUMMY_HOOK_INPUT", &input)
        .current_dir(&source)
        .args([
            "push",
            "--no-follow-tags",
            "--porcelain",
            "origin",
            &format!("{sha}:refs/heads/intercat/dummy"),
        ])
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(arguments)?,
        format!("origin\n{}\norigin\n{}\n", urls[0], urls[1])
    );
    let line = format!("{sha} {sha} refs/heads/intercat/dummy {}\n", "0".repeat(40));
    assert_eq!(fs::read_to_string(input)?, format!("{line}{line}"));
    Ok(())
}
fn fixture_git(home: &std::path::Path, path: &std::path::Path, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new("/usr/bin/git")
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(path)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "Dummy fixture Git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?
        .trim_end_matches('\n')
        .into())
}

#[test]
fn saved_destinations_hide_credentials_and_query_values_but_bind_every_byte() -> Result<()> {
    let raw = "https://DUMMY_LOGIN:DUMMY_PASSWORD@example.invalid/team/repo%3Fname.git?dummy_access=DUMMY_QUERY#DUMMY_FRAGMENT";
    let destination = Destination::capture(raw)?;
    assert_eq!(
        destination.display,
        "https://example.invalid/team/repo%3Fname.git"
    );
    assert!(destination.credentials);
    let saved = serde_json::to_string(&destination)?;
    for value in [
        "DUMMY_LOGIN",
        "DUMMY_PASSWORD",
        "DUMMY_QUERY",
        "DUMMY_FRAGMENT",
    ] {
        assert!(!saved.contains(value));
    }
    let changed = Destination::capture(&raw.replace("DUMMY_PASSWORD", "DUMMY_OTHER_PASSWORD"))?;
    assert_eq!(changed.display, destination.display);
    assert_ne!(changed.fingerprint, destination.fingerprint);
    let ssh = Destination::capture("git@example.invalid:team/repo.git")?;
    assert!(ssh.credentials);
    assert!(!ssh.display.contains("git@"));
    for raw in [
        "ext::sh -c DUMMY_PASSWORD",
        "helper::DUMMY_PASSWORD",
        "ssh://-oProxyCommand=BAD/repo",
        "https://invalid host/DUMMY_PASSWORD",
    ] {
        let error = Destination::capture(raw)
            .expect_err("Unsafe destination accepted")
            .to_string();
        assert!(!error.contains("DUMMY_PASSWORD"));
    }
    Ok(())
}

#[tokio::test]
async fn frozen_push_uses_all_resolved_urls_and_keeps_its_sha_when_the_branch_advances()
-> Result<()> {
    use intercat_daemon::{git_tool::GitTool, push_inspect::Inspector};
    use intercat_git::Repository;
    use std::{fs, os::unix::fs::PermissionsExt};
    use tokio::sync::watch;
    let root = tempfile::tempdir()?;
    let home = root.path().join("home");
    fs::create_dir(&home)?;
    let source = root.path().join("source");
    fs::create_dir(&source)?;
    fixture_git(&home, &source, &["init", "-q", "-b", "main"])?;
    fixture_git(&home, &source, &["config", "user.name", "Dummy Push"])?;
    fixture_git(
        &home,
        &source,
        &["config", "user.email", "dummy@example.invalid"],
    )?;
    fs::write(source.join("dummy.txt"), "First dummy commit\n")?;
    fixture_git(&home, &source, &["add", "dummy.txt"])?;
    fixture_git(&home, &source, &["commit", "-qm", "First dummy commit"])?;
    let first = fixture_git(&home, &source, &["rev-parse", "HEAD"])?;
    let remotes = root.path().join("remotes");
    fs::create_dir(&remotes)?;
    for name in ["a.git", "b.git", "c.git"] {
        fixture_git(&home, &remotes, &["init", "--bare", "-q", name])?;
    }
    let base = reqwest::Url::from_directory_path(remotes.canonicalize()?)
        .map_err(|_| "Invalid dummy URL")?;
    fs::write(
        home.join(".gitconfig"),
        format!("[url \"{base}\"]\n insteadOf = ic-dummy:\n pushInsteadOf = ic-push:\n"),
    )?;
    fixture_git(
        &home,
        &source,
        &["remote", "add", "origin", "ic-dummy:a.git"],
    )?;
    fixture_git(
        &home,
        &source,
        &["config", "--add", "remote.origin.pushurl", "ic-dummy:a.git"],
    )?;
    fixture_git(
        &home,
        &source,
        &["config", "--add", "remote.origin.pushurl", "ic-dummy:b.git"],
    )?;
    fixture_git(
        &home,
        &source,
        &["remote", "add", "push-only", "ic-push:a.git"],
    )?;
    fixture_git(
        &home,
        &source,
        &["remote", "add", "explicit", "ic-push:a.git"],
    )?;
    fixture_git(
        &home,
        &source,
        &["config", "remote.explicit.pushurl", "ic-push:b.git"],
    )?;
    // These settings must not cause any repository-controlled process during inspection.
    let marker = source.join("must-not-run");
    let script = root.path().join("fixture-helper");
    fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
    fixture_git(
        &home,
        &source,
        &["config", "core.fsmonitor", script.to_str().unwrap()],
    )?;
    fixture_git(
        &home,
        &source,
        &["config", "core.sshCommand", script.to_str().unwrap()],
    )?;
    let folder = intercat_daemon::project::inspect(
        uuid::Uuid::now_v7(),
        "Dummy Push".into(),
        vec![source.clone()],
    )
    .await?
    .folders
    .remove(0);
    let inspector = Inspector::new(
        GitTool::system().await?,
        home.clone(),
        home.join(".config"),
        false,
    )?;
    assert_eq!(
        inspector.remotes(&folder, watch::channel(false).1).await?,
        vec!["explicit", "origin", "push-only"]
    );
    assert_eq!(
        inspector
            .destinations(&folder, "push-only", watch::channel(false).1)
            .await?,
        vec![base.join("a.git")?.to_string()]
    );
    // Git ignores pushInsteadOf when a remote has an explicit pushurl.
    // https://git-scm.com/docs/git-config#Documentation/git-config.txt-urlltbasegtpushInsteadOf
    assert_eq!(
        inspector
            .destinations(&folder, "explicit", watch::channel(false).1)
            .await?,
        vec!["ic-push:b.git"]
    );
    let original_head = fs::read(source.join(".git/HEAD"))?;
    let original_index = fs::read(source.join(".git/index"))?;
    let target = inspector
        .freeze(
            &folder,
            "refs/heads/main",
            "origin",
            "refs/heads/intercat/dummy",
            None,
            watch::channel(false).1,
        )
        .await?;
    assert_eq!(target.sha, first);
    assert_eq!(target.destinations.len(), 2);
    assert_eq!(
        target.destinations[0].display,
        base.join("a.git")?.to_string()
    );
    assert_eq!(
        target.destinations[1].display,
        base.join("b.git")?.to_string()
    );
    assert_eq!(fs::read(source.join(".git/HEAD"))?, original_head);
    assert_eq!(fs::read(source.join(".git/index"))?, original_index);
    assert!(!marker.exists());
    fixture_git(&home, &source, &["config", "--unset", "core.fsmonitor"])?;
    fs::write(source.join("dummy.txt"), "Second dummy commit\n")?;
    fixture_git(&home, &source, &["add", "dummy.txt"])?;
    fixture_git(&home, &source, &["commit", "-qm", "Second dummy commit"])?;
    let second = fixture_git(&home, &source, &["rev-parse", "HEAD"])?;
    assert_ne!(second, first);
    let urls = inspector
        .destinations(&folder, "origin", watch::channel(false).1)
        .await?;
    target.verify_destinations("origin", "refs/heads/intercat/dummy", &urls)?;
    assert_eq!(
        target.refspec()?,
        format!("{first}:refs/heads/intercat/dummy")
    );
    let earlier = inspector
        .freeze(
            &folder,
            "refs/heads/main",
            "origin",
            "refs/heads/intercat/dummy",
            Some(first.parse()?),
            watch::channel(false).1,
        )
        .await?;
    assert_eq!(earlier.sha, first);
    fixture_git(
        &home,
        &source,
        &[
            "config",
            "--replace-all",
            "remote.origin.pushurl",
            "ic-dummy:c.git",
        ],
    )?;
    let changed = inspector
        .destinations(&folder, "origin", watch::channel(false).1)
        .await?;
    assert!(
        target
            .verify_destinations("origin", "refs/heads/intercat/dummy", &changed)
            .is_err()
    );
    assert!(
        target
            .verify_destinations("origin", "refs/heads/another", &urls)
            .is_err()
    );
    fixture_git(&home, &source, &["config", "remote.origin.mirror", "true"])?;
    assert!(
        inspector
            .destinations(&folder, "origin", watch::channel(false).1)
            .await
            .expect_err("Mirror accepted")
            .to_string()
            .contains("mirrors every ref")
    );
    for name in ["a.git", "b.git", "c.git"] {
        assert!(
            Repository::open(remotes.join(name))?
                .reference_target("refs/heads/intercat/dummy")?
                .is_none()
        );
    }
    assert!(!marker.exists());
    Ok(())
}

#[tokio::test]
async fn lfs_upload_endpoints_use_frozen_configuration_push_precedence_and_offline_resolution()
-> Result<()> {
    use intercat_daemon::{git_tool::GitTool, push_inspect::Inspector, push_lfs::Tool};
    use std::{fs, path::Path};
    use tokio::sync::watch;
    let root = tempfile::tempdir()?;
    let home = root.path().join("home");
    let source = root.path().join("source");
    fs::create_dir(&home)?;
    fs::create_dir(&source)?;
    fixture_git(&home, &source, &["init", "-q", "-b", "main"])?;
    fixture_git(&home, &source, &["config", "user.name", "Dummy"])?;
    fixture_git(
        &home,
        &source,
        &["config", "user.email", "dummy@example.invalid"],
    )?;
    fs::write(source.join("dummy.txt"), "dummy LFS inspection\n")?;
    fixture_git(&home, &source, &["add", "."])?;
    fixture_git(&home, &source, &["commit", "-qm", "dummy base"])?;
    fixture_git(
        &home,
        &source,
        &[
            "remote",
            "add",
            "origin",
            "https://first.example.invalid/dummy.git",
        ],
    )?;
    fixture_git(
        &home,
        &source,
        &[
            "config",
            "--add",
            "remote.origin.pushurl",
            "https://first.example.invalid/dummy.git",
        ],
    )?;
    fixture_git(
        &home,
        &source,
        &[
            "config",
            "--add",
            "remote.origin.pushurl",
            "https://second.example.invalid/dummy.git",
        ],
    )?;
    let project = intercat_daemon::project::inspect(
        uuid::Uuid::now_v7(),
        "Dummy LFS".into(),
        vec![source.clone()],
    )
    .await?;
    let folder = &project.folders[0];
    let inspector = Inspector::new(
        GitTool::system().await?,
        home.clone(),
        home.join(".config"),
        false,
    )?;
    let lfs = Tool::open(
        GitTool::system().await?,
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.tools/git-lfs-3.8.1/git-lfs"),
    )?;
    let target = inspector
        .freeze(
            folder,
            "refs/heads/main",
            "origin",
            "refs/heads/intercat/dummy",
            None,
            watch::channel(false).1,
        )
        .await?;
    let urls = inspector
        .destinations(folder, "origin", watch::channel(false).1)
        .await?;
    let endpoints = lfs
        .resolve(&inspector, folder, &target, &urls, watch::channel(false).1)
        .await?;
    assert_eq!(endpoints.len(), 2);
    assert_eq!(
        endpoints[0].api_url,
        "https://first.example.invalid/dummy.git/info/lfs"
    );
    assert_eq!(
        endpoints[1].api_url,
        "https://second.example.invalid/dummy.git/info/lfs"
    );
    fs::write(
        source.join(".lfsconfig"),
        "[lfs]\n url = https://frozen.example.invalid/objects\n[include]\n path = must-not-read\n",
    )?;
    fixture_git(&home, &source, &["add", ".lfsconfig"])?;
    fixture_git(
        &home,
        &source,
        &["commit", "-qm", "dummy frozen LFS endpoint"],
    )?;
    let target = inspector
        .freeze(
            folder,
            "refs/heads/main",
            "origin",
            "refs/heads/intercat/dummy",
            None,
            watch::channel(false).1,
        )
        .await?;
    let index = fs::read(folder.common_dir.join("index"))?;
    let head = fixture_git(&home, &source, &["rev-parse", "HEAD"])?;
    // The working file no longer agrees with the saved SHA; only the saved blob applies.
    fs::write(
        source.join(".lfsconfig"),
        "[lfs]\n url = https://mutable.example.invalid/wrong\n",
    )?;
    let endpoints = lfs
        .resolve(&inspector, folder, &target, &urls, watch::channel(false).1)
        .await?;
    assert!(
        endpoints
            .iter()
            .all(|endpoint| endpoint.api_url == "https://frozen.example.invalid/objects")
    );
    fs::write(
        home.join(".gitconfig"),
        "[lfs]\n url = https://global.example.invalid/objects\n",
    )?;
    let frozen_bindings = endpoints
        .iter()
        .map(intercat_daemon::push_lfs::Binding::capture)
        .collect::<Result<Vec<_>>>()?;
    fixture_git(
        &home,
        &source,
        &[
            "config",
            "remote.origin.lfspushurl",
            "https://remote-push.example.invalid/objects",
        ],
    )?;
    let endpoints = lfs
        .resolve(&inspector, folder, &target, &urls, watch::channel(false).1)
        .await?;
    assert!(
        endpoints
            .iter()
            .all(|endpoint| endpoint.api_url == "https://global.example.invalid/objects")
    );
    assert!(intercat_daemon::push_lfs::Binding::verify(&frozen_bindings, &endpoints).is_err());
    fixture_git(
        &home,
        &source,
        &[
            "config",
            "lfs.pushurl",
            "https://upload.example.invalid/objects",
        ],
    )?;
    let endpoints = lfs
        .resolve(&inspector, folder, &target, &urls, watch::channel(false).1)
        .await?;
    assert!(
        endpoints
            .iter()
            .all(|endpoint| endpoint.api_url == "https://upload.example.invalid/objects")
    );
    fixture_git(
        &home,
        &source,
        &["config", "lfs.pushurl", "lfs-dummy:objects"],
    )?;
    fixture_git(
        &home,
        &source,
        &[
            "config",
            "url.https://ordinary.example.invalid/.insteadOf",
            "lfs-dummy:",
        ],
    )?;
    fixture_git(
        &home,
        &source,
        &[
            "config",
            "url.https://push-alias.example.invalid/.pushInsteadOf",
            "lfs-dummy:",
        ],
    )?;
    let endpoints = lfs
        .resolve(&inspector, folder, &target, &urls, watch::channel(false).1)
        .await?;
    assert!(
        endpoints
            .iter()
            .all(|endpoint| endpoint.api_url == "https://push-alias.example.invalid/objects")
    );
    fixture_git(
        &home,
        &source,
        &[
            "config",
            "lfs.pushurl",
            "ssh://dummy@127.0.0.1:22222/dummy-lfs.git",
        ],
    )?;
    let endpoints = lfs
        .resolve(&inspector, folder, &target, &urls, watch::channel(false).1)
        .await?;
    assert_eq!(endpoints[0].api_url, "https://127.0.0.1/dummy-lfs.git");
    let destinations = endpoints[0].destinations()?;
    assert_eq!(destinations.len(), 2);
    assert!(destinations[0].display.contains(":22222/"));
    assert!(!serde_json::to_string(&destinations)?.contains("dummy@"));
    assert_eq!(fs::read(folder.common_dir.join("index"))?, index);
    assert_eq!(fixture_git(&home, &source, &["rev-parse", "HEAD"])?, head);
    assert!(fs::read_to_string(source.join(".lfsconfig"))?.contains("mutable.example.invalid"));
    Ok(())
}

#[tokio::test]
async fn ssh_aliases_are_resolved_offline_and_uri_user_port_override_the_reviewed_config()
-> Result<()> {
    use intercat_daemon::{
        git_tool::GitTool, project::Folder, push_inspect::Inspector, push_ssh_config::Configuration,
    };
    use intercat_git::Repository;
    use std::{fs, net::TcpListener};
    use tokio::sync::watch;
    let root = tempfile::tempdir()?;
    let home = root.path().join("home with spaces");
    let source = root.path().join("source");
    fs::create_dir_all(home.join(".ssh"))?;
    fs::create_dir(&source)?;
    fixture_git(&home, &source, &["init", "-q", "-b", "main"])?;
    fixture_git(&home, &source, &["config", "user.name", "Dummy"])?;
    fixture_git(
        &home,
        &source,
        &["config", "user.email", "dummy@example.invalid"],
    )?;
    fs::write(source.join("dummy.txt"), "dummy SSH inspection\n")?;
    fixture_git(&home, &source, &["add", "."])?;
    fixture_git(&home, &source, &["commit", "-qm", "dummy SSH"])?;
    let source = Repository::worktree_source(&source, None)?;
    let folder = Folder {
        id: uuid::Uuid::now_v7().to_string(),
        name: "Dummy".into(),
        path: source.root.clone(),
        root: source.root.clone(),
        common_dir: source.common_dir.clone(),
        base: source.head.to_string(),
        base_branch: Some("main".into()),
    };
    let inspector = Inspector::new(
        GitTool::system().await?,
        home.clone(),
        home.join(".config"),
        false,
    )?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    fs::write(
        home.join(".ssh/config"),
        format!(
            "Host dummy-alias\n HostName 127.0.0.1\n Port {port}\n User generated-dummy\n IdentityAgent none\n"
        ),
    )?;
    let url = "ssh://dummy-alias/safe/dummy.git";
    let first = Configuration::inspect(&inspector, &folder, url, watch::channel(false).1)
        .await?
        .binding(url)?;
    assert_eq!(first.host, "127.0.0.1");
    assert_eq!(first.port, port);
    assert!(!serde_json::to_string(&first)?.contains("generated-dummy"));
    let explicit = "ssh://uri-user@dummy-alias:33333/safe/dummy.git";
    let second = Configuration::inspect(&inspector, &folder, explicit, watch::channel(false).1)
        .await?
        .binding(explicit)?;
    assert_eq!(second.port, 33333);
    assert_ne!(first.user_fingerprint, second.user_fingerprint);
    fs::write(
        home.join(".ssh/config"),
        "Host dummy-alias\n HostName 127.0.0.2\n Port 44444\n User generated-dummy\n IdentityAgent none\n",
    )?;
    let moved = Configuration::inspect(&inspector, &folder, url, watch::channel(false).1)
        .await?
        .binding(url)?;
    assert_ne!(first, moved);
    let marker = root.path().join("must-not-run");
    fs::write(
        home.join(".ssh/config"),
        format!(
            "Match exec \"/usr/bin/touch {}\"\n HostName 127.0.0.1\n",
            marker.display()
        ),
    )?;
    assert!(
        Configuration::inspect(&inspector, &folder, url, watch::channel(false).1)
            .await
            .is_err()
    );
    assert!(!marker.exists());
    let included = source.root.join("ssh-settings");
    fs::write(&included, "Host *\n HostName 127.0.0.1\n")?;
    fs::write(
        home.join(".ssh/config"),
        format!("Include {}\n", included.display()),
    )?;
    assert!(
        Configuration::inspect(&inspector, &folder, url, watch::channel(false).1)
            .await
            .is_err()
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        fixture_git(&home, &source.root, &["rev-parse", "HEAD"])?,
        source.head.to_string()
    );
    Ok(())
}
