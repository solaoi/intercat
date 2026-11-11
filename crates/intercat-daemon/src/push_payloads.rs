//! Copy only the LFS values needed by new commit diffs. Parent values support
//! the scan; only values introduced by the new trees can be uploaded.
use crate::{Result, push_transfer::Snapshot};
use intercat_git::{
    ObjectId, Repository, SavedFile,
    lfs::{Entry, POINTER_LIMIT, Pointer},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

pub struct Payloads {
    entries: Vec<Entry>,
    root: PathBuf,
    head: String,
}
impl Payloads {
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
    pub(crate) fn verify(&self, snapshot: &Snapshot) -> Result<()> {
        if self.root != snapshot.root || self.head != snapshot.member().target.sha {
            return Err("The LFS payloads belong to another Push review.".into());
        }
        Ok(())
    }
}
fn pointer(repo: &Repository, path: &str, file: &SavedFile) -> Result<Option<Entry>> {
    if !matches!(file.mode, 0o100644 | 0o100755) || repo.saved_size(file)? >= POINTER_LIMIT {
        return Ok(None);
    }
    Ok(
        Pointer::parse(&repo.saved_bytes(file)?).map(|pointer| Entry {
            path: path.into(),
            pointer,
        }),
    )
}
impl Snapshot {
    pub fn copy_lfs_range(
        &self,
        commits: &[ObjectId],
        source: &Path,
        interrupted: &AtomicBool,
    ) -> Result<Payloads> {
        let repo = Repository::open(&self.git_directory)?;
        let mut seen = BTreeSet::new();
        let mut scan = BTreeMap::new();
        let mut upload = BTreeMap::new();
        for commit in commits {
            if interrupted.load(Ordering::Acquire) {
                return Err("Push payload preparation was stopped.".into());
            }
            if !self.commits.iter().any(|known| commit == known) || !seen.insert(*commit) {
                return Err("The LFS range does not match the frozen Push history.".into());
            }
            let details = repo.commit_details(*commit)?;
            let before = match details.parents.first() {
                Some(parent) => repo.push_tree_files(repo.tree_id(*parent)?)?,
                None => BTreeMap::new(),
            };
            for (path, file) in repo.push_tree_files(details.tree)? {
                if before
                    .get(&path)
                    .is_some_and(|old| old.object == file.object)
                {
                    continue;
                }
                if let Some(entry) = pointer(&repo, &path, &file)? {
                    upload.insert(entry.pointer.oid().to_owned(), entry.clone());
                    scan.insert(entry.pointer.oid().to_owned(), entry);
                }
                if let Some(old) = before.get(&path)
                    && let Some(entry) = pointer(&repo, &path, old)?
                {
                    scan.insert(entry.pointer.oid().to_owned(), entry);
                }
            }
        }
        crate::lfs_store::transfer(
            source,
            &self.git_directory.join("lfs"),
            &scan.into_values().collect::<Vec<_>>(),
        )?;
        Ok(Payloads {
            entries: upload.into_values().collect(),
            root: self.root.clone(),
            head: self.member().target.sha.clone(),
        })
    }
}
