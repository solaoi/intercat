//! Normalize a private snapshot index without changing the checkout's sparse state.
use crate::Result;
use gix::{bstr::ByteSlice, index::entry::Flags};

#[derive(Debug, Clone)]
pub struct NestedRepository {
    pub path: String,
}
impl std::fmt::Display for NestedRepository {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is a separate repository · its files can’t be saved",
            self.path
        )
    }
}
impl std::error::Error for NestedRepository {}

pub fn validate_exclusion(path: &str) -> Result<()> {
    use std::path::{Component, Path};
    if path.is_empty()
        || path.contains(['\\', '\0'])
        || !Path::new(path).components().all(
            |part| matches!(part, Component::Normal(name) if !name.eq_ignore_ascii_case(".git")),
        )
    {
        return Err("Invalid checkpoint exclusion path.".into());
    }
    Ok(())
}

pub(crate) fn excluded(path: &str, roots: &[String]) -> bool {
    roots.iter().any(|root| {
        path == root
            || path
                .strip_prefix(root)
                .is_some_and(|tail| tail.starts_with('/'))
    })
}

pub(crate) fn check_nested(
    repo: &gix::Repository,
    index: &gix::index::File,
    exclusions: &[String],
    attachments: &[String],
) -> Result<()> {
    let mut found = Vec::new();
    let mut entries = gix::dir::walk::delegate::Collect::default();
    repo.dirwalk(
        index,
        std::iter::empty::<gix::bstr::BString>(),
        &std::sync::atomic::AtomicBool::new(false),
        repo.dirwalk_options()?.emit_tracked(true),
        &mut entries,
    )?;
    for (entry, _) in entries.into_entries_by_path() {
        if entry.disk_kind == Some(gix::dir::entry::Kind::Repository) {
            found.push(
                std::str::from_utf8(&entry.rela_path)?
                    .trim_end_matches('/')
                    .to_owned(),
            );
        }
    }
    found.extend(
        index
            .entries()
            .iter()
            .filter(|entry| entry.mode == gix::index::entry::Mode::COMMIT)
            .map(|entry| std::str::from_utf8(entry.path(index)).map(str::to_owned))
            .collect::<std::result::Result<Vec<_>, _>>()?,
    );
    found.sort();
    if let Some(path) = found
        .into_iter()
        .find(|path| !excluded(path, exclusions) && !excluded(path, attachments))
    {
        return Err(NestedRepository { path }.into());
    }
    Ok(())
}

pub(crate) fn expand(
    repo: &gix::Repository,
    mut index: gix::index::File,
) -> Result<gix::index::File> {
    if index.is_sparse() {
        let mut full = gix::index::State::new(repo.object_hash());
        for entry in index.entries() {
            let path = entry.path(&index);
            if entry.mode == gix::index::entry::Mode::DIR {
                let prefix = std::str::from_utf8(path)?.trim_end_matches('/');
                let children = repo.index_from_tree(&entry.id)?;
                for child in children.entries() {
                    let name = format!("{prefix}/{}", std::str::from_utf8(child.path(&children))?);
                    full.dangerously_push_entry(
                        Default::default(),
                        child.id,
                        child.flags | Flags::SKIP_WORKTREE | Flags::EXTENDED,
                        child.mode,
                        name.as_bytes().as_bstr(),
                    );
                }
            } else {
                full.dangerously_push_entry(entry.stat, entry.id, entry.flags, entry.mode, path);
            }
        }
        full.sort_entries();
        full.verify_entries()?;
        index = gix::index::File::from_state(full, index.path());
    }
    Ok(index)
}

pub(crate) fn prepare(repo: &gix::Repository, index: gix::index::File) -> Result<gix::index::File> {
    let mut index = expand(repo, index)?;
    for entry in index.entries_mut() {
        entry
            .flags
            .remove(Flags::ASSUME_VALID | Flags::SKIP_WORKTREE | Flags::FSMONITOR_VALID);
        entry.stat = Default::default();
    }
    let config = repo.config_snapshot();
    if config.boolean("core.sparseCheckout").unwrap_or(false) {
        let patterns = gix::worktree::sparse::read_patterns(repo.git_dir())?
            .ok_or("Sparse checkout is enabled without its patterns.")?;
        let case = if config.boolean("core.ignoreCase").unwrap_or(false) {
            gix::glob::pattern::Case::Fold
        } else {
            gix::glob::pattern::Case::Sensitive
        };
        gix::worktree::sparse::apply_patterns(
            &mut index,
            &patterns,
            &repo.git_dir().join("info/sparse-checkout"),
            config.boolean("core.sparseCheckoutCone").unwrap_or(false),
            case,
        )?;
    }
    Ok(index)
}
