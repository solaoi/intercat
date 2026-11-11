//! Resolve upload endpoints offline. `git lfs env` reports download endpoints,
//! so upload selection must precede its URL canonicalization.
use crate::{
    Result, command,
    git_tool::GitTool,
    project::Folder,
    push_inspect::Inspector,
    push_target::{Destination, Target},
};
use intercat_git::{RemoteAddress, Repository, Transport};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;

pub struct Tool {
    _private: tempfile::TempDir,
    executable: PathBuf,
    git: GitTool,
}
pub struct Endpoint {
    pub git_url: String,
    /// The original upload URL, retained only in memory. SSH keeps its port.
    pub upload_url: String,
    pub api_url: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Binding {
    pub git: Destination,
    pub upload: Destination,
    pub api: Destination,
}
impl Binding {
    pub fn capture(endpoint: &Endpoint) -> Result<Self> {
        Ok(Self {
            git: Destination::capture(&endpoint.git_url)?,
            upload: Destination::capture(&endpoint.upload_url)?,
            api: Destination::capture(&endpoint.api_url)?,
        })
    }
    pub fn verify(saved: &[Self], live: &[Endpoint]) -> Result<()> {
        let mut expected = saved.to_vec();
        let mut actual = live.iter().map(Self::capture).collect::<Result<Vec<_>>>()?;
        let order = |a: &Self, b: &Self| {
            (
                &a.git.fingerprint,
                &a.upload.fingerprint,
                &a.api.fingerprint,
            )
                .cmp(&(
                    &b.git.fingerprint,
                    &b.upload.fingerprint,
                    &b.api.fingerprint,
                ))
        };
        expected.sort_by(order);
        actual.sort_by(order);
        if expected != actual {
            return Err("LFS destination changed since you reviewed · Review".into());
        }
        Ok(())
    }
}
impl Endpoint {
    pub fn destinations(&self) -> Result<Vec<Destination>> {
        let mut result = vec![Destination::capture(&self.api_url)?];
        if RemoteAddress::parse(&self.upload_url)?.transport == Transport::Ssh {
            result.insert(0, Destination::capture(&self.upload_url)?);
        }
        Ok(result)
    }
}
struct Settings {
    values: BTreeMap<String, String>,
    aliases: Vec<(String, String, bool)>,
}
impl Settings {
    fn new(mut values: BTreeMap<String, String>, configured: Vec<(String, String)>) -> Self {
        let mut aliases = Vec::new();
        for (key, value) in configured {
            if let Some(base) = key.strip_prefix("url.") {
                if let Some(base) = base.strip_suffix(".pushinsteadof") {
                    aliases.push((value, base.into(), true));
                } else if let Some(base) = base.strip_suffix(".insteadof") {
                    aliases.push((value, base.into(), false));
                }
            } else {
                values.insert(key, value);
            }
        }
        Self { values, aliases }
    }
    fn upload(&self, remote: &str, git_url: &str) -> (String, bool) {
        let selected = [
            "lfs.pushurl".into(),
            "lfs.url".into(),
            format!("remote.{remote}.lfspushurl"),
            format!("remote.{remote}.lfsurl"),
        ]
        .iter()
        .find_map(|key| self.values.get(key));
        let Some(raw) = selected else {
            return (git_url.into(), false);
        };
        // git-lfs selects a push alias first; it does not recursively rewrite
        // the result. Resolved Git URLs already have their Git rewrite applied.
        let rewritten = [true, false]
            .into_iter()
            .find_map(|push| {
                self.aliases
                    .iter()
                    .filter(|(prefix, _, kind)| *kind == push && raw.starts_with(prefix))
                    .max_by_key(|(prefix, _, _)| prefix.len())
                    .map(|(prefix, base, _)| format!("{base}{}", &raw[prefix.len()..]))
            })
            .unwrap_or_else(|| raw.clone());
        (rewritten, true)
    }
}
fn quote(path: &Path) -> Result<String> {
    Ok(serde_json::to_string(
        path.to_str().ok_or("Git LFS needs UTF-8 tool paths.")?,
    )?)
}
impl Tool {
    pub(crate) fn executable(&self) -> &Path {
        &self.executable
    }
    pub(crate) fn git(&self) -> &GitTool {
        &self.git
    }
    pub fn open(git: GitTool, executable: PathBuf) -> Result<Self> {
        let executable = executable.canonicalize()?;
        let metadata = fs::metadata(&executable)?;
        if !metadata.is_file() || metadata.len() > 64 * 1024 * 1024 {
            return Err("Git LFS must be a regular executable no larger than 64 MiB.".into());
        }
        // The application can itself be the open project. Keep its fixed tool
        // outside the source deny rule, rather than granting source access.
        let private = tempfile::Builder::new()
            .prefix("intercat-lfs-tool-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let staged = private.path().canonicalize()?.join("git-lfs");
        fs::copy(executable, &staged)?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o500))?;
        Ok(Self {
            _private: private,
            git,
            executable: staged,
        })
    }
    async fn canonicalize(
        &self,
        upload: &str,
        explicit: bool,
        protocol: Option<&str>,
        cancel: watch::Receiver<bool>,
    ) -> Result<String> {
        let root = tempfile::Builder::new()
            .prefix("intercat-lfs-endpoint-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let root_path = root.path().canonicalize()?;
        let directory = root_path.join("repo");
        Repository::init_isolated(&directory)?;
        let runtime = self
            .git
            .binary()
            .parent()
            .and_then(Path::parent)
            .ok_or("Missing Git runtime.")?;
        let profile = format!(
            "(version 1)(deny default)(allow process-fork)(allow process-info*)(allow sysctl-read)(allow mach-lookup)(allow signal (target self))(allow file-read-metadata)(allow file-read* (literal \"/\") (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/dev\") (subpath \"/private/var/db/dyld\") (subpath \"/private/preboot\") (subpath {}) (subpath {}) (literal {}))(allow file-write* (subpath {}) (literal \"/dev/null\"))(allow process-exec (literal {}) (literal {}) (subpath {}))",
            quote(&root_path)?,
            quote(runtime)?,
            quote(&self.executable)?,
            quote(&root_path)?,
            quote(self.git.binary())?,
            quote(&self.executable)?,
            quote(self.git.exec_dir())?,
        );
        let mut child = command::gated_command("/usr/bin/sandbox-exec", None);
        child
            .args(["-p", &profile])
            .arg(&self.executable)
            .arg("env")
            .current_dir(&directory)
            .env("HOME", &root_path)
            .env("TMPDIR", &root_path)
            .env(
                "PATH",
                std::env::join_paths([
                    self.git.binary().parent().unwrap(),
                    Path::new("/usr/bin"),
                    Path::new("/bin"),
                ])?,
            )
            .env("GIT_EXEC_PATH", self.git.exec_dir())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0");
        let mut config = vec![
            ("remote.origin.url", upload),
            ("core.hooksPath", "/dev/null"),
            ("lfs.storage", "lfs"),
        ];
        if explicit {
            config.push(("lfs.url", upload));
        }
        if let Some(protocol) = protocol {
            config.push(("lfs.gitprotocol", protocol));
        }
        child.env("GIT_CONFIG_COUNT", config.len().to_string());
        for (index, (key, value)) in config.iter().enumerate() {
            child
                .env(format!("GIT_CONFIG_KEY_{index}"), key)
                .env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
        let output = command::run(&mut child, cancel, Duration::from_secs(30)).await?;
        if !output.success() || output.truncated {
            return Err("The Git LFS upload endpoint could not be resolved completely.".into());
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|_| "Git LFS returned an invalid upload endpoint.")?;
        if !text.starts_with("git-lfs/3.8.1 ") {
            return Err("Intercat needs the installed Git LFS 3.8.1 tool.".into());
        }
        let endpoints = text
            .lines()
            .filter_map(|line| line.strip_prefix("Endpoint="))
            .collect::<Vec<_>>();
        if endpoints.len() != 1 {
            return Err("Git LFS did not identify one upload endpoint.".into());
        }
        let url = endpoints[0]
            .rsplit_once(" (auth=")
            .map(|(url, _)| url)
            .ok_or("Git LFS returned an invalid upload endpoint.")?;
        Destination::capture(url)?;
        Ok(url.into())
    }
    pub async fn resolve(
        &self,
        inspector: &Inspector,
        folder: &Folder,
        target: &Target,
        git_urls: &[String],
        cancel: watch::Receiver<bool>,
    ) -> Result<Vec<Endpoint>> {
        target.verify_destinations(&target.remote, &target.target_ref, git_urls)?;
        let frozen = Repository::open(&folder.root)?
            .push_lfs_configuration(target.sha.parse()?, &target.remote)?;
        let settings = Settings::new(
            frozen,
            inspector
                .lfs_settings(folder, &target.remote, cancel.clone())
                .await?,
        );
        let mut endpoints = Vec::new();
        for git_url in git_urls {
            let (upload_url, explicit) = settings.upload(&target.remote, git_url);
            Destination::capture(&upload_url)?;
            let api_url = self
                .canonicalize(
                    &upload_url,
                    explicit,
                    settings.values.get("lfs.gitprotocol").map(String::as_str),
                    cancel.clone(),
                )
                .await?;
            endpoints.push(Endpoint {
                git_url: git_url.clone(),
                upload_url,
                api_url,
            });
        }
        Ok(endpoints)
    }
}
