//! File-transport execution uses the same frozen SHA as network transports.
//! Only Git, its built-in helpers and its fixed shell launcher can execute.
use crate::push_wire::{self, Advertised, Receipt};
use crate::{
    Result, command, git_tool::GitTool, push_target::Destination, push_transfer::Snapshot,
};
use intercat_git::{RemoteAddress, Repository, Transport};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;

pub struct Client<'a> {
    snapshot: &'a Snapshot,
    git: GitTool,
    url: String,
    remote: PathBuf,
}
fn quote(path: &Path) -> Result<String> {
    Ok(serde_json::to_string(
        path.to_str().ok_or("Git needs UTF-8 transport paths.")?,
    )?)
}
impl<'a> Client<'a> {
    pub fn new(snapshot: &'a Snapshot, git: GitTool, url: &str) -> Result<Self> {
        let address = RemoteAddress::parse(url)?;
        if address.transport != Transport::File {
            return Err("This connection needs a network transport.".into());
        }
        let destination = Destination::capture(url)?;
        if !snapshot.member().target.destinations.contains(&destination) {
            return Err("Destination changed since you reviewed · Review".into());
        }
        let path = if url.starts_with("file:") {
            reqwest::Url::parse(url)
                .map_err(|_| "Invalid local Push destination.")?
                .to_file_path()
                .map_err(|_| "Invalid local Push destination.")?
        } else {
            let path = Path::new(url);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                snapshot.member().folder.root.join(path)
            }
        };
        let remote = Repository::open(path)?.common_directory()?;
        if remote.starts_with(&snapshot.root)
            || snapshot.root.starts_with(&remote)
            || remote == snapshot.member().folder.common_dir
        {
            return Err(
                "The Push destination must be separate from the source and transport store.".into(),
            );
        }
        Ok(Self {
            snapshot,
            git,
            url: url.into(),
            remote,
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
            "(version 1)(deny default)(allow process-fork)(allow process-info*)(allow sysctl-read)(allow mach-lookup)(allow signal (target self))(allow file-read-metadata)(allow file-read* (literal \"/\") (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/dev\") (subpath \"/private/var/db/dyld\") (subpath \"/private/preboot\")",
        );
        for path in [runtime, &self.snapshot.git_directory, &self.remote] {
            profile.push_str(&format!(" (subpath {})", quote(path)?));
        }
        for ancestor in self
            .snapshot
            .root
            .ancestors()
            .chain(self.remote.ancestors())
        {
            profile.push_str(&format!(" (literal {})", quote(ancestor)?));
        }
        profile.push_str(&format!(
            ")(allow file-write* (subpath {}) (literal \"/dev/null\"))(allow process-exec (literal {}) (subpath {}) (literal \"/bin/sh\") (literal \"/bin/bash\"))(deny file-read* file-write* (subpath {}) (subpath {}))",
            quote(&self.remote)?, quote(self.git.binary())?, quote(self.git.exec_dir())?,
            quote(&self.snapshot.member().folder.root)?, quote(&self.snapshot.member().folder.common_dir)?,
        ));
        Ok(profile)
    }
    async fn execute(
        &self,
        arguments: &[&str],
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
                "maintenance.auto=false",
                "-c",
                "gc.auto=0",
                "-c",
                "protocol.allow=never",
                "-c",
                "protocol.file.allow=always",
            ])
            .args(arguments)
            .current_dir(&self.snapshot.git_directory)
            .env("HOME", &self.snapshot.root)
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
            .env("GIT_NO_REPLACE_OBJECTS", "1");
        command::run(&mut child, cancel, Duration::from_secs(120)).await
    }
    pub async fn advertise(&self, cancel: watch::Receiver<bool>) -> Result<Vec<Advertised>> {
        let output = self
            .execute(&["ls-remote", "--", &self.url], cancel)
            .await?;
        push_wire::advertisement(output)
    }
    pub async fn send(&self, cancel: watch::Receiver<bool>) -> Result<Receipt> {
        let refspec = self.snapshot.member().target.refspec()?;
        let output = self
            .execute(
                &[
                    "push",
                    "--porcelain",
                    "--no-verify",
                    "--no-follow-tags",
                    "--",
                    &self.url,
                    &refspec,
                ],
                cancel,
            )
            .await?;
        push_wire::receipt(output, &self.snapshot.member().target, &self.url)
    }
}
