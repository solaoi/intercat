//! Integration checkout and command boundary. Transfer reads the original Git
//! store; project code only runs after transfer, in a different sandbox profile.
use crate::{Result, command::Output, worktree::GitTool};
use intercat_git::{ObjectId, Repository, WorktreeSource};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;

const ATTRIBUTE_LIMIT: u64 = 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Configuration {
    pub values: BTreeMap<String, String>,
    pub attributes: Option<Vec<u8>>,
    pub info_attributes: Option<Vec<u8>>,
    pub hooks_directory: PathBuf,
}

fn read_attributes(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() && meta.len() <= ATTRIBUTE_LIMIT => {
            Ok(Some(std::fs::read(path)?))
        }
        Ok(_) => Err("Git attributes must be a regular file no larger than 1 MiB.".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

impl Configuration {
    pub fn capture(source: &WorktreeSource) -> Result<Self> {
        let config = Repository::open(&source.root)?.checkout_configuration()?;
        Ok(Self {
            values: config.values,
            attributes: config
                .attributes_file
                .as_deref()
                .map(read_attributes)
                .transpose()?
                .flatten(),
            info_attributes: read_attributes(&source.common_dir.join("info/attributes"))?,
            hooks_directory: config.hooks_directory,
        })
    }
}

fn quote(path: &Path) -> Result<String> {
    let path = path.to_str().ok_or("Non-UTF8 sandbox path.")?;
    if path.contains(['\n', '\r', '\0']) {
        return Err("Invalid sandbox path.".into());
    }
    Ok(format!(
        "\"{}\"",
        path.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

#[derive(Clone, Copy)]
enum Scope {
    Transfer,
    Git,
    Code,
}

pub struct Checkout {
    pub attachment_roots: Vec<String>,
    pub root: PathBuf,
    pub workdir: PathBuf,
    pub temporary: PathBuf,
    pub configuration: Configuration,
    source: WorktreeSource,
    tool: GitTool,
    path: OsString,
    toolchain: Vec<PathBuf>,
}

impl Checkout {
    pub async fn resume(
        root: &Path,
        source: WorktreeSource,
        configuration: Configuration,
        path: OsString,
        toolchain: Vec<PathBuf>,
    ) -> Result<Self> {
        for directory in [root.to_path_buf(), root.join("tmp"), root.join("control")] {
            let meta = std::fs::symlink_metadata(&directory)?;
            if !meta.is_dir()
                || meta.file_type().is_symlink()
                || meta.uid() != unsafe { libc::geteuid() }
                || meta.mode() & 0o022 != 0
            {
                return Err("The suspended integration workspace is not private.".into());
            }
        }
        let root = root.canonicalize()?;
        if root.starts_with(&source.root) || root.starts_with(&source.common_dir) {
            return Err("The integration workspace must be outside the source.".into());
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut allowed = Vec::new();
        for path in toolchain {
            let path = path.canonicalize()?;
            if !path.is_dir()
                || source.root.starts_with(&path)
                || home.as_ref().is_some_and(|home| home.starts_with(&path))
            {
                return Err("Unsafe resumed toolchain path.".into());
            }
            allowed.push(path);
        }
        let checkout = Self {
            attachment_roots: crate::attachments::resume_checkout(
                &root.join("control"),
                &root.join("repo"),
            )?,
            workdir: root.join("repo"),
            temporary: root.join("tmp"),
            root,
            source,
            configuration,
            path,
            toolchain: allowed,
            tool: GitTool::system().await?,
        };
        checkout.verify_storage()?;
        Ok(checkout)
    }
    pub fn verify_storage(&self) -> Result<()> {
        for path in [&self.workdir, &self.workdir.join(".git")] {
            let metadata = std::fs::symlink_metadata(path)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err("The integration Git directory was replaced.".into());
            }
        }
        for entry in walkdir::WalkDir::new(self.workdir.join(".git")).follow_links(false) {
            let entry = entry?;
            if entry.file_type().is_symlink() {
                return Err("The integration Git metadata contains a symlink.".into());
            }
        }
        if self.workdir.join(".git/objects/info/alternates").exists()
            || self.workdir.join(".git/commondir").exists()
        {
            return Err("The integration checkout no longer has isolated Git storage.".into());
        }
        let source = Repository::worktree_source(&self.workdir, None)?;
        if source.root != self.workdir || source.common_dir != self.workdir.join(".git") {
            return Err("The integration Git directory was redirected.".into());
        }
        Ok(())
    }
    pub async fn create(
        root: &Path,
        source: WorktreeSource,
        head: ObjectId,
        configuration: Configuration,
        path: OsString,
        toolchain: Vec<PathBuf>,
        cancel: watch::Receiver<bool>,
    ) -> Result<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut allowed_tools = Vec::new();
        for path in toolchain {
            if !path.is_absolute() {
                return Err("Toolchain read locations must be absolute.".into());
            }
            let path = path.canonicalize()?;
            if !path.is_dir()
                || source.root.starts_with(&path)
                || home.as_ref().is_some_and(|home| home.starts_with(&path))
            {
                return Err(
                    "A toolchain read location would expose the home or source repository.".into(),
                );
            }
            allowed_tools.push(path);
        }
        std::fs::create_dir(root)?;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))?;
        let root = root.canonicalize()?;
        let meta = std::fs::symlink_metadata(&root)?;
        if !meta.is_dir()
            || meta.uid() != unsafe { libc::geteuid() }
            || root.starts_with(&source.root)
            || root.starts_with(&source.common_dir)
        {
            return Err("The integration checkout must be in its own private directory.".into());
        }
        let temporary = root.join("tmp");
        let control = root.join("control");
        for dir in [
            &temporary,
            &control,
            &temporary.join("home"),
            &control.join("empty-hooks"),
            &control.join("empty-template"),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        let mut checkout = Self {
            attachment_roots: Vec::new(),
            workdir: root.join("repo"),
            root,
            temporary,
            configuration,
            source,
            tool: GitTool::system().await?,
            path,
            toolchain: allowed_tools,
        };
        let url = reqwest::Url::from_file_path(&checkout.source.root)
            .map_err(|_| "Invalid source repository path.")?;
        // Git clone does not resolve a raw object ID advertised only under a
        // private ref when the remote HEAD is unborn. Name our pinned ref and
        // still verify the received object against the frozen ID below.
        let pinned = format!("refs/intercat/starting-points/{head}");
        let revision =
            if Repository::open(&checkout.source.root)?.reference_target(&pinned)? == Some(head) {
                pinned
            } else {
                head.to_string()
            };
        checkout
            .git_in(
                &checkout.root,
                &[
                    OsString::from("clone"),
                    OsString::from("--quiet"),
                    OsString::from("--depth=1"),
                    OsString::from("--no-tags"),
                    OsString::from("--no-checkout"),
                    OsString::from(format!(
                        "--upload-pack={}",
                        checkout.tool.exec_dir().join("git-upload-pack").display()
                    )),
                    OsString::from(format!("--revision={revision}")),
                    OsString::from(format!(
                        "--template={}",
                        checkout.root.join("control/empty-template").display()
                    )),
                    OsString::from("--"),
                    OsString::from(url.as_str()),
                    checkout.workdir.clone().into_os_string(),
                ],
                true,
                cancel.clone(),
            )
            .await?
            .require_success("Isolated clone")?;
        // Verify transfer before allowing any repository-controlled process.
        let cloned = Repository::worktree_source(&checkout.workdir, Some(head))?;
        if cloned.head != head
            || checkout
                .workdir
                .join(".git/objects/info/alternates")
                .exists()
            || checkout.workdir.join(".git/commondir").exists()
        {
            return Err("The integration clone shares Git storage.".into());
        }
        let actual = checkout
            .git(
                &[OsString::from("rev-parse"), OsString::from("HEAD")],
                false,
                cancel.clone(),
            )
            .await?
            .require_success("Clone verification")?;
        if String::from_utf8(actual.stdout)?
            .trim()
            .parse::<ObjectId>()?
            != head
        {
            return Err("The integration clone has the wrong base.".into());
        }
        // The local source URL must not be available to hooks or Check.
        std::fs::create_dir_all(checkout.workdir.join(".git/info"))?;
        // Generated dependency directories must remain untracked even in a
        // repository without a matching .gitignore. Tracked files still win.
        std::fs::write(
            checkout.workdir.join(".git/info/exclude"),
            "node_modules/\n.venv/\n",
        )?;
        checkout
            .git(
                &["remote".into(), "remove".into(), "origin".into()],
                false,
                cancel.clone(),
            )
            .await?
            .require_success("Remove clone transport")?;
        for (key, value) in &checkout.configuration.values {
            checkout
                .git(
                    &["config".into(), "--local".into(), key.into(), value.into()],
                    false,
                    cancel.clone(),
                )
                .await?
                .require_success("Copy checkout settings")?;
        }
        // A configured process filter must succeed. Git otherwise silently
        // falls back to unfiltered content when required=false, which could
        // make a Check pass against the wrong on-disk representation.
        for key in checkout
            .configuration
            .values
            .keys()
            .filter(|key| key.ends_with(".process"))
        {
            let required = format!("{}.required", key.trim_end_matches(".process"));
            checkout
                .git(
                    &[
                        "config".into(),
                        "--local".into(),
                        required.into(),
                        "true".into(),
                    ],
                    false,
                    cancel.clone(),
                )
                .await?
                .require_success("Require process filter")?;
        }
        if let Some(bytes) = &checkout.configuration.attributes {
            let path = checkout.root.join("control/attributes");
            std::fs::write(&path, bytes)?;
            checkout
                .git(
                    &[
                        "config".into(),
                        "--local".into(),
                        "core.attributesFile".into(),
                        path.into_os_string(),
                    ],
                    false,
                    cancel.clone(),
                )
                .await?
                .require_success("Copy attributes")?;
        }
        if let Some(bytes) = &checkout.configuration.info_attributes {
            std::fs::create_dir_all(checkout.workdir.join(".git/info"))?;
            std::fs::write(checkout.workdir.join(".git/info/attributes"), bytes)?;
        }
        // A clone always owns its LFS storage, even if the original configured
        // an absolute path. Object population belongs to the LFS preparation.
        checkout
            .git(
                &[
                    "config".into(),
                    "--local".into(),
                    "lfs.storage".into(),
                    checkout.workdir.join(".git/lfs").into_os_string(),
                ],
                false,
                cancel,
            )
            .await?
            .require_success("Isolate LFS storage")?;
        checkout.attachment_roots = crate::attachments::reserve_checkout(
            &checkout.root.join("control"),
            &checkout.workdir,
            &head.to_string(),
        )?;
        Ok(checkout)
    }

    fn profile(&self, scope: Scope) -> Result<String> {
        let transfer = matches!(scope, Scope::Transfer);
        let code = matches!(scope, Scope::Code);
        let mut profile = String::from(
            "(version 1)(deny default)(allow process-fork)(allow process-info*)(allow sysctl-read)(allow mach-lookup)(allow signal (target self))(allow file-read-metadata)(allow file-read* (literal \"/\") (subpath \"/System\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/dev\") (subpath \"/private/var/db/dyld\") (subpath \"/private/preboot\") (subpath \"/Library/Developer\")",
        );
        for path in [
            &self.workdir,
            &self.temporary,
            &self.root.join("control"),
            self.tool.exec_dir(),
            self.tool.binary().parent().unwrap(),
            self.tool
                .exec_dir()
                .parent()
                .and_then(Path::parent)
                .ok_or("Invalid Git installation.")?,
        ] {
            profile.push_str(&format!(" (subpath {})", quote(path)?));
        }
        // getcwd() needs to inspect directory entries in the ancestor chain.
        // Literal directory access does not grant reads of neighboring files.
        for ancestor in self.root.ancestors() {
            profile.push_str(&format!(" (literal {})", quote(ancestor)?));
        }
        if transfer {
            profile.push_str(&format!(
                " (subpath {}) (subpath {})",
                quote(&self.source.root)?,
                quote(&self.source.common_dir)?
            ));
        }
        if code {
            for path in &self.toolchain {
                profile.push_str(&format!(" (subpath {})", quote(path)?));
            }
        }
        profile.push(')');
        if code {
            profile.push_str("(allow process-exec)");
        } else {
            profile.push_str(&format!(
                "(allow process-exec (literal {}) (subpath {}))",
                quote(self.tool.binary())?,
                quote(self.tool.exec_dir())?
            ));
            // Git's file:// transport invokes this fixed upload-pack command
            // through /bin/sh, including for paths without shell metacharacters.
            // Only the private transfer call gets this permission.
            if transfer {
                profile
                    .push_str("(allow process-exec (literal \"/bin/sh\") (literal \"/bin/bash\"))");
            }
        }
        profile.push_str(&format!(
            "(allow file-write* (subpath {}) (subpath {}) (literal \"/dev/null\"))",
            quote(&self.workdir)?,
            quote(&self.temporary)?
        ));
        if !transfer {
            profile.push_str(&format!(
                "(deny file-read* file-write* (subpath {}) (subpath {}))",
                quote(&self.source.root)?,
                quote(&self.source.common_dir)?
            ));
        }
        Ok(profile)
    }

    fn command(
        &self,
        cwd: &Path,
        program: &Path,
        arguments: &[OsString],
        scope: Scope,
    ) -> Result<tokio::process::Command> {
        let mut command = crate::command::gated_command("/usr/bin/sandbox-exec");
        command
            .args([OsStr::new("-p"), OsStr::new(&self.profile(scope)?)])
            .arg(program)
            .args(arguments)
            .current_dir(cwd)
            .env("PATH", &self.path)
            .env("HOME", self.temporary.join("home"))
            .env("TMPDIR", &self.temporary)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_ATTR_NOSYSTEM", "1");
        Ok(command)
    }

    async fn git_in(
        &self,
        cwd: &Path,
        args: &[OsString],
        transfer: bool,
        cancel: watch::Receiver<bool>,
    ) -> Result<Output> {
        let mut arguments = vec![
            "-c".into(),
            "core.fsmonitor=false".into(),
            "-c".into(),
            "commit.gpgsign=false".into(),
            "-c".into(),
            "gc.auto=0".into(),
            "-c".into(),
            "maintenance.auto=false".into(),
            "-c".into(),
            "uploadpack.packObjectsHook=".into(),
            "-c".into(),
            format!(
                "core.hooksPath={}",
                self.root.join("control/empty-hooks").display()
            )
            .into(),
        ];
        if transfer {
            arguments.extend(["-c".into(), "core.attributesFile=/dev/null".into()]);
        }
        arguments.extend_from_slice(args);
        let mut command = self.command(
            cwd,
            self.tool.binary(),
            &arguments,
            if transfer {
                Scope::Transfer
            } else {
                Scope::Git
            },
        )?;
        crate::command::run(&mut command, cancel, Duration::from_secs(60)).await
    }
    pub async fn git(
        &self,
        args: &[OsString],
        code: bool,
        cancel: watch::Receiver<bool>,
    ) -> Result<Output> {
        if !code {
            return self.git_in(&self.workdir, args, false, cancel).await;
        }
        let mut arguments = vec![
            "-c".into(),
            "core.fsmonitor=false".into(),
            "-c".into(),
            "commit.gpgsign=false".into(),
            "-c".into(),
            format!(
                "core.hooksPath={}",
                self.root.join("control/empty-hooks").display()
            )
            .into(),
        ];
        arguments.extend_from_slice(args);
        let mut command =
            self.command(&self.workdir, self.tool.binary(), &arguments, Scope::Code)?;
        crate::command::run(&mut command, cancel, Duration::from_secs(60)).await
    }
    pub async fn deploy(&self, tree: ObjectId, cancel: watch::Receiver<bool>) -> Result<()> {
        self.verify_storage()?;
        let root = self.workdir.clone();
        let source = self.source.root.clone();
        let roots = self.attachment_roots.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let repo = Repository::open(root)?;
            repo.import_tree(&source, tree)?;
            let incoming = repo.tree_files(tree)?;
            intercat_git::AttachmentPaths::from_paths(incoming.keys().map(String::as_str))
                .with_roots(&roots)?;
            repo.sync_objects(&[tree])
        })
        .await??;
        self.git(
            &[
                "read-tree".into(),
                "--reset".into(),
                "-u".into(),
                tree.to_string().into(),
            ],
            true,
            cancel,
        )
        .await?
        .require_success("Checkout")?;
        self.ensure_attachments()?;
        Ok(())
    }
    pub fn ensure_attachments(&self) -> Result<()> {
        let roots = crate::attachments::resume_checkout(&self.root.join("control"), &self.workdir)?;
        if roots != self.attachment_roots {
            return Err("The attachment directories changed.".into());
        }
        Ok(())
    }
    pub async fn run_check(
        &self,
        script: &str,
        environment: &BTreeMap<String, String>,
        cancel: watch::Receiver<bool>,
        timeout: Duration,
    ) -> Result<Output> {
        let mut command = self.command(
            &self.workdir,
            Path::new("/bin/sh"),
            &["-c".into(), script.into()],
            Scope::Code,
        )?;
        if environment
            .keys()
            .any(|name| !crate::secrets::valid_environment_name(name))
        {
            return Err("Check environment contains a reserved or invalid variable.".into());
        }
        command.envs(environment);
        crate::command::run(&mut command, cancel, timeout).await
    }

    /// Start from the candidate tree so force-added ignored files remain in
    /// the temporary index. The real index and worktree are never reset here.
    pub async fn tree_on_disk(
        &self,
        expected: ObjectId,
        cancel: watch::Receiver<bool>,
    ) -> Result<ObjectId> {
        self.verify_storage()?;
        let index = self
            .temporary
            .join(format!("index-{}", uuid::Uuid::now_v7()));
        let executable = shell_quote(self.tool.binary().as_os_str())?;
        let index_name = shell_quote(index.as_os_str())?;
        let script = format!(
            "export GIT_INDEX_FILE={index_name}\n{executable} -c core.fsmonitor=false read-tree {expected} && {executable} -c core.fsmonitor=false add -A -- ."
        );
        let result = async {
            let output = self
                .run_check(&script, &BTreeMap::new(), cancel, Duration::from_secs(60))
                .await?
                .require_success("Compare checkout with candidate")?;
            if output.truncated {
                return Err("Tree comparison output exceeded the limit.".into());
            }
            let repo = Repository::open(&self.workdir)?;
            repo.tree_from_index_without_attachments(&index, expected, &self.attachment_roots)
        }
        .await;
        let _ = std::fs::remove_file(index);
        result
    }

    pub async fn stage(
        &self,
        original_base: ObjectId,
        cancel: watch::Receiver<bool>,
    ) -> Result<Vec<String>> {
        self.git(
            &["add".into(), "-A".into(), "--".into(), ".".into()],
            true,
            cancel,
        )
        .await?
        .require_success("Stage candidate")?;
        self.verify_storage()?;
        let repository = Repository::open(&self.workdir)?;
        let index = self.workdir.join(".git/index");
        let before = repository.tree_files(repository.tree_from_index(&index)?)?;
        repository.sanitize_snapshot_index(original_base, &index, &self.attachment_roots)?;
        let after = repository.tree_files(repository.tree_from_index(&index)?)?;
        Ok(before
            .keys()
            .filter(|path| !after.contains_key(*path))
            .cloned()
            .collect())
    }

    pub fn import_commit(&self, commit: ObjectId) -> Result<intercat_git::CommitDetails> {
        self.verify_storage()?;
        let repository = Repository::open(&self.workdir)?;
        let details = repository.commit_details(commit)?;
        let target = Repository::open(&self.source.root)?;
        target.import_commit(&repository, commit)?;
        target.sync_objects(&[commit])?;
        Ok(details)
    }
}

pub(crate) fn shell_quote(value: &OsStr) -> Result<String> {
    let value = value.to_str().ok_or("Non-UTF8 command path.")?;
    if value.contains('\0') {
        return Err("Invalid command argument.".into());
    }
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

#[cfg(test)]
mod boundary_tests {
    use super::*;

    #[tokio::test]
    async fn checkout_profiles_cannot_connect_to_the_private_rpc_socket() -> Result<()> {
        let probe = crate::sandbox_probe::Probe::new()?;
        let checkout = Checkout {
            attachment_roots: vec![],
            root: probe.path("job"),
            workdir: probe.path("job/repo"),
            temporary: probe.path("job/tmp"),
            source: probe.source(),
            configuration: Configuration {
                values: BTreeMap::new(),
                attributes: None,
                info_attributes: None,
                hooks_directory: probe.path("source/.git/hooks"),
            },
            tool: GitTool::new(probe.program.clone()).await?,
            path: "/usr/bin:/bin".into(),
            toolchain: vec![],
        };
        for (name, scope) in [
            ("transfer", Scope::Transfer),
            ("git", Scope::Git),
            ("code", Scope::Code),
        ] {
            probe.check(name, &checkout.profile(scope)?)?;
        }
        Ok(())
    }
}
