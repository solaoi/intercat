//! Admit one captured solution, pin its tree, then queue the same realignment.
//! File writes still belong to realign_apply's idle boundary and recovery intent.
use crate::{Result, journal::Journal, realign_store::Change, repair_run::Run};
use intercat_git::Repository;
use tokio::sync::watch;
use uuid::Uuid;

pub(crate) async fn apply(
    journal: &Journal,
    run: &Run,
    workspace: &crate::realign_merge::Workspace,
    cancel: watch::Receiver<bool>,
) -> Result<Run> {
    let id = Uuid::new_v5(&run.id, b"apply-realignment-merge");
    let result = if let Some(saved) = journal.operation(id).await? {
        if saved["kind"] != "realign.merge.result" {
            return Err("Merge result identity reused.".into());
        }
        let result: crate::realign_merge_store::Applied =
            serde_json::from_value(saved["payload"].clone())?;
        if result.run != run.id || result.input != workspace.input.id {
            return Err("The merge result belongs to another realignment.".into());
        }
        if saved["state"] == "complete" {
            return journal.repair_run(run.id).await;
        }
        result
    } else {
        let (live, source) = journal.repair_source(run.id).await?;
        let crate::run_input::Source::Realign(input) = source else {
            return Err("The merge belongs to a Commit review.".into());
        };
        workspace.validate(&input)?;
        if live.revision != run.revision {
            return Err("The captured merge changed.".into());
        }
        let resolved = if let Some(snapshot) = &live.manual {
            crate::merge_editor::resolve(&workspace.files, snapshot, cancel).await?
        } else {
            workspace.files.finish(cancel).await?
        };
        if !resolved.left_out.is_empty() {
            return Err("The realignment merge contains files that cannot be saved.".into());
        }
        let files = input.files(run.id)?;
        let result = crate::realign_merge_store::Applied {
            id,
            run: run.id,
            revision: run.revision,
            input: input.id,
            choice: crate::merge_choices::Solved {
                base: files.base,
                target: files.target,
                incoming: files.incoming,
                tree: resolved.tree,
            },
        };
        journal
            .change_realign(Change::BeginMergeResult(Box::new(result.clone())))
            .await?;
        result
    };
    let input = workspace.input.clone();
    let source = workspace.files.checkout.workdir.clone();
    let tree = result.choice.tree.parse()?;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(&input.workspace.path)?;
        if repo.common_directory()? != input.project.common_dir {
            return Err("The worker repository changed before saving its merge.".into());
        }
        repo.import_tree(&source, tree)?;
        crate::lfs_store::transfer(
            &source.join(".git/lfs"),
            &input.settings.checkout.lfs_directory,
            &repo.lfs_entries(tree)?,
        )?;
        repo.sync_objects(&[tree])?;
        let reference = format!("{}-realign-results/{id}", input.workspace.reference);
        repo.compare_and_swap(&reference, None, tree)?;
        repo.sync_ref(&reference)
    })
    .await??;
    journal
        .change_realign(Change::CompleteMergeResult(id))
        .await?;
    journal.repair_run(run.id).await
}
