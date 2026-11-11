//! Authenticated HTTP transport. Only approved destination IPs/ports can be
//! reached; repository hooks, URL rewrites and alternate helpers are absent.
use crate::{
    Result, command,
    git_tool::GitTool,
    push_auth::Authentication,
    push_target::Destination,
    push_transfer::Snapshot,
    push_wire::{self, Advertised, Receipt},
};
use intercat_git::{RemoteAddress, Transport};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};
use tokio::sync::watch;

pub struct Client<'a> {
    snapshot: &'a Snapshot,
    git: GitTool,
    url: String,
    auth: Authentication,
    broker: crate::push_broker::Broker,
    temporary: tempfile::TempDir,
}
fn quote(path: &Path) -> Result<String> {
    Ok(serde_json::to_string(
        path.to_str()
            .ok_or("Git authentication needs UTF-8 paths.")?,
    )?)
}
impl<'a> Client<'a> {
    pub async fn new(
        snapshot: &'a Snapshot,
        git: GitTool,
        url: &str,
        auth: Authentication,
    ) -> Result<Self> {
        if auth.folder_id != snapshot.member().folder.id {
            return Err("The Git authentication settings belong to another folder.".into());
        }
        let address = RemoteAddress::parse(url)?;
        if !matches!(address.transport, Transport::Http | Transport::Https) {
            return Err("This connection needs another Git transport.".into());
        }
        let destination = Destination::capture(url)?;
        if auth.destination != destination {
            return Err("The Git authentication settings belong to another destination.".into());
        }
        if !snapshot.member().target.destinations.contains(&destination) {
            return Err("Destination changed since you reviewed · Review".into());
        }
        let host = address.host.ok_or("The HTTP destination has no host.")?;
        let port = address
            .port
            .unwrap_or(if address.transport == Transport::Https {
                443
            } else {
                80
            });
        let broker = crate::push_broker::Broker::start(&host, port).await?;
        let temporary = tempfile::Builder::new()
            .prefix("intercat-http-push-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()?;
        Ok(Self {
            snapshot,
            git,
            url: url.into(),
            auth,
            broker,
            temporary,
        })
    }
    fn profile(&self) -> Result<String> {
        let root = self.temporary.path().canonicalize()?;
        let runtime = self
            .git
            .binary()
            .parent()
            .and_then(Path::parent)
            .ok_or("Missing Git runtime.")?;
        let mut profile = String::from(
            "(version 1)(deny default)(allow process-fork)(allow process-info*)(allow sysctl-read)(allow mach-lookup)(allow signal (target self))(allow file-read-metadata)(allow file-read* (literal \"/\") (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/dev\") (subpath \"/private/var/db/dyld\") (subpath \"/private/preboot\") (subpath \"/private/etc/ssl\")",
        );
        for path in [
            runtime,
            &root,
            &self.snapshot.git_directory,
            &self.auth.home,
            &self.auth.xdg,
        ] {
            profile.push_str(&format!(" (subpath {})", quote(path)?));
        }
        for path in self.auth.helpers.iter().chain(&self.auth.files) {
            profile.push_str(&format!(
                " ({} {})",
                if path.is_dir() { "subpath" } else { "literal" },
                quote(path)?
            ));
        }
        for ancestor in self.snapshot.root.ancestors().chain(root.ancestors()) {
            profile.push_str(&format!(" (literal {})", quote(ancestor)?));
        }
        profile.push_str(&format!(")(allow file-write* (subpath {}) (literal \"/dev/null\"))(allow process-exec (literal {}) (subpath {}) (literal \"/bin/sh\") (literal \"/bin/bash\")",quote(&root)?,quote(self.git.binary())?,quote(self.git.exec_dir())?));
        for helper in &self.auth.helpers {
            profile.push_str(&format!(" (literal {})", quote(helper)?));
        }
        profile.push_str(&format!(
            ")(allow network-outbound (literal {}))(allow file-read* (subpath {}))",
            quote(self.broker.socket())?,
            quote(self.broker.root())?
        ));
        profile.push_str(&format!(
            "(deny file-read* file-write* (subpath {}) (subpath {}))",
            quote(&self.snapshot.member().folder.root)?,
            quote(&self.snapshot.member().folder.common_dir)?
        ));
        Ok(profile)
    }
    async fn execute(
        &self,
        args: &[&str],
        cancel: watch::Receiver<bool>,
    ) -> Result<command::Output> {
        let mut child = command::gated_command("/usr/bin/sandbox-exec", None);
        child.args(["-p", &self.profile()?]).arg(self.git.binary());
        // Authentication and the private proxy capability stay out of argv.
        let mut values = self.auth.values.clone();
        values.push(("http.proxy".into(), self.broker.proxy_url()));
        values.push((
            "remote.intercat-push-destination.url".into(),
            self.url.clone(),
        ));
        child.env("GIT_CONFIG_COUNT", values.len().to_string());
        for (index, (key, value)) in values.iter().enumerate() {
            child
                .env(format!("GIT_CONFIG_KEY_{index}"), key)
                .env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
        child
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "-c",
                "protocol.allow=never",
                "-c",
                "protocol.http.allow=always",
                "-c",
                "protocol.https.allow=always",
                "-c",
                "http.followRedirects=false",
            ])
            .args(args)
            .current_dir(&self.snapshot.git_directory)
            .env("HOME", &self.auth.home)
            .env("XDG_CONFIG_HOME", &self.auth.xdg)
            .env("TMPDIR", self.temporary.path())
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(self.git.binary().parent().unwrap().to_path_buf())
                        .chain(std::iter::once(self.git.exec_dir().to_path_buf()))
                        .chain(
                            self.auth
                                .helpers
                                .iter()
                                .filter_map(|helper| helper.parent().map(Path::to_path_buf)),
                        )
                        .chain(["/usr/bin".into(), "/bin".into()]),
                )?,
            )
            .env("GIT_EXEC_PATH", self.git.exec_dir())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1");
        command::run(&mut child, cancel, Duration::from_secs(120)).await
    }
    pub async fn advertise(&self, cancel: watch::Receiver<bool>) -> Result<Vec<Advertised>> {
        push_wire::advertisement(
            self.execute(&["ls-remote", "--", "intercat-push-destination"], cancel)
                .await?,
        )
    }
    pub async fn send(&self, cancel: watch::Receiver<bool>) -> Result<Receipt> {
        let refspec = self.snapshot.member().target.refspec()?;
        push_wire::receipt(
            self.execute(
                &[
                    "push",
                    "--porcelain",
                    "--no-verify",
                    "--no-follow-tags",
                    "--",
                    "intercat-push-destination",
                    &refspec,
                ],
                cancel,
            )
            .await?,
            &self.snapshot.member().target,
            &self.url,
        )
    }
}
