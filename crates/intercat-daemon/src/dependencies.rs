//! Locked npm downloads and offline installation. Only the daemon fetches
//! verified archives; project and dependency scripts never receive network access.
use crate::{
    Result as DaemonResult,
    checkout::{Checkout, shell_quote},
    command::Output,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use intercat_git::{ObjectId, Repository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::watch;

const ARCHIVE_LIMIT: u64 = 64 * 1024 * 1024;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FailureKind {
    Configuration,
    Temporary,
    Credentials,
    Integrity,
    Unsupported,
    Cancelled,
    Environment,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Failure {
    pub kind: FailureKind,
    pub message: String,
    pub output: Option<String>,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Failure {}
type Result<T> = std::result::Result<T, Failure>;
fn fail(kind: FailureKind, message: impl Into<String>) -> Failure {
    Failure {
        kind,
        message: message.into(),
        output: None,
    }
}
impl From<std::io::Error> for Failure {
    fn from(_: std::io::Error) -> Self {
        fail(
            FailureKind::Environment,
            "The dependency cache could not be read or written.",
        )
    }
}
impl From<serde_json::Error> for Failure {
    fn from(_: serde_json::Error) -> Self {
        fail(
            FailureKind::Configuration,
            "A dependency manifest or lockfile is not valid JSON.",
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NpmTool {
    pub node: PathBuf,
    pub cli: PathBuf,
}
impl NpmTool {
    pub fn discover(path: &str) -> DaemonResult<Option<Self>> {
        let find = |name: &str| {
            std::env::split_paths(path)
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(name))
                .find(|file| file.is_file())
        };
        let (Some(node), Some(npm)) = (find("node"), find("npm")) else {
            return Ok(None);
        };
        let cli = npm.canonicalize()?;
        let root = cli
            .parent()
            .and_then(Path::parent)
            .ok_or("Invalid npm installation.")?;
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("package.json"))?)?;
        if manifest["name"] != "npm" {
            return Err("The selected npm executable is not an npm installation.".into());
        }
        Ok(Some(Self {
            node: node.canonicalize()?,
            cli,
        }))
    }
    fn command(&self) -> DaemonResult<String> {
        Ok(format!(
            "{} {}",
            shell_quote(self.node.as_os_str())?,
            shell_quote(self.cli.as_os_str())?
        ))
    }
}

#[derive(Clone, Debug)]
pub struct Artifact {
    url: reqwest::Url,
    digest: Vec<u8>,
    key: String,
}
#[derive(Clone, Debug)]
pub struct Install {
    pub directory: String,
    pub artifacts: Vec<Artifact>,
}
#[derive(Clone, Debug)]
pub struct Plan {
    pub fingerprint: String,
    pub installs: Vec<Install>,
}

fn relevant(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "package.json"
        || name == "package-lock.json"
        || name == "npm-shrinkwrap.json"
        || name == "pnpm-lock.yaml"
        || name == "yarn.lock"
        || name == "Cargo.toml"
        || name == "Cargo.lock"
        || name == "pyproject.toml"
        || name == "uv.lock"
        || name.starts_with("requirements") && name.ends_with(".txt")
        || [".npmrc", ".nvmrc", ".node-version", ".yarnrc.yml"].contains(&name)
}
fn has_setup(manifest: &serde_json::Value) -> bool {
    [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ]
    .iter()
    .any(|key| manifest[key].as_object().is_some_and(|map| !map.is_empty()))
        || ["preinstall", "install", "postinstall", "prepare"]
            .iter()
            .any(|key| {
                manifest["scripts"][key]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            })
}
fn matches_platform(value: &serde_json::Value, target: &str) -> bool {
    let Some(items) = value.as_array() else {
        return true;
    };
    if items
        .iter()
        .any(|v| v.as_str() == Some(&format!("!{target}")))
    {
        return false;
    }
    let positive: Vec<_> = items
        .iter()
        .filter_map(|v| v.as_str())
        .filter(|s| !s.starts_with('!'))
        .collect();
    positive.is_empty() || positive.contains(&target)
}

// npm resolves directory links relative to the lockfile's installation root.
// Normalize from the saved tree, never from the caller's mutable filesystem.
fn local_directory(directory: &str, target: &str) -> Result<String> {
    if target.is_empty() || target.contains(['\\', ':', '\0']) || target.starts_with('~') {
        return Err(fail(
            FailureKind::Configuration,
            "A local dependency path is invalid.",
        ));
    }
    let mut parts = directory
        .split('/')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();
    for part in Path::new(target).components() {
        match part {
            Component::Normal(name) => {
                let name = name.to_str().ok_or_else(|| {
                    fail(
                        FailureKind::Configuration,
                        "A local dependency path is not UTF-8.",
                    )
                })?;
                if matches!(name, ".git" | "node_modules") {
                    return Err(fail(
                        FailureKind::Configuration,
                        "Local dependencies must refer to source directories in the repository.",
                    ));
                }
                parts.push(name);
            }
            Component::CurDir => {}
            Component::ParentDir if parts.pop().is_some() => {}
            _ => {
                return Err(fail(
                    FailureKind::Configuration,
                    "A local dependency points outside the repository.",
                ));
            }
        }
    }
    if parts.is_empty() {
        return Err(fail(
            FailureKind::Configuration,
            "A local dependency cannot link the repository root.",
        ));
    }
    Ok(parts.join("/"))
}

fn dependency_table(
    value: &serde_json::Value,
    key: &str,
) -> Result<serde_json::Map<String, serde_json::Value>> {
    let mut table = value
        .get(key)
        .map(|v| {
            v.as_object()
                .cloned()
                .ok_or_else(|| fail(FailureKind::Configuration, "A dependency list is invalid."))
        })
        .transpose()?
        .unwrap_or_default();
    // npm's optionalDependencies replace declarations with the same name in
    // dependencies, and the lockfile keeps only the effective declaration.
    if key == "dependencies"
        && let Some(optional) = value["optionalDependencies"].as_object()
    {
        table.retain(|name, _| !optional.contains_key(name));
    }
    Ok(table)
}
fn verify_dependencies(manifest: &serde_json::Value, locked: &serde_json::Value) -> Result<()> {
    for key in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ] {
        if dependency_table(manifest, key)? != dependency_table(locked, key)? {
            return Err(fail(
                FailureKind::Configuration,
                "The package manifest and lockfile dependencies differ. Update the lockfile before preparing dependencies.",
            ));
        }
    }
    Ok(())
}

fn local_spec(value: &str) -> Option<&str> {
    value.strip_prefix("file:").or_else(|| {
        ["./", "../", "/", "~/"]
            .iter()
            .any(|prefix| value.starts_with(prefix))
            .then_some(value)
    })
}

fn verify_local_links(
    install: &str,
    origin: &str,
    manifest: &serde_json::Value,
    packages: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let directory = if origin.is_empty() {
        install.trim_end_matches('/').to_owned()
    } else {
        local_directory(install, origin)?
    };
    for field in [
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ] {
        for (name, value) in dependency_table(manifest, field)? {
            let Some(target) = value.as_str().and_then(local_spec) else {
                continue;
            };
            let expected = local_directory(&directory, target)?;
            let mut ancestor = Some(Path::new(origin));
            let mut locked = None;
            while let Some(path) = ancestor {
                let candidate = path
                    .join("node_modules")
                    .join(&name)
                    .to_string_lossy()
                    .into_owned();
                if let Some(package) = packages.get(&candidate) {
                    locked = Some(package);
                    break;
                }
                ancestor = path.parent();
            }
            let actual = locked
                .filter(|p| p["link"] == true)
                .and_then(|p| p["resolved"].as_str())
                .ok_or_else(|| {
                    fail(
                        FailureKind::Configuration,
                        "A local dependency has no matching directory link in the lockfile.",
                    )
                })?;
            if local_directory(install, actual)? != expected {
                return Err(fail(
                    FailureKind::Configuration,
                    "A local dependency points to a different locked directory.",
                ));
            }
        }
    }
    Ok(())
}

impl Plan {
    pub fn inspect(repository: &Repository, tree: ObjectId) -> Result<Self> {
        let files = repository.tree_files(tree).map_err(|_| {
            fail(
                FailureKind::Configuration,
                "The dependency tree cannot be read.",
            )
        })?;
        let mut hash = Sha256::new();
        hash.update(b"intercat.npm-plan.v2\0");
        let mut json = BTreeMap::new();
        for (path, file) in &files {
            if path.split('/').any(|part| part == "node_modules") || !relevant(path) {
                continue;
            }
            hash.update(path.as_bytes());
            hash.update([0]);
            hash.update(file.object.to_string().as_bytes());
            hash.update([0]);
            let name = path.rsplit('/').next().unwrap();
            if ["Cargo.toml", "pyproject.toml", "uv.lock"].contains(&name)
                || name.starts_with("requirements") && name.ends_with(".txt")
            {
                return Err(fail(
                    FailureKind::Unsupported,
                    "This dependency manager does not have an isolated installation path yet.",
                ));
            }
            let bytes = repository.saved_bytes(file).map_err(|_| {
                fail(
                    FailureKind::Configuration,
                    "A dependency file cannot be read.",
                )
            })?;
            if name == ".npmrc" {
                for line in String::from_utf8_lossy(&bytes)
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.starts_with(['#', ';']))
                {
                    let key = line
                        .split('=')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase();
                    if key.contains("_auth")
                        || key.contains("password")
                        || key.ends_with("keyfile")
                        || key.ends_with("certfile")
                    {
                        return Err(fail(
                            FailureKind::Credentials,
                            "Dependency installation needs project credentials to be configured explicitly.",
                        ));
                    }
                }
            } else if name.ends_with(".json") {
                json.insert(
                    path.clone(),
                    serde_json::from_slice::<serde_json::Value>(&bytes)?,
                );
            }
        }
        let root_workspace = json
            .get("package.json")
            .is_some_and(|manifest| manifest.get("workspaces").is_some());
        let mut linked_directories = BTreeSet::new();
        let mut local_entries = BTreeMap::new();
        for (path, lock) in &json {
            let Some(directory) = path
                .strip_suffix("package-lock.json")
                .or_else(|| path.strip_suffix("npm-shrinkwrap.json"))
            else {
                continue;
            };
            if path.ends_with("package-lock.json")
                && json.contains_key(&format!("{directory}npm-shrinkwrap.json"))
            {
                continue;
            }
            let Some(packages) = lock["packages"].as_object() else {
                continue;
            };
            let mut targets = BTreeSet::new();
            for package in packages.values().filter(|p| p["link"] == true) {
                let target = package["resolved"].as_str().ok_or_else(|| {
                    fail(
                        FailureKind::Configuration,
                        "A local dependency has no locked target.",
                    )
                })?;
                let local = local_directory(directory, target)?;
                let manifest_path = format!("{local}/package.json");
                if !files
                    .get(&manifest_path)
                    .is_some_and(|f| f.mode & 0o170000 == 0o100000)
                {
                    return Err(fail(
                        FailureKind::Configuration,
                        "A local dependency has no regular package.json in the saved repository tree.",
                    ));
                }
                let manifest = json.get(&manifest_path).ok_or_else(|| {
                    fail(
                        FailureKind::Configuration,
                        "A local dependency manifest is unavailable.",
                    )
                })?;
                let locked = packages
                    .get(target)
                    .filter(|p| p.is_object() && p["link"] != true)
                    .ok_or_else(|| {
                        fail(
                            FailureKind::Configuration,
                            "The local dependency target is missing from the lockfile.",
                        )
                    })?;
                verify_dependencies(manifest, locked)?;
                verify_local_links(directory, target, manifest, packages)?;
                if manifest.get("version") != locked.get("version") {
                    return Err(fail(
                        FailureKind::Configuration,
                        "A local dependency version differs from its lockfile entry.",
                    ));
                }
                targets.insert(target.to_owned());
                linked_directories.insert(local);
            }
            local_entries.insert(path.clone(), targets);
        }
        for (path, manifest) in json
            .iter()
            .filter(|(path, _)| path.ends_with("package.json"))
        {
            let directory = path.strip_suffix("package.json").unwrap();
            for field in [
                "dependencies",
                "devDependencies",
                "optionalDependencies",
                "peerDependencies",
            ] {
                for value in dependency_table(manifest, field)?.values() {
                    let local = value.as_str().and_then(local_spec);
                    if let Some(local) = local {
                        let target = local_directory(directory, local)?;
                        if !linked_directories.contains(&target) {
                            return Err(fail(
                                FailureKind::Configuration,
                                "A local dependency is not recorded as a directory link in a lockfile.",
                            ));
                        }
                    }
                }
            }
        }
        // npm uses the working tree for directory links. Include every saved
        // source blob and mode, so source-only edits invalidate a previous install.
        for directory in &linked_directories {
            let prefix = format!("{directory}/");
            for (path, file) in files.iter().filter(|(path, _)| {
                path.starts_with(&prefix) && !path.split('/').any(|p| p == "node_modules")
            }) {
                hash.update(path.as_bytes());
                hash.update([0]);
                hash.update(file.mode.to_be_bytes());
                hash.update(file.object.to_string().as_bytes());
                hash.update([0]);
            }
        }
        let mut installs = Vec::new();
        for (path, manifest) in json
            .iter()
            .filter(|(path, _)| path.rsplit('/').next() == Some("package.json"))
        {
            if root_workspace && path != "package.json" {
                continue;
            }
            let directory = path.strip_suffix("package.json").unwrap();
            if linked_directories.contains(directory.trim_end_matches('/')) {
                continue;
            }
            let lock_path = format!("{directory}package-lock.json");
            let shrinkwrap = format!("{directory}npm-shrinkwrap.json");
            if manifest["packageManager"]
                .as_str()
                .is_some_and(|manager| !manager.starts_with("npm@"))
                || files.contains_key(&format!("{directory}pnpm-lock.yaml"))
                || files.contains_key(&format!("{directory}yarn.lock"))
            {
                return Err(fail(
                    FailureKind::Unsupported,
                    "This project selects a package manager other than npm.",
                ));
            }
            let lock = json.get(&shrinkwrap).or_else(|| json.get(&lock_path));
            let Some(lock) = lock else {
                if has_setup(manifest) || manifest.get("workspaces").is_some() {
                    return Err(fail(
                        FailureKind::Configuration,
                        format!("{path} needs a package-lock.json for a locked install."),
                    ));
                }
                continue;
            };
            if !matches!(lock["lockfileVersion"].as_u64(), Some(2 | 3)) {
                return Err(fail(
                    FailureKind::Unsupported,
                    "The npm lockfile must use lockfileVersion 2 or 3.",
                ));
            }
            let packages = lock["packages"].as_object().ok_or_else(|| {
                fail(
                    FailureKind::Configuration,
                    "The npm lockfile has no packages table.",
                )
            })?;
            verify_dependencies(manifest, &packages.get("").cloned().unwrap_or_default())?;
            verify_local_links(directory, "", manifest, packages)?;
            let locked_path = if json.contains_key(&shrinkwrap) {
                &shrinkwrap
            } else {
                &lock_path
            };
            let locals = &local_entries[locked_path];
            let mut artifacts = BTreeMap::new();
            for (location, package) in packages {
                if location.is_empty() {
                    continue;
                }
                if locals.contains(location) {
                    continue;
                }
                if Path::new(location).is_absolute()
                    || Path::new(location)
                        .components()
                        .any(|c| !matches!(c, Component::Normal(_)))
                {
                    return Err(fail(
                        FailureKind::Configuration,
                        "The npm lockfile contains an unsafe package location.",
                    ));
                }
                if package["link"] == true
                    || package["inBundle"] == true
                    || root_workspace && !location.split('/').any(|part| part == "node_modules")
                {
                    continue;
                }
                let os = if cfg!(target_os = "macos") {
                    "darwin"
                } else {
                    std::env::consts::OS
                };
                let arch = if cfg!(target_arch = "aarch64") {
                    "arm64"
                } else {
                    std::env::consts::ARCH
                };
                if !matches_platform(&package["os"], os) || !matches_platform(&package["cpu"], arch)
                {
                    continue;
                }
                let url = package["resolved"].as_str().ok_or_else(|| {
                    fail(
                        FailureKind::Configuration,
                        "A locked package is missing its download URL.",
                    )
                })?;
                let url = reqwest::Url::parse(url).map_err(|_| {
                    fail(
                        FailureKind::Unsupported,
                        "Only locked registry archives can be installed.",
                    )
                })?;
                let integrity = package["integrity"].as_str().ok_or_else(|| {
                    fail(
                        FailureKind::Integrity,
                        "A locked package has no integrity hash.",
                    )
                })?;
                let encoded = integrity
                    .split_whitespace()
                    .find_map(|value| value.strip_prefix("sha512-"))
                    .ok_or_else(|| {
                        fail(
                            FailureKind::Unsupported,
                            "Locked npm packages must have a SHA-512 integrity hash.",
                        )
                    })?;
                let digest = STANDARD.decode(encoded).map_err(|_| {
                    fail(
                        FailureKind::Integrity,
                        "A package integrity hash is invalid.",
                    )
                })?;
                if digest.len() != 64 {
                    return Err(fail(
                        FailureKind::Integrity,
                        "A package integrity hash is invalid.",
                    ));
                }
                let key = digest
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                artifacts
                    .entry(key.clone())
                    .or_insert_with(|| Artifact { url, digest, key });
            }
            installs.push(Install {
                directory: directory.trim_end_matches('/').to_string(),
                artifacts: artifacts.into_values().collect(),
            });
        }
        Ok(Self {
            fingerprint: hash
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            installs,
        })
    }
}

fn sync(path: &Path) -> Result<()> {
    let file = File::open(path)?;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
fn directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            Ok(())
        }
        _ => Err(fail(
            FailureKind::Environment,
            "A dependency directory was replaced or is not a directory.",
        )),
    }
}
fn verify(path: &Path, expected: &[u8]) -> Result<bool> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > ARCHIVE_LIMIT {
        return Err(fail(
            FailureKind::Integrity,
            "An archive cache entry is not a regular bounded file.",
        ));
    }
    let mut file = File::open(path)?;
    let mut hash = Sha512::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hash.finalize().as_slice() == expected)
}
async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

pub struct Cache {
    root: PathBuf,
    origins: BTreeSet<String>,
    client: reqwest::Client,
}
impl Cache {
    pub fn new(root: PathBuf, origins: &[String]) -> Result<Self> {
        fs::create_dir_all(&root)?;
        let metadata = fs::symlink_metadata(&root)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(fail(
                FailureKind::Environment,
                "The download cache is not a private directory.",
            ));
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
        let mut allowed = BTreeSet::new();
        for origin in origins {
            let url = reqwest::Url::parse(origin).map_err(|_| {
                fail(
                    FailureKind::Configuration,
                    "An allowed registry URL is invalid.",
                )
            })?;
            let local = url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
            if (url.scheme() != "https" && !(url.scheme() == "http" && local))
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || url.path() != "/"
            {
                return Err(fail(
                    FailureKind::Configuration,
                    "Registry permissions must be HTTPS origins.",
                ));
            }
            allowed.insert(url.origin().ascii_serialization());
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(90))
            .user_agent("Intercat dependency preparation")
            .build()
            .map_err(|_| {
                fail(
                    FailureKind::Environment,
                    "The package downloader could not be initialized.",
                )
            })?;
        Ok(Self {
            root: root.canonicalize()?,
            origins: allowed,
            client,
        })
    }
    pub fn path(&self, artifact: &Artifact) -> PathBuf {
        self.root
            .join(&artifact.key[..2])
            .join(format!("{}.tgz", &artifact.key[2..]))
    }
    fn allowed(&self, url: &reqwest::Url) -> bool {
        url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && self.origins.contains(&url.origin().ascii_serialization())
    }
    pub async fn fetch(
        &self,
        artifact: &Artifact,
        mut cancel: watch::Receiver<bool>,
    ) -> Result<(PathBuf, bool)> {
        if !self.allowed(&artifact.url) {
            return Err(fail(
                FailureKind::Credentials,
                "This locked package uses a registry that has not been approved.",
            ));
        }
        let path = self.path(artifact);
        if verify(&path, &artifact.digest)? {
            return Ok((path, true));
        }
        if path.exists() {
            fs::remove_file(&path)?;
        }
        directory(path.parent().unwrap())?;
        let mut temporary = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        let mut url = artifact.url.clone();
        for redirects in 0..=3 {
            if !self.allowed(&url) {
                return Err(fail(
                    FailureKind::Credentials,
                    "A registry redirected a package outside the approved origins.",
                ));
            }
            let response = tokio::select! {_=cancelled(&mut cancel)=>return Err(fail(FailureKind::Cancelled,"Dependency download cancelled.")),response=self.client.get(url.clone()).send()=>response.map_err(|_|fail(FailureKind::Temporary,"A locked package could not be downloaded. Retry when the registry is reachable."))?};
            let status = response.status();
            if status.is_redirection() {
                if redirects == 3 {
                    return Err(fail(
                        FailureKind::Temporary,
                        "The package registry redirected too many times.",
                    ));
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| {
                        fail(FailureKind::Configuration, "A package redirect is invalid.")
                    })?;
                url = url.join(location).map_err(|_| {
                    fail(FailureKind::Configuration, "A package redirect is invalid.")
                })?;
                continue;
            }
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(fail(
                    FailureKind::Credentials,
                    "The package registry needs project credentials.",
                ));
            }
            if !status.is_success() {
                return Err(fail(
                    if status.is_server_error() {
                        FailureKind::Temporary
                    } else {
                        FailureKind::Configuration
                    },
                    format!(
                        "The registry returned HTTP {} for a locked package.",
                        status.as_u16()
                    ),
                ));
            }
            if response
                .content_length()
                .is_some_and(|size| size > ARCHIVE_LIMIT)
            {
                return Err(fail(
                    FailureKind::Unsupported,
                    "A package archive exceeds 64 MiB.",
                ));
            }
            let mut response = response;
            let mut hash = Sha512::new();
            let mut size = 0u64;
            loop {
                let chunk = tokio::select! {_=cancelled(&mut cancel)=>return Err(fail(FailureKind::Cancelled,"Dependency download cancelled.")),chunk=response.chunk()=>chunk.map_err(|_|fail(FailureKind::Temporary,"The package download was interrupted."))?};
                let Some(chunk) = chunk else {
                    break;
                };
                size += chunk.len() as u64;
                if size > ARCHIVE_LIMIT {
                    return Err(fail(
                        FailureKind::Unsupported,
                        "A package archive exceeds 64 MiB.",
                    ));
                }
                hash.update(&chunk);
                temporary.write_all(&chunk)?;
            }
            if hash.finalize().as_slice() != artifact.digest {
                return Err(fail(
                    FailureKind::Integrity,
                    "A downloaded package does not match the lockfile integrity hash.",
                ));
            }
            sync(temporary.path())?;
            match temporary.persist_noclobber(&path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if !verify(&path, &artifact.digest)? {
                        return Err(fail(
                            FailureKind::Integrity,
                            "A shared archive changed during download.",
                        ));
                    }
                }
                Err(error) => return Err(error.error.into()),
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
            sync(path.parent().unwrap())?;
            return Ok((path, false));
        }
        Err(fail(
            FailureKind::Temporary,
            "The package could not be downloaded.",
        ))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub fingerprint: String,
    pub directory: String,
    pub forced: bool,
    pub cache_hits: usize,
    pub downloads: usize,
    pub elapsed_ms: u64,
    pub stdout: String,
    pub stderr: String,
}
pub struct Preparer {
    cache: Cache,
    npm: Option<NpmTool>,
    last: Option<String>,
}
impl Preparer {
    pub fn new(cache: Cache, npm: Option<NpmTool>) -> Self {
        Self {
            cache,
            npm,
            last: None,
        }
    }
    pub async fn prepare(
        &mut self,
        checkout: &Checkout,
        repository: &Repository,
        tree: ObjectId,
        force: bool,
        cancel: watch::Receiver<bool>,
    ) -> Result<Vec<Run>> {
        let plan = Plan::inspect(repository, tree)?;
        if !force && self.last.as_ref() == Some(&plan.fingerprint) {
            return Ok(vec![]);
        }
        let mut runs = Vec::new();
        for install in &plan.installs {
            let started = Instant::now();
            let npm = self.npm.as_ref().ok_or_else(|| {
                fail(
                    FailureKind::Environment,
                    "npm is not configured in the selected toolchain.",
                )
            })?;
            let command = npm.command().map_err(|_| {
                fail(
                    FailureKind::Environment,
                    "The npm toolchain path is invalid.",
                )
            })?;
            let archives = checkout.temporary.join("archives");
            directory(&checkout.temporary)?;
            directory(&archives)?;
            let cache = checkout.temporary.join("npm-cache");
            let cwd = checkout.workdir.join(&install.directory);
            let prefix = format!(
                "cd {} && {command}",
                shell_quote(cwd.as_os_str()).map_err(|_| fail(
                    FailureKind::Configuration,
                    "The install path is invalid."
                ))?
            );
            let user_config = checkout.root.join("control/npm-userconfig");
            let global_config = checkout.root.join("control/npm-globalconfig");
            fs::write(&user_config, [])?;
            fs::write(&global_config, [])?;
            let flags = format!(
                "--offline --no-audit --no-fund --no-progress --update-notifier=false --cache {} --userconfig={} --globalconfig={}",
                shell_quote(cache.as_os_str()).unwrap(),
                shell_quote(user_config.as_os_str()).unwrap(),
                shell_quote(global_config.as_os_str()).unwrap()
            );
            let mut paths = Vec::new();
            let mut hits = 0;
            let mut downloads = 0;
            for artifact in &install.artifacts {
                let (source, hit) = self.cache.fetch(artifact, cancel.clone()).await?;
                if hit {
                    hits += 1;
                } else {
                    downloads += 1;
                }
                let target = archives.join(format!("{}.tgz", artifact.key));
                // Never share hard links with code that can write its cache.
                if fs::symlink_metadata(&target).is_ok() {
                    fs::remove_file(&target)?;
                }
                let mut input = File::open(source)?;
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target)?;
                std::io::copy(&mut input, &mut output)?;
                output.set_permissions(fs::Permissions::from_mode(0o600))?;
                paths.push(target);
            }
            for batch in paths.chunks(128) {
                let names = batch
                    .iter()
                    .map(|path| shell_quote(path.as_os_str()).unwrap())
                    .collect::<Vec<_>>()
                    .join(" ");
                let result = checkout
                    .run_check(
                        &format!("{prefix} cache add {names} {flags} --ignore-scripts"),
                        &std::collections::BTreeMap::new(),
                        cancel.clone(),
                        Duration::from_secs(300),
                    )
                    .await
                    .map_err(|_| {
                        fail(
                            FailureKind::Environment,
                            "The npm cache import could not run.",
                        )
                    })?;
                require_success(result, "Importing a locked package into the job cache")?;
            }
            let result = checkout
                .run_check(
                    &format!("{prefix} ci {flags} --ignore-scripts=false"),
                    &std::collections::BTreeMap::new(),
                    cancel.clone(),
                    Duration::from_secs(300),
                )
                .await
                .map_err(|_| {
                    fail(
                        FailureKind::Environment,
                        "The locked npm install could not run.",
                    )
                })?;
            let result = require_success(result, "The locked npm install")?;
            runs.push(Run {
                fingerprint: plan.fingerprint.clone(),
                directory: install.directory.clone(),
                forced: force,
                cache_hits: hits,
                downloads,
                elapsed_ms: started.elapsed().as_millis() as u64,
                stdout: result.stdout_text(),
                stderr: result.stderr_text(),
            });
        }
        self.last = Some(plan.fingerprint);
        Ok(runs)
    }
}
fn require_success(output: Output, stage: &str) -> Result<Output> {
    if output.success() {
        return Ok(output);
    }
    let text = format!("{}\n{}", output.stdout_text(), output.stderr_text());
    let kind = if output.cancelled {
        FailureKind::Cancelled
    } else if output.timed_out {
        FailureKind::Temporary
    } else {
        FailureKind::Configuration
    };
    Err(Failure {
        kind,
        message: format!("{stage} failed (exit {:?}).", output.code),
        output: Some(text),
    })
}
