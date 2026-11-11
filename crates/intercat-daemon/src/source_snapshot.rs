//! Normalize source files through the same isolated clean-filter path as a
//! checkpoint. Repository discovery and index comparison never execute code.
use crate::{
    Result,
    snapshot::{self, Purpose},
};
use intercat_git::{Repository, WorktreeSource};
use std::{collections::BTreeSet, path::Path};

pub async fn clean_project(path: &Path) -> Result<WorktreeSource> {
    let path = path.to_path_buf();
    let source = tokio::task::spawn_blocking(move || Repository::project_source(&path)).await??;
    let snapshot = snapshot::capture(&source.root, source.head, Purpose::Inspection).await?;
    let source_copy = source.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(&source_copy.root)?;
        if snapshot.tree != repo.tree_id(source_copy.head)? {
            return Err(
                "Your checkout has uncommitted changes. Commit or stash them before starting."
                    .into(),
            );
        }
        crate::lfs_store::publish(
            &snapshot.root.join("lfs"),
            &snapshot.configuration.lfs_directory,
            &repo.lfs_entries(snapshot.tree)?,
        )?;
        repo.sync_objects(&[source_copy.head])
    })
    .await??;
    Ok(source)
}

pub(crate) async fn verify_paths(path: &Path, allowed: &BTreeSet<String>) -> Result<()> {
    let path = path.to_path_buf();
    let source =
        tokio::task::spawn_blocking(move || Repository::worktree_source(&path, None)).await??;
    let snapshot = snapshot::capture(&source.root, source.head, Purpose::Inspection).await?;
    let allowed = allowed.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(&source.root)?;
        if !repo
            .changed_index_paths(&snapshot.root.join("index"), source.head)?
            .is_subset(&allowed)
        {
            return Err("The branch worktree changed before activation.".into());
        }
        Ok(())
    })
    .await?
}
