//! A closed worker can expose model metadata without resuming or starting a turn.
use crate::{Result, host, journal::Journal, preparation::Workspace, runtime::Config};
use std::{path::Path, sync::Arc, time::Duration};
use tokio::sync::watch;
use uuid::Uuid;
#[allow(clippy::too_many_arguments)]
pub(crate) async fn read(
    memory: Arc<crate::host_memory::Pool>,
    journal: &Journal,
    config: &Config,
    data: &Path,
    rpc: &Path,
    mut stopping: watch::Receiver<bool>,
    workspace: &Workspace,
    session: &str,
) -> Result<muse_code_sdk::msp::ModelListResult> {
    if *stopping.borrow() {
        return Err("Shutdown has started.".into());
    }
    let binary = config.binary.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        if host::hash(&binary)? != host::BINARY_SHA256 {
            return Err("The owned Muse binary changed.".into());
        }
        Ok(())
    })
    .await??;
    let operation = Uuid::now_v7();
    let memory = tokio::time::timeout(
        Duration::from_secs(10),
        memory
            .request(format!("model-options-{operation}"))
            .acquire(crate::msp::StartGuard::Shutdown(stopping.clone())),
    )
    .await
    .map_err(|_| "Model choices are waiting for memory. Try again when a host is free.")??;
    let directory = data
        .join("hosts")
        .join(&workspace.track_id)
        .join(operation.to_string());
    let folder = crate::folders::from_snapshot(&journal.snapshot().await?, &workspace.track_id)?;
    let denied = [folder.root, folder.common_dir];
    let catalog = journal.fingerprint_catalog().await?;
    if catalog.current.revision > 0 && catalog.sealed.as_ref() != Some(&catalog.current) {
        return Err("Confirm the saved protection records before reading model choices.".into());
    }
    let mask = journal.session_mask(&workspace.track_id, session).await?;
    let mut probe = host::Host::spawn(
        &config.binary,
        &directory,
        &workspace.path,
        config.path.as_ref(),
        Some(&config.credentials),
        host::Boundary {
            steps: None,
            profile: host::Profile::Worker,
            secrets: &[],
            rpc,
            denied: &denied,
        },
    )
    .await?;
    memory.attach(probe.identity);
    probe.connection.client.set_redaction(mask);
    let query = async {
        probe.connection.client.initialize().await?;
        let resumed = probe
            .connection
            .client
            .request(
                "session/resume",
                serde_json::json!({"commandId":operation,"sessionId":session,"excludeItems":true}),
            )
            .await?;
        if resumed["session"]["sessionId"] != session
            || resumed["session"]["permissionState"]["profileId"] != "intercat-worker"
        {
            return Err("The source returned a different session or permission profile.".into());
        }
        if !resumed["session"]["activeTurnId"].is_null() {
            return Err("The source session is still running outside this worker. Reconnect before choosing two ways.".into());
        }
        let value = probe
            .connection
            .client
            .request("model/list", serde_json::json!({"sessionId":session}))
            .await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(serde_json::from_value(value)?)
    };
    let result: Result<muse_code_sdk::msp::ModelListResult> = tokio::select! {
        biased;
        _=stopping.changed()=>Err("Shutdown interrupted the model query.".into()),
        result=tokio::time::timeout(Duration::from_secs(10),query)=>match result {
            Ok(value)=>value,Err(_)=>Err("Reading model choices timed out.".into()),
        }
    };
    // Metadata reads never acquire authority to stop a turn on another host.
    let released = probe.release_session(session, false).await;
    let stopped = probe.shutdown().await;
    let result = result?;
    released?;
    stopped?;
    Ok(result)
}
