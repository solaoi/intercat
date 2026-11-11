//! Import and protect a private solution, then save and pin the next stage
//! before waking the Restore. This never edits the original worker itself.
use crate::{
    Result,
    journal::Journal,
    repair_run::Run,
    restore_merge_store::{Applied, Change},
};
use intercat_git::Repository;
use uuid::Uuid;
pub(crate) async fn apply(
    journal: &Journal,
    run: &Run,
    workspace: &crate::restore_merge::Workspace,
    cancel: tokio::sync::watch::Receiver<bool>,
) -> Result<Run> {
    let id = Uuid::new_v5(&run.id, b"apply-restore-merge");
    let (saved, state) = if let Some(old) = journal.operation(id).await? {
        if old["kind"] != "restore.merge.result" {
            return Err("Restore result identity reused.".into());
        }
        let saved: crate::restore_merge_store::Saved =
            serde_json::from_value(old["payload"].clone())?;
        if saved.result.run != run.id || saved.result.input.id != workspace.input.id {
            return Err("The saved result belongs to another Restore conflict.".into());
        }
        if old["state"] == "complete" {
            return journal.repair_run(run.id).await;
        }
        (
            saved,
            old["state"]
                .as_str()
                .ok_or("Missing Restore result state.")?
                .to_owned(),
        )
    } else {
        let (live, source) = journal.repair_source(run.id).await?;
        let crate::run_input::Source::Restore(input) = source else {
            return Err("This merge is not owned by Restore.".into());
        };
        workspace.validate(&input)?;
        if live.revision != run.revision {
            return Err("The captured Restore merge changed.".into());
        }
        let resolved = if let Some(snapshot) = &live.manual {
            crate::merge_editor::resolve(&workspace.files, snapshot, cancel).await?
        } else {
            workspace.files.finish(cancel).await?
        };
        if !resolved.left_out.is_empty() {
            return Err("The Restore merge has files that cannot be saved.".into());
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
            .change_restore_merge(Change::Begin(Box::new(result.clone())))
            .await?;
        (
            crate::restore_merge_store::Saved { result, next: None },
            "prepared".into(),
        )
    };
    if !matches!(state.as_str(), "prepared" | "dispatched") {
        return Err("The Restore result is unavailable.".into());
    }
    let source = workspace.files.checkout.workdir.clone();
    let pin = saved.result.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(&pin.input.project.root)?;
        if repo.common_directory()? != pin.input.project.common_dir
            || !repo.owns_session(
                &pin.input.source.entry.slug,
                &pin.input.source.entry.session,
            )?
        {
            return Err("The Restore result repository reservation changed.".into());
        }
        let tree = pin.choice.tree.parse()?;
        repo.import_tree(&source, tree)?;
        crate::lfs_store::transfer(
            &source.join(".git/lfs"),
            &pin.input.settings.checkout.lfs_directory,
            &repo.lfs_entries(tree)?,
        )?;
        repo.sync_objects(&[tree])?;
        let reference = format!(
            "refs/intercat/{}/restore-results/{id}",
            pin.input.source.entry.slug
        );
        repo.compare_and_swap(&reference, None, tree)?;
        repo.sync_ref(&reference)
    })
    .await??;
    let next = if let Some(next) = saved.next {
        next
    } else {
        let result = saved.result.clone();
        let next = tokio::task::spawn_blocking(move || {
            let mut inputs = result.input.inputs.clone();
            inputs.resolutions.push(result.choice);
            crate::path_restore_apply::plan(&result.input.source, inputs, result.input.anchor)
        })
        .await??;
        journal
            .change_restore_merge(Change::Plan {
                id,
                next: Box::new(next.clone()),
            })
            .await?;
        next
    };
    let app = crate::path_restore_store::Application {
        request: saved.result.input.request.clone(),
        source: saved.result.input.source,
        plan: next,
    };
    tokio::task::spawn_blocking(move || crate::path_restore_apply::pin(&app)).await??;
    journal.change_restore_merge(Change::Complete(id)).await?;
    journal.repair_run(run.id).await
}
