//! Read Git's resolved push URLs without network access or executable helpers.
//! The source checkout, index and configuration are never changed here.
use crate::{
    Result,
    git_tool::GitTool,
    project::Folder,
    push_target::{Destination, Target},
};
use intercat_git::{ObjectId, Repository};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;

pub struct Inspector {
    git: GitTool,
    home: PathBuf,
    xdg: PathBuf,
    system: bool,
}
fn quote(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or("Git needs UTF-8 configuration paths.")?;
    if path.contains(['\0', '\n', '\r']) {
        return Err("Invalid Git configuration path.".into());
    }
    Ok(format!(
        "\"{}\"",
        path.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}
impl Inspector {
    pub(crate) fn user_paths(&self) -> (&Path, &Path) {
        (&self.home, &self.xdg)
    }
    pub(crate) fn system(&self) -> bool {
        self.system
    }

    pub(crate) async fn authentication(
        &self,
        folder: &Folder,
        cancel: watch::Receiver<bool>,
    ) -> Result<Vec<(String, String, String)>> {
        let output = self
            .query(
                folder,
                &[
                    "config",
                    "--show-scope",
                    "--null",
                    "--get-regexp",
                    "^(credential\\..*|http\\..*)$",
                ],
                cancel,
            )
            .await?;
        if output.code == Some(1) {
            return Ok(Vec::new());
        }
        let text = Self::text(output)?;
        let fields = text.split_terminator('\0').collect::<Vec<_>>();
        if !fields.len().is_multiple_of(2) {
            return Err("Invalid Git authentication configuration.".into());
        }
        fields
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let (key, value) = pair[1]
                    .split_once('\n')
                    .ok_or("Invalid Git authentication configuration.")?;
                Ok((pair[0].into(), key.into(), value.into()))
            })
            .collect()
    }

    pub fn new(git: GitTool, home: PathBuf, xdg: PathBuf, system: bool) -> Result<Self> {
        if !home.is_absolute() || !xdg.is_absolute() {
            return Err("Git needs absolute user configuration paths.".into());
        }
        Ok(Self {
            git,
            home: crate::physical_path::resolve(&home)?,
            xdg: crate::physical_path::resolve(&xdg)?,
            system,
        })
    }
    async fn query(
        &self,
        folder: &Folder,
        args: &[&str],
        cancel: watch::Receiver<bool>,
    ) -> Result<crate::command::Output> {
        {
            let repo = Repository::open(&folder.root)?;
            if repo.common_directory()? != folder.common_dir
                || repo.working_directory()? != folder.root
            {
                return Err("The Push source now belongs to another repository.".into());
            }
        }
        let mut profile = String::from(
            "(version 1)(deny default)(allow process-fork)(allow process-info*)(allow sysctl-read)(allow file-read-metadata)(allow file-read* (literal \"/\") (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/dev\") (subpath \"/private/var/db/dyld\") (subpath \"/private/preboot\") (subpath \"/private/etc\")",
        );
        let tool_root = self
            .git
            .binary()
            .parent()
            .and_then(Path::parent)
            .ok_or("Missing Git runtime directory.")?;
        for path in [
            self.home.as_path(),
            self.xdg.as_path(),
            &folder.root,
            &folder.common_dir,
            tool_root,
        ] {
            profile.push_str(&format!(" (subpath {})", quote(path)?));
        }
        profile.push_str(&format!(") (allow file-write* (literal \"/dev/null\"))(allow process-exec (literal {}) (subpath {}))",quote(self.git.binary())?,quote(self.git.exec_dir())?));
        let mut command = crate::command::gated_command("/usr/bin/sandbox-exec", None);
        command
            .args(["-p", &profile])
            .arg(self.git.binary())
            .args(args)
            .current_dir(&folder.root)
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.xdg)
            .env("GIT_CONFIG_NOSYSTEM", if self.system { "0" } else { "1" })
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("GIT_EXEC_PATH", self.git.exec_dir())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0");
        let output = crate::command::run(&mut command, cancel, Duration::from_secs(15)).await?;
        if output.truncated || output.timed_out || output.cancelled {
            return Err("The Push configuration could not be read completely.".into());
        }
        Ok(output)
    }
    fn text(output: crate::command::Output) -> Result<String> {
        if !output.success() {
            return Err("The Push configuration could not be read.".into());
        }
        String::from_utf8(output.stdout)
            .map_err(|_| "Git returned a non-UTF8 Push configuration.".into())
    }
    pub async fn remotes(
        &self,
        folder: &Folder,
        cancel: watch::Receiver<bool>,
    ) -> Result<Vec<String>> {
        let text = Self::text(self.query(folder, &["remote"], cancel).await?)?;
        let names = text
            .split_terminator('\n')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        for name in &names {
            crate::push_target::validate_remote(name)?;
        }
        Ok(names)
    }
    pub async fn destinations(
        &self,
        folder: &Folder,
        remote: &str,
        cancel: watch::Receiver<bool>,
    ) -> Result<Vec<String>> {
        crate::push_target::validate_remote(remote)?;
        let mirror = self
            .query(
                folder,
                &[
                    "config",
                    "--bool",
                    "--get",
                    &format!("remote.{remote}.mirror"),
                ],
                cancel.clone(),
            )
            .await?;
        if mirror.success() {
            if String::from_utf8(mirror.stdout)?.trim() == "true" {
                return Err(format!(
                    "{remote} mirrors every ref · Intercat pushes one branch only"
                )
                .into());
            }
        } else if mirror.code != Some(1) {
            return Err("The remote mirror setting could not be read.".into());
        }
        let text = Self::text(
            self.query(
                folder,
                &["remote", "get-url", "--push", "--all", "--", remote],
                cancel,
            )
            .await?,
        )?;
        let urls = text
            .split_terminator('\n')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if urls.is_empty() || urls.len() > 16 {
            return Err("Push needs between one and sixteen destinations.".into());
        }
        for url in &urls {
            Destination::capture(url)?;
        }
        Ok(urls)
    }
    pub(crate) async fn lfs_settings(
        &self,
        folder: &Folder,
        remote: &str,
        cancel: watch::Receiver<bool>,
    ) -> Result<Vec<(String, String)>> {
        crate::push_target::validate_remote(remote)?;
        let expression = format!(
            "^(lfs\\.(url|pushurl|gitprotocol)|remote\\.{}\\.(lfsurl|lfspushurl)|url\\..*\\.(insteadof|pushinsteadof))$",
            regex::escape(remote),
        );
        let output = self
            .query(
                folder,
                &["config", "--null", "--get-regexp", &expression],
                cancel,
            )
            .await?;
        if output.code == Some(1) {
            return Ok(Vec::new());
        }
        let text = Self::text(output)?;
        let mut values = Vec::new();
        for record in text.split_terminator('\0') {
            let (key, value) = record
                .split_once('\n')
                .ok_or("Invalid Git LFS configuration.")?;
            values.push((key.into(), value.into()));
        }
        Ok(values)
    }

    pub async fn freeze(
        &self,
        folder: &Folder,
        source_ref: &str,
        remote: &str,
        target_ref: &str,
        sha: Option<ObjectId>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Target> {
        intercat_git::validate_branch_reference(source_ref)?;
        intercat_git::validate_branch_reference(target_ref)?;
        let head = {
            let repo = Repository::open(&folder.root)?;
            let head = repo
                .reference_target(source_ref)?
                .ok_or("This branch has no saved Commit to push.")?;
            let selected = sha.unwrap_or(head);
            repo.commit_details(selected)?;
            if selected != head && repo.merge_base(selected, head)? != Some(selected) {
                return Err("Push to here must select a commit on the reviewed branch.".into());
            }
            selected.to_string()
        };
        let urls = self.destinations(folder, remote, cancel).await?;
        let target = Target {
            source_ref: source_ref.into(),
            sha: head,
            remote: remote.into(),
            target_ref: target_ref.into(),
            destinations: urls
                .iter()
                .map(|url| Destination::capture(url))
                .collect::<Result<_>>()?,
        };
        target.validate()?;
        Ok(target)
    }
}
