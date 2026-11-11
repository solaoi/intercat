//! A shared merge result is imported and protected before waking any consumer.
use crate::{
    Result,
    journal::Journal,
    repair_run::Run,
    shared_merge_store::{Applied, Change},
};
use intercat_git::Repository;
use uuid::Uuid;
pub(crate) async fn apply(
    journal: &Journal,
    run: &Run,
    workspace: &crate::shared_merge::Workspace,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<Run> {
    let id = Uuid::new_v5(&run.id, b"apply-shared-merge");
    let result = if let Some(saved) = journal.operation(id).await? {
        if saved["kind"] != "shared.merge.result" {
            return Err("Shared merge result identity reused.".into());
        }
        let result: Applied = serde_json::from_value(saved["payload"].clone())?;
        if result.run != run.id || result.input.id != workspace.input.id {
            return Err("The saved result belongs to another shared merge.".into());
        }
        if saved["state"] == "complete" {
            return journal.repair_run(run.id).await;
        }
        if saved["state"] != "prepared" {
            return Err("The saved shared result is unavailable.".into());
        }
        result
    } else {
        let (live, source) = journal.repair_source(run.id).await?;
        let crate::run_input::Source::Shared(input) = source else {
            return Err("This merge does not belong to shared changes.".into());
        };
        workspace.validate(&input)?;
        if live.revision != run.revision {
            return Err("The captured shared merge changed.".into());
        }
        let resolved = if let Some(snapshot) = &live.manual {
            crate::merge_editor::resolve(&workspace.files, snapshot, cancel).await?
        } else {
            workspace.files.finish(cancel).await?
        };
        if !resolved.left_out.is_empty() {
            return Err("The shared merge has files that cannot be saved.".into());
        }
        let files = input.files(run.id)?;
        let result = Applied {
            id,
            run: run.id,
            revision: run.revision,
            input: *input,
            choice: crate::merge_choices::Solved {
                base: files.base,
                target: files.target,
                incoming: files.incoming,
                tree: resolved.tree,
            },
        };
        journal
            .change_shared_merge(Change::Begin(Box::new(result.clone())))
            .await?;
        result
    };
    let source = workspace.files.checkout.workdir.clone();
    let pin = result.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let original = &pin.input.record.input;
        let repo = Repository::open(&original.project.root)?;
        if repo.common_directory()? != original.project.common_dir
            || !repo.owns_session(&original.slug, &original.session)?
        {
            return Err("The shared result's repository reservation changed.".into());
        }
        let tree = pin.choice.tree.parse()?;
        repo.import_tree(&source, tree)?;
        crate::lfs_store::transfer(
            &source.join(".git/lfs"),
            &pin.input.settings.checkout.lfs_directory,
            &repo.lfs_entries(tree)?,
        )?;
        repo.sync_objects(&[tree])?;
        let reference = format!("refs/intercat/{}/shared-results/{id}", original.slug);
        repo.compare_and_swap(&reference, None, tree)?;
        repo.sync_ref(&reference)
    })
    .await??;
    journal.change_shared_merge(Change::Complete(id)).await?;
    journal.repair_run(run.id).await
}
