//! Preserve a captured undo solution before advancing its review and Check.
use crate::{
    Result,
    journal::Journal,
    repair_run::Run,
    undo_merge_store::{Applied, Change},
};
use intercat_git::Repository;
use tokio::sync::watch;
use uuid::Uuid;

pub(crate) async fn apply(
    journal: &Journal,
    run: &Run,
    workspace: &crate::undo_merge::Workspace,
    cancel: watch::Receiver<bool>,
) -> Result<Run> {
    let id = Uuid::new_v5(&run.id, b"apply-undo-merge");
    let result = if journal.operation(id).await?.is_some() {
        let (result, state) = journal.undo_merge_result(id).await?;
        if result.run != run.id || result.input.id != workspace.input.id {
            return Err("The saved solution belongs to another undo merge.".into());
        }
        if matches!(state.as_str(), "dispatched" | "complete") {
            return journal.repair_run(run.id).await;
        }
        if state != "prepared" {
            return Err("The saved undo result is unavailable.".into());
        }
        result
    } else {
        let (live, source) = journal.repair_source(run.id).await?;
        let crate::run_input::Source::Undo(input) = source else {
            return Err("This merge does not belong to an undo.".into());
        };
        workspace.validate(&input)?;
        if live.revision != run.revision {
            return Err("The captured undo merge changed.".into());
        }
        let resolved = if let Some(snapshot) = &live.manual {
            crate::merge_editor::resolve(&workspace.files, snapshot, cancel).await?
        } else {
            workspace.files.finish(cancel).await?
        };
        if !resolved.left_out.is_empty() {
            return Err("The undo merge contains files that cannot be saved.".into());
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
            .change_undo_merge(Change::Begin(Box::new(result.clone())))
            .await?;
        result
    };
    let source = workspace.files.checkout.workdir.clone();
    let pin = result.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(&pin.input.project.root)?;
        if repo.common_directory()? != pin.input.project.common_dir
            || !repo.owns_session(&pin.input.slug, &pin.input.session_id)?
        {
            return Err("The undo result's repository reservation changed.".into());
        }
        let tree = pin.choice.tree.parse()?;
        repo.import_tree(&source, tree)?;
        crate::lfs_store::transfer(
            &source.join(".git/lfs"),
            &pin.input.settings.checkout.lfs_directory,
            &repo.lfs_entries(tree)?,
        )?;
        repo.sync_objects(&[tree])?;
        let reference = format!("refs/intercat/{}/undo-results/{}", pin.input.slug, pin.id);
        repo.compare_and_swap(&reference, None, tree)?;
        repo.sync_ref(&reference)
    })
    .await??;
    journal.change_undo_merge(Change::Admit(id)).await?;
    journal.repair_run(run.id).await
}
