//! Git object and ref operations. Durability is a separate daemon responsibility.

mod attachments;
pub mod lfs;
mod lfs_config;
pub mod merge;
mod project_location;
mod push_objects;
mod remote_address;
mod snapshot_index;
mod worker_base;
pub use attachments::AttachmentPaths;
pub use project_location::ProjectLocation;
pub use push_objects::PushHistory;
pub use remote_address::{RemoteAddress, Transport, validate_branch_reference};
pub use snapshot_index::{NestedRepository, validate_exclusion};

use std::{collections::HashSet, fs::File, os::fd::AsRawFd, path::Path};

pub use gix::ObjectId;
use gix::bstr::BString;
use gix::prelude::Write;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub struct Repository(gix::Repository);

pub fn validate_reference(reference: &str) -> Result<()> {
    if reference.len() > 1024 {
        return Err("Git reference is too long.".into());
    }
    let _: gix::refs::FullName = reference.try_into().map_err(|_| "Invalid Git reference.")?;
    Ok(())
}

#[derive(Debug, Clone)]
pub struct WorktreeSource {
    pub root: std::path::PathBuf,
    pub common_dir: std::path::PathBuf,
    pub head: ObjectId,
}

pub struct SnapshotIndex {
    pub git_dir: std::path::PathBuf,
    pub common_dir: std::path::PathBuf,
    pub object_dirs: Vec<std::path::PathBuf>,
    pub unmerged_paths: Vec<String>,
    pub add_exclusions: Vec<String>,
}
/// Only settings needed to reproduce checkout conversion. Executable values
/// are data here; the daemon runs them in its isolated checkout sandbox.
pub struct CheckoutConfiguration {
    pub values: std::collections::BTreeMap<String, String>,
    pub attributes_file: Option<std::path::PathBuf>,
    pub hooks_directory: std::path::PathBuf,
    pub lfs_directory: std::path::PathBuf,
}
pub struct CommitDetails {
    pub tree: ObjectId,
    pub parents: Vec<ObjectId>,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SavedFile {
    pub object: ObjectId,
    pub mode: u16,
}
enum TreeScope {
    Workspace,
    Push,
}

/// Added/replaced line ranges in the new bytes, zero based and end exclusive.
/// Object bytes are compared directly: no attributes, filters, or external diff.
pub fn added_lines(before: &[u8], after: &[u8]) -> Vec<std::ops::Range<u32>> {
    use gix::diff::blob::{Algorithm, Diff, InternedInput};
    let input = InternedInput::new(before, after);
    Diff::compute(Algorithm::Histogram, &input)
        .hunks()
        .filter(|hunk| !hunk.after.is_empty())
        .map(|hunk| hunk.after)
        .collect()
}

/// Whether a diff changes an existing, one-based inclusive line range.
/// Insertions before or after the range only move it; insertions inside touch it.
pub fn touches_lines(before: &[u8], after: &[u8], start: u32, end: u32) -> bool {
    if start == 0 || end < start {
        return true;
    }
    use gix::diff::blob::{Algorithm, Diff, InternedInput};
    let input = InternedInput::new(before, after);
    Diff::compute(Algorithm::Histogram, &input)
        .hunks()
        .any(|hunk| {
            if hunk.before.is_empty() {
                hunk.before.start > start - 1 && hunk.before.start < end
            } else {
                hunk.before.start < end && hunk.before.end > start - 1
            }
        })
}

pub const ENV_EXAMPLES: [&str; 4] = [".env.example", ".env.sample", ".env.template", ".env.dist"];

pub fn secret_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    !ENV_EXAMPLES.contains(&name)
        && (name == ".env"
            || name.starts_with(".env.")
            || name == ".envrc"
            || name == ".netrc"
            || name.ends_with(".pem")
            || name.ends_with(".key"))
}

#[derive(Debug)]
pub enum Merge {
    Clean(ObjectId),
    Conflicts {
        base: ObjectId,
        target: ObjectId,
        incoming: ObjectId,
        paths: Vec<BString>,
    },
}

#[cfg(not(target_os = "macos"))]
compile_error!("Intercat currently requires macOS and F_FULLFSYNC");

fn full_sync(path: &Path) -> std::io::Result<()> {
    let file = File::open(path)?;
    // SAFETY: file owns a live descriptor for the whole call. F_FULLFSYNC has no
    // pointer argument. An unsupported filesystem is an error, never a weaker sync.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

impl Repository {
    pub fn worktree_root(&self) -> Result<std::path::PathBuf> {
        Ok(self
            .0
            .workdir()
            .ok_or("A working repository is required.")?
            .canonicalize()?)
    }
    pub fn common_directory(&self) -> Result<std::path::PathBuf> {
        Ok(self.0.common_dir().canonicalize()?)
    }
    pub fn working_directory(&self) -> Result<std::path::PathBuf> {
        Ok(self
            .0
            .workdir()
            .ok_or("A worker needs a working repository.")?
            .canonicalize()?)
    }
    pub fn commit_details(&self, id: ObjectId) -> Result<CommitDetails> {
        let commit = self.0.find_commit(id)?;
        let decoded = commit.decode()?;
        Ok(CommitDetails {
            tree: decoded.tree(),
            parents: decoded.parents().collect(),
            message: std::str::from_utf8(decoded.message)?.to_string(),
            author_name: std::str::from_utf8(decoded.author()?.name)?.to_string(),
            author_email: std::str::from_utf8(decoded.author()?.email)?.to_string(),
        })
    }
    pub fn checkout_configuration(&self) -> Result<CheckoutConfiguration> {
        let mut permissions = gix::open::Permissions::isolated();
        permissions.config = gix::open::permissions::Config {
            git_binary: false,
            system: true,
            git: true,
            user: true,
            env: false,
            includes: true,
        };
        permissions.env.home = gix::sec::Permission::Allow;
        permissions.env.xdg_config_home = gix::sec::Permission::Allow;
        let root = self.0.workdir().ok_or("A checkout is required.")?;
        let repo = gix::open_opts(
            root,
            gix::open::Options::isolated().permissions(permissions),
        )?;
        let config = repo.config_snapshot();
        let mut values = std::collections::BTreeMap::new();
        for key in [
            "core.autocrlf",
            "core.eol",
            "core.symlinks",
            "core.filemode",
        ] {
            if let Some(value) = config.string(key) {
                values.insert(key.to_string(), std::str::from_utf8(&value)?.to_string());
            }
        }
        for section in config.sections_by_name("filter").into_iter().flatten() {
            let Some(name) = section.header().subsection_name() else {
                continue;
            };
            let name = std::str::from_utf8(name)?;
            for field in ["process", "clean", "smudge", "required"] {
                let key = format!("filter.{name}.{field}");
                if let Some(value) = config.string(key.as_str()) {
                    values.insert(key, std::str::from_utf8(&value)?.to_string());
                }
            }
        }
        let path = |path: std::path::PathBuf| {
            if path.is_absolute() {
                path
            } else {
                root.join(path)
            }
        };
        let attributes_file = if config.string("core.attributesFile").is_some() {
            config.trusted_path("core.attributesFile")?.map(path)
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .filter(|value| !value.is_empty())
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|home| std::path::PathBuf::from(home).join(".config"))
                })
                .map(|root| root.join("git/attributes"))
        };
        let hooks_directory = config
            .trusted_path("core.hooksPath")?
            .map(path)
            .unwrap_or_else(|| repo.common_dir().join("hooks"));
        // Git LFS resolves relative storage against the shared Git directory,
        // including when this is a linked worktree. This is metadata only.
        let lfs_directory = match config.string("lfs.storage") {
            Some(value) if !value.is_empty() => {
                let value = std::path::PathBuf::from(std::str::from_utf8(&value)?);
                if value.is_absolute() {
                    value
                } else {
                    repo.common_dir().join(value)
                }
            }
            _ => repo.common_dir().join("lfs"),
        };
        Ok(CheckoutConfiguration {
            values,
            attributes_file,
            hooks_directory,
            lfs_directory,
        })
    }
    pub fn tree_id(&self, commit: ObjectId) -> Result<ObjectId> {
        Ok(self.0.find_commit(commit)?.tree_id()?.detach())
    }
    pub fn saved_files(
        &self,
        commit: ObjectId,
    ) -> Result<std::collections::BTreeMap<String, SavedFile>> {
        self.tree_files(self.tree_id(commit)?)
    }
    pub fn tree_files(
        &self,
        tree: ObjectId,
    ) -> Result<std::collections::BTreeMap<String, SavedFile>> {
        self.files_in_tree(tree, TreeScope::Workspace)
    }
    /// Gitlink IDs are metadata of another repository, not uploaded blobs.
    /// Workspace operations continue to reject them through `tree_files`.
    pub fn push_tree_files(
        &self,
        tree: ObjectId,
    ) -> Result<std::collections::BTreeMap<String, SavedFile>> {
        self.files_in_tree(tree, TreeScope::Push)
    }
    fn files_in_tree(
        &self,
        tree: ObjectId,
        scope: TreeScope,
    ) -> Result<std::collections::BTreeMap<String, SavedFile>> {
        let mut files = std::collections::BTreeMap::new();
        let mut pending = vec![(String::new(), tree)];
        while let Some((prefix, tree)) = pending.pop() {
            let object = self.0.find_object(tree)?;
            for entry in gix::objs::TreeRefIter::from_bytes(&object.data, self.0.object_hash()) {
                let entry = entry?;
                let path = format!("{prefix}{}", std::str::from_utf8(entry.filename)?);
                if entry.mode.is_tree() {
                    pending.push((format!("{path}/"), entry.oid.to_owned()));
                } else if entry.mode.is_commit() {
                    if matches!(scope, TreeScope::Workspace) {
                        return Err("Nested repository in saved tree.".into());
                    }
                } else {
                    files.insert(
                        path,
                        SavedFile {
                            object: entry.oid.to_owned(),
                            mode: entry.mode.value(),
                        },
                    );
                }
            }
        }
        Ok(files)
    }
    pub fn saved_bytes(&self, file: &SavedFile) -> Result<Vec<u8>> {
        let object = self.0.find_object(file.object)?;
        if object.kind != gix::objs::Kind::Blob {
            return Err("Saved file is not a blob.".into());
        }
        Ok(object.data.to_vec())
    }
    pub fn saved_size(&self, file: &SavedFile) -> Result<u64> {
        let header = self.0.find_header(file.object)?;
        if header.kind() != gix::objs::Kind::Blob {
            return Err("Saved file is not a blob.".into());
        }
        Ok(header.size())
    }
    /// Apply only Git's built-in clean conversions, using the fixed base's
    /// attributes. An external filter is not a safe source of comparison bytes.
    pub fn normalized_file(&self, base: ObjectId, path: &str, bytes: &[u8]) -> Result<Vec<u8>> {
        use std::io::Read;
        let root = self.0.workdir().ok_or("A worktree is required.")?;
        let mut repo = Self::open(root)?.0;
        let mut config = repo.config_snapshot_mut();
        config.append_config(["core.attributesFile=/dev/null"], gix::config::Source::Api)?;
        config.commit()?;
        let index = repo.index_from_tree(&repo.find_commit(base)?.tree_id()?.detach())?;
        let cache = repo.attributes_only(
            &index,
            gix::worktree::stack::state::attributes::Source::IdMapping,
        )?;
        let (mut pipeline, mut cache) =
            gix::filter::Pipeline::new(&repo, cache.detach())?.into_parts();
        let entry = cache.at_entry(gix::bstr::BStr::new(path.as_bytes()), None, &repo.objects)?;
        let mut external = false;
        let mut converted = pipeline.convert_to_git(
            std::io::Cursor::new(bytes),
            Path::new(path),
            &mut |_, attributes| {
                entry.matching_attributes(attributes);
                external = attributes.iter_selected().any(|value| {
                    value.assignment.name.as_str() == "filter"
                        && matches!(
                            value.assignment.state,
                            gix::attrs::StateRef::Value(_) | gix::attrs::StateRef::Set
                        )
                });
            },
            &mut |buffer| {
                let Some(entry) = index.entry_by_path(gix::bstr::BStr::new(path.as_bytes())) else {
                    return Ok(None);
                };
                let blob = repo.find_blob(entry.id)?;
                buffer.extend_from_slice(&blob.data);
                Ok(Some(()))
            },
        )?;
        if external {
            return Err("External clean filter is not supported in file comparison.".into());
        }
        let mut result = Vec::new();
        converted
            .by_ref()
            .take(64 * 1024 * 1024 + 1)
            .read_to_end(&mut result)?;
        if result.len() > 64 * 1024 * 1024 {
            return Err("Normalized file exceeds the comparison limit.".into());
        }
        Ok(result)
    }
    pub fn tracked_paths(&self) -> Result<Vec<String>> {
        let index = self.0.index_or_empty()?;
        let mut paths = Vec::new();
        for entry in index.entries() {
            paths.push(std::str::from_utf8(entry.path(&index))?.to_owned());
        }
        if !self.0.head()?.is_unborn() {
            paths.extend(self.saved_files(self.0.head_id()?.detach())?.into_keys());
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
    /// Read effective fetch and push destinations, including Git URL rewrites.
    /// Local remotes have no host. This never connects to a remote.
    pub fn remote_hosts(&self) -> Result<Vec<String>> {
        let mut hosts = std::collections::BTreeSet::new();
        for name in self.0.remote_names() {
            let remote = self.0.find_remote(name)?;
            for direction in [gix::remote::Direction::Fetch, gix::remote::Direction::Push] {
                for url in remote.urls(direction) {
                    if let Some(host) = &url.host {
                        hosts.insert(host.trim_end_matches('.').to_ascii_lowercase());
                    }
                }
            }
        }
        Ok(hosts.into_iter().collect())
    }
    /// Decode the actual linked-worktree index (including a split index), then
    /// serialize a standalone copy. Never write the user's index or its cache.
    pub fn snapshot_index(
        &self,
        base: ObjectId,
        destination: &Path,
        attachments: &[String],
        exclusions: &[String],
    ) -> Result<SnapshotIndex> {
        let index = snapshot_index::expand(&self.0, self.0.open_index()?)?;
        snapshot_index::check_nested(&self.0, &index, exclusions, attachments)?;
        self.write_snapshot_index(index, base, destination, attachments, exclusions)
    }

    pub fn sanitize_snapshot_index(
        &self,
        base: ObjectId,
        path: &Path,
        attachments: &[String],
        exclusions: &[String],
    ) -> Result<()> {
        let index = gix::index::File::at(path, self.0.object_hash(), false, Default::default())?;
        self.write_snapshot_index(index, base, path, attachments, exclusions)?;
        Ok(())
    }

    /// Prepare a source-inspection index. Starting points respect manual
    /// assume-unchanged/skip-worktree flags; normal entries are rehashed. Never
    /// hide staged changes, secret paths or untracked files during inspection.
    pub fn inspection_index(&self, base: ObjectId, destination: &Path) -> Result<SnapshotIndex> {
        let mut index = snapshot_index::expand(
            &self.0,
            self.0.index_or_load_from_head_or_empty()?.into_owned(),
        )?;
        let expected = self.tree_files(self.tree_id(base)?)?;
        let actual = self.files_in_index(&index)?;
        if actual != expected {
            return Err(
                "Your checkout has uncommitted changes. Commit or stash them before starting."
                    .into(),
            );
        }
        snapshot_index::check_nested(&self.0, &index, &[], &[])?;
        for entry in index.entries_mut() {
            entry
                .flags
                .remove(gix::index::entry::Flags::FSMONITOR_VALID);
            entry.stat = Default::default();
        }
        self.store_snapshot_index(index, destination, Vec::new(), Vec::new())
    }

    fn files_in_index(
        &self,
        index: &gix::index::File,
    ) -> Result<std::collections::BTreeMap<String, SavedFile>> {
        index
            .entries()
            .iter()
            .map(|entry| {
                if entry.stage_raw() != 0 || entry.mode == gix::index::entry::Mode::COMMIT {
                    return Err(
                        "The source index contains conflicts or a nested repository.".into(),
                    );
                }
                Ok((
                    std::str::from_utf8(entry.path(index))?.into(),
                    SavedFile {
                        object: entry.id,
                        mode: entry.mode.bits() as u16,
                    },
                ))
            })
            .collect()
    }

    pub fn changed_index_paths(
        &self,
        index: &Path,
        base: ObjectId,
    ) -> Result<std::collections::BTreeSet<String>> {
        let index = gix::index::File::at(index, self.0.object_hash(), false, Default::default())?;
        let current = self.files_in_index(&index)?;
        let expected = self.tree_files(self.tree_id(base)?)?;
        Ok(current
            .keys()
            .chain(expected.keys())
            .filter(|path| current.get(*path) != expected.get(*path))
            .cloned()
            .collect())
    }

    fn write_snapshot_index(
        &self,
        index: gix::index::File,
        base: ObjectId,
        destination: &Path,
        owned_attachments: &[String],
        exclusions: &[String],
    ) -> Result<SnapshotIndex> {
        for path in exclusions {
            validate_exclusion(path)?;
        }
        let mut index = snapshot_index::prepare(&self.0, index)?;
        let mut baseline = HashSet::new();
        let mut pending = vec![(String::new(), self.0.find_commit(base)?.tree_id()?.detach())];
        while let Some((prefix, tree)) = pending.pop() {
            let object = self.0.find_object(tree)?;
            for entry in gix::objs::TreeRefIter::from_bytes(&object.data, self.0.object_hash()) {
                let entry = entry?;
                let name = std::str::from_utf8(entry.filename)?;
                let path = format!("{prefix}{name}");
                if entry.mode.is_tree() {
                    pending.push((format!("{path}/"), entry.oid.to_owned()));
                } else {
                    baseline.insert(path);
                }
            }
        }
        let mut unmerged_paths = Vec::new();
        let attachments = AttachmentPaths::from_paths(baseline.iter().map(String::as_str))
            .with_roots(owned_attachments)?;
        for (entry, path) in index.entries_mut_with_paths() {
            let path = std::str::from_utf8(path)?;
            if entry.stage_raw() != 0 && !attachments.excludes(path) {
                unmerged_paths.push(path.to_owned());
            }
        }
        index.remove_entries(|_, path, _| {
            std::str::from_utf8(path).is_ok_and(|path| {
                attachments.excludes(path)
                    || snapshot_index::excluded(path, exclusions)
                    || (secret_path(path) && !baseline.contains(path))
            })
        });
        if let Some(entry) = index
            .entries()
            .iter()
            .find(|entry| entry.mode == gix::index::entry::Mode::COMMIT)
        {
            return Err(NestedRepository {
                path: std::str::from_utf8(entry.path(&index))?.to_owned(),
            }
            .into());
        }
        let mut add_exclusions = Vec::new();
        let mut ignores = self.0.excludes(
            &index,
            None,
            gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
        )?;
        for path in exclusions.iter().chain(owned_attachments) {
            if !ignores
                .at_path(Path::new(path), Some(gix::index::entry::Mode::DIR))?
                .is_excluded()
            {
                add_exclusions.push(path.clone());
            }
        }
        add_exclusions.sort();
        add_exclusions.dedup();
        self.store_snapshot_index(index, destination, unmerged_paths, add_exclusions)
    }

    fn store_snapshot_index(
        &self,
        mut index: gix::index::File,
        destination: &Path,
        mut unmerged_paths: Vec<String>,
        add_exclusions: Vec<String>,
    ) -> Result<SnapshotIndex> {
        index.remove_tree();
        index.write_to(
            File::create(destination)?,
            gix::index::write::Options {
                extensions: gix::index::write::Extensions::None,
                skip_hash: false,
            },
        )?;
        unmerged_paths.sort();
        unmerged_paths.dedup();
        let object_dirs = self.object_directories()?;
        Ok(SnapshotIndex {
            git_dir: self.0.git_dir().canonicalize()?,
            common_dir: self.0.common_dir().canonicalize()?,
            object_dirs,
            unmerged_paths,
            add_exclusions,
        })
    }

    /// Copy only a reachable tree. Objects already present here may fill gaps
    /// left by an isolated checkpoint writer; no commit history is imported.
    pub fn import_tree(&self, isolated: &Path, tree: ObjectId) -> Result<()> {
        let source = gix::open_opts(isolated, gix::open::Options::isolated())?;
        let mut pending = vec![tree];
        let mut seen = HashSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            let object = source.find_object(id).or_else(|_| self.0.find_object(id))?;
            if object.kind == gix::objs::Kind::Tree {
                for entry in gix::objs::TreeRefIter::from_bytes(&object.data, self.0.object_hash())
                {
                    let entry = entry?;
                    if entry.mode.is_commit() {
                        return Err(std::io::Error::other(
                            "A nested repository cannot be saved in a checkpoint.",
                        )
                        .into());
                    }
                    pending.push(entry.oid.to_owned());
                }
            }
            if self.0.write_buf(object.kind, &object.data)? != id {
                return Err(std::io::Error::other("Object hash mismatch.").into());
            }
        }
        Ok(())
    }

    pub fn init_isolated(path: &Path) -> Result<()> {
        gix::init_bare(path)?;
        Ok(())
    }

    pub fn checkpoint_commit(
        &self,
        tree: ObjectId,
        parent: ObjectId,
        message: &str,
    ) -> Result<ObjectId> {
        let signature = gix::actor::Signature {
            name: "Intercat".into(),
            email: "intercat@localhost".into(),
            time: gix::date::Time::now_local_or_utc(),
        };
        self.write_commit(tree, &[parent], message, signature)
    }
    /// Immutable derived context, reproducible across independent consumers.
    /// It does not change a worker checkpoint or any reference.
    pub fn context_commit(
        &self,
        tree: ObjectId,
        parent: ObjectId,
        message: &str,
    ) -> Result<ObjectId> {
        self.write_commit(
            tree,
            &[parent],
            message,
            gix::actor::Signature {
                name: "Intercat".into(),
                email: "intercat@localhost".into(),
                time: gix::date::Time {
                    seconds: 0,
                    offset: 0,
                },
            },
        )
    }
    /// Discover a clean repository without changing its HEAD or index. An
    /// unborn repository receives an empty, parentless starting-point object.
    pub fn project_source(path: &Path) -> Result<WorktreeSource> {
        let selected = path.canonicalize()?;
        let discovered = gix::discover_opts(
            &selected,
            Default::default(),
            gix::open::Options::isolated(),
        )?;
        let root = discovered
            .workdir()
            .ok_or_else(|| {
                std::io::Error::other("Choose a working folder, not a bare repository.")
            })?
            .canonicalize()?;
        if selected != root {
            return Err(std::io::Error::other(
                "Choose the repository root folder for this session.",
            )
            .into());
        }
        let repo = Self::open(&root)?;
        let head = if repo.0.head()?.is_unborn() {
            repo.empty_starting_point()?
        } else {
            repo.head_commit()?
        };
        let source = Self::worktree_source(&root, Some(head))?;
        Ok(source)
    }

    /// A reproducible root object, also recoverable if Git prunes it before the
    /// project-selection intent has published its protecting reference.
    pub fn empty_starting_point(&self) -> Result<ObjectId> {
        let tree = self.write_tree(&[])?;
        self.write_commit(
            tree,
            &[],
            "Intercat starting point\n",
            gix::actor::Signature {
                name: "Intercat".into(),
                email: "intercat@localhost".into(),
                time: gix::date::Time {
                    seconds: 0,
                    offset: 0,
                },
            },
        )
    }

    /// Validate the repository before a checkout can execute any configured filter.
    pub fn worktree_source(
        path: &Path,
        starting_commit: Option<ObjectId>,
    ) -> Result<WorktreeSource> {
        let repo = gix::open_opts(path, gix::open::Options::isolated())?;
        let root = repo
            .workdir()
            .ok_or_else(|| std::io::Error::other("a working repository is required"))?
            .canonicalize()?;
        if repo
            .index_or_empty()?
            .entries()
            .iter()
            .any(|entry| entry.stage_raw() != 0)
        {
            return Err(std::io::Error::other("source checkout has unresolved conflicts").into());
        }
        let head = match starting_commit {
            Some(id) => id,
            None => repo.head_id()?.detach(),
        };
        if project_location::has_submodules(&repo, Some(head))? {
            return Err(std::io::Error::other("submodules are not supported yet").into());
        }
        Ok(WorktreeSource {
            root,
            common_dir: repo.common_dir().canonicalize()?,
            head,
        })
    }
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        // No global configuration, environment redirection, or git executable lookup.
        let mut repo = gix::open_opts(path.as_ref(), gix::open::Options::isolated())?;
        let mut config = repo.config_snapshot_mut();
        // gix's high-level merge APIs DO read custom merge/filter drivers. Remove
        // executable configuration in memory before constructing their pipelines.
        // The repository's on-disk configuration is never changed.
        for section in ["filter", "merge", "diff"] {
            let ids: Vec<_> = config
                .sections_and_ids_by_name(section)
                .into_iter()
                .flatten()
                .map(|(_, id)| id)
                .collect();
            for id in ids {
                config.remove_section_by_id(id);
            }
        }
        config.append_config(
            [
                "core.fsmonitor=false",
                "commit.gpgsign=false",
                "status.showUntrackedFiles=all",
                // Ref-log entries describe daemon operations. Do not depend
                // on a user's global identity (isolated opens omit it).
                "committer.name=Intercat",
                "committer.email=intercat@localhost",
            ],
            gix::config::Source::Api,
        )?;
        config.commit()?;
        Ok(Self(repo))
    }

    pub fn write_blob(&self, bytes: &[u8]) -> Result<ObjectId> {
        Ok(self.0.write_blob(bytes)?.detach())
    }

    pub fn reference_target(&self, name: &str) -> Result<Option<ObjectId>> {
        Ok(self
            .0
            .try_find_reference(name)?
            .and_then(|r| r.target().try_id().map(ToOwned::to_owned)))
    }

    /// Name a fixed object through a currently protecting ref when one exists.
    /// Git clone needs that name when the source HEAD is unborn. The receiver
    /// still verifies the resulting object against the frozen ID.
    pub fn reference_to(&self, id: ObjectId) -> Result<Option<String>> {
        let references = self.0.references()?;
        for reference in references.all()? {
            let reference = reference?;
            if reference.target().try_id() == Some(&id) {
                return Ok(Some(
                    std::str::from_utf8(reference.name().as_bstr())?.into(),
                ));
            }
        }
        Ok(None)
    }

    pub fn object_directories(&self) -> Result<Vec<std::path::PathBuf>> {
        let mut paths = vec![self.0.objects.store_ref().path().canonicalize()?];
        paths.extend(
            self.0
                .objects
                .store_ref()
                .alternate_db_paths()?
                .into_iter()
                .map(|path| path.canonicalize())
                .collect::<std::io::Result<Vec<_>>>()?,
        );
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
    pub fn head_commit(&self) -> Result<ObjectId> {
        Ok(self.0.head_id()?.detach())
    }
    pub fn base_branch(&self) -> Result<Option<String>> {
        let head = self.0.head()?;
        Ok(if head.is_unborn() {
            None
        } else {
            head.referent_name().map(|name| name.as_bstr().to_string())
        })
    }
    pub fn branch_is_checked_out(&self, name: &str) -> Result<bool> {
        for worktree in self.0.worktrees_including_main()? {
            let worktree = worktree?;
            if worktree
                .head()?
                .referent_name()
                .is_some_and(|referent| referent.as_bstr() == name)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn session_slug_occupied(&self, slug: &str) -> Result<bool> {
        if !slug.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(std::io::Error::other("invalid session slug").into());
        }
        if self
            .0
            .try_find_reference(&format!("refs/heads/intercat/{slug}"))?
            .is_some()
        {
            return Ok(true);
        }
        let prefix = format!("refs/intercat/{slug}/");
        let refs = self.0.references()?;
        Ok(refs
            .prefixed(prefix.as_str())?
            .next()
            .transpose()?
            .is_some())
    }

    pub fn owns_session(&self, slug: &str, session_id: &str) -> Result<bool> {
        let Some(marker) = self.reference_target(&format!("refs/intercat/{slug}/session"))? else {
            return Ok(false);
        };
        let blob = self.0.find_blob(marker)?;
        Ok(blob.data == format!("Intercat session {session_id}\n").as_bytes())
    }

    pub fn verify_prepared_worktree(path: &Path, source: &WorktreeSource) -> Result<()> {
        let repo = gix::open_opts(path, gix::open::Options::isolated())?;
        if std::fs::symlink_metadata(path)?.file_type().is_symlink()
            || repo
                .workdir()
                .map(Path::canonicalize)
                .transpose()?
                .as_deref()
                != Some(path)
            || repo.common_dir().canonicalize()? != source.common_dir
            || !repo.head()?.is_detached()
            || repo.head_id()?.detach() != source.head
        {
            return Err(std::io::Error::other(
                "Prepared worktree does not match its recorded source.",
            )
            .into());
        }
        Ok(())
    }

    pub fn write_tree(&self, entries: &[(&str, ObjectId)]) -> Result<ObjectId> {
        let mut tree = self
            .0
            .edit_tree(ObjectId::empty_tree(self.0.object_hash()))?;
        for (path, id) in entries {
            tree.upsert(*path, gix::objs::tree::EntryKind::Blob, *id)?;
        }
        Ok(tree.write()?.detach())
    }

    /// Read the supplied index, not the original checkout's index. No file filters run.
    pub fn tree_from_index(&self, index: &Path) -> Result<ObjectId> {
        let index = gix::index::File::at(index, self.0.object_hash(), false, Default::default())?;
        self.index_tree(&index)
    }

    pub fn install_worktree_index(&self, captured: &Path) -> Result<()> {
        let mut index =
            gix::index::File::at(captured, self.0.object_hash(), false, Default::default())?;
        if self.files_in_index(&index)? != self.saved_files(self.head_commit()?)? {
            return Err(
                "The materialized worker index does not match its frozen starting point.".into(),
            );
        }
        let destination = self.0.index_path();
        index.set_path(&destination);
        index.remove_tree();
        index.write(gix::index::write::Options {
            extensions: gix::index::write::Extensions::None,
            skip_hash: false,
        })?;
        full_sync(&destination)?;
        full_sync(self.0.git_dir())?;
        Ok(())
    }

    pub fn tree_from_index_without_attachments(
        &self,
        index: &Path,
        baseline: ObjectId,
        owned_attachments: &[String],
    ) -> Result<ObjectId> {
        let files = self.tree_files(baseline)?;
        let attachments = AttachmentPaths::from_paths(files.keys().map(String::as_str))
            .with_roots(owned_attachments)?;
        let mut index =
            gix::index::File::at(index, self.0.object_hash(), false, Default::default())?;
        index.remove_entries(|_, path, _| {
            std::str::from_utf8(path).is_ok_and(|path| attachments.excludes(path))
        });
        self.index_tree(&index)
    }

    /// Remove reserved copy trees from a newly created candidate commit while
    /// preserving its message, authorship, parents and timestamps. No ref moves.
    pub fn commit_without_attachments(
        &self,
        id: ObjectId,
        baseline: ObjectId,
        owned_attachments: &[String],
    ) -> Result<(ObjectId, Vec<String>)> {
        let original = self.0.find_commit(id)?;
        let mut commit = original.decode()?.to_owned()?;
        let base_files = self.saved_files(baseline)?;
        let attachments = AttachmentPaths::from_paths(base_files.keys().map(String::as_str))
            .with_roots(owned_attachments)?;
        let files = self.tree_files(commit.tree)?;
        let removed = files
            .keys()
            .filter(|path| attachments.excludes(path))
            .cloned()
            .collect::<Vec<_>>();
        if removed.is_empty() {
            return Ok((id, removed));
        }
        let roots = removed
            .iter()
            .filter_map(|path| attachments.excluded_root(path))
            .collect::<std::collections::BTreeSet<_>>();
        let mut tree = self.0.edit_tree(commit.tree)?;
        for root in roots {
            tree.remove(root.as_str())?;
        }
        commit.tree = tree.write()?.detach();
        // A signature could not remain valid after changing a tree. Intercat
        // candidate commits are unsigned, including after a hook adds a header.
        commit.extra_headers.retain(|(name, _)| {
            name.as_slice() != b"gpgsig" && name.as_slice() != b"gpgsig-sha256"
        });
        Ok((self.0.write_object(&commit)?.detach(), removed))
    }

    fn index_tree(&self, index: &gix::index::File) -> Result<ObjectId> {
        let mut tree = self
            .0
            .edit_tree(ObjectId::empty_tree(self.0.object_hash()))?;
        for entry in index.entries() {
            if entry.stage_raw() != 0 {
                return Err(std::io::Error::other("index contains unresolved conflicts").into());
            }
            let mode = entry
                .mode
                .to_tree_entry_mode()
                .ok_or_else(|| std::io::Error::other("unsupported index entry mode"))?;
            tree.upsert(BString::from(entry.path(index)), mode.kind(), entry.id)?;
        }
        Ok(tree.write()?.detach())
    }

    /// Create the object without changing a ref; the daemon records intent before CAS.
    pub fn write_commit(
        &self,
        tree: ObjectId,
        parents: &[ObjectId],
        message: &str,
        author: gix::actor::Signature,
    ) -> Result<ObjectId> {
        Ok(self
            .0
            .write_object(&gix::objs::Commit {
                tree,
                parents: parents.iter().copied().collect(),
                author: author.clone(),
                committer: author,
                encoding: None,
                message: message.into(),
                extra_headers: Default::default(),
            })?
            .detach())
    }

    /// Import one commit and its complete tree from the isolated integration clone.
    /// Parents must already exist here; do not import the clone's history.
    pub fn import_commit(&self, source: &Self, commit: ObjectId) -> Result<()> {
        let decoded = source.0.find_commit(commit)?.decode()?.to_owned()?;
        for parent in &decoded.parents {
            self.0.find_commit(*parent)?;
        }
        let mut pending = vec![decoded.tree];
        let mut seen = HashSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            let object = source.0.find_object(id)?;
            if object.kind == gix::objs::Kind::Tree {
                for entry in
                    gix::objs::TreeRefIter::from_bytes(&object.data, source.0.object_hash())
                {
                    let entry = entry?;
                    if entry.mode.is_commit() {
                        return Err(std::io::Error::other("submodules are not supported").into());
                    }
                    pending.push(entry.oid.to_owned());
                }
            }
            let written = self.0.write_buf(object.kind, &object.data)?;
            if written != id {
                return Err(std::io::Error::other("object hash mismatch").into());
            }
        }
        let object = source.0.find_object(commit)?;
        if self.0.write_buf(object.kind, &object.data)? != commit {
            return Err(std::io::Error::other("commit hash mismatch").into());
        }
        Ok(())
    }

    /// Flush an object and its tree before the daemon records an intent referring to it.
    /// Commit parents already belong to the repository and are not traversed.
    pub fn sync_objects(&self, roots: &[ObjectId]) -> Result<()> {
        let object_dir = self.0.objects.store_ref().path();
        let mut object_dirs = vec![object_dir.to_path_buf()];
        object_dirs.extend(self.0.objects.store_ref().alternate_db_paths()?);
        let mut pending = roots.to_vec();
        let mut seen = HashSet::new();
        let mut directories = HashSet::new();
        let mut sync_packs = false;
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            let object = self.0.find_object(id)?;
            match object.kind {
                gix::objs::Kind::Commit => {
                    pending.push(object.try_into_commit()?.tree_id()?.detach())
                }
                gix::objs::Kind::Tree => {
                    for entry in
                        gix::objs::TreeRefIter::from_bytes(&object.data, self.0.object_hash())
                    {
                        let entry = entry?;
                        if !entry.mode.is_commit() {
                            pending.push(entry.oid.to_owned());
                        }
                    }
                }
                _ => {}
            }
            let hex = id.to_string();
            let path = object_dirs
                .iter()
                .map(|dir| dir.join(&hex[..2]).join(&hex[2..]))
                .find(|path| path.is_file());
            if let Some(path) = path {
                full_sync(&path)?;
                directories.insert(path.parent().unwrap().to_path_buf());
            } else {
                sync_packs = true;
            }
        }
        if sync_packs {
            for dir in &object_dirs {
                let pack_dir = dir.join("pack");
                if pack_dir.is_dir() {
                    for entry in std::fs::read_dir(&pack_dir)? {
                        let entry = entry?;
                        if entry.file_type()?.is_file() {
                            full_sync(&entry.path())?;
                        }
                    }
                    directories.insert(pack_dir);
                }
            }
        }
        for directory in directories {
            full_sync(&directory)?;
        }
        for dir in object_dirs {
            full_sync(&dir)?;
        }
        Ok(())
    }

    /// Called after CAS and before marking the operation done in SQLite.
    pub fn sync_ref(&self, name: &str) -> Result<()> {
        if !(name.starts_with("refs/intercat/") || name.starts_with("refs/heads/intercat/")) {
            return Err(std::io::Error::other("ref must belong to Intercat").into());
        }
        // Validate the full name before forming paths, including during recovery.
        let _: gix::refs::FullName = name.try_into()?;
        let root = self.0.common_dir();
        let path = root.join(name);
        let stored_path = if path.is_file() {
            path
        } else {
            self.0.find_reference(name)?;
            root.join("packed-refs")
        };
        full_sync(&stored_path)?;
        let mut directory = stored_path.parent();
        while let Some(parent) = directory {
            full_sync(parent)?;
            if parent == root {
                break;
            }
            directory = parent.parent();
        }
        Ok(())
    }

    /// Atomic expected-value update, including create-only for a missing reference.
    /// A repeated application of an already committed intent is harmless.
    pub fn compare_and_swap(
        &self,
        name: &str,
        expected: Option<ObjectId>,
        next: ObjectId,
    ) -> Result<()> {
        if !(name.starts_with("refs/intercat/") || name.starts_with("refs/heads/intercat/")) {
            return Err(std::io::Error::other("ref must belong to Intercat").into());
        }
        let expected = expected.map_or(gix::refs::transaction::PreviousValue::MustNotExist, |id| {
            gix::refs::transaction::PreviousValue::MustExistAndMatch(id.into())
        });
        match self.0.reference(name, next, expected, "Intercat") {
            Ok(_) => Ok(()),
            Err(error) => {
                // Only accept a failed CAS if the exact desired target is already there.
                // This check follows the locked CAS, not a read-before-write shortcut.
                if self
                    .0
                    .try_find_reference(name)?
                    .and_then(|r| r.target().try_id().map(ToOwned::to_owned))
                    == Some(next)
                {
                    Ok(())
                } else {
                    Err(error.into())
                }
            }
        }
    }

    pub fn delete_ref(&self, name: &str, expected: ObjectId) -> Result<()> {
        if !name.starts_with("refs/intercat/") {
            return Err("Only an Intercat hidden ref can be deleted.".into());
        }
        let name: gix::refs::FullName = name.try_into()?;
        if self
            .0
            .try_find_reference(std::str::from_utf8(name.as_bstr())?)?
            .is_some()
        {
            self.0
                .edit_reference(gix::refs::transaction::RefEdit::delete(
                    name.clone(),
                    gix::refs::transaction::PreviousValue::MustExistAndMatch(expected.into()),
                ))?;
        }
        let root = self.0.common_dir();
        let packed = root.join("packed-refs");
        if packed.is_file() {
            full_sync(&packed)?;
        }
        let path = root.join(std::str::from_utf8(name.as_bstr())?);
        for parent in path.ancestors().skip(1) {
            if parent.is_dir() {
                full_sync(parent)?;
            }
            if parent == root {
                break;
            }
        }
        Ok(())
    }

    pub fn merge_base(&self, ours: ObjectId, theirs: ObjectId) -> Result<Option<ObjectId>> {
        Ok(self.0.merge_base(ours, theirs)?.map(|id| id.detach()))
    }
}
