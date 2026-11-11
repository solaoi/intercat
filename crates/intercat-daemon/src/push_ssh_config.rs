//! Resolve SSH aliases without connecting or authenticating. A review binds
//! the resulting host, port and user; the raw username never enters the record.
use crate::{Result, command, project::Folder, push_inspect::Inspector, push_target::Destination};
use intercat_git::{RemoteAddress, Transport};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Binding {
    pub destination: Destination,
    pub host: String,
    pub port: u16,
    pub user_fingerprint: String,
}
pub struct Configuration {
    _root: tempfile::TempDir,
    pub(crate) file: PathBuf,
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) user: String,
    pub(crate) agent: Option<String>,
}
fn quote(path: &Path) -> Result<String> {
    Ok(serde_json::to_string(
        path.to_str()
            .ok_or("SSH needs UTF-8 configuration paths.")?,
    )?)
}
impl Configuration {
    pub fn root(&self) -> &Path {
        self.file
            .parent()
            .expect("A private SSH configuration has a parent")
    }
    pub fn binding(&self, url: &str) -> Result<Binding> {
        Ok(Binding {
            destination: Destination::capture(url)?,
            host: self.host.clone(),
            port: self.port,
            user_fingerprint: Sha256::digest(self.user.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        })
    }
    pub async fn inspect(
        inspector: &Inspector,
        folder: &Folder,
        url: &str,
        cancel: watch::Receiver<bool>,
    ) -> Result<Self> {
        let address = RemoteAddress::parse(url)?;
        if address.transport != Transport::Ssh {
            return Err("This destination does not use SSH.".into());
        }
        let (home, _) = inspector.user_paths();
        let root = tempfile::Builder::new()
            .prefix("intercat-ssh-config-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let root_path = root.path().canonicalize()?;
        let file = root_path.join("config");
        let mut text = format!("Include {}\n", quote(&home.join(".ssh/config"))?);
        if inspector.system() {
            text.push_str("Include /private/etc/ssh/ssh_config\n");
        }
        fs::write(&file, text)?;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
        let mut profile = String::from(
            "(version 1)(deny default)(allow process-fork)(allow process-info*)(allow sysctl-read)(allow mach-lookup)(allow signal (target self))(allow file-read-metadata)(allow file-read* (literal \"/\") (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/dev\") (subpath \"/private/var/db/dyld\") (subpath \"/private/preboot\") (subpath \"/private/etc/ssh\")",
        );
        for path in [root_path.as_path(), home] {
            profile.push_str(&format!(" (subpath {})", quote(path)?));
        }
        for ancestor in root_path.ancestors() {
            profile.push_str(&format!(" (literal {})", quote(ancestor)?));
        }
        profile.push_str(&format!(")(allow file-write* (literal \"/dev/null\"))(allow process-exec (literal \"/usr/bin/ssh\"))(deny file-read* file-write* (subpath {}) (subpath {}))",quote(&folder.root)?,quote(&folder.common_dir)?));
        let mut child = command::gated_command("/usr/bin/sandbox-exec", None);
        child
            .args(["-p", &profile, "/usr/bin/ssh", "-G", "-T", "-F"])
            .arg(&file)
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "CanonicalizeHostname=no",
            ])
            .current_dir(&root_path)
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        if !inspector.system() {
            child
                .args(["-o", "GlobalKnownHostsFile=/dev/null", "-o"])
                .arg(format!(
                    "UserKnownHostsFile={}",
                    quote(&home.join(".ssh/known_hosts"))?
                ));
        }
        if let Some(port) = address.explicit_port {
            child.args(["-p", &port.to_string()]);
        }
        if let Some(user) = address.user {
            child.args(["-l", &user]);
        }
        child
            .arg("--")
            .arg(address.host.ok_or("The SSH destination has no host.")?);
        let output = command::run(&mut child, cancel, Duration::from_secs(15)).await?;
        if !output.success() || output.truncated || !output.stderr.is_empty() {
            return Err("SSH configuration could not be inspected without running another command or connecting.".into());
        }
        let text = String::from_utf8(output.stdout)
            .map_err(|_| "SSH returned an invalid configuration.")?;
        let mut values = BTreeMap::new();
        for line in text.lines() {
            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            if ![
                "hostname",
                "port",
                "user",
                "identityagent",
                "proxycommand",
                "proxyjump",
            ]
            .contains(&key)
            {
                continue;
            }
            if values.insert(key.to_owned(), value.to_owned()).is_some() {
                return Err("SSH returned an ambiguous configuration.".into());
            }
        }
        if ["proxycommand", "proxyjump"]
            .iter()
            .any(|key| values.get(*key).is_some_and(|value| value != "none"))
        {
            return Err("This SSH connection uses a proxy or jump host. Its destination must be reviewed before sending.".into());
        }
        let host = values
            .remove("hostname")
            .ok_or("SSH did not identify its host.")?;
        let host = host
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|_| host.trim_end_matches('.').to_ascii_lowercase());
        let port: u16 = values
            .remove("port")
            .ok_or("SSH did not identify its port.")?
            .parse()
            .map_err(|_| "SSH returned an invalid port.")?;
        let user = values
            .remove("user")
            .ok_or("SSH did not identify its user.")?;
        if port == 0
            || host.is_empty()
            || user.is_empty()
            || host.chars().chain(user.chars()).any(char::is_control)
        {
            return Err("SSH returned an invalid destination.".into());
        }
        Ok(Self {
            _root: root,
            file,
            host,
            port,
            user,
            agent: values.remove("identityagent"),
        })
    }
}
