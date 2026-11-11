//! A fixed source checkout supplies only the frozen objects of a merge.
//! Domain owners validate the binding before opening these shared files.
use crate::{Result, candidate::Settings, checkout::Checkout, merge_workspace::Input};
use intercat_git::Repository;
use std::path::Path;
use tokio::sync::watch;
use uuid::Uuid;

pub(crate) async fn open(
    repository: &Path,
    parent: &Path,
    id: Uuid,
    input: Input,
    settings: &Settings,
    cancel: watch::Receiver<bool>,
) -> Result<Checkout> {
    std::fs::create_dir_all(parent)?;
    let root = parent.join(format!("merge-source-{id}"));
    let manifest = root.join("control/merge-source.json");
    let binding = serde_json::json!({"repository":repository,"input":input});
    let source = Repository::worktree_source(repository, Some(input.head.parse()?))?;
    if manifest.exists() {
        if serde_json::from_slice::<serde_json::Value>(&std::fs::read(&manifest)?)? != binding {
            return Err("The private merge source changed.".into());
        }
        return Checkout::resume(
            &root,
            source,
            settings.checkout.clone(),
            settings.path.clone().into(),
            settings.toolchain.clone(),
        )
        .await;
    }
    if root.exists() {
        crate::repair_service::remove_unprepared(&root)?;
    }
    let original = Checkout::create(
        &root,
        source,
        input.head.parse()?,
        settings.checkout.clone(),
        settings.path.clone().into(),
        settings.toolchain.clone(),
        cancel,
    )
    .await?;
    let repo = Repository::open(&original.workdir)?;
    for tree in [&input.base, &input.target, &input.incoming] {
        let tree = tree.parse()?;
        repo.import_tree(repository, tree)?;
        crate::lfs_store::transfer(
            &settings.checkout.lfs_directory,
            &original.workdir.join(".git/lfs"),
            &repo.lfs_entries(tree)?,
        )?;
        repo.sync_objects(&[tree])?;
    }
    std::fs::write(manifest, serde_json::to_vec(&binding)?)?;
    Ok(original)
}
