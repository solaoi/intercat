//! Transport history lives in a separate bare store. Project hooks never read
//! this store: their shallow checkout has a different execution profile.
use crate::{ObjectId, Repository, Result, full_sync};
use gix::prelude::Write;
use std::{
    collections::HashSet,
    sync::atomic::{AtomicBool, Ordering},
};

pub struct PushHistory {
    pub commits: Vec<ObjectId>,
    pub shallow: Vec<ObjectId>,
}
impl Repository {
    /// Exclude histories reachable from advertised remote tips that are also
    /// known locally. Unknown tips cannot prove that any local object is remote.
    pub fn push_commits(
        &self,
        head: ObjectId,
        remote_tips: &[ObjectId],
        interrupted: &AtomicBool,
    ) -> Result<Vec<ObjectId>> {
        self.0.find_commit(head)?;
        let boundaries: HashSet<ObjectId> = self
            .0
            .shallow_commits()?
            .map(|commits| commits.iter().copied().collect())
            .unwrap_or_default();
        let mut excluded = HashSet::new();
        let mut pending = remote_tips
            .iter()
            .copied()
            .filter(|id| self.0.find_commit(*id).is_ok())
            .collect::<Vec<_>>();
        while let Some(id) = pending.pop() {
            if interrupted.load(Ordering::Acquire) {
                return Err("Push range inspection was stopped.".into());
            }
            if !excluded.insert(id) || boundaries.contains(&id) {
                continue;
            }
            pending.extend(
                self.0
                    .find_commit(id)?
                    .parent_ids()
                    .map(|parent| parent.detach()),
            );
        }
        let mut pending = vec![head];
        let mut seen = HashSet::new();
        let mut commits = Vec::new();
        while let Some(id) = pending.pop() {
            if interrupted.load(Ordering::Acquire) {
                return Err("Push range inspection was stopped.".into());
            }
            if !seen.insert(id) || excluded.contains(&id) {
                continue;
            }
            commits.push(id);
            if !boundaries.contains(&id) {
                pending.extend(
                    self.0
                        .find_commit(id)?
                        .parent_ids()
                        .map(|parent| parent.detach()),
                );
            }
        }
        Ok(commits)
    }

    pub fn import_push_history(
        &self,
        source: &Self,
        head: ObjectId,
        interrupted: &AtomicBool,
    ) -> Result<PushHistory> {
        if self.0.workdir().is_some() || self.common_directory()? == source.common_directory()? {
            return Err("Push history needs a separate bare object store.".into());
        }
        source.0.find_commit(head)?;
        let boundaries: HashSet<ObjectId> = source
            .0
            .shallow_commits()?
            .map(|commits| commits.iter().copied().collect())
            .unwrap_or_default();
        let mut pending = vec![head];
        let mut seen = HashSet::new();
        let mut commits = vec![];
        let mut shallow = vec![];
        while let Some(id) = pending.pop() {
            if interrupted.load(Ordering::Acquire) {
                return Err("Push history preparation was stopped.".into());
            }
            if !seen.insert(id) {
                continue;
            }
            let object = source.0.find_object(id)?;
            match object.kind {
                gix::objs::Kind::Commit => {
                    let commit =
                        gix::objs::CommitRef::from_bytes(&object.data, source.0.object_hash())?;
                    pending.push(commit.tree());
                    if boundaries.contains(&id) {
                        shallow.push(id);
                    } else {
                        pending.extend(commit.parents());
                    }
                    commits.push(id);
                }
                gix::objs::Kind::Tree => {
                    for entry in
                        gix::objs::TreeRefIter::from_bytes(&object.data, source.0.object_hash())
                    {
                        let entry = entry?;
                        // Old gitlinks are tree metadata. Their objects belong
                        // to other repositories and are never copied or run.
                        if !entry.mode.is_commit() {
                            pending.push(entry.oid.to_owned());
                        }
                    }
                }
                gix::objs::Kind::Blob => {}
                _ => return Err("Unexpected object in Push history.".into()),
            }
            if self.0.write_buf(object.kind, &object.data)? != id {
                return Err("Push object hash mismatch.".into());
            }
        }
        self.sync_objects(&commits)?;
        let path = self.0.common_dir().join("shallow");
        shallow.sort();
        if !shallow.is_empty() {
            let mut lock = gix::lock::File::acquire_to_update_resource(
                &path,
                gix::lock::acquire::Fail::Immediately,
                None,
                0,
            )?;
            use std::io::Write;
            for id in &shallow {
                writeln!(lock, "{id}")?;
            }
            full_sync(lock.lock_path())?;
            lock.commit()?;
            full_sync(&path)?;
        } else if path.exists() {
            std::fs::remove_file(&path)?;
        }
        full_sync(self.0.common_dir())?;
        Ok(PushHistory { commits, shallow })
    }
}
