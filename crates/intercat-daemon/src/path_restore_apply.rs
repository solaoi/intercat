//! Frozen Restore effects. A file intent survives pinning, partial writes and
//! checkpoint replay; the caller saves its actor state before releasing holds.
use crate::{
    Result,
    checkpoint::Checkpoint,
    inheritance_apply::Guard,
    journal::Journal,
    path_restore::{Outcome, Request},
    path_restore_store::{Anchor, Application, Change, Files, Plan},
};
use intercat_git::Repository;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub(crate) enum Boundary {
    Live(crate::inheritance_apply::Boundary),
    Offline {
        guard: Guard,
        directory: PathBuf,
        workspace: PathBuf,
        expected: Option<crate::process::Identity>,
    },
}
impl Boundary {
    fn guard(&self) -> &Guard {
        match self {
            Self::Live(boundary) => &boundary.guard,
            Self::Offline { guard, .. } => guard,
        }
    }
    async fn idle(&self) -> Result<()> {
        if !self.guard().current() {
            return Err(crate::journal::StartSuperseded.into());
        }
        match self {
            Self::Live(boundary) => boundary.idle().await,
            Self::Offline {
                directory,
                workspace,
                expected,
                ..
            } => {
                let (directory, workspace, expected) =
                    (directory.clone(), workspace.clone(), *expected);
                tokio::task::spawn_blocking(move || -> Result<()> {
                    if let Some(mut tracker) =
                        crate::process::Tracker::restore(&directory, &workspace, expected)?
                        && !tracker.refresh()?.is_empty()
                    {
                        return Err(crate::inheritance_issue::Blocker::Processes.into());
                    }
                    Ok(())
                })
                .await?
            }
        }
    }
    async fn access(&self, journal: &Journal, request: &Request, root: &Path) -> Result<()> {
        let root = root.to_owned();
        let mut owned = vec![crate::process::identity(std::process::id())?];
        if let Self::Live(boundary) = self {
            owned.push(boundary.identity);
        }
        let inspection =
            tokio::task::spawn_blocking(move || crate::workspace_access::inspect(&root, &owned))
                .await??;
        if !inspection.clear()
            && !journal
                .path_restore_access_allowed(request.clone(), inspection.fingerprint.clone())
                .await?
        {
            return Err(Box::new(crate::workspace_access::ConfirmationRequired {
                inspection,
            }));
        }
        Ok(())
    }
}
pub(crate) struct Applied {
    pub operation: Uuid,
    pub checkpoint: Checkpoint,
}
fn worker_repository(source: &crate::path_restore_source::Context) -> Result<Repository> {
    let repo = Repository::open(&source.entry.workspace.path)?;
    if repo.common_directory()? != source.common_directory
        || repo.working_directory()? != source.entry.workspace.path.canonicalize()?
    {
        return Err("The Restore workspace belongs to another repository.".into());
    }
    Ok(repo)
}
pub(crate) async fn apply(
    journal: &Journal,
    request: Request,
    boundary: Boundary,
) -> Result<Option<Applied>> {
    let operation = crate::path_restore::file_operation(&request);
    let app = if let Some((app, state)) = journal.path_restore_application(request.clone()).await? {
        if state == "complete" {
            return Ok(None);
        }
        app
    } else {
        let source = journal.path_restore_source(request.retirement_id).await?;
        source.verify_repository()?;
        if !boundary.guard().current() {
            return Err(crate::journal::StartSuperseded.into());
        }
        let plan = if source.cancelled.is_empty() {
            Plan::Reopen
        } else {
            boundary.idle().await?;
            boundary
                .access(journal, &request, &source.entry.workspace.path)
                .await?;
            worker_repository(&source)?;
            let previous = source.entry.saved.as_ref().unwrap().checkpoint.as_ref();
            let captured = crate::checkpoint::capture(
                &source.entry.workspace,
                previous
                    .map(|cp| cp.sha.as_str())
                    .unwrap_or(&source.entry.workspace.base),
                "Before Restore",
                journal,
            )
            .await?;
            if !captured.unmerged_paths.is_empty() {
                return Err("Resolve the worker's saved index conflicts before Restore.".into());
            }
            let inputs = crate::path_restore::Inputs {
                track: source.entry.track.clone(),
                before: captured.sha,
                basis: source.basis.clone(),
                target: source.target.clone(),
                coverage: source.coverage.clone(),
                prefix: source.prefix.clone(),
                cancelled: source.cancelled.clone(),
                resolutions: vec![],
            };
            let source_files = source.clone();
            let request_files = request.clone();
            tokio::task::spawn_blocking(move || -> Result<Plan> {
                let anchor = anchor(&source_files, &request_files)?;
                plan(&source_files, inputs, anchor)
            })
            .await??
        };
        let app = Application {
            request: request.clone(),
            source,
            plan,
        };
        journal
            .change_path_restore(Change::Begin(Box::new(app.clone())))
            .await?;
        app
    };
    if !boundary.guard().current() {
        return Err(crate::journal::StartSuperseded.into());
    }
    if let Plan::Reopen = &app.plan {
        app.source.verify_repository()?;
        journal
            .change_path_restore(Change::Complete(operation))
            .await?;
        return Ok(None);
    }
    let pins = app.clone();
    tokio::task::spawn_blocking(move || pin(&pins)).await??;
    if matches!(app.plan, Plan::Conflict { .. }) {
        crate::restore_merge::freeze(journal, app.clone()).await?;
        return Err(crate::path_restore::MergeRequired.into());
    }
    let Plan::Files(files) = &app.plan else {
        unreachable!()
    };
    let checkpoint_id = Uuid::new_v5(&files.anchor.checkpoint_turn, b"checkpoint");
    if journal.operation(checkpoint_id).await?.is_none() {
        boundary.idle().await?;
        boundary
            .access(journal, &request, &app.source.entry.workspace.path)
            .await?;
        app.source.verify_repository()?;
        let state = journal
            .operation(operation)
            .await?
            .ok_or("Missing Restore file intent.")?;
        if state["state"] == "prepared" {
            let observed = crate::checkpoint::capture(
                &app.source.entry.workspace,
                &files.prepared.inputs.before,
                "Verify Restore boundary",
                journal,
            )
            .await?;
            {
                let repo = worker_repository(&app.source)?;
                if repo.tree_id(observed.sha.parse()?)?
                    != repo.tree_id(files.prepared.inputs.before.parse()?)?
                {
                    return Err(
                        "Working files changed before Restore. Your edits were kept.".into(),
                    );
                }
            }
            journal.finish(&operation.to_string(), "dispatched").await?;
        }
        let transition = crate::workspace_transition::prepare(
            &app.source.entry.workspace,
            files.prepared.inputs.before.parse()?,
            files.after.parse()?,
        )
        .await?;
        boundary.idle().await?;
        boundary
            .access(journal, &request, &app.source.entry.workspace.path)
            .await?;
        let source = app.source.clone();
        let applied_files = files.clone();
        let guard = boundary.guard().clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let repo = worker_repository(&source)?;
            repo.verify_worker_base(
                applied_files.anchor.expected_head.parse()?,
                applied_files.prepared.basis.base.parse()?,
            )?;
            if !guard.current() {
                return Err(crate::journal::StartSuperseded.into());
            }
            transition.apply(&source.entry.workspace.path)?;
            repo.advance_worker_base(
                applied_files.anchor.expected_head.parse()?,
                applied_files.prepared.basis.base.parse()?,
            )
        })
        .await??;
        boundary.idle().await?;
        let saved = crate::checkpoint::capture(
            &app.source.entry.workspace,
            &files.prepared.basis.base,
            "Restored worker checkpoint",
            journal,
        )
        .await?;
        {
            let repo = worker_repository(&app.source)?;
            crate::tree_transition::verify(
                &repo,
                repo.tree_id(files.prepared.inputs.before.parse()?)?,
                repo.tree_id(files.after.parse()?)?,
                repo.tree_id(saved.sha.parse()?)?,
            )?;
        }
        journal
            .change_path_restore(Change::Checkpoint {
                operation,
                checkpoint: Checkpoint {
                    turn_id: files.anchor.checkpoint_turn,
                    sha: saved.sha,
                    reference: format!(
                        "{}-checkpoints/{}",
                        app.source.entry.workspace.reference, files.anchor.checkpoint_turn
                    ),
                    unmerged_paths: saved.unmerged_paths,
                    live_processes: false,
                },
                lfs_directory: saved.lfs_directory,
            })
            .await?;
    }
    boundary.idle().await?;
    worker_repository(&app.source)?.advance_worker_base(
        files.anchor.expected_head.parse()?,
        files.prepared.basis.base.parse()?,
    )?;
    let checkpoint = crate::checkpoint::save(
        app.source.entry.workspace.clone(),
        files.anchor.checkpoint_turn,
        app.source.entry.saved.as_ref().unwrap().checkpoint.clone(),
        false,
        journal.clone(),
    )
    .await?;
    let (prefix, label) = app
        .source
        .entry
        .workspace
        .reference
        .rsplit_once("/tracks/")
        .ok_or("Invalid Restore worker reference.")?;
    let base_ref = format!("{prefix}/base/{label}");
    let repo = worker_repository(&app.source)?;
    repo.compare_and_swap(
        &base_ref,
        files
            .anchor
            .expected_base
            .as_ref()
            .map(|id| id.parse())
            .transpose()?,
        files.prepared.basis.base.parse()?,
    )?;
    repo.sync_ref(&base_ref)?;
    Ok(Some(Applied {
        operation,
        checkpoint,
    }))
}

pub(crate) fn anchor(
    source: &crate::path_restore_source::Context,
    request: &Request,
) -> Result<Anchor> {
    let repo = worker_repository(source)?;
    let (prefix, label) = source
        .entry
        .workspace
        .reference
        .rsplit_once("/tracks/")
        .ok_or("Invalid Restore worker reference.")?;
    Ok(Anchor {
        expected_head: repo.worker_base()?.to_string(),
        expected_base: repo
            .reference_target(&format!("{prefix}/base/{label}"))?
            .map(|id| id.to_string()),
        checkpoint_turn: Uuid::new_v5(
            &crate::path_restore::file_operation(request),
            b"restored-checkpoint",
        ),
    })
}
pub(crate) fn plan(
    source: &crate::path_restore_source::Context,
    inputs: crate::path_restore::Inputs,
    anchor: Anchor,
) -> Result<Plan> {
    let repo = worker_repository(source)?;
    match crate::path_restore::prepare(&repo, &source.graph, &inputs)? {
        Outcome::Conflict(conflict) => {
            repo.sync_objects(&[
                inputs.before.parse()?,
                conflict.base.parse()?,
                conflict.target.parse()?,
                conflict.incoming.parse()?,
            ])?;
            Ok(Plan::Conflict {
                inputs: Box::new(inputs),
                conflict,
                anchor,
            })
        }
        Outcome::Prepared(prepared) => {
            let after = repo.checkpoint_commit(
                prepared.tree.parse()?,
                prepared.basis.base.parse()?,
                "Planned Restore",
            )?;
            repo.sync_objects(&[inputs.before.parse()?, after, prepared.basis.base.parse()?])?;
            Ok(Plan::Files(Box::new(Files {
                prepared: *prepared,
                after: after.to_string(),
                anchor,
            })))
        }
    }
}
pub(crate) fn pin(app: &Application) -> Result<()> {
    let repo = worker_repository(&app.source)?;
    let operation = crate::path_restore::file_operation(&app.request);
    let (scope, objects) = match &app.plan {
        Plan::Reopen => return Ok(()),
        Plan::Files(files) => {
            files.prepared.verify(&repo, &app.source.graph)?;
            (
                String::new(),
                vec![
                    ("before", files.prepared.inputs.before.as_str()),
                    ("after", files.after.as_str()),
                    ("base", files.prepared.basis.base.as_str()),
                ],
            )
        }
        Plan::Conflict {
            inputs, conflict, ..
        } => {
            let Outcome::Conflict(actual) =
                crate::path_restore::prepare(&repo, &app.source.graph, inputs)?
            else {
                return Err("The saved Restore no longer has this conflict.".into());
            };
            if actual != *conflict {
                return Err("The saved Restore conflict changed.".into());
            }
            (
                format!("conflicts/{}/", inputs.resolutions.len() + 1),
                vec![
                    ("before", inputs.before.as_str()),
                    ("base", conflict.base.as_str()),
                    ("target", conflict.target.as_str()),
                    ("incoming", conflict.incoming.as_str()),
                ],
            )
        }
    };
    for (name, sha) in objects {
        let reference = format!(
            "{}-restore/{operation}/{scope}{name}",
            app.source.entry.workspace.reference
        );
        repo.compare_and_swap(&reference, None, sha.parse()?)?;
        repo.sync_ref(&reference)?;
    }
    Ok(())
}
