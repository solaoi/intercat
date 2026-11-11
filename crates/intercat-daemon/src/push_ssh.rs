//! SSH runs through one private Unix tunnel with a reviewed effective peer.
//! Its agent and host-key files are available; project files remain denied.
use crate::{
    Result, command,
    git_tool::GitTool,
    push_auth::Authentication,
    push_target::Destination,
    push_transfer::Snapshot,
    push_wire::{self, Advertised, Receipt},
};
use intercat_git::{RemoteAddress, Transport};
use std::{path::Path, time::Duration};
use tokio::sync::watch;

pub struct Client<'a> {
    snapshot: &'a Snapshot,
    git: GitTool,
    url: String,
    auth: Authentication,
    access: crate::push_ssh_access::Access,
}
fn quote(path: &Path) -> Result<String> {
    Ok(serde_json::to_string(
        path.to_str().ok_or("SSH needs UTF-8 paths.")?,
    )?)
}
impl<'a> Client<'a> {
    pub async fn new(
        snapshot: &'a Snapshot,
        git: GitTool,
        url: &str,
        mut auth: Authentication,
        cancel: watch::Receiver<bool>,
    ) -> Result<Self> {
        let destination = Destination::capture(url)?;
        if auth.folder_id != snapshot.member().folder.id
            || auth.destination != destination
            || !snapshot.member().target.destinations.contains(&destination)
            || RemoteAddress::parse(url)?.transport != Transport::Ssh
        {
            return Err("The SSH connection does not match this Push review.".into());
        }
        let access =
            crate::push_ssh_access::Access::open(snapshot, &git, url, &mut auth, cancel).await?;
        Ok(Self {
            snapshot,
            git,
            url: url.into(),
            auth,
            access,
        })
    }
    fn profile(&self) -> Result<String> {
        let runtime = self
            .git
            .binary()
            .parent()
            .and_then(Path::parent)
            .ok_or("Missing Git runtime.")?;
        let mut profile = String::from(
            "(version 1)(deny default)(allow process-fork)(allow process-info*)(allow sysctl-read)(allow mach-lookup)(allow signal (target self))(allow file-read-metadata)(allow file-read* (literal \"/\") (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/dev\") (subpath \"/private/var/db/dyld\") (subpath \"/private/preboot\") (subpath \"/private/etc/ssh\")",
        );
        for path in [
            runtime,
            self.access.root(),
            self.access.tunnel.root(),
            &self.auth.home,
            &self.auth.xdg,
            &self.snapshot.git_directory,
        ] {
            profile.push_str(&format!(" (subpath {})", quote(path)?));
        }
        if let Some(agent) = &self.auth.agent {
            profile.push_str(&format!(" (literal {})", quote(agent)?));
        }
        for ancestor in self
            .snapshot
            .root
            .ancestors()
            .chain(self.access.root().ancestors())
        {
            profile.push_str(&format!(" (literal {})", quote(ancestor)?));
        }
        profile.push_str(&format!(")(allow file-write* (literal \"/dev/null\"))(allow process-exec (literal {}) (subpath {}) (literal \"/usr/bin/ssh\") (literal \"/usr/bin/nc\") (literal \"/bin/sh\") (literal \"/bin/bash\"))(allow network-outbound (literal {})",quote(self.git.binary())?,quote(self.git.exec_dir())?,quote(self.access.tunnel.socket())?));
        if let Some(agent) = &self.auth.agent {
            profile.push_str(&format!(" (literal {})", quote(agent)?));
        }
        profile.push_str(&format!(
            ")(deny file-read* file-write* (subpath {}) (subpath {}))",
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
        child
            .args(["-p", &self.profile()?])
            .arg(self.git.binary())
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
                "protocol.ssh.allow=always",
            ])
            .args(args)
            .current_dir(&self.snapshot.git_directory)
            .env("HOME", &self.auth.home)
            .env("XDG_CONFIG_HOME", &self.auth.xdg)
            .env("TMPDIR", self.access.root())
            .env(
                "PATH",
                std::env::join_paths([
                    self.git.binary().parent().unwrap(),
                    self.git.exec_dir(),
                    Path::new("/usr/bin"),
                    Path::new("/bin"),
                ])?,
            )
            .env("GIT_EXEC_PATH", self.git.exec_dir())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_SSH_VARIANT", "ssh")
            .env("GIT_SSH_COMMAND", &self.access.command)
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "remote.intercat-push-destination.url")
            .env("GIT_CONFIG_VALUE_0", &self.url);
        if let Some(agent) = &self.auth.agent {
            child.env("SSH_AUTH_SOCK", agent);
        }
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
