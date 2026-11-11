//! Isolated merge files, frozen converted sides and held editor snapshots.
//! Commit attribution and realignment planning validate their own domain binding.
use crate::{Result, checkout::Checkout};
use intercat_git::Repository;
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Input {
    pub binding: String,
    pub track: String,
    pub head: String,
    pub base: String,
    pub target: String,
    pub incoming: String,
    pub conflicts: Vec<String>,
    pub marker: Uuid,
}
pub struct Resolved {
    pub tree: String,
    pub left_out: Vec<String>,
}
pub struct Workspace {
    pub checkout: Checkout,
    pub(crate) input: Input,
    pub(crate) original: PathBuf,
}
impl Workspace {
    pub async fn prepare(
        original: &Checkout,
        root: &Path,
        input: Input,
        path: &str,
        toolchain: &[PathBuf],
        cancel: watch::Receiver<bool>,
    ) -> Result<Self> {
        let source = Repository::worktree_source(&original.workdir, Some(input.head.parse()?))?;
        let checkout = Checkout::create(
            root,
            source,
            input.head.parse()?,
            original.child_configuration()?,
            OsString::from(path),
            toolchain.to_vec(),
            cancel.clone(),
        )
        .await?;
        let index = checkout.root.join("control/merge.index");
        let display = {
            let private = Repository::open(&checkout.workdir)?;
            for tree in [&input.base, &input.target, &input.incoming] {
                let tree = tree.parse()?;
                private.import_tree(&original.workdir, tree)?;
                crate::lfs_store::transfer(
                    &original.workdir.join(".git/lfs"),
                    &checkout.workdir.join(".git/lfs"),
                    &private.lfs_entries(tree)?,
                )?;
            }
            let prepared = private.merge_for_resolution(
                input.base.parse()?,
                input.target.parse()?,
                input.incoming.parse()?,
                &input.marker.to_string(),
                &index,
            )?;
            if prepared.conflicts.is_empty() {
                return Err(
                    "The frozen merge is no longer conflicting. Review this merge again.".into(),
                );
            }
            private.resolution_display_tree(&index, prepared.tree)?
        };
        checkout.deploy(display, cancel.clone()).await?;
        Repository::open(&checkout.workdir)?.install_resolution_index(&index, display)?;
        crate::merge_content::prepare(&checkout, &original.workdir, &input, cancel).await?;
        Ok(Self {
            checkout,
            input,
            original: original.workdir.clone(),
        })
    }
    pub async fn resume(
        original: &Checkout,
        root: &Path,
        input: Input,
        path: &str,
        toolchain: &[PathBuf],
    ) -> Result<Self> {
        crate::merge_content::paths(&original.workdir, &input)?;
        let source = Repository::worktree_source(&original.workdir, Some(input.head.parse()?))?;
        let checkout = Checkout::resume(
            root,
            source,
            original.child_configuration()?,
            OsString::from(path),
            toolchain.to_vec(),
        )
        .await?;
        if Repository::open(&checkout.workdir)?.head_commit()?
            != input.head.parse::<intercat_git::ObjectId>()?
        {
            return Err("The held merge changed its Git head.".into());
        }
        Ok(Self {
            checkout,
            input,
            original: original.workdir.clone(),
        })
    }
    pub fn snapshot_workspace(&self, run: Uuid) -> crate::preparation::Workspace {
        crate::preparation::Workspace {
            denied_roots: self.checkout.configuration.denied_roots.clone(),
            attachment_roots: self.checkout.attachment_roots.clone(),
            track_id: self.input.track.clone(),
            path: self.checkout.workdir.clone(),
            reference: format!("refs/intercat/repairs/{run}"),
            base: self.input.head.clone(),
            state: "ready".into(),
            error: None,
        }
    }
    pub async fn finish(&self, cancel: watch::Receiver<bool>) -> Result<Resolved> {
        self.checkout.verify_storage()?;
        crate::merge_content::paths(&self.original, &self.input)?;
        let repository = Repository::open(&self.checkout.workdir)?;
        if repository.head_commit()? != self.input.head.parse::<intercat_git::ObjectId>()? {
            return Err("The merge changed its Git head. Keep edits in the working tree.".into());
        }
        // An unchanged binary or deletion cannot count as resolved merely
        // because a general add -A silently cleared its stages.
        if !repository.unmerged_paths()?.is_empty() {
            return Err(
                "The merge still has unresolved paths. Resolve and stage each conflict first."
                    .into(),
            );
        }
        let left_out = self
            .checkout
            .stage(self.input.head.parse()?, cancel)
            .await?;
        let tree = repository.tree_from_index(&self.checkout.workdir.join(".git/index"))?;
        for (path, file) in repository
            .tree_files(tree)?
            .into_iter()
            .filter(|(_, file)| matches!(file.mode, 0o100644 | 0o100755))
        {
            let contents = repository.saved_bytes(&file)?;
            if crate::merge_manual::has_markers(&contents, self.input.marker) {
                return Err("The merge still contains its conflict markers.".into());
            }
            if let Some(pointer) = intercat_git::lfs::Pointer::parse(&contents) {
                let entry = intercat_git::lfs::Entry { path, pointer };
                if let Some(file) =
                    crate::lfs_store::read(&self.checkout.workdir.join(".git/lfs"), &entry)?
                    && crate::merge_manual::contains_markers(file, self.input.marker)?
                {
                    return Err(
                        "The merge's LFS content still contains its conflict markers.".into(),
                    );
                }
            }
        }
        repository.sync_objects(&[tree])?;
        Ok(Resolved {
            tree: tree.to_string(),
            left_out,
        })
    }
}
