//! Move an owned, detached worker's index and HEAD without checking out files
//! or executing filters. The daemon logs both commits before calling this.
use crate::{ObjectId, Repository, Result, full_sync};

impl Repository {
    /// Staged changes need their own resolution; a working-tree snapshot alone
    /// must never silently discard their distinct index contents.
    pub fn worker_base(&self) -> Result<ObjectId> {
        let head = self.head_commit()?;
        self.verify_worker_base(head, head)?;
        Ok(head)
    }
    pub fn verify_worker_base(&self, before: ObjectId, after: ObjectId) -> Result<()> {
        if !self.0.head()?.is_detached() {
            return Err("The worker HEAD is no longer detached.".into());
        }
        let head = self.head_commit()?;
        if head != before && head != after {
            return Err("The worker HEAD changed outside this realignment.".into());
        }
        let index = gix::index::File::at(
            self.0.index_path(),
            self.0.object_hash(),
            false,
            Default::default(),
        )?;
        let files = self.files_in_index(&index)?;
        if files != self.saved_files(before)? && files != self.saved_files(after)? {
            return Err(
                "The worker has separate staged changes. Review its index before realigning it."
                    .into(),
            );
        }
        Ok(())
    }
    pub fn advance_worker_base(&self, before: ObjectId, after: ObjectId) -> Result<()> {
        let index_path = self.0.index_path();
        let mut lock = gix::lock::File::acquire_to_update_resource(
            &index_path,
            gix::lock::acquire::Fail::Immediately,
            None,
            0,
        )?;
        // Validate under the index lock, so a concurrent git add cannot be
        // replaced between the comparison and publication.
        self.verify_worker_base(before, after)?;
        let mut index = self.0.index_from_tree(&self.tree_id(after)?)?;
        index.remove_tree();
        index.write_to(
            &mut lock,
            gix::index::write::Options {
                extensions: gix::index::write::Extensions::None,
                skip_hash: false,
            },
        )?;
        full_sync(lock.lock_path())?;
        lock.commit()?;
        full_sync(&index_path)?;
        full_sync(self.0.git_dir())?;
        // A crash between these two writes is a known before/after pair. This
        // CAS never dereferences an unexpected symbolic HEAD into a branch.
        if let Err(error) = self.0.reference(
            "HEAD",
            after,
            gix::refs::transaction::PreviousValue::MustExistAndMatch(before.into()),
            "Intercat realign",
        ) && (!self.0.head()?.is_detached() || self.head_commit()? != after)
        {
            return Err(error.into());
        }
        full_sync(&self.0.git_dir().join("HEAD"))?;
        full_sync(self.0.git_dir())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn index(repo: &Repository, commit: ObjectId) -> Result<()> {
        repo.0
            .index_from_tree(&repo.tree_id(commit)?)?
            .write(Default::default())?;
        Ok(())
    }
    fn head(repo: &Repository, commit: ObjectId) -> Result<()> {
        repo.0.reference(
            "HEAD",
            commit,
            gix::refs::transaction::PreviousValue::Any,
            "Dummy worker",
        )?;
        Ok(())
    }
    fn fixture() -> Result<(tempfile::TempDir, Repository, ObjectId, ObjectId)> {
        let root = tempfile::tempdir()?;
        gix::init(root.path())?;
        let repo = Repository::open(root.path())?;
        let base = repo.empty_starting_point()?;
        let old = repo.write_blob(b"old")?;
        let new = repo.write_blob(b"new")?;
        let before =
            repo.checkpoint_commit(repo.write_tree(&[("a", old)])?, base, "Dummy before")?;
        let after = repo.checkpoint_commit(
            repo.write_tree(&[("a", new), ("b", new)])?,
            before,
            "Dummy target",
        )?;
        head(&repo, before)?;
        index(&repo, before)?;
        fs::write(root.path().join("a"), b"uncommitted work")?;
        fs::write(root.path().join("late"), b"uncommitted addition")?;
        Ok((root, repo, before, after))
    }
    #[test]
    fn index_and_detached_head_resume_either_partial_pair_without_checking_out_files() -> Result<()>
    {
        for advanced_head in [false, true] {
            let (root, repo, before, after) = fixture()?;
            if advanced_head {
                head(&repo, after)?;
            } else {
                index(&repo, after)?;
            }
            repo.advance_worker_base(before, after)?;
            repo.advance_worker_base(before, after)?;
            assert_eq!(repo.worker_base()?, after);
            assert_eq!(fs::read(root.path().join("a"))?, b"uncommitted work");
            assert_eq!(fs::read(root.path().join("late"))?, b"uncommitted addition");
            assert!(
                !root.path().join("b").exists(),
                "Moving the index must not deploy files"
            );
        }
        Ok(())
    }
    #[test]
    fn separate_staged_changes_and_symbolic_head_are_kept() -> Result<()> {
        let (root, repo, before, after) = fixture()?;
        let staged = repo.checkpoint_commit(
            repo.write_tree(&[("staged-only", repo.write_blob(b"keep")?)])?,
            before,
            "Dummy staged change",
        )?;
        index(&repo, staged)?;
        let saved = fs::read(root.path().join(".git/index"))?;
        assert!(repo.advance_worker_base(before, after).is_err());
        assert_eq!(fs::read(root.path().join(".git/index"))?, saved);
        assert_eq!(repo.head_commit()?, before);
        index(&repo, before)?;
        repo.0.reference(
            "refs/heads/main",
            before,
            gix::refs::transaction::PreviousValue::Any,
            "Dummy branch",
        )?;
        fs::write(root.path().join(".git/HEAD"), b"ref: refs/heads/main\n")?;
        let saved = fs::read(root.path().join(".git/index"))?;
        assert!(repo.advance_worker_base(before, after).is_err());
        assert_eq!(fs::read(root.path().join(".git/index"))?, saved);
        assert_eq!(repo.reference_target("refs/heads/main")?, Some(before));
        Ok(())
    }
}
