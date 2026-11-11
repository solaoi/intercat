//! The pinned git-lfs client uploads only the scanned object IDs. It reaches
//! the reviewed HTTP service through an authenticated loopback SOCKS proxy.
use crate::{
    Result, command,
    push_auth::Authentication,
    push_broker::TcpBroker,
    push_lfs::{Binding, Endpoint, Tool},
    push_payloads::Payloads,
    push_target::Destination,
    push_transfer::Snapshot,
};
use intercat_git::{RemoteAddress, Transport};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path, time::Duration};
use tokio::sync::watch;
fn quote(path: &Path) -> Result<String> {
    Ok(serde_json::to_string(
        path.to_str()
            .ok_or("Git LFS needs UTF-8 transport paths.")?,
    )?)
}
impl Tool {
    pub async fn push_network(
        &self,
        snapshot: &Snapshot,
        endpoint: &Endpoint,
        payloads: &Payloads,
        mut auth: Authentication,
        cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        payloads.verify(snapshot)?;
        let binding = Binding::capture(endpoint)?;
        if !snapshot.member().lfs.contains(&binding)
            || auth.folder_id != snapshot.member().folder.id
            || auth.destination != Destination::capture(&endpoint.api_url)?
        {
            return Err("The LFS connection does not match this Push review.".into());
        }
        let access = match RemoteAddress::parse(&endpoint.upload_url)?.transport {
            Transport::Http | Transport::Https => None,
            Transport::Ssh => Some(
                crate::push_ssh_access::Access::open(
                    snapshot,
                    self.git(),
                    &endpoint.upload_url,
                    &mut auth,
                    cancel.clone(),
                )
                .await?,
            ),
            _ => return Err("This LFS upload needs another transport.".into()),
        };
        if payloads.entries().is_empty() {
            return Ok(());
        }
        crate::lfs_store::sync(&snapshot.git_directory.join("lfs"), payloads.entries())?;
        let address = RemoteAddress::parse(&endpoint.api_url)?;
        let broker = TcpBroker::for_lfs_service(
            &address.host.ok_or("The LFS endpoint has no host.")?,
            address.port.ok_or("The LFS endpoint has no port.")?,
        )
        .await?;
        let temporary = tempfile::Builder::new()
            .prefix("intercat-lfs-http-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()?;
        let root = temporary.path().canonicalize()?;
        let mut stdin = tempfile::NamedTempFile::new_in(&root)?;
        for entry in payloads.entries() {
            writeln!(stdin, "{}", entry.pointer.oid())?;
        }
        let git = self.git();
        let runtime = git
            .binary()
            .parent()
            .and_then(Path::parent)
            .ok_or("Missing Git runtime.")?;
        let mut profile = String::from(
            "(version 1)(deny default)(allow process-fork)(allow process-info*)(allow sysctl-read)(allow mach-lookup)(allow signal (target self))(allow file-read-metadata)(allow file-read* (literal \"/\") (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/dev\") (subpath \"/private/var/db/dyld\") (subpath \"/private/preboot\") (subpath \"/private/etc/ssl\")",
        );
        for path in [
            runtime,
            &snapshot.git_directory,
            &root,
            &auth.home,
            &auth.xdg,
        ] {
            profile.push_str(&format!(" (subpath {})", quote(path)?));
        }
        for path in std::iter::once(self.executable())
            .chain(auth.helpers.iter().map(|path| path.as_path()))
            .chain(auth.files.iter().map(|path| path.as_path()))
        {
            profile.push_str(&format!(
                " ({} {})",
                if path.is_dir() { "subpath" } else { "literal" },
                quote(path)?
            ));
        }
        for ancestor in snapshot.root.ancestors().chain(root.ancestors()) {
            profile.push_str(&format!(" (literal {})", quote(ancestor)?));
        }
        profile.push_str(&format!(")(allow file-write* (subpath {}) (subpath {}) (literal \"/dev/null\"))(allow process-exec (literal {}) (literal {}) (subpath {}) (literal \"/bin/sh\") (literal \"/bin/bash\")",quote(&root)?,quote(&snapshot.git_directory.join("lfs"))?,quote(git.binary())?,quote(self.executable())?,quote(git.exec_dir())?));
        for helper in &auth.helpers {
            profile.push_str(&format!(" (literal {})", quote(helper)?));
        }
        if let Some(access) = &access {
            profile.push_str(" (literal \"/usr/bin/ssh\") (literal \"/usr/bin/nc\")");
            profile.push_str(&format!(
                ")(allow file-read* (subpath {}) (subpath {}))",
                quote(access.root())?,
                quote(access.tunnel.root())?
            ));
            for ancestor in access.root().ancestors() {
                profile.push_str(&format!(
                    "(allow file-read* (literal {}))",
                    quote(ancestor)?
                ));
            }
            profile.push_str(&format!(
                "(allow network-outbound (literal {})",
                quote(access.tunnel.socket())?
            ));
            if let Some(agent) = &auth.agent {
                profile.push_str(&format!(" (literal {})", quote(agent)?));
            }
            profile.push_str(")(allow file-read* (subpath \"/private/etc/ssh\")");
            if let Some(agent) = &auth.agent {
                profile.push_str(&format!(" (literal {})", quote(agent)?));
            }
        }
        profile.push_str(&format!(")(allow network-outbound (remote tcp \"localhost:{}\"))(deny file-read* file-write* (subpath {}) (subpath {}))",broker.port(),quote(&snapshot.member().folder.root)?,quote(&snapshot.member().folder.common_dir)?));
        let mut child = command::gated_command("/usr/bin/sandbox-exec", Some(stdin.path()));
        child
            .args(["-p", &profile])
            .arg(self.executable())
            .args(["push", "--object-id", "intercat-lfs-destination", "--stdin"])
            .current_dir(&snapshot.git_directory)
            .env("HOME", &auth.home)
            .env("XDG_CONFIG_HOME", &auth.xdg)
            .env("TMPDIR", &root)
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(git.binary().parent().unwrap().to_path_buf())
                        .chain(std::iter::once(git.exec_dir().to_path_buf()))
                        .chain(
                            auth.helpers
                                .iter()
                                .filter_map(|helper| helper.parent().map(Path::to_path_buf)),
                        )
                        .chain([
                            self.executable().parent().unwrap().to_path_buf(),
                            "/usr/bin".into(),
                            "/bin".into(),
                        ]),
                )?,
            )
            .env("GIT_EXEC_PATH", git.exec_dir())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0");
        if let Some(access) = &access {
            child
                .env("GIT_SSH_COMMAND", &access.command)
                .env("GIT_SSH_VARIANT", "ssh");
            if let Some(agent) = &auth.agent {
                child.env("SSH_AUTH_SOCK", agent);
            }
        }
        let mut values = auth.values;
        values.extend([
            ("http.proxy".into(), broker.proxy_url()),
            (
                "remote.intercat-lfs-destination.url".into(),
                endpoint.upload_url.clone(),
            ),
            (
                "lfs.url".into(),
                if access.is_some() {
                    endpoint.upload_url.clone()
                } else {
                    endpoint.api_url.clone()
                },
            ),
            ("lfs.allowincompletepush".into(), "false".into()),
            ("lfs.basictransfersonly".into(), "true".into()),
            ("core.hooksPath".into(), "/dev/null".into()),
        ]);
        child.env("GIT_CONFIG_COUNT", values.len().to_string());
        for (index, (key, value)) in values.iter().enumerate() {
            child
                .env(format!("GIT_CONFIG_KEY_{index}"), key)
                .env(format!("GIT_CONFIG_VALUE_{index}"), value);
        }
        let output = command::run(&mut child, cancel, Duration::from_secs(120)).await?;
        if !output.success() || output.truncated {
            return Err("The LFS upload did not finish. No Git ref was sent by this step.".into());
        }
        Ok(())
    }
}
