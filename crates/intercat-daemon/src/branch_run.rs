use crate::{
    Result,
    branch::{Branch, Change, Phase, Request},
    host,
    journal::Journal,
    runtime::Config,
};
use intercat_git::{ObjectId, Repository};
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

async fn verify_workspace(branch: &Branch, project: &crate::project::Folder) -> Result<()> {
    let path = branch.workspace.path.clone();
    let expected = ObjectId::from_hex(branch.point.sha().as_bytes())?;
    let common = project.common_dir.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let source = Repository::worktree_source(&path, None)?;
        Repository::verify_prepared_worktree(&path, &source)?;
        if source.head != expected || source.common_dir != common {
            return Err("The branch worktree changed before activation.".into());
        }
        Ok(())
    })
    .await??;
    crate::branch_instructions::verify(branch).await
}
async fn finish(journal: &Journal, operation: Uuid) -> Result<()> {
    if let Some(record) = journal.operation(operation).await? {
        if record["state"] == "prepared" {
            journal.finish(&operation.to_string(), "dispatched").await?;
        }
        if record["state"] != "complete" {
            journal.finish(&operation.to_string(), "complete").await?;
        }
    }
    Ok(())
}
pub async fn prepare(
    host_memory: std::sync::Arc<crate::host_memory::Pool>,
    journal: &Journal,
    config: Config,
    data: &Path,
    rpc: &Path,
    mut stopping: tokio::sync::watch::Receiver<bool>,
    request: Request,
) -> Result<Value> {
    let mut branch = journal.branch(Change::Prepare(request)).await?;
    if branch.phase == Phase::Preparing {
        let snapshot = journal.snapshot().await?;
        let project = crate::folders::from_snapshot(&snapshot, &branch.track_id)?;
        let mut source = project.source().await?;
        source.head = ObjectId::from_hex(branch.point.sha().as_bytes())?;
        let repository = source.root.clone();
        let reference = branch.point.reference().to_owned();
        let expected = source.head;
        tokio::task::spawn_blocking(move || -> Result<()> {
            if Repository::open(repository)?.reference_target(&reference)? != Some(expected) {
                return Err("The source checkpoint reference changed.".into());
            }
            Ok(())
        })
        .await??;
        crate::worktree::prepare(
            &source,
            &branch.workspace.path,
            &branch.workspace.reference,
            Uuid::new_v5(&branch.request.op_id, b"branch-worktree"),
            journal,
        )
        .await?;
        if branch.instructions.is_none() {
            let frozen = crate::branch_instructions::freeze(&branch, &project.root).await?;
            branch = journal
                .branch(Change::Instructions {
                    operation: branch.request.op_id,
                    snapshot: frozen,
                })
                .await?;
        }
        if branch.workspace.attachment_roots.is_empty() {
            let roots =
                crate::attachments::reserve(&branch.workspace.path, &branch.workspace.base)?;
            branch = journal
                .branch(Change::WorkspaceReady {
                    operation: branch.request.op_id,
                    roots,
                })
                .await?;
        } else {
            crate::attachments::ensure(
                &branch.workspace.path,
                &branch.workspace.base,
                &branch.workspace.attachment_roots,
            )?;
        }
        journal
            .restore_frozen_attachments(branch.request.op_id, branch.workspace.clone())
            .await?;
        crate::branch_instructions::apply(&branch).await?;
        if branch.point.checkpoint().is_some() {
            let baseline = Uuid::new_v5(&branch.request.op_id, b"branch-baseline");
            let reference = format!("{}-checkpoint", branch.workspace.reference);
            journal
                .intent(
                    &baseline.to_string(),
                    "branch.baseline",
                    json!({"ref":reference,"sha":branch.point.sha()}),
                )
                .await?;
            let repository = project.root.clone();
            let head = source.head;
            tokio::task::spawn_blocking(move || -> Result<()> {
                let repo = Repository::open(repository)?;
                repo.compare_and_swap(&reference, None, head)?;
                repo.sync_ref(&reference)?;
                Ok(())
            })
            .await??;
            finish(journal, baseline).await?;
        }
        verify_workspace(&branch, &project).await?;
        let binary = config.binary.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            if host::hash(&binary)? != host::BINARY_SHA256 {
                return Err("The owned Muse binary changed.".into());
            }
            Ok(())
        })
        .await??;
        release_hosts(data, &branch).await?;
        let host_dir = data
            .join("branch-hosts")
            .join(branch.request.op_id.to_string())
            .join(Uuid::now_v7().to_string());
        if *stopping.borrow() {
            return Err("Shutdown has started.".into());
        }
        let denied = vec![project.root, project.common_dir];
        let memory = host_memory
            .request(branch.request.op_id.to_string())
            .acquire(crate::msp::StartGuard::Shutdown(stopping.clone()))
            .await?;
        let mut host = host::Host::spawn(
            &config.binary,
            &host_dir,
            &branch.workspace.path,
            config.path.as_ref(),
            Some(&config.credentials),
            host::Boundary {
                steps: None,
                profile: crate::host::Profile::Worker,
                secrets: &[],
                rpc,
                denied: &denied,
            },
        )
        .await?;
        memory.attach(host.identity);
        let operation = async {
            host.connection.client.set_redaction(
                journal
                    .session_mask(&branch.request.parent_track_id, &branch.parent_session_id)
                    .await?,
            );
            if host.connection.client.initialize().await?["sessionDurability"] != "durable" {
                return Err("The branch host has no durable sessions.".into());
            }
            let operation = branch.request.op_id.to_string();
            let initial = branch.point.checkpoint().is_none();
            if initial {
                let before = host
                    .connection
                    .client
                    .request(
                        "session/read",
                        json!({"sessionId":branch.parent_session_id,"excludeItems":true}),
                    )
                    .await?;
                crate::branch_start::verify_empty(&before, &branch.parent_session_id)?;
            }
            let mut params = json!({"commandId":operation,"sessionId":branch.parent_session_id,"excludeItems":true});
            if let Some(checkpoint) = branch.point.checkpoint() {
                params["cutPoint"] = json!({"lastTurnId":checkpoint.turn_id});
            }
            journal
                .intent(&operation, "session/fork", params.clone())
                .await?;
            let response = host
                .connection
                .client
                .request("session/fork", params)
                .await?;
            let session = response["session"]["sessionId"]
                .as_str()
                .ok_or("The fork returned no child session.")?
                .to_owned();
            let provenance = &response["session"]["forkedFrom"];
            if response["session"]["permissionState"]["profileId"] != "intercat-worker"
                || provenance["sessionId"] != branch.parent_session_id
                || provenance["commandId"] != operation
                || provenance["cutExplicit"] != !initial
                || session == branch.parent_session_id
            {
                return Err(
                    "The fork returned a different session boundary or permission profile.".into(),
                );
            }
            if initial {
                crate::branch_start::verify_empty(&response, &session)?;
                let after = host
                    .connection
                    .client
                    .request(
                        "session/read",
                        json!({"sessionId":branch.parent_session_id,"excludeItems":true}),
                    )
                    .await?;
                crate::branch_start::verify_empty(&after, &branch.parent_session_id)?;
            }
            // Only metadata is retained until the inherited fingerprints are sealed.
            // In particular, do not record the inline session title or parent history.
            let leave = Uuid::now_v7().to_string();
            let accepted = host
                .connection
                .client
                .request(
                    "session/leave",
                    json!({"commandId":leave,"sessionId":session,"stop":true}),
                )
                .await?;
            if accepted["status"] != "accepted" {
                return Err("The child session could not be released.".into());
            }
            tokio::time::timeout(std::time::Duration::from_secs(15), async {
                while let Some(event) = host.connection.next_event().await {
                    if let crate::msp::Event::Frame(frame) = event {
                        let p = &frame.frame["params"];
                        if frame.frame["method"] == "session/leaveCompleted"
                            && p["commandId"] == leave
                            && p["sessionId"] == session
                        {
                            return if p["outcome"] == "released" {
                                Ok(())
                            } else {
                                Err("The child session release failed.".into())
                            };
                        }
                    } else {
                        break;
                    }
                }
                Err::<(), Box<dyn std::error::Error + Send + Sync>>(
                    "The child host closed before release.".into(),
                )
            })
            .await
            .map_err(|_| "The child session release timed out.")??;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(session)
        };
        let result = tokio::select! {
            biased;
            _=stopping.changed()=>Err("Shutdown interrupted branch preparation.".into()),
            result=operation=>result,
        };
        host.shutdown().await?;
        let session = result?;
        branch = journal
            .branch(Change::Received {
                operation: branch.request.op_id,
                session,
            })
            .await?;
    }
    finish(journal, branch.request.op_id).await?;
    Ok(json!({"branch":branch,"catalog":journal.fingerprint_catalog().await?}))
}
pub async fn activate(journal: &Journal, operation: Uuid) -> Result<Branch> {
    let snapshot = journal.snapshot().await?;
    let branch: Branch = serde_json::from_value(
        snapshot["branches"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|b| b["request"]["opId"] == operation.to_string())
            .ok_or("Unknown branch.")?
            .clone(),
    )?;
    if branch.phase == Phase::Ready {
        return Ok(branch);
    }
    let project = crate::folders::from_snapshot(&snapshot, &branch.track_id)?;
    verify_workspace(&branch, &project).await?;
    journal.branch(Change::Activate(operation)).await
}

/// A branch command owns its metadata hosts even across daemon restarts.
pub(crate) async fn release_hosts(data: &Path, branch: &Branch) -> Result<()> {
    let directory = data
        .join("branch-hosts")
        .join(branch.request.op_id.to_string());
    let workspace = branch.workspace.path.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        if !directory.try_exists()? {
            return Ok(());
        }
        let workspace = workspace.canonicalize()?;
        let mut failures = Vec::new();
        for entry in std::fs::read_dir(&directory)? {
            let attempt = (|| -> Result<()> {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    return Err("Unexpected branch host entry.".into());
                }
                let record =
                    crate::process_ledger::read(&entry.path().join(crate::process_ledger::NAME))?;
                if record.workspace != workspace {
                    return Err("The branch host workspace changed.".into());
                }
                let mut tracker = crate::process::Tracker::restore_host(&entry.path(), None)?;
                tracker.stop_confirmed()?;
                Ok(())
            })();
            if let Err(error) = attempt {
                failures.push(error.to_string());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("\n").into())
        }
    })
    .await?
}
