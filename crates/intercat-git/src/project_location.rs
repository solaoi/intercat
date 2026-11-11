//! Read-only project selection. Resolve repository identity before any filter,
//! starting-point object, or application record can be written.
use crate::{Repository, Result};
use std::path::{Path, PathBuf};

pub struct ProjectLocation {
    pub selected: PathBuf,
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub ignored: bool,
    pub submodules: bool,
}

pub(crate) fn has_submodules(repo: &gix::Repository, head: Option<gix::ObjectId>) -> Result<bool> {
    let mut trees = match head {
        Some(head) => vec![repo.find_commit(head)?.tree_id()?.detach()],
        None => vec![],
    };
    while let Some(tree) = trees.pop() {
        let object = repo.find_object(tree)?;
        for entry in gix::objs::TreeRefIter::from_bytes(&object.data, repo.object_hash()) {
            let entry = entry?;
            if entry.mode.is_commit() || entry.filename == ".gitmodules" {
                return Ok(true);
            }
            if entry.mode.is_tree() {
                trees.push(entry.oid.to_owned());
            }
        }
    }
    Ok(false)
}

impl Repository {
    pub fn project_location(path: &Path) -> Result<ProjectLocation> {
        let selected = path.canonicalize()?;
        if !selected.is_dir() {
            return Err("Choose a project folder.".into());
        }
        let repo = gix::discover_opts(
            &selected,
            Default::default(),
            gix::open::Options::isolated(),
        )?;
        let root = repo
            .workdir()
            .ok_or("Choose a working folder, not a bare repository.")?
            .canonicalize()?;
        let relative = selected.strip_prefix(&root)?;
        let index = repo.index_or_empty()?;
        let mut excludes = repo.excludes(
            &index,
            None,
            gix::worktree::stack::state::ignore::Source::WorktreeThenIdMappingIfNotSkipped,
        )?;
        let ignored = !relative.as_os_str().is_empty()
            && excludes
                .at_path(relative, Some(gix::index::entry::Mode::DIR))?
                .is_excluded();
        let head = repo.head()?;
        let submodules = root.join(".gitmodules").symlink_metadata().is_ok()
            || repo
                .index_or_empty()?
                .entries()
                .iter()
                .any(|entry| entry.mode == gix::index::entry::Mode::COMMIT)
            || has_submodules(&repo, head.id().map(|id| id.detach()))?;
        Ok(ProjectLocation {
            selected,
            root,
            common_dir: repo.common_dir().canonicalize()?,
            ignored,
            submodules,
        })
    }
}
