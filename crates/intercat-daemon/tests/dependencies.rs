use base64::{Engine as _, engine::general_purpose::STANDARD};
use intercat_daemon::{
    Result,
    checkout::{Checkout, Configuration},
    dependencies::{Cache, FailureKind, NpmTool, Plan, Preparer},
};
use intercat_git::{ObjectId, Repository, WorktreeSource};
use serde_json::{Value, json};
use sha2::{Digest, Sha512};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::watch,
    task::JoinHandle,
};

#[derive(Clone)]
struct Reply {
    status: u16,
    body: Vec<u8>,
    location: Option<String>,
    delay: Duration,
}
impl Reply {
    fn ok(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            body,
            location: None,
            delay: Duration::ZERO,
        }
    }
}
struct Registry {
    origin: String,
    routes: Arc<Mutex<BTreeMap<String, Reply>>>,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}
impl Registry {
    async fn new() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let origin = format!("http://{}/", listener.local_addr()?);
        let routes = Arc::new(Mutex::new(BTreeMap::<String, Reply>::new()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let values = routes.clone();
        let seen = requests.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut bytes = Vec::new();
                let mut buffer = [0; 1024];
                while !bytes.ends_with(b"\r\n\r\n") && bytes.len() < 8192 {
                    let Ok(count) = socket.read(&mut buffer).await else {
                        break;
                    };
                    if count == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..count]);
                }
                let path = String::from_utf8_lossy(&bytes)
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                seen.lock().unwrap().push(path.clone());
                let reply = values.lock().unwrap().get(&path).cloned().unwrap_or(Reply {
                    status: 404,
                    body: vec![],
                    location: None,
                    delay: Duration::ZERO,
                });
                tokio::time::sleep(reply.delay).await;
                let location = reply
                    .location
                    .map(|location| format!("Location: {location}\r\n"))
                    .unwrap_or_default();
                let head = format!(
                    "HTTP/1.1 {} response\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",
                    reply.status,
                    reply.body.len(),
                    location
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(&reply.body).await;
            }
        });
        Ok(Self {
            origin,
            routes,
            requests,
            task,
        })
    }
    fn put(&self, path: &str, reply: Reply) {
        self.routes.lock().unwrap().insert(path.into(), reply);
    }
    fn stop(&self) {
        self.task.abort();
    }
}
impl Drop for Registry {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("/usr/bin/git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "core.hooksPath=/dev/null"])
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
struct Fixture {
    root: tempfile::TempDir,
    source: WorktreeSource,
    registry: Registry,
    archive: Vec<u8>,
    npm: NpmTool,
    path: String,
    lock: Value,
}
impl Fixture {
    async fn new(install: Option<String>) -> Result<Self> {
        let root = tempfile::tempdir()?;
        let registry = Registry::new().await?;
        let package = root.path().join("package");
        fs::create_dir(&package)?;
        let mut manifest = json!({"name":"fixture-dep","version":"1.0.0","main":"index.js"});
        if let Some(script) = install {
            manifest["scripts"] = json!({"install":"node install.cjs"});
            fs::write(package.join("install.cjs"), script)?;
        }
        fs::write(package.join("package.json"), manifest.to_string())?;
        fs::write(package.join("index.js"), "module.exports = 42;\n")?;
        let archive_path = root.path().join("package.tgz");
        let status = Command::new("/usr/bin/tar")
            .args([
                "-czf",
                archive_path.to_str().unwrap(),
                "-C",
                root.path().to_str().unwrap(),
                "package",
            ])
            .env("COPYFILE_DISABLE", "1")
            .status()?;
        assert!(status.success());
        let archive = fs::read(&archive_path)?;
        registry.put("/package.tgz", Reply::ok(archive.clone()));
        let integrity = format!("sha512-{}", STANDARD.encode(Sha512::digest(&archive)));
        let source = root.path().join("source");
        fs::create_dir(&source)?;
        git(&source, &["init", "-q", "-b", "main"]);
        git(&source, &["config", "user.name", "Fixture"]);
        git(
            &source,
            &["config", "user.email", "fixture@example.invalid"],
        );
        fs::write(source.join("package.json"),json!({"name":"fixture","version":"1.0.0","dependencies":{"fixture-dep":"1.0.0"},"scripts":{"prepare":"node prepare.cjs","test":"node -e \"if (require('fixture-dep') !== 42) process.exit(1)\""}}).to_string())?;
        let lock = json!({"name":"fixture","version":"1.0.0","lockfileVersion":3,"requires":true,"packages":{"":{"name":"fixture","version":"1.0.0","dependencies":{"fixture-dep":"1.0.0"}},"node_modules/fixture-dep":{"version":"1.0.0","resolved":format!("{}package.tgz",registry.origin),"integrity":integrity,"hasInstallScript":manifest["scripts"].is_object()}}});
        fs::write(source.join("package-lock.json"), lock.to_string())?;
        fs::write(
            source.join("prepare.cjs"),
            "const fs=require('node:fs'); fs.mkdirSync('dist',{recursive:true}); fs.writeFileSync('dist/result',fs.readFileSync('input.txt'));\n",
        )?;
        fs::write(source.join("input.txt"), "one\n")?;
        fs::write(source.join(".gitignore"), "node_modules/\ndist/\n")?;
        fs::write(
            source.join(".npmrc"),
            "update-notifier=false\naudit=false\nfund=false\n",
        )?;
        git(&source, &["add", "."]);
        git(&source, &["commit", "-qm", "base"]);
        let source = Repository::worktree_source(&source, None)?;
        let path = std::env::var("PATH")?;
        let npm = NpmTool::discover(&path)?.ok_or("npm unavailable")?;
        Ok(Self {
            root,
            source,
            registry,
            archive,
            npm,
            path,
            lock,
        })
    }
    fn cache(&self) -> Result<Cache> {
        Ok(Cache::new(
            self.root.path().join("cache"),
            std::slice::from_ref(&self.registry.origin),
        )?)
    }
    async fn checkout(&self) -> Result<Checkout> {
        let tools = vec![
            self.npm.node.parent().unwrap().parent().unwrap().into(),
            self.npm.cli.parent().unwrap().parent().unwrap().into(),
        ];
        let checkout = Checkout::create(
            &self
                .root
                .path()
                .join(format!("job-{}", uuid::Uuid::now_v7())),
            self.source.clone(),
            self.source.head,
            Configuration::capture(&self.source)?,
            self.path.clone().into(),
            tools,
            no_cancel(),
        )
        .await?;
        checkout
            .deploy(
                Repository::open(&self.source.root)?.tree_id(self.source.head)?,
                no_cancel(),
            )
            .await?;
        Ok(checkout)
    }
    fn tree(&self) -> ObjectId {
        Repository::open(&self.source.root)
            .unwrap()
            .tree_id(self.source.head)
            .unwrap()
    }
    fn commit(&mut self) {
        git(&self.source.root, &["add", "."]);
        git(&self.source.root, &["commit", "-qm", "change"]);
        self.source.head = git(&self.source.root, &["rev-parse", "HEAD"])
            .parse()
            .unwrap();
    }
}

fn add_local_dependencies(f: &mut Fixture) -> Result<()> {
    let tiny = f.source.root.join("vendor/tiny-dep");
    let leaf = f.source.root.join("vendor/leaf");
    fs::create_dir_all(&tiny)?;
    fs::create_dir_all(&leaf)?;
    fs::write(tiny.join("package.json"),json!({"name":"tiny-dep","version":"1.0.0","main":"index.js","dependencies":{"leaf":"file:../leaf","fixture-dep":"1.0.0"}}).to_string())?;
    fs::write(
        tiny.join("index.js"),
        "module.exports = require('leaf') + require('fixture-dep');\n",
    )?;
    fs::write(
        leaf.join("package.json"),
        json!({"name":"leaf","version":"1.0.0","main":"index.js"}).to_string(),
    )?;
    fs::write(leaf.join("index.js"), "module.exports = 1;\n")?;
    let manifest = f.source.root.join("package.json");
    let mut value: Value = serde_json::from_slice(&fs::read(&manifest)?)?;
    value["dependencies"]["tiny-dep"] = json!("file:vendor/tiny-dep");
    fs::write(manifest, value.to_string())?;
    let user_config = f.root.path().join("npm-userconfig");
    let global_config = f.root.path().join("npm-globalconfig");
    fs::write(&user_config, [])?;
    fs::write(&global_config, [])?;
    let output = Command::new(&f.npm.node)
        .arg(&f.npm.cli)
        .args([
            "install",
            "--package-lock-only",
            "--ignore-scripts",
            "--offline",
            "--no-audit",
            "--no-fund",
        ])
        .arg("--userconfig")
        .arg(user_config)
        .arg("--globalconfig")
        .arg(global_config)
        .arg("--cache")
        .arg(f.root.path().join("lock-cache"))
        .current_dir(&f.source.root)
        .env_clear()
        .env("PATH", &f.path)
        .env("HOME", f.root.path())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    f.lock = serde_json::from_slice(&fs::read(f.source.root.join("package-lock.json"))?)?;
    f.commit();
    Ok(())
}

#[tokio::test]
async fn local_directory_dependencies_use_saved_sources_and_invalidate_on_source_only_edits()
-> Result<()> {
    let mut f = Fixture::new(None).await?;
    add_local_dependencies(&mut f)?;
    let repo = Repository::open(&f.source.root)?;
    let before = Plan::inspect(&repo, f.tree())?;
    assert_eq!(before.installs.len(), 1);
    assert_eq!(before.installs[0].artifacts.len(), 1);
    let index = fs::read(f.source.root.join(".git/index"))?;
    let lock = fs::read(f.source.root.join("package-lock.json"))?;
    let checkout = f.checkout().await?;
    // An edit in the original checkout must not leak into the saved candidate.
    fs::write(
        f.source.root.join("vendor/leaf/index.js"),
        "module.exports = 99;\n",
    )?;
    let mut prepare = Preparer::new(f.cache()?, Some(f.npm.clone()));
    let first = prepare
        .prepare(&checkout, &repo, f.tree(), false, no_cancel())
        .await?;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].downloads, 1);
    let checked = checkout
        .run_check(
            "node -e \"if(require('tiny-dep')!==43)process.exit(1)\"",
            &std::collections::BTreeMap::new(),
            no_cancel(),
            Duration::from_secs(30),
        )
        .await?;
    assert!(
        checked.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert_eq!(
        fs::canonicalize(checkout.workdir.join("node_modules/tiny-dep"))?,
        fs::canonicalize(checkout.workdir.join("vendor/tiny-dep"))?
    );
    assert_eq!(fs::read(f.source.root.join(".git/index"))?, index);
    assert_eq!(fs::read(f.source.root.join("package-lock.json"))?, lock);
    assert!(!f.source.root.join("node_modules").exists());
    fs::write(
        f.source.root.join("vendor/leaf/index.js"),
        "module.exports = 2;\n",
    )?;
    f.commit();
    let after = Plan::inspect(&repo, f.tree())?;
    assert_ne!(before.fingerprint, after.fingerprint);
    checkout.deploy(f.tree(), no_cancel()).await?;
    let second = prepare
        .prepare(&checkout, &repo, f.tree(), false, no_cancel())
        .await?;
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].cache_hits, 1);
    let checked = checkout
        .run_check(
            "node -e \"if(require('tiny-dep')!==44)process.exit(1)\"",
            &std::collections::BTreeMap::new(),
            no_cancel(),
            Duration::from_secs(30),
        )
        .await?;
    assert!(
        checked.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    Ok(())
}

#[tokio::test]
async fn local_links_require_matching_locked_manifests_and_repository_targets() -> Result<()> {
    let mut f = Fixture::new(None).await?;
    add_local_dependencies(&mut f)?;
    let manifest = fs::read(f.source.root.join("package.json"))?;
    let original = f.lock.clone();
    let mut optional: Value = serde_json::from_slice(&manifest)?;
    optional["optionalDependencies"] = json!({"tiny-dep":"file:vendor/tiny-dep"});
    let mut optional_lock = original.clone();
    optional_lock["packages"][""]["dependencies"]
        .as_object_mut()
        .unwrap()
        .remove("tiny-dep");
    optional_lock["packages"][""]["optionalDependencies"] =
        json!({"tiny-dep":"file:vendor/tiny-dep"});
    fs::write(f.source.root.join("package.json"), optional.to_string())?;
    fs::write(
        f.source.root.join("package-lock.json"),
        optional_lock.to_string(),
    )?;
    f.commit();
    assert!(Plan::inspect(&Repository::open(&f.source.root)?, f.tree()).is_ok());
    fs::write(
        f.source.root.join("package-lock.json"),
        original.to_string(),
    )?;
    let mut changed: Value = serde_json::from_slice(&manifest)?;
    changed["dependencies"]["tiny-dep"] = json!("file:vendor/leaf");
    fs::write(f.source.root.join("package.json"), changed.to_string())?;
    f.commit();
    assert_eq!(
        Plan::inspect(&Repository::open(&f.source.root)?, f.tree())
            .unwrap_err()
            .kind,
        FailureKind::Configuration
    );
    fs::write(f.source.root.join("package.json"), manifest)?;
    for target in [
        "../../outside",
        "/private/tmp/outside",
        ".git",
        "vendor/missing",
        "vendor/leaf",
    ] {
        let mut lock = original.clone();
        lock["packages"]["node_modules/tiny-dep"]["resolved"] = json!(target);
        fs::write(f.source.root.join("package-lock.json"), lock.to_string())?;
        f.commit();
        assert_eq!(
            Plan::inspect(&Repository::open(&f.source.root)?, f.tree())
                .unwrap_err()
                .kind,
            FailureKind::Configuration,
            "{target}"
        );
    }
    // npm-shrinkwrap.json is the selected lock when both filenames exist.
    fs::write(
        f.source.root.join("npm-shrinkwrap.json"),
        original.to_string(),
    )?;
    f.commit();
    assert!(Plan::inspect(&Repository::open(&f.source.root)?, f.tree()).is_ok());
    assert!(f.registry.requests.lock().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn installs_from_verified_archives_and_forces_fresh_generated_files_before_check()
-> Result<()> {
    let mut f = Fixture::new(Some(
        "require('fs').writeFileSync('installed','yes');".into(),
    ))
    .await?;
    let checkout = f.checkout().await?;
    let cache = f.cache()?;
    let plan = Plan::inspect(&Repository::open(&f.source.root)?, f.tree())?;
    let cached = cache.path(&plan.installs[0].artifacts[0]);
    let mut prepare = Preparer::new(cache, Some(f.npm.clone()));
    let first = prepare
        .prepare(
            &checkout,
            &Repository::open(&f.source.root)?,
            f.tree(),
            false,
            no_cancel(),
        )
        .await?;
    assert_eq!(first.len(), 1);
    assert_eq!((first[0].downloads, first[0].cache_hits), (1, 0));
    assert_eq!(
        fs::read_to_string(checkout.workdir.join("dist/result"))?,
        "one\n"
    );
    assert!(
        checkout
            .workdir
            .join("node_modules/fixture-dep/installed")
            .is_file()
    );
    assert!(!f.source.root.join("node_modules").exists());
    assert_eq!(fs::read(&cached)?, f.archive);
    assert!(
        prepare
            .prepare(
                &checkout,
                &Repository::open(&f.source.root)?,
                f.tree(),
                false,
                no_cancel()
            )
            .await?
            .is_empty()
    );
    fs::write(f.source.root.join("input.txt"), "two\n")?;
    f.commit();
    checkout.deploy(f.tree(), no_cancel()).await?;
    assert!(
        prepare
            .prepare(
                &checkout,
                &Repository::open(&f.source.root)?,
                f.tree(),
                false,
                no_cancel()
            )
            .await?
            .is_empty()
    );
    assert_eq!(
        fs::read_to_string(checkout.workdir.join("dist/result"))?,
        "one\n"
    );
    f.registry.stop();
    checkout
        .git(&["clean".into(), "-ffdx".into()], true, no_cancel())
        .await?;
    let final_run = prepare
        .prepare(
            &checkout,
            &Repository::open(&f.source.root)?,
            f.tree(),
            true,
            no_cancel(),
        )
        .await?;
    assert!(final_run[0].forced);
    assert_eq!(final_run[0].cache_hits, 1);
    assert_eq!(
        fs::read_to_string(checkout.workdir.join("dist/result"))?,
        "two\n"
    );
    let result = checkout
        .run_check(
            "npm test",
            &std::collections::BTreeMap::new(),
            no_cancel(),
            Duration::from_secs(10),
        )
        .await?;
    assert!(
        result.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}

#[tokio::test]
async fn installation_scripts_cannot_connect_to_registry_or_modify_source_or_shared_archives()
-> Result<()> {
    let mut f = Fixture::new(None).await?;
    let package = f.root.path().join("package");
    let script = format!(
        "const fs=require('fs'); const http=require('http'); try {{fs.writeFileSync({},'bad');process.exit(9)}} catch {{}} http.get({},()=>process.exit(10)).on('error',()=>fs.writeFileSync('network-denied','yes'));",
        serde_json::to_string(&f.source.root.join("escaped").to_str().unwrap())?,
        serde_json::to_string(&format!("{}leak", f.registry.origin))?
    );
    fs::write(package.join("package.json"),json!({"name":"fixture-dep","version":"1.0.0","main":"index.js","scripts":{"install":"node install.cjs"}}).to_string())?;
    fs::write(package.join("install.cjs"), script)?;
    let archive = f.root.path().join("script.tgz");
    assert!(
        Command::new("/usr/bin/tar")
            .args([
                "-czf",
                archive.to_str().unwrap(),
                "-C",
                f.root.path().to_str().unwrap(),
                "package"
            ])
            .env("COPYFILE_DISABLE", "1")
            .status()?
            .success()
    );
    f.archive = fs::read(archive)?;
    f.registry.put("/package.tgz", Reply::ok(f.archive.clone()));
    f.lock["packages"]["node_modules/fixture-dep"]["integrity"] =
        format!("sha512-{}", STANDARD.encode(Sha512::digest(&f.archive))).into();
    f.lock["packages"]["node_modules/fixture-dep"]["hasInstallScript"] = true.into();
    fs::write(f.source.root.join("package-lock.json"), f.lock.to_string())?;
    f.commit();
    let checkout = f.checkout().await?;
    let cache = f.cache()?;
    let plan = Plan::inspect(&Repository::open(&f.source.root)?, f.tree())?;
    let cached = cache.path(&plan.installs[0].artifacts[0]);
    let mut prepare = Preparer::new(cache, Some(f.npm.clone()));
    prepare
        .prepare(
            &checkout,
            &Repository::open(&f.source.root)?,
            f.tree(),
            true,
            no_cancel(),
        )
        .await?;
    assert!(!f.source.root.join("escaped").exists());
    assert!(
        checkout
            .workdir
            .join("node_modules/fixture-dep/network-denied")
            .exists()
    );
    assert_eq!(*f.registry.requests.lock().unwrap(), vec!["/package.tgz"]);
    let file = serde_json::to_string(cached.to_str().unwrap())?;
    let js = format!(
        "const fs=require('fs'); try {{fs.chmodSync({file},0o600); fs.writeFileSync({file},'bad'); process.exit(9)}} catch {{console.log('blocked')}}"
    );
    let script = format!("node -e '{}'", js.replace('\'', "'\\''"));
    let blocked = checkout
        .run_check(
            &script,
            &std::collections::BTreeMap::new(),
            no_cancel(),
            Duration::from_secs(10),
        )
        .await?;
    assert!(
        blocked.success(),
        "{}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    assert_eq!(fs::read(cached)?, f.archive);
    Ok(())
}

#[tokio::test]
async fn manifest_nested_manifest_and_install_configuration_changes_invalidate_preparation()
-> Result<()> {
    let mut f = Fixture::new(None).await?;
    let original = Plan::inspect(&Repository::open(&f.source.root)?, f.tree())?;
    fs::write(
        f.source.root.join(".npmrc"),
        "update-notifier=false\naudit=false\nfund=true\n",
    )?;
    f.commit();
    let configuration = Plan::inspect(&Repository::open(&f.source.root)?, f.tree())?;
    assert_ne!(configuration.fingerprint, original.fingerprint);
    fs::create_dir_all(f.source.root.join("packages/child"))?;
    fs::write(
        f.source.root.join("packages/child/package.json"),
        "{\"name\":\"child\"}",
    )?;
    f.commit();
    assert_ne!(
        Plan::inspect(&Repository::open(&f.source.root)?, f.tree())?.fingerprint,
        configuration.fingerprint
    );
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(f.source.root.join("package.json"))?)?;
    manifest["dependencies"]["fixture-dep"] = "2.0.0".into();
    fs::write(f.source.root.join("package.json"), manifest.to_string())?;
    f.commit();
    let checkout = f.checkout().await?;
    let mut prepare = Preparer::new(f.cache()?, Some(f.npm.clone()));
    let failure = prepare
        .prepare(
            &checkout,
            &Repository::open(&f.source.root)?,
            f.tree(),
            false,
            no_cancel(),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.kind, FailureKind::Configuration);
    Ok(())
}

#[tokio::test]
async fn checksum_auth_redirect_and_unapproved_registry_fail_before_installation() -> Result<()> {
    let mut f = Fixture::new(None).await?;
    let mut lock = f.lock.clone();
    lock["packages"]["node_modules/fixture-dep"]["integrity"] =
        format!("sha512-{}", STANDARD.encode([1; 64])).into();
    fs::write(f.source.root.join("package-lock.json"), lock.to_string())?;
    f.commit();
    let plan = Plan::inspect(&Repository::open(&f.source.root)?, f.tree())?;
    let cache = f.cache()?;
    assert_eq!(
        cache
            .fetch(&plan.installs[0].artifacts[0], no_cancel())
            .await
            .unwrap_err()
            .kind,
        FailureKind::Integrity
    );
    assert!(!cache.path(&plan.installs[0].artifacts[0]).exists());
    f.registry.put(
        "/package.tgz",
        Reply {
            status: 401,
            body: vec![],
            location: None,
            delay: Duration::ZERO,
        },
    );
    assert_eq!(
        cache
            .fetch(&plan.installs[0].artifacts[0], no_cancel())
            .await
            .unwrap_err()
            .kind,
        FailureKind::Credentials
    );
    let outside = Registry::new().await?;
    f.registry.put(
        "/package.tgz",
        Reply {
            status: 302,
            body: vec![],
            location: Some(format!("{}package.tgz", outside.origin)),
            delay: Duration::ZERO,
        },
    );
    assert_eq!(
        cache
            .fetch(&plan.installs[0].artifacts[0], no_cancel())
            .await
            .unwrap_err()
            .kind,
        FailureKind::Credentials
    );
    assert!(outside.requests.lock().unwrap().is_empty());
    let denied = Cache::new(
        f.root.path().join("denied-cache"),
        &["https://registry.npmjs.org/".into()],
    )?;
    let count = f.registry.requests.lock().unwrap().len();
    assert_eq!(
        denied
            .fetch(&plan.installs[0].artifacts[0], no_cancel())
            .await
            .unwrap_err()
            .kind,
        FailureKind::Credentials
    );
    assert_eq!(f.registry.requests.lock().unwrap().len(), count);
    Ok(())
}

#[tokio::test]
async fn cancellation_never_publishes_a_partial_download_and_concurrent_downloads_converge()
-> Result<()> {
    let f = Fixture::new(None).await?;
    let cache = f.cache()?;
    let plan = Plan::inspect(&Repository::open(&f.source.root)?, f.tree())?;
    let artifact = &plan.installs[0].artifacts[0];
    f.registry.put(
        "/package.tgz",
        Reply {
            delay: Duration::from_secs(10),
            ..Reply::ok(f.archive.clone())
        },
    );
    let (cancel, receive) = watch::channel(false);
    let fetch = cache.fetch(artifact, receive);
    let stop = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.send(true).unwrap();
    };
    let (result, _) = tokio::join!(fetch, stop);
    assert_eq!(result.unwrap_err().kind, FailureKind::Cancelled);
    assert!(!cache.path(artifact).exists());
    f.registry.stop();
    let second = Fixture::new(None).await?;
    let cache = second.cache()?;
    let plan = Plan::inspect(&Repository::open(&second.source.root)?, second.tree())?;
    let artifact = &plan.installs[0].artifacts[0];
    let (one, two) = tokio::join!(
        cache.fetch(artifact, no_cancel()),
        cache.fetch(artifact, no_cancel())
    );
    assert_eq!(one?.0, two?.0);
    assert_eq!(fs::read(cache.path(artifact))?, second.archive);
    Ok(())
}

#[tokio::test]
async fn credential_files_and_missing_locks_are_not_silently_accepted() -> Result<()> {
    let mut f = Fixture::new(None).await?;
    fs::write(
        f.source.root.join(".npmrc"),
        "//registry.npmjs.org/:_authToken=fixture-only\n",
    )?;
    f.commit();
    assert_eq!(
        Plan::inspect(&Repository::open(&f.source.root)?, f.tree())
            .unwrap_err()
            .kind,
        FailureKind::Credentials
    );
    fs::write(f.source.root.join(".npmrc"), "audit=false\n")?;
    fs::remove_file(f.source.root.join("package-lock.json"))?;
    f.commit();
    assert_eq!(
        Plan::inspect(&Repository::open(&f.source.root)?, f.tree())
            .unwrap_err()
            .kind,
        FailureKind::Configuration
    );
    Ok(())
}

async fn candidate_fixture(
    f: &Fixture,
) -> Result<(
    intercat_daemon::integration::Job,
    intercat_daemon::candidate::Settings,
)> {
    use intercat_daemon::{
        candidate::Settings,
        check_run::CheckCommand,
        checkpoint::Checkpoint,
        integration::{self, Job, Status, Track},
    };
    let repo = Repository::open(&f.source.root)?;
    let session = uuid::Uuid::now_v7();
    let slug = "dependency-fixture";
    let marker = repo.write_blob(format!("Intercat session {session}\n").as_bytes())?;
    repo.compare_and_swap(&format!("refs/intercat/{slug}/session"), None, marker)?;
    let mut entries: BTreeMap<String, ObjectId> = repo
        .saved_files(f.source.head)?
        .into_iter()
        .map(|(path, file)| (path, file.object))
        .collect();
    entries.insert("input.txt".into(), repo.write_blob(b"two\n")?);
    let tree = repo.write_tree(
        &entries
            .iter()
            .map(|(path, id)| (path.as_str(), *id))
            .collect::<Vec<_>>(),
    )?;
    let checkpoint = repo.checkpoint_commit(tree, f.source.head, "change input")?;
    let reference = format!("refs/intercat/{slug}/tracks/A-checkpoint");
    repo.compare_and_swap(&reference, None, checkpoint)?;
    repo.sync_objects(&[marker, checkpoint])?;
    let mut job = Job {
        lineage: Default::default(),
        omitted_conflicts: Default::default(),
        path_review: Default::default(),
        path_state: None,
        retirements: Vec::new(),
        order_review: None,
        assumptions: Vec::new(),
        assumptions_changed: false,
        assumptions_pending: 0,
        assumptions_failed: 0,
        id: uuid::Uuid::now_v7(),
        revision: 1,
        status: Status::Preparing,
        opened_at: 0,
        session_id: session.to_string(),
        slug: slug.into(),
        session_name: "Dependencies".into(),
        project: intercat_daemon::project::inspect(
            uuid::Uuid::now_v7(),
            "Dummy project".into(),
            vec![f.source.root.clone()],
        )
        .await?
        .only_folder()?
        .clone(),
        tracks: vec![Track {
            id: uuid::Uuid::now_v7().to_string(),
            label: "A".into(),
            title: "Change input".into(),
            basis: intercat_daemon::lineage::Basis::root(f.source.head.to_string()),
            checkpoint: Some(Checkpoint {
                turn_id: uuid::Uuid::now_v7(),
                sha: checkpoint.to_string(),
                reference,
                unmerged_paths: vec![],
                live_processes: false,
            }),
            checkpoint_count: 1,
            last_clip: Some(1),
            included: true,
            integration_count: 0,
        }],
        plan: None,
        undo: None,
        error: None,
        candidate: None,
        messages: std::collections::BTreeMap::new(),
        publication: None,
        kept_secrets: Default::default(),
        contributions: Default::default(),
        fixes: Default::default(),
    };
    job.lineage.points.push(intercat_daemon::lineage::Point {
        views: Vec::new(),
        parents: Default::default(),
        track: job.tracks[0].id.clone(),
        number: 1,
        sha: checkpoint.to_string(),
        base_at: f.source.head.to_string(),
        base_inherits_at: Default::default(),
        inherits: Default::default(),
    });
    job.plan = Some(integration::freeze(&job)?);
    job.plan.as_mut().unwrap().coverage = Some(Default::default());
    job.plan = Some(integration::plan(&job)?);
    let settings = Settings {
        checks_revision: 0,
        history: false,
        environment: None,
        scanner: intercat_daemon::secret_scan::Engine::capture()?,
        command: CheckCommand::Auto,
        npm: Some(f.npm.clone()),
        registry_origins: vec![f.registry.origin.clone()],
        timeout_ms: 10000,
        path: f.path.clone(),
        toolchain: vec![
            f.npm.node.parent().unwrap().parent().unwrap().into(),
            f.npm.cli.parent().unwrap().parent().unwrap().into(),
        ],
        checkout: Configuration::capture(&f.source)?,
        skip_hooks: false,
    };
    Ok((job, settings))
}

#[tokio::test]
async fn candidate_installs_before_hooks_and_reinstalls_before_check_without_committing_dependencies()
-> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut f = Fixture::new(None).await?;
    fs::write(f.source.root.join(".gitignore"), "dist/\n")?;
    f.commit();
    let hook = f.source.common_dir.join("hooks/pre-commit");
    fs::write(
        &hook,
        "#!/bin/sh\nnode -e \"require('fs').writeFileSync('used.txt', String(require('fixture-dep')))\"\ngit add used.txt\n",
    )?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;
    let (job, settings) = candidate_fixture(&f).await?;
    let candidate = intercat_daemon::candidate::build(
        &job,
        &f.root.path().join("candidate"),
        &f.root.path().join("cache"),
        &settings,
        &BTreeMap::new(),
        no_cancel(),
    )
    .await?;
    assert_eq!(
        candidate.state,
        intercat_daemon::candidate::State::Checked,
        "{candidate:?}"
    );
    assert_eq!(candidate.dependency_runs.len(), 2);
    assert!(!candidate.dependency_runs[0].forced && candidate.dependency_runs[1].forced);
    let files = Repository::open(&f.source.root)?.saved_files(candidate.head.parse()?)?;
    assert!(files.contains_key("used.txt"));
    assert!(!files.keys().any(|path| path.starts_with("node_modules/")));
    assert_eq!(
        fs::read_to_string(f.source.root.join("input.txt"))?,
        "one\n"
    );
    assert!(!f.source.root.join("node_modules").exists());
    Ok(())
}

#[tokio::test]
async fn a_forced_install_that_changes_tracked_files_stops_before_check() -> Result<()> {
    let mut f = Fixture::new(None).await?;
    fs::write(f.source.root.join("counter"), "0")?;
    fs::write(
        f.source.root.join("prepare.cjs"),
        "const fs=require('fs'); fs.writeFileSync('counter',String(Number(fs.readFileSync('counter','utf8'))+1));\n",
    )?;
    f.commit();
    let (job, settings) = candidate_fixture(&f).await?;
    let candidate = intercat_daemon::candidate::build(
        &job,
        &f.root.path().join("candidate"),
        &f.root.path().join("cache"),
        &settings,
        &BTreeMap::new(),
        no_cancel(),
    )
    .await?;
    assert_eq!(
        candidate.state,
        intercat_daemon::candidate::State::DirtyBeforeCheck,
        "{candidate:?}"
    );
    assert!(candidate.check.is_none());
    Ok(())
}

#[tokio::test]
async fn a_corrupt_shared_archive_is_replaced_only_after_reverification() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new(None).await?;
    let cache = f.cache()?;
    let plan = Plan::inspect(&Repository::open(&f.source.root)?, f.tree())?;
    let artifact = &plan.installs[0].artifacts[0];
    let (path, _) = cache.fetch(artifact, no_cancel()).await?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    fs::write(&path, "corrupt")?;
    let (repaired, hit) = cache.fetch(artifact, no_cancel()).await?;
    assert!(!hit);
    assert_eq!(repaired, path);
    assert_eq!(fs::read(path)?, f.archive);
    assert_eq!(f.registry.requests.lock().unwrap().len(), 2);
    Ok(())
}

#[tokio::test]
#[ignore = "requires access to the public npm registry"]
async fn live_https_registry_archive_matches_its_declared_integrity() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let manifest: Value = reqwest::Client::builder()
        .no_proxy()
        .build()?
        .get("https://registry.npmjs.org/is-number/7.0.0")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let source = directory.path().join("source");
    fs::create_dir(&source)?;
    git(&source, &["init", "-q", "-b", "main"]);
    git(&source, &["config", "user.name", "Fixture"]);
    git(
        &source,
        &["config", "user.email", "fixture@example.invalid"],
    );
    fs::write(
        source.join("package.json"),
        "{\"dependencies\":{\"is-number\":\"7.0.0\"}}",
    )?;
    fs::write(source.join("package-lock.json"),json!({"lockfileVersion":3,"packages":{"":{"dependencies":{"is-number":"7.0.0"}},"node_modules/is-number":{"version":"7.0.0","resolved":manifest["dist"]["tarball"],"integrity":manifest["dist"]["integrity"]}}}).to_string())?;
    git(&source, &["add", "."]);
    git(&source, &["commit", "-qm", "fixture"]);
    let repo = Repository::open(&source)?;
    let head: ObjectId = git(&source, &["rev-parse", "HEAD"]).parse()?;
    let plan = Plan::inspect(&repo, repo.tree_id(head)?)?;
    let cache = Cache::new(
        directory.path().join("cache"),
        &["https://registry.npmjs.org/".into()],
    )?;
    let (path, hit) = cache
        .fetch(&plan.installs[0].artifacts[0], no_cancel())
        .await?;
    assert!(!hit && path.metadata()?.len() > 0);
    assert!(
        cache
            .fetch(&plan.installs[0].artifacts[0], no_cancel())
            .await?
            .1
    );
    Ok(())
}
