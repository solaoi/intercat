//! Upload a fixed list of scanned LFS objects before changing any Git ref.
use crate::{
    Result, command,
    push_lfs::{Binding, Endpoint, Tool},
    push_payloads::Payloads,
    push_transfer::Snapshot,
};
use intercat_git::{RemoteAddress, Repository, Transport};
use std::{fs, io::Write, os::unix::fs::PermissionsExt, path::Path, time::Duration};
use tokio::sync::watch;

fn quote(path: &Path) -> Result<String> {
    Ok(serde_json::to_string(
        path.to_str()
            .ok_or("Git LFS needs UTF-8 transport paths.")?,
    )?)
}
impl Tool {
    pub async fn push_file(
        &self,
        snapshot: &Snapshot,
        endpoint: &Endpoint,
        payloads: &Payloads,
        cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        payloads.verify(snapshot)?;
        let binding = Binding::capture(endpoint)?;
        if !snapshot.member().lfs.contains(&binding) {
            return Err("LFS destination changed since you reviewed · Review".into());
        }
        if RemoteAddress::parse(&endpoint.api_url)?.transport != Transport::File {
            return Err("This LFS endpoint needs a network transport.".into());
        }
        let remote = reqwest::Url::parse(&endpoint.api_url)
            .map_err(|_| "Invalid local LFS endpoint.")?
            .to_file_path()
            .map_err(|_| "Invalid local LFS endpoint.")?;
        let remote = Repository::open(remote)?.common_directory()?;
        if remote.starts_with(&snapshot.root)
            || snapshot.root.starts_with(&remote)
            || remote == snapshot.member().folder.common_dir
        {
            return Err(
                "The LFS destination must be separate from the source and transport store.".into(),
            );
        }
        if payloads.entries().is_empty() {
            return Ok(());
        }
        let storage = snapshot.git_directory.join("lfs");
        crate::lfs_store::sync(&storage, payloads.entries())?;
        let temporary = tempfile::Builder::new()
            .prefix("intercat-lfs-push-")
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
        let mut profile = format!(
            "(version 1)(deny default)(allow process-fork)(allow process-info*)(allow sysctl-read)(allow mach-lookup)(allow signal (target self))(allow file-read-metadata)(allow file-read* (literal \"/\") (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/dev\") (subpath \"/private/var/db/dyld\") (subpath \"/private/preboot\") (subpath {}) (subpath {}) (subpath {}) (subpath {}) (literal {}))(allow file-write* (subpath {}) (subpath {}) (subpath {}) (literal \"/dev/null\"))(allow process-exec (literal {}) (literal {}) (subpath {}) (literal \"/bin/sh\") (literal \"/bin/bash\"))(deny file-read* file-write* (subpath {}) (subpath {}))",
            quote(runtime)?,
            quote(&snapshot.git_directory)?,
            quote(&remote)?,
            quote(&root)?,
            quote(self.executable())?,
            quote(&remote)?,
            quote(&root)?,
            quote(&storage)?,
            quote(git.binary())?,
            quote(self.executable())?,
            quote(git.exec_dir())?,
            quote(&snapshot.member().folder.root)?,
            quote(&snapshot.member().folder.common_dir)?,
        );
        for path in [&snapshot.git_directory, &remote, &root] {
            for ancestor in path.ancestors() {
                profile.push_str(&format!(
                    "(allow file-read* (literal {}))",
                    quote(ancestor)?
                ));
            }
        }
        let mut child = command::gated_command("/usr/bin/sandbox-exec", Some(stdin.path()));
        child
            .args(["-p", &profile])
            .arg(self.executable())
            .args(["push", "--object-id", &endpoint.api_url, "--stdin"])
            .current_dir(&snapshot.git_directory)
            .env("HOME", &root)
            .env("TMPDIR", &root)
            .env(
                "PATH",
                std::env::join_paths([
                    git.binary().parent().unwrap(),
                    self.executable().parent().unwrap(),
                    Path::new("/usr/bin"),
                    Path::new("/bin"),
                ])?,
            )
            .env("GIT_EXEC_PATH", git.exec_dir())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0");
        let values = [
            ("lfs.url", endpoint.api_url.as_str()),
            ("lfs.allowincompletepush", "false"),
            ("core.hooksPath", "/dev/null"),
        ];
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
