//! File-only realignment at the worker actor's idle boundary. A saved operation
//! replays its frozen before/after delta; no ordinary request is dispatched.
use crate::{
    Result,
    checkpoint::Checkpoint,
    inheritance_apply::Boundary,
    journal::Journal,
    realign::{Plan, Request, Resolution},
    realign_store::{Application, Change},
};
use intercat_git::Repository;
use uuid::Uuid;

pub(crate) struct Applied {
    pub operation: Uuid,
    pub checkpoint: Checkpoint,
}
fn verify_repository(path: &std::path::Path, common: &std::path::Path) -> Result<Repository> {
    let repo = Repository::open(path)?;
    if repo.common_directory()? != common || repo.working_directory()? != path.canonicalize()? {
        return Err("The worker workspace now belongs to another repository.".into());
    }
    Ok(repo)
}
async fn check_access(
    journal: &Journal,
    request: &Request,
    root: &std::path::Path,
    host: crate::process::Identity,
) -> Result<()> {
    let root = root.to_path_buf();
    let inspection = tokio::task::spawn_blocking(move || {
        crate::workspace_access::inspect(
            &root,
            &[host, crate::process::identity(std::process::id())?],
        )
    })
    .await??;
    if !inspection.clear()
        && !journal
            .realign_access_allowed(request.clone(), inspection.fingerprint.clone())
            .await?
    {
        return Err(Box::new(crate::workspace_access::ConfirmationRequired {
            inspection,
        }));
    }
    Ok(())
}
pub(crate) async fn apply(
    journal: &Journal,
    request: Request,
    session: &str,
    boundary: Boundary,
) -> Result<Option<Applied>> {
    if let Some(saved) = journal
        .operation(crate::realign::file_operation(&request))
        .await?
    {
        if saved["kind"] != "workspace.realign"
            || saved["payload"]["request"] != serde_json::to_value(&request)?
        {
            return Err("Realignment operation ID reused.".into());
        }
        if saved["state"] == "complete" {
            return Ok(None);
        }
    }
    boundary.idle().await?;
    let mut context = journal.realign_context(request.clone()).await?;
    if context.session != session {
        return Err("The worker session changed before realignment.".into());
    }
    verify_repository(&context.workspace.path, &context.common_directory)?;
    check_access(
        journal,
        &request,
        &context.workspace.path,
        boundary.identity,
    )
    .await?;
    if let crate::realign_store::Origin::Shared(shared) = &context.origin {
        match crate::shared_context_service::advance(journal, shared.clone()).await? {
            crate::shared_context_store::Preparation::Ready(views) => {
                context = journal.realign_context(request.clone()).await?;
                if context.session != session {
                    return Err("The worker session changed while preparing shared context.".into());
                }
                let crate::realign_store::Origin::Reparent { input, .. } = &context.origin else {
                    return Err("The shared dependency was not admitted for this worker.".into());
                };
                if views.iter().any(|view| !input.views.contains(view)) {
                    return Err("The shared context changed before file application.".into());
                }
                verify_repository(&context.workspace.path, &context.common_directory)?;
                check_access(
                    journal,
                    &request,
                    &context.workspace.path,
                    boundary.identity,
                )
                .await?;
                boundary.idle().await?;
            }
            crate::shared_context_store::Preparation::Step(record) => {
                crate::shared_merge::freeze(journal, request.clone(), *record).await?;
                return Err(Box::new(crate::shared_context_service::Required));
            }
        }
    }
    let common_directory = context.common_directory;
    let app = if let Some((app, state)) = context.saved {
        if state == "complete" {
            return Ok(None);
        }
        app
    } else {
        let before = crate::checkpoint::capture(
            &context.workspace,
            context
                .previous
                .as_ref()
                .map(|cp| cp.sha.as_str())
                .unwrap_or(&context.workspace.base),
            "Before realignment",
            journal,
        )
        .await?;
        if !before.unmerged_paths.is_empty() {
            return Err("Resolve the worker's saved index conflicts before realigning it.".into());
        }
        let before_sha = if let Some(input) = &context.merge {
            let repo = Repository::open(&context.workspace.path)?;
            if repo.tree_id(before.sha.parse()?)? != repo.tree_id(input.plan.before.parse()?)? {
                return Err(
                    "Working files changed while resolving realignment. Your edits were kept."
                        .into(),
                );
            }
            input.plan.before.clone()
        } else {
            before.sha
        };
        let (plan, head) = {
            let repo = Repository::open(&context.workspace.path)?;
            if repo.reference_target(&context.target_ref)? != Some(context.target.parse()?) {
                return Err("The session branch changed. Review realignment again.".into());
            }
            let plan = match &context.origin {
                crate::realign_store::Origin::Reparent { input, .. } => Plan::prepare_reparent(
                    &repo,
                    &context.graph,
                    &context.basis,
                    before_sha.parse()?,
                    *input.clone(),
                    &context.resolutions,
                )?,
                crate::realign_store::Origin::Commit(coverage) => Plan::prepare(
                    &repo,
                    &context.graph,
                    crate::realign::Input {
                        track: &request.track_id,
                        basis: &context.basis,
                        before: before_sha.parse()?,
                        target: context.target.parse()?,
                        coverage,
                        resolutions: &context.resolutions,
                    },
                )?,
                crate::realign_store::Origin::Shared(_) => {
                    return Err("The shared dependency has not been admitted yet.".into());
                }
            };
            let head = match &plan.resolution {
                Resolution::Conflict { target, .. } => Some(
                    repo.checkpoint_commit(
                        target.parse()?,
                        plan.before.parse()?,
                        "Isolated realignment merge",
                    )?
                    .to_string(),
                ),
                _ => None,
            };
            (plan, head)
        };
        if matches!(plan.resolution, Resolution::Conflict { .. }) {
            let input =
                if let Some(saved) = context.merge.as_ref().filter(|saved| saved.plan == plan) {
                    saved.clone()
                } else {
                    let settings = if let Some(boundary) = &context.merge {
                        boundary.settings.clone()
                    } else {
                        let mut settings = journal
                            .integration_settings(context.source.id, context.source.revision)
                            .await?
                            .ok_or("Missing frozen realignment settings.")?;
                        let source = Repository::worktree_source(
                            &context.workspace.path,
                            Some(plan.before.parse()?),
                        )?;
                        settings.checkout = crate::checkout::Configuration::capture(&source)?;
                        settings
                    };
                    let head = head.ok_or("Missing realignment merge anchor.")?;
                    let id = crate::realign_merge::Frozen::identity(&request, &plan)?;
                    crate::realign_merge::Frozen {
                        id,
                        request: request.clone(),
                        workspace: context.workspace.clone(),
                        session: context.session.clone(),
                        project: context.source.project.clone(),
                        session_name: context.source.session_name.clone(),
                        label: context.label,
                        plan,
                        settings,
                        head,
                    }
                };
            // Durable intent precedes the object roots. Replaying this boundary
            // completes a partial pin without touching the worker's files.
            let id = input.id;
            let objects = input.clone();
            tokio::task::spawn_blocking(move || objects.sync_objects()).await??;
            journal
                .change_realign(Change::FreezeMerge(Box::new(input.clone())))
                .await?;
            tokio::task::spawn_blocking(move || input.pin()).await??;
            journal.change_realign(Change::MergeReady(id)).await?;
            return Err(Box::new(crate::realign_merge::Required));
        }
        let path = context.workspace.path.clone();
        let reference = context.target_ref.clone();
        let target = context.target.clone();
        let operation = crate::realign::file_operation(&request);
        let worker_ref = context.workspace.reference.clone();
        let (plan, after, expected_base, expected_head) =
            tokio::task::spawn_blocking(move || -> Result<_> {
                let repo = Repository::open(&path)?;
                let expected_head = repo.worker_base()?.to_string();
                if repo.reference_target(&reference)? != Some(target.parse()?) {
                    return Err("The session branch changed. Review realignment again.".into());
                }
                let Resolution::Ready { tree } = &plan.resolution else {
                    return Err("Resolve the realignment conflict before applying files.".into());
                };
                let base = plan.basis.base.parse()?;
                let after = repo.checkpoint_commit(tree.parse()?, base, "Planned realignment")?;
                repo.sync_objects(&[plan.before.parse()?, after, base, target.parse()?])?;
                let prefix = worker_ref
                    .rsplit_once("/tracks/")
                    .ok_or("Invalid worker reference.")?;
                let expected = repo
                    .reference_target(&format!("{}/base/{}", prefix.0, prefix.1))?
                    .map(|sha| sha.to_string());
                Ok((plan, after.to_string(), expected, expected_head))
            })
            .await??;
        boundary.idle().await?;
        let app = Application {
            operation,
            request,
            workspace: context.workspace,
            session: context.session,
            previous: context.previous,
            target_ref: context.target_ref,
            plan,
            after,
            expected_base,
            expected_head,
            checkpoint_turn: Uuid::new_v5(&operation, b"realigned-checkpoint"),
        };
        journal
            .change_realign(Change::Begin(Box::new(app.clone())))
            .await?;
        app
    };
    let operation = app.operation;
    // The durable application owns these exact objects before any protecting
    // ref is moved. A crash during pinning reuses this application, rather
    // than taking another timestamped checkpoint under the same ref names.
    let pins = app.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(&pins.workspace.path)?;
        let objects = [
            ("before", pins.plan.before.parse()?),
            ("after", pins.after.parse()?),
            ("base", pins.plan.basis.base.parse()?),
        ];
        repo.sync_objects(&objects.iter().map(|(_, sha)| *sha).collect::<Vec<_>>())?;
        for (name, sha) in objects {
            let reference = format!(
                "{}-realign/{}/{name}",
                pins.workspace.reference, pins.operation
            );
            repo.compare_and_swap(&reference, None, sha)?;
            repo.sync_ref(&reference)?;
        }
        Ok(())
    })
    .await??;
    let checkpoint_id = Uuid::new_v5(&app.checkpoint_turn, b"checkpoint");
    if journal.operation(checkpoint_id).await?.is_none() {
        boundary.idle().await?;
        let state = journal
            .operation(operation)
            .await?
            .ok_or("Missing realignment intent.")?;
        if state["state"] == "prepared" {
            let observed = crate::checkpoint::capture(
                &app.workspace,
                &app.plan.before,
                "Verify realignment boundary",
                journal,
            )
            .await?;
            let changed = {
                let repo = Repository::open(&app.workspace.path)?;
                repo.tree_id(observed.sha.parse()?)? != repo.tree_id(app.plan.before.parse()?)?
            };
            if changed {
                return Err(
                    "Working files changed before realignment. Your edits were kept.".into(),
                );
            }
            journal.finish(&operation.to_string(), "dispatched").await?;
        }
        let files = crate::workspace_transition::prepare(
            &app.workspace,
            app.plan.before.parse()?,
            app.after.parse()?,
        )
        .await?;
        boundary.idle().await?;
        let path = app.workspace.path.clone();
        let expected_head = app.expected_head.parse()?;
        let target = app.plan.basis.base.parse()?;
        let expected_common = common_directory.clone();
        check_access(
            journal,
            &app.request,
            &app.workspace.path,
            boundary.identity,
        )
        .await?;
        boundary.idle().await?;
        let guard = boundary.guard.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let repo = verify_repository(&path, &expected_common)?;
            repo.verify_worker_base(expected_head, target)?;
            if !guard.current() {
                return Err(crate::journal::StartSuperseded.into());
            }
            files.apply(&path)?;
            repo.advance_worker_base(expected_head, target)
        })
        .await??;
        boundary.scan().await?;
        let saved = crate::checkpoint::capture(
            &app.workspace,
            &app.plan.basis.base,
            "Realigned worker checkpoint",
            journal,
        )
        .await?;
        {
            let repo = Repository::open(&app.workspace.path)?;
            crate::tree_transition::verify(
                &repo,
                repo.tree_id(app.plan.before.parse()?)?,
                repo.tree_id(app.after.parse()?)?,
                repo.tree_id(saved.sha.parse()?)?,
            )?;
        }
        journal
            .change_realign(Change::Checkpoint {
                operation,
                checkpoint: Checkpoint {
                    turn_id: app.checkpoint_turn,
                    sha: saved.sha,
                    reference: format!(
                        "{}-checkpoints/{}",
                        app.workspace.reference, app.checkpoint_turn
                    ),
                    unmerged_paths: saved.unmerged_paths,
                    live_processes: false,
                },
                lfs_directory: saved.lfs_directory,
            })
            .await?;
    }
    {
        let path = app.workspace.path.clone();
        let expected_common = common_directory.clone();
        let expected_head = app.expected_head.parse()?;
        let target = app.plan.basis.base.parse()?;
        tokio::task::spawn_blocking(move || {
            verify_repository(&path, &expected_common)?.advance_worker_base(expected_head, target)
        })
        .await??;
    }
    let checkpoint = crate::checkpoint::save(
        app.workspace.clone(),
        app.checkpoint_turn,
        app.previous,
        false,
        journal.clone(),
    )
    .await?;
    let path = app.workspace.path;
    let reference = app.workspace.reference;
    let expected = app.expected_base;
    let target = app.plan.basis.base;
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repo = Repository::open(&path)?;
        let (prefix, label) = reference
            .rsplit_once("/tracks/")
            .ok_or("Invalid worker reference.")?;
        let reference = format!("{prefix}/base/{label}");
        repo.compare_and_swap(
            &reference,
            expected.map(|sha| sha.parse()).transpose()?,
            target.parse()?,
        )?;
        repo.sync_ref(&reference)
    })
    .await??;
    Ok(Some(Applied {
        operation,
        checkpoint,
    }))
}
