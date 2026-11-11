use crate::{
    Result, host,
    journal::Journal,
    pickup::{Change, Phase, Recovery},
    runtime::Config,
};
use intercat_git::Repository;
use serde_json::{Value, json};
use std::path::Path;
use uuid::Uuid;

#[derive(Clone, Default)]
pub(crate) struct Runs(
    std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<Uuid, tokio::sync::watch::Sender<bool>>>,
    >,
);
pub(crate) struct Active {
    runs: Runs,
    id: Uuid,
    pub stopping: tokio::sync::watch::Receiver<bool>,
}
impl Runs {
    pub fn begin(&self, id: Uuid) -> Result<Active> {
        let mut runs = self.0.lock().unwrap();
        if runs.contains_key(&id) {
            return Err("This file recovery is already running.".into());
        }
        let (signal, stopping) = tokio::sync::watch::channel(false);
        runs.insert(id, signal);
        Ok(Active {
            runs: self.clone(),
            id,
            stopping,
        })
    }
    pub fn cancel(&self, id: Uuid) {
        if let Some(signal) = self.0.lock().unwrap().get(&id) {
            let _ = signal.send(true);
        }
    }
    pub fn shutdown(&self) {
        for signal in self.0.lock().unwrap().values() {
            let _ = signal.send(true);
        }
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        self.runs.0.lock().unwrap().remove(&self.id);
    }
}
fn check(stopping: &tokio::sync::watch::Receiver<bool>) -> Result<()> {
    if *stopping.borrow() {
        return Err("File recovery was interrupted. Files were kept.".into());
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn prepare(
    host_memory: std::sync::Arc<crate::host_memory::Pool>,
    journal: &Journal,
    config: Config,
    data: &Path,
    rpc: &Path,
    mut recovery: Recovery,
    capacity: std::sync::Arc<crate::scheduler::Capacity>,
    mut stopping: tokio::sync::watch::Receiver<bool>,
) -> Result<Value> {
    check(&stopping)?;
    if matches!(recovery.phase, Phase::Cancelling | Phase::Cancelled) {
        return Err("This file recovery was cancelled. Files were kept.".into());
    }
    if matches!(
        recovery.phase,
        Phase::Capturing | Phase::Publishing | Phase::Summarizing | Phase::Starting
    ) {
        journal
            .restore_frozen_attachments(recovery.request.op_id, recovery.workspace.clone())
            .await?;
        check(&stopping)?;
    }
    if recovery.phase == Phase::Capturing {
        let parent = recovery
            .previous
            .checkpoint
            .as_ref()
            .map_or(&recovery.workspace.base, |c| &c.sha);
        let captured = crate::checkpoint::capture(
            &recovery.workspace,
            parent,
            "Intercat file recovery",
            journal,
        )
        .await?;
        let checkpoint = crate::checkpoint::Checkpoint {
            turn_id: Uuid::new_v5(&recovery.request.op_id, b"recovery-snapshot"),
            sha: captured.sha,
            reference: format!(
                "{}-recovery/{}",
                recovery.workspace.reference, recovery.request.op_id
            ),
            unmerged_paths: captured.unmerged_paths,
            live_processes: false,
        };
        check(&stopping)?;
        recovery = journal
            .pickup(Change::Captured(recovery.request.op_id, checkpoint))
            .await?;
    }
    if recovery.phase == Phase::Publishing {
        check(&stopping)?;
        let path = recovery.workspace.path.clone();
        let checkpoint = recovery
            .snapshot
            .clone()
            .ok_or("Missing recovery snapshot.")?;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let repository = Repository::open(path)?;
            let sha = checkpoint.sha.parse()?;
            match repository.reference_target(&checkpoint.reference)? {
                None => repository.compare_and_swap(&checkpoint.reference, None, sha)?,
                Some(current) if current == sha => {}
                Some(_) => return Err("The recovery snapshot reference changed.".into()),
            }
            repository.sync_ref(&checkpoint.reference)?;
            Ok(())
        })
        .await??;
        check(&stopping)?;
        recovery = journal
            .pickup(Change::Published(recovery.request.op_id))
            .await?;
    }
    if recovery.phase == Phase::Summarizing {
        check(&stopping)?;
        let mut source = recovery.clone();
        let path = source.workspace.path.clone();
        let previous = source
            .previous
            .checkpoint
            .as_ref()
            .map_or(&source.workspace.base, |c| &c.sha)
            .clone();
        let snapshot = source
            .snapshot
            .clone()
            .ok_or("Missing recovery snapshot.")?;
        let changes=tokio::task::spawn_blocking(move||->Result<Value>{
            let repo=Repository::open(path)?;
            let before=repo.saved_files(previous.parse()?)?;let after=repo.saved_files(snapshot.sha.parse()?)?;
            let paths=before.keys().chain(after.keys()).cloned().collect::<std::collections::BTreeSet<_>>();
            let mut changes=Vec::new();
            for path in paths {
                let kind=match (before.get(&path),after.get(&path)) {
                    (None,Some(_))=>"added",(Some(_),None)=>"deleted",
                    (Some(a),Some(b)) if a.object!=b.object || a.mode!=b.mode=>"modified",_=>continue,
                };
                changes.push(json!({"path":path,"kind":kind}));
            }
            Ok(json!({"address":snapshot.reference,"changes":changes,"unmergedPaths":snapshot.unmerged_paths}))
        }).await??;
        source.context["snapshotChanges"] = changes;
        let summary = crate::handoff_run::run(
            host_memory.clone(),
            journal,
            &config,
            data,
            rpc,
            &crate::handoff::Source {
                id: source.request.op_id,
                identity: crate::handoff::Identity {
                    track_id: source.request.track_id.clone(),
                    session_id: source.request.session_id.clone(),
                },
                context: source.context,
            },
            capacity,
            crate::msp::StartGuard::Shutdown(stopping.clone()),
            stopping.clone(),
        )
        .await?;
        check(&stopping)?;
        recovery = journal
            .pickup(Change::Summarized(recovery.request.op_id, summary))
            .await?;
    }
    if recovery.phase == Phase::Starting {
        check(&stopping)?;
        let binary = config.binary.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            if host::hash(&binary)? != host::BINARY_SHA256 {
                return Err("The saved Muse binary changed.".into());
            }
            Ok(())
        })
        .await??;
        check(&stopping)?;
        let memory = host_memory
            .request(recovery.request.op_id.to_string())
            .acquire(crate::msp::StartGuard::Shutdown(stopping.clone()))
            .await?;
        let mut host = host::Host::spawn(
            &config.binary,
            &data
                .join("hosts")
                .join(&recovery.request.track_id)
                .join(Uuid::now_v7().to_string()),
            &recovery.workspace.path,
            config.path.as_ref(),
            Some(&config.credentials),
            host::Boundary {
                steps: None,
                profile: crate::host::Profile::Worker,
                secrets: &[],
                rpc,
                denied: &[],
            },
        )
        .await?;
        memory.attach(host.identity);
        let result=async {
            check(&stopping)?;
            tokio::select! { biased;
            _ = stopping.wait_for(|value| *value) => Err("File recovery was interrupted. Files were kept.".into()),
            result = async {
            host.connection.client.set_redaction(journal.session_mask(&recovery.request.track_id,&recovery.request.session_id).await?);
            if host.connection.client.initialize().await?["sessionDurability"]!="durable" {return Err("File recovery needs durable sessions.".into());}
            let params=json!({"commandId":recovery.start_command,"sessionId":recovery.session_id,"workspaceRoot":recovery.workspace.path,"approvalMode":"promptUnmatched","providerId":"meta","modelId":"muse-spark-1.3-contributor","recoveryMode":"on"});
            journal.intent(&recovery.start_command.to_string(),"session/start",params.clone()).await?;
            let opened=host.connection.client.request("session/start",params).await?;
            if opened["session"]["sessionId"]!=recovery.session_id || opened["session"]["permissionState"]["profileId"]!="intercat-worker" {return Err("The recovery host returned a different session or permission profile.".into());}
            host.release_session(&recovery.session_id, true).await?;
            Ok::<_,Box<dyn std::error::Error+Send+Sync>>(())
            } => result,
            }
        }.await;
        host.shutdown().await?;
        result?;
        check(&stopping)?;
        let operation = journal
            .operation(recovery.start_command)
            .await?
            .ok_or("Missing session-start intent.")?;
        if operation["state"] == "prepared" {
            journal
                .finish(&recovery.start_command.to_string(), "dispatched")
                .await?;
        }
        if operation["state"] != "complete" {
            journal
                .finish(&recovery.start_command.to_string(), "complete")
                .await?;
        }
        recovery = journal
            .pickup(Change::Received(recovery.request.op_id))
            .await?;
    }
    Ok(json!({"recovery":recovery,"catalog":journal.fingerprint_catalog().await?}))
}

pub async fn verify_snapshot(recovery: &Recovery) -> Result<()> {
    let workspace = recovery.workspace.clone();
    let saved = recovery
        .snapshot
        .clone()
        .ok_or("Missing recovery snapshot.")?;
    let previous = recovery.previous.checkpoint.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let repository = Repository::open(&workspace.path)?;
        if repository.reference_target(&saved.reference)? != Some(saved.sha.parse()?) {
            return Err("The saved recovery snapshot is unavailable.".into());
        }
        if let Some(previous) = previous
            && repository.reference_target(&previous.reference)? != Some(previous.sha.parse()?)
        {
            return Err("The last branchable checkpoint changed during recovery.".into());
        }
        Ok(())
    })
    .await?
}
