//! Integration checkout and command boundary. Transfer reads the original Git
//! store; project code only runs after transfer, in a different sandbox profile.
use crate::{Result, command::Output, git_tool::GitTool};
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
    pub lfs_directory: PathBuf,
    pub object_directories: Vec<PathBuf>,
    pub denied_roots: Vec<PathBuf>,
}

fn read_attributes(path: &Path) -> Result<Option<Vec<u8>>> {
    // Git uses this path to disable attributes. It is a setting, not a file
    // whose device type should be accepted as an arbitrary attributes input.
    if path == Path::new("/dev/null") {
        return Ok(None);
    }
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
            lfs_directory: crate::physical_path::resolve(&config.lfs_directory)?,
            object_directories: Repository::open(&source.root)?.object_directories()?,
            denied_roots: Vec::new(),
        })
    }

    /// Command-line settings override repository and user configuration. Both
    /// checkpoint clean filters and integration smudge filters use this set.
    pub(crate) fn isolated_values(
        &self,
        control: &Path,
        storage: &Path,
    ) -> Result<BTreeMap<String, String>> {
        let mut values = self.values.clone();
        for key in self.values.keys().filter(|key| key.ends_with(".process")) {
            values.insert(
                format!("{}.required", key.trim_end_matches(".process")),
                "true".into(),
            );
        }
        let attributes = if let Some(bytes) = &self.attributes {
            let path = control.join("attributes");
            std::fs::write(&path, bytes)?;
            path
        } else {
            PathBuf::from("/dev/null")
        };
        for (key, path) in [
            ("core.attributesFile", attributes.as_path()),
            ("lfs.storage", storage),
        ] {
            values.insert(
                key.into(),
                path.to_str()
                    .ok_or("Non-UTF8 Git configuration path.")?
                    .into(),
            );
        }
        Ok(values)
    }
}

pub(crate) fn toolchain_paths(path: &str, source: &Path) -> Result<Vec<PathBuf>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut paths = Vec::new();
    for directory in std::env::split_paths(path).filter(|dir| dir.is_absolute() && dir.is_dir()) {
        let directory = directory.canonicalize()?;
        if !source.starts_with(&directory)
            && !home
                .as_ref()
                .is_some_and(|home| home.starts_with(&directory))
        {
            paths.push(directory);
        }
    }
    if let Some(node) = std::env::split_paths(path)
        .map(|dir| dir.join("node"))
        .find(|path| path.is_file())
    {
        let node = node.canonicalize()?;
        if let Some(root) = node.parent().and_then(Path::parent) {
            paths.push(root.to_path_buf());
        }
    }
    paths.sort();
    paths.dedup();
    paths.retain(|directory| {
        !source.starts_with(directory)
            && !home
                .as_ref()
                .is_some_and(|home| home.starts_with(directory))
    });
    Ok(paths)
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

#[derive(Clone)]
enum Scope {
    Transfer,
    Git,
    Code,
    Worktree(PathBuf),
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
    pub(crate) fn source(&self) -> &WorktreeSource {
        &self.source
    }

    /// A private child reads payloads from this candidate, including objects
    /// that the secret gate has deliberately kept out of the project store.
    pub fn child_configuration(&self) -> Result<Configuration> {
        self.verify_storage()?;
        let mut configuration = self.configuration.clone();
        configuration.denied_roots.extend([
            self.source.root.clone(),
            self.source.common_dir.clone(),
            self.root.clone(),
            self.configuration.lfs_directory.clone(),
        ]);
        configuration
            .denied_roots
            .extend(self.configuration.object_directories.iter().cloned());
        configuration.denied_roots.sort();
        configuration.denied_roots.dedup();
        configuration.lfs_directory = self.workdir.join(".git/lfs");
        configuration.object_directories = Repository::open(&self.workdir)?.object_directories()?;
        Ok(configuration)
    }

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
        if root.starts_with(&source.root)
            || root.starts_with(&source.common_dir)
            || root.starts_with(&configuration.lfs_directory)
        {
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
        let repository = Repository::open(&self.workdir)?;
        if repository.worktree_root()? != self.workdir
            || repository.common_directory()? != self.workdir.join(".git")
        {
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
            || root.starts_with(&configuration.lfs_directory)
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
        // private ref when the remote HEAD is unborn. Name a protecting ref and
        // still verify the received object against the frozen ID below.
        let revision = Repository::open(&checkout.source.root)?
            .reference_to(head)?
            .unwrap_or_else(|| head.to_string());
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
        checkout.configure(cancel).await?;
        checkout.attachment_roots = crate::attachments::reserve_checkout(
            &checkout.root.join("control"),
            &checkout.workdir,
            &head.to_string(),
        )?;
        Ok(checkout)
    }

    async fn configure(&self, cancel: watch::Receiver<bool>) -> Result<()> {
        let names = self
            .git(
                &[
                    "config".into(),
                    "--local".into(),
                    "--name-only".into(),
                    "--get-regexp".into(),
                    "^(filter\\.|core\\.(autocrlf|eol|symlinks|attributesfile)$|lfs\\.storage$)"
                        .into(),
                ],
                false,
                cancel.clone(),
            )
            .await?;
        if !names.success() && names.code != Some(1) {
            names.require_success("Read copied checkout settings")?;
        } else {
            let mut keys: Vec<_> = String::from_utf8(names.stdout)?
                .lines()
                .map(str::to_owned)
                .collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                self.git(
                    &[
                        "config".into(),
                        "--local".into(),
                        "--unset-all".into(),
                        key.into(),
                    ],
                    false,
                    cancel.clone(),
                )
                .await?
                .require_success("Remove previous checkout settings")?;
            }
        }
        for (key, value) in self
            .configuration
            .isolated_values(&self.root.join("control"), &self.workdir.join(".git/lfs"))?
        {
            self.git(
                &["config".into(), "--local".into(), key.into(), value.into()],
                false,
                cancel.clone(),
            )
            .await?
            .require_success("Copy checkout settings")?;
        }
        if let Some(bytes) = &self.configuration.info_attributes {
            std::fs::create_dir_all(self.workdir.join(".git/info"))?;
            std::fs::write(self.workdir.join(".git/info/attributes"), bytes)?;
        } else if self.workdir.join(".git/info/attributes").exists() {
            std::fs::remove_file(self.workdir.join(".git/info/attributes"))?;
        }
        Ok(())
    }

    /// A different physical path ensures an old Check process cannot write
    /// into this generation. Request APFS clones, never hard links, and copy
    /// symbolic links without following them.
    pub async fn next_check(&self, head: ObjectId, cancel: watch::Receiver<bool>) -> Result<Self> {
        self.verify_storage()?;
        if Repository::open(&self.workdir)?.head_commit()? != head {
            return Err(
                "The candidate head changed before its Check generation was copied.".into(),
            );
        }
        let parent = self
            .root
            .parent()
            .ok_or("The Check workspace needs a parent directory.")?;
        let directory = tempfile::Builder::new()
            .prefix("check-")
            .tempdir_in(parent)?;
        let root = directory.path().canonicalize()?;
        let mut copy = crate::command::gated_command("/bin/cp", None);
        copy.args([
            OsStr::new("-cRP"),
            self.root.join(".").as_os_str(),
            root.as_os_str(),
        ]);
        crate::command::run(&mut copy, cancel.clone(), Duration::from_secs(300))
            .await?
            .require_success("Copy Check generation")?;
        let roots = crate::attachments::clone_checkout(
            &self.root.join("control"),
            &self.workdir,
            &root.join("control"),
            &root.join("repo"),
        )?;
        if roots != self.attachment_roots {
            return Err(
                "The attachment directories changed while the Check generation was copied.".into(),
            );
        }
        let checkout = Self {
            attachment_roots: roots,
            workdir: root.join("repo"),
            temporary: root.join("tmp"),
            root,
            configuration: self.configuration.clone(),
            source: self.source.clone(),
            tool: self.tool.clone(),
            path: self.path.clone(),
            toolchain: self.toolchain.clone(),
        };
        checkout.verify_storage()?;
        {
            let repo = Repository::open(&checkout.workdir)?;
            if repo.head_commit()? != head {
                return Err("The copied candidate head changed.".into());
            }
            crate::lfs_store::sync(
                &checkout.workdir.join(".git/lfs"),
                &repo.lfs_entries(repo.tree_id(head)?)?,
            )?;
        }
        checkout.configure(cancel).await?;
        let _ = directory.keep();
        Ok(checkout)
    }

    /// History is opt-in for repository code. Turning it off replaces the
    /// object database instead of merely removing refs to historical objects.
    pub async fn set_history(
        &self,
        enabled: bool,
        head: ObjectId,
        cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        self.verify_storage()?;
        let marker = self.root.join("control/full-history");
        let url = reqwest::Url::from_file_path(&self.source.root)
            .map_err(|_| "Invalid history source.")?;
        let upload = format!(
            "--upload-pack={}",
            self.tool.exec_dir().join("git-upload-pack").display()
        );
        if enabled {
            // Publish the marker before fetching so a restart cannot mistake
            // a full object store for a shallow one.
            std::fs::write(&marker, b"full history and tags for Check and hooks\n")?;
            std::fs::File::open(&marker)?.sync_all()?;
            std::fs::File::open(marker.parent().unwrap())?.sync_all()?;
            let mut args: Vec<OsString> = vec![
                "fetch".into(),
                "--force".into(),
                "--tags".into(),
                upload.into(),
            ];
            if self.workdir.join(".git/shallow").exists() {
                args.push("--unshallow".into());
            }
            args.extend(["--".into(), url.as_str().into(), head.to_string().into()]);
            self.git_in(&self.workdir, &args, true, cancel.clone())
                .await?
                .require_success("Load Check history")?;
        } else if marker.exists() || !self.workdir.join(".git/shallow").is_file() {
            let temporary = tempfile::Builder::new()
                .prefix("shallow-")
                .tempdir_in(&self.temporary)?;
            let target = temporary.path().join("repo");
            let revision = Repository::open(&self.source.root)?
                .reference_to(head)?
                .unwrap_or_else(|| head.to_string());
            self.git_in(
                &self.root,
                &[
                    "clone".into(),
                    "--quiet".into(),
                    "--depth=1".into(),
                    "--no-tags".into(),
                    "--no-checkout".into(),
                    upload.into(),
                    format!("--revision={revision}").into(),
                    format!(
                        "--template={}",
                        self.root.join("control/empty-template").display()
                    )
                    .into(),
                    "--".into(),
                    url.as_str().into(),
                    target.clone().into_os_string(),
                ],
                true,
                cancel.clone(),
            )
            .await?
            .require_success("Remove Check history")?;
            if Repository::open(&target)?.head_commit()? != head {
                return Err("The shallow Check clone does not match its candidate.".into());
            }
            let repo = Repository::open(&self.workdir)?;
            crate::lfs_store::transfer(
                &self.workdir.join(".git/lfs"),
                &target.join(".git/lfs"),
                &repo.lfs_entries(repo.tree_id(head)?)?,
            )?;
            std::fs::remove_dir_all(self.workdir.join(".git"))?;
            std::fs::rename(target.join(".git"), self.workdir.join(".git"))?;
            std::fs::remove_file(marker)?;
        }
        self.configure(cancel).await?;
        if Repository::open(&self.workdir)?.head_commit()? != head {
            return Err("Loading history changed the candidate head.".into());
        }
        Ok(())
    }

    fn profile(&self, scope: &Scope) -> Result<String> {
        let transfer = matches!(scope, Scope::Transfer);
        let code = matches!(scope, Scope::Code | Scope::Worktree(_));
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
            for path in &self.configuration.object_directories {
                profile.push_str(&format!(" (subpath {})", quote(path)?));
            }
        }
        if code {
            for path in &self.toolchain {
                profile.push_str(&format!(" (subpath {})", quote(path)?));
            }
        }
        if let Scope::Worktree(path) = scope {
            profile.push_str(&format!(" (subpath {})", quote(path)?));
            for ancestor in path.ancestors() {
                profile.push_str(&format!(" (literal {})", quote(ancestor)?));
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
        if let Scope::Worktree(path) = scope {
            profile.push_str(&format!("(allow file-write* (subpath {}))", quote(path)?));
        }
        if !transfer {
            for path in &self.configuration.denied_roots {
                profile.push_str(&format!(
                    "(deny file-read* file-write* (subpath {}))",
                    quote(path)?
                ));
            }
            profile.push_str(&format!(
                "(deny file-read* file-write* (subpath {}) (subpath {}) (subpath {}))",
                quote(&self.source.root)?,
                quote(&self.source.common_dir)?,
                quote(&self.configuration.lfs_directory)?
            ));
            for path in &self.configuration.object_directories {
                profile.push_str(&format!(
                    "(deny file-read* file-write* (subpath {}))",
                    quote(path)?
                ));
            }
        }
        Ok(profile)
    }

    fn command(
        &self,
        cwd: &Path,
        program: &Path,
        arguments: &[OsString],
        scope: Scope,
        input: Option<&Path>,
    ) -> Result<tokio::process::Command> {
        let mut command = crate::command::gated_command("/usr/bin/sandbox-exec", input);
        command
            .args([OsStr::new("-p"), OsStr::new(&self.profile(&scope)?)])
            .arg(program)
            .args(arguments)
            .current_dir(cwd)
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(self.tool.binary().parent().unwrap().to_path_buf())
                        .chain(std::env::split_paths(&self.path)),
                )?,
            )
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
            None,
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
        let mut command = self.command(
            &self.workdir,
            self.tool.binary(),
            &arguments,
            Scope::Code,
            None,
        )?;
        crate::command::run(&mut command, cancel, Duration::from_secs(60)).await
    }
    pub async fn deploy(&self, tree: ObjectId, cancel: watch::Receiver<bool>) -> Result<()> {
        self.verify_storage()?;
        let root = self.workdir.clone();
        let source = self.source.root.clone();
        let roots = self.attachment_roots.clone();
        let lfs_source = self.configuration.lfs_directory.clone();
        let lfs_target = self.workdir.join(".git/lfs");
        tokio::task::spawn_blocking(move || -> Result<()> {
            let repo = Repository::open(root)?;
            repo.import_tree(&source, tree)?;
            let incoming = repo.tree_files(tree)?;
            intercat_git::AttachmentPaths::from_paths(incoming.keys().map(String::as_str))
                .with_roots(&roots)?;
            crate::lfs_store::transfer(&lfs_source, &lfs_target, &repo.lfs_entries(tree)?)?;
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

    /// Materialize a linked worker using this clone's objects and conversion
    /// settings. Filters have no access to the original Git/LFS stores. Only
    /// the daemon publishes the resulting index into the linked Git directory.
    pub(crate) async fn materialize_into(
        &self,
        destination: &Path,
        cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        self.verify_storage()?;
        let destination = destination.canonicalize()?;
        if destination.starts_with(&self.source.root)
            || destination.starts_with(&self.source.common_dir)
            || destination.starts_with(&self.configuration.lfs_directory)
            || destination == self.workdir
        {
            return Err(
                "Worker files must be separate from the source and its object stores.".into(),
            );
        }
        Repository::verify_prepared_worktree(&destination, &self.source)?;
        let tree = {
            let repo = Repository::open(&self.workdir)?;
            let tree = repo.tree_id(self.source.head)?;
            crate::lfs_store::transfer(
                &self.configuration.lfs_directory,
                &self.workdir.join(".git/lfs"),
                &repo.lfs_entries(tree)?,
            )?;
            tree
        };
        let mut command = self.command(
            &destination,
            self.tool.binary(),
            &[
                "-c".into(),
                "core.fsmonitor=false".into(),
                "-c".into(),
                "core.hooksPath=/dev/null".into(),
                "read-tree".into(),
                "--reset".into(),
                "-u".into(),
                tree.to_string().into(),
            ],
            Scope::Worktree(destination.clone()),
            None,
        )?;
        command
            .env("GIT_DIR", self.workdir.join(".git"))
            .env("GIT_WORK_TREE", &destination)
            .env("GIT_INDEX_FILE", self.workdir.join(".git/index"));
        crate::command::run(&mut command, cancel, Duration::from_secs(60))
            .await?
            .require_success("Prepare worker files")?;
        Repository::verify_prepared_worktree(&destination, &self.source)?;
        Repository::open(&destination)?.install_worktree_index(&self.workdir.join(".git/index"))?;
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
            None,
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

    /// Project hooks run offline with the same file boundary as Check. Input
    /// is held open until the tracked child and its descendants have exited.
    pub(crate) async fn run_pre_push(
        &self,
        hook: &Path,
        remote: &str,
        url: &str,
        input: &[u8],
        cancel: watch::Receiver<bool>,
    ) -> Result<Output> {
        use std::io::Write;
        self.verify_storage()?;
        if !hook.starts_with(self.root.join("control")) {
            return Err("The Push hook must be prepared in its private checkout.".into());
        }
        let mut stdin = tempfile::NamedTempFile::new_in(&self.temporary)?;
        stdin.write_all(input)?;
        let mut command = self.command(
            &self.workdir,
            hook,
            &[remote.into(), url.into()],
            Scope::Code,
            Some(stdin.path()),
        )?;
        command.env("GIT_LFS_SKIP_PUSH", "1");
        crate::command::run(&mut command, cancel, Duration::from_secs(300)).await
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
        repository.sanitize_snapshot_index(original_base, &index, &self.attachment_roots, &[])?;
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
        crate::lfs_store::transfer(
            &self.workdir.join(".git/lfs"),
            &self.configuration.lfs_directory,
            &repository.lfs_entries(details.tree)?,
        )?;
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

pub(crate) fn failure(error: &(dyn std::error::Error + Send + Sync + 'static)) -> String {
    match error.downcast_ref::<crate::lfs_store::Problem>() {
        Some(crate::lfs_store::Problem::Missing { path }) => {
            format!("LFS objects missing locally · fetch them in your checkout · {path}")
        }
        _ => error.to_string(),
    }
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
                lfs_directory: probe.path("source/.git/lfs"),
                object_directories: vec![probe.path("source/.git/objects")],
                denied_roots: vec![],
            },
            tool: GitTool::new(probe.program.clone()).await?,
            path: "/usr/bin:/bin".into(),
            toolchain: vec![],
        };
        for (name, scope) in [
            ("transfer", Scope::Transfer),
            ("git", Scope::Git),
            ("code", Scope::Code),
            ("worktree", Scope::Worktree(probe.path("worker"))),
        ] {
            probe.check(name, &checkout.profile(&scope)?)?;
        }
        Ok(())
    }
}
