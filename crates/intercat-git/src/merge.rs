//! Object-only merges and isolated indexes for explicit conflict resolution.
use crate::{Merge, ObjectId, Repository, Result, full_sync};
use gix::bstr::{BString, ByteSlice};
use gix::prelude::Write;
use std::path::Path;

pub struct Prepared {
    pub tree: ObjectId,
    pub conflicts: Vec<BString>,
}
pub struct Stages {
    pub path: String,
    pub base: Option<crate::SavedFile>,
    pub target: Option<crate::SavedFile>,
    pub incoming: Option<crate::SavedFile>,
}

/// Merge already converted working-file bytes. Conversion programs must never
/// receive a synthetic conflict between LFS pointers or encoded Git blobs.
pub fn text(base: &[u8], target: &[u8], incoming: &[u8], marker: &str) -> Vec<u8> {
    use gix::merge::blob::builtin_driver::{self, text::*};
    let labels = [
        format!("Intercat base {marker}"),
        format!("Intercat target {marker}"),
        format!("Intercat change {marker}"),
    ];
    let size = (7..=255u8)
        .find(|size| {
            [base, target, incoming].iter().all(|bytes| {
                !bytes.split(|byte| *byte == b'\n').any(|line| {
                    let line = line.strip_suffix(b"\r").unwrap_or(line);
                    line.len() == usize::from(*size) && line.iter().all(|byte| *byte == b'=')
                })
            })
        })
        .unwrap_or(7);
    let mut output = Vec::new();
    builtin_driver::text(
        &mut output,
        &mut Default::default(),
        Labels {
            ancestor: Some(labels[0].as_bytes().as_bstr()),
            current: Some(labels[1].as_bytes().as_bstr()),
            other: Some(labels[2].as_bytes().as_bstr()),
        },
        target,
        base,
        incoming,
        Options {
            conflict: Conflict::Keep {
                style: ConflictStyle::Merge,
                marker_size: size.try_into().unwrap(),
            },
            ..Default::default()
        },
    );
    output
}
fn conflicts(outcome: &gix::merge::plumbing::tree::Outcome<'_>) -> Vec<BString> {
    let mut paths = Vec::new();
    for conflict in &outcome.conflicts {
        if conflict.is_unresolved(gix::merge::tree::TreatAsUnresolved::git()) {
            paths.push(BString::from(conflict.ours.location()));
            paths.push(BString::from(conflict.theirs.location()));
        }
    }
    paths.sort();
    paths.dedup();
    paths
}
impl Repository {
    pub fn replace_files(
        &self,
        tree: ObjectId,
        replacements: &[(String, Option<crate::SavedFile>)],
    ) -> Result<ObjectId> {
        let mut tree = self.0.edit_tree(tree)?;
        for (path, _) in replacements {
            tree.remove(path.as_str())?;
        }
        for (path, file) in replacements {
            if let Some(file) = file {
                let kind = match file.mode {
                    0o100644 => gix::objs::tree::EntryKind::Blob,
                    0o100755 => gix::objs::tree::EntryKind::BlobExecutable,
                    0o120000 => gix::objs::tree::EntryKind::Link,
                    _ => return Err("Unsupported saved conflict file type.".into()),
                };
                tree.upsert(path.as_str(), kind, file.object)?;
            }
        }
        Ok(tree.write()?.detach())
    }
    pub fn resolution_stages(&self, path: &Path) -> Result<Vec<Stages>> {
        let index = gix::index::File::at(path, self.0.object_hash(), false, Default::default())?;
        let mut paths = std::collections::BTreeMap::new();
        for entry in index
            .entries()
            .iter()
            .filter(|entry| entry.stage_raw() != 0)
        {
            let path = std::str::from_utf8(entry.path(&index))?.to_owned();
            let stages = paths.entry(path.clone()).or_insert_with(|| Stages {
                path,
                base: None,
                target: None,
                incoming: None,
            });
            let file = Some(crate::SavedFile {
                object: entry.id,
                mode: entry.mode.bits() as u16,
            });
            match entry.stage_raw() {
                1 => stages.base = file,
                2 => stages.target = file,
                3 => stages.incoming = file,
                _ => unreachable!("Git has only three unmerged stages"),
            }
        }
        Ok(paths.into_values().collect())
    }

    /// A display checkout uses valid canonical blobs, while its separate index
    /// retains every unresolved stage. No side is thereby accepted as resolved.
    pub fn resolution_display_tree(&self, index: &Path, provisional: ObjectId) -> Result<ObjectId> {
        let mut tree = self.0.edit_tree(provisional)?;
        for stages in self.resolution_stages(index)? {
            tree.remove(stages.path.as_str())?;
            if let Some(file) = stages.target.or(stages.incoming) {
                let mode = match file.mode {
                    0o100644 => gix::objs::tree::EntryKind::Blob,
                    0o100755 => gix::objs::tree::EntryKind::BlobExecutable,
                    0o120000 => gix::objs::tree::EntryKind::Link,
                    _ => return Err("Unsupported conflict side type.".into()),
                };
                tree.upsert(stages.path.as_str(), mode, file.object)?;
            }
        }
        Ok(tree.write()?.detach())
    }
    pub fn unmerged_paths(&self) -> Result<Vec<String>> {
        let index = self.0.index()?;
        let mut paths = index
            .entries()
            .iter()
            .filter(|entry| entry.stage_raw() != 0)
            .map(|entry| Ok(std::str::from_utf8(entry.path(&index))?.to_owned()))
            .collect::<Result<Vec<_>>>()?;
        paths.sort();
        paths.dedup();
        Ok(paths)
    }

    /// Install the conflict stages only over the tree just deployed for this
    /// resolution. Preparing a merge never changes the source checkout index.
    pub fn install_resolution_index(&self, captured: &Path, deployed: ObjectId) -> Result<()> {
        let destination = self.0.index_path();
        if self.tree_from_index(&destination)? != deployed {
            return Err("The merge workspace changed before installing conflict stages.".into());
        }
        let mut index =
            gix::index::File::at(captured, self.0.object_hash(), false, Default::default())?;
        if !index.entries().iter().any(|entry| entry.stage_raw() != 0) {
            return Err("The resolution index has no unresolved conflict.".into());
        }
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

    fn merge_outcome(
        &self,
        base: ObjectId,
        ours: ObjectId,
        theirs: ObjectId,
        labels: gix::merge::blob::builtin_driver::text::Labels<'_>,
    ) -> Result<gix::merge::plumbing::tree::Outcome<'_>> {
        // The high-level API uses the repository's current index for
        // attributes. That index belongs to the user's checkout, not this
        // integration. Resolve versioned attributes from the frozen target.
        let index = self.0.index_from_tree(&ours)?;
        let attrs = self
            .0
            .attributes_only(
                &index,
                gix::worktree::stack::state::attributes::Source::IdMapping,
            )?
            .detach();
        let mut diff_cache = self.0.diff_resource_cache_for_tree_diff()?;
        diff_cache.attr_stack = attrs.clone();
        let mut blob_merge = self.0.merge_resource_cache(Default::default())?;
        blob_merge.attr_stack = attrs;
        let mut options: gix::merge::plumbing::tree::Options = self.0.tree_merge_options()?.into();
        options.fail_on_conflict = None;
        Ok(gix::merge::plumbing::tree(
            &base,
            &ours,
            &theirs,
            labels,
            &self.0,
            |buf| self.0.write_buf(gix::objs::Kind::Blob, buf),
            &mut Default::default(),
            &mut diff_cache,
            &mut blob_merge,
            options,
        )?)
    }

    fn write_merge_tree(
        &self,
        outcome: &mut gix::merge::plumbing::tree::Outcome<'_>,
    ) -> Result<ObjectId> {
        outcome.tree.write(|tree| -> Result<ObjectId> {
            let id = self.0.write_object(tree)?.detach();
            Ok(self.0.edit_tree(id)?.write()?.detach())
        })
    }
    pub fn merge_trees(&self, base: ObjectId, ours: ObjectId, theirs: ObjectId) -> Result<Merge> {
        let mut outcome = self.merge_outcome(base, ours, theirs, Default::default())?;
        let conflicts = conflicts(&outcome);
        if conflicts.is_empty() {
            Ok(Merge::Clean(self.write_merge_tree(&mut outcome)?))
        } else {
            Ok(Merge::Conflicts {
                base,
                target: ours,
                incoming: theirs,
                paths: conflicts,
            })
        }
    }

    /// Write the provisional tree and a separate index with unmerged stages.
    /// No checkout, filter, hook or merge program is invoked. The caller must
    /// deploy this tree before installing the index in its private workspace.
    pub fn merge_for_resolution(
        &self,
        base: ObjectId,
        ours: ObjectId,
        theirs: ObjectId,
        marker: &str,
        index: &Path,
    ) -> Result<Prepared> {
        if marker.is_empty()
            || marker.len() > 128
            || !marker
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
        {
            return Err("Invalid integration conflict marker.".into());
        }
        if index == self.0.index_path() {
            return Err("Prepare conflict stages in a separate private index.".into());
        }
        let ancestor = format!("Intercat base {marker}");
        let current = format!("Intercat target {marker}");
        let other = format!("Intercat change {marker}");
        let mut outcome = self.merge_outcome(
            base,
            ours,
            theirs,
            gix::merge::blob::builtin_driver::text::Labels {
                ancestor: Some(ancestor.as_bytes().as_bstr()),
                current: Some(current.as_bytes().as_bstr()),
                other: Some(other.as_bytes().as_bstr()),
            },
        )?;
        let conflicts = conflicts(&outcome);
        let tree = self.write_merge_tree(&mut outcome)?;
        let mut stages = self.0.index_from_tree(&tree)?;
        outcome.index_changed_after_applying_conflicts(
            &mut stages,
            gix::merge::tree::TreatAsUnresolved::git(),
            gix::merge::tree::apply_index_entries::RemovalMode::Prune,
        );
        stages.set_path(index);
        stages.remove_tree();
        stages.write(gix::index::write::Options {
            extensions: gix::index::write::Extensions::None,
            skip_hash: false,
        })?;
        full_sync(index)?;
        full_sync(index.parent().ok_or("Missing conflict index directory.")?)?;
        Ok(Prepared { tree, conflicts })
    }
}
