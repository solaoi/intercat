//! A repair host is independently owned and registered before any MSP start.
use crate::{
    Result,
    host::{self, Host},
    journal::Journal,
    repair_run::{Event, Failure, Outcome, Phase, Run},
    runtime::Config,
};
use std::path::Path;
use uuid::Uuid;

pub struct Launch<'a> {
    pub worker: &'a Config,
    pub directory: &'a Path,
    pub rpc: &'a Path,
    pub project_secrets: crate::project_secrets::Store,
    pub one_time: Option<&'a crate::secret_input::Value>,
}

pub struct Hosted {
    host: Host,
    run: Uuid,
}

impl Hosted {
    pub(crate) fn host_mut(&mut self) -> &mut Host {
        &mut self.host
    }

    pub fn run_id(&self) -> Uuid {
        self.run
    }
    pub fn identity(&self) -> crate::process::Identity {
        self.host.identity
    }
    pub fn nonce(&self) -> String {
        self.host.nonce.clone()
    }

    /// Closing uses the Run bound at creation, never an unrelated worker ID.
    pub async fn close(self, journal: &Journal, outcome: Outcome) -> Result<Run> {
        let mut run = match journal.repair_input(self.run).await {
            Ok((run, _, _)) => run,
            Err(error) => {
                self.host.shutdown().await?;
                return Err(error);
            }
        };
        close_host(journal, &mut run, self.host, outcome).await?;
        Ok(run)
    }
}

async fn transition(journal: &Journal, run: &mut Run, event: Event) -> Result<()> {
    *run = journal
        .repair_transition(Uuid::now_v7(), run.id, run.revision, event)
        .await?;
    Ok(())
}

async fn stopping(journal: &Journal, id: Uuid) {
    let mut changes = journal.subscribe();
    loop {
        changes.borrow_and_update();
        match journal.repair_input(id).await {
            Ok((run, _, _)) if !matches!(run.phase, Phase::Preparing | Phase::Running) => return,
            Err(_) => return,
            _ => {}
        }
        if changes.changed().await.is_err() {
            return;
        }
    }
}

/// The caller has claimed Prepare and created/validated the private workspace.
/// On success the caller owns shutdown of this host and its descendants.
pub async fn open(
    journal: &Journal,
    run: &mut Run,
    workspace: &crate::repair::Workspace,
    launch: Launch<'_>,
) -> Result<Hosted> {
    let (current, job, settings) = journal.repair_input(run.id).await?;
    if current.phase == Phase::Stopping && current.host.is_none() {
        *run = current;
        transition(
            journal,
            run,
            Event::HostClosed {
                outcome: Outcome::Stopped,
            },
        )
        .await?;
        return Err("Repair preparation was stopped.".into());
    }
    if current.revision != run.revision
        || current.phase != Phase::Preparing
        || current.host.is_some()
    {
        return Err("The repair host preparation was superseded.".into());
    }
    workspace.validate(&job, &run.track, &settings)?;
    if !run.workspace_ready {
        transition(journal, run, Event::WorkspaceReady).await?;
    }
    let denied = vec![
        job.project.root,
        job.project.common_dir,
        workspace
            .original
            .parent()
            .ok_or("Missing original candidate directory.")?
            .to_path_buf(),
    ];
    let binary = launch.worker.binary.clone();
    let verified = tokio::task::spawn_blocking(move || host::hash(&binary)).await;
    if !matches!(verified, Ok(Ok(ref hash)) if hash == host::BINARY_SHA256) {
        *run = journal.repair_input(run.id).await?.0;
        if run.host.is_some() {
            return Err("Another host already owns this repair Run.".into());
        }
        let outcome = if run.phase == Phase::Stopping {
            Outcome::Stopped
        } else {
            Outcome::Failed {
                failure: Failure::Host,
            }
        };
        transition(journal, run, Event::HostClosed { outcome }).await?;
        return Err("The saved Muse binary changed. Configure workers again.".into());
    }
    *run = journal.repair_input(run.id).await?.0;
    if run.phase != Phase::Preparing || run.host.is_some() {
        if run.phase == Phase::Stopping && run.host.is_none() {
            transition(
                journal,
                run,
                Event::HostClosed {
                    outcome: Outcome::Stopped,
                },
            )
            .await?;
        }
        return Err("Repair preparation was stopped or superseded.".into());
    }
    let project = launch
        .project_secrets
        .for_host(&journal.snapshot().await?["projectSecrets"])?;
    let secret_revision = project.as_ref().map(|config| config.manifest.revision);
    let mut secrets = project
        .as_ref()
        .map_or_else(Vec::new, |config| config.values.clone());
    if let Some(value) = launch.one_time {
        secrets.push(value.clone());
    }
    let workspace_path = workspace.checkout.workdir.canonicalize()?;
    let mut host = match Host::spawn(
        &launch.worker.binary,
        launch.directory,
        &workspace.checkout.workdir,
        std::ffi::OsStr::new(&launch.worker.path),
        Some(&launch.worker.credentials),
        host::Boundary {
            profile: crate::host::Profile::Worker,
            secrets: &secrets,
            rpc: launch.rpc,
            denied: &denied,
        },
    )
    .await
    {
        Ok(host) => host,
        Err(_) => {
            // Do not put paths or host output into this durable failure code.
            *run = journal.repair_input(run.id).await?.0;
            if run.host.is_some() {
                return Err("Another host already owns this repair Run.".into());
            }
            let outcome = if run.phase == Phase::Stopping {
                Outcome::Stopped
            } else {
                Outcome::Failed {
                    failure: Failure::Host,
                }
            };
            transition(journal, run, Event::HostClosed { outcome }).await?;
            return Err("The repair host could not start.".into());
        }
    };
    let prepared = async {
        // A Stop can race spawn. Register the host even in Stopping so cleanup
        // retains its exact identity instead of leaving an unowned process.
        *run = journal.repair_input(run.id).await?.0;
        transition(journal, run, Event::HostStarted { host: host.identity, nonce: host.nonce.parse()?, secret_revision }).await?;
        if let Some(project) = &project && !project.manifest.fingerprints.is_empty() {
            journal.project_host_use(crate::project_secret_usage::HostUse {
                project_id: project.manifest.project_id.clone(), nonce: host.nonce.clone(),
                track_id: run.track.clone(), repair_run_id: Some(run.id),
                revision: project.manifest.revision, fingerprints: project.manifest.fingerprints.clone(),
                workspace: workspace_path, identity: host.identity,
            }).await?;
        }
        if run.phase != Phase::Preparing {
            return Err("Repair preparation was stopped.".into());
        }
        let previous = run.session.clone();
        let history_scanner = settings.scanner()?;
        let session = tokio::select! {
            biased;
            _ = stopping(journal, run.id) => return Err("Repair preparation was stopped.".into()),
            session = crate::runtime::open_session(&mut host.connection, journal, &workspace.checkout.workdir, previous.as_deref(), &run.track, Some(&history_scanner),crate::reconcile::Expected::default()) => session?,
        };
        if let Some(recovery)=session.recovery {
            if recovery.active_turn.is_some() || recovery.pending_approvals>0 || !recovery.prompts.is_empty() {
                return Err("The previous repair still has active work or unanswered requests. Review it before starting another turn.".into());
            }
            if recovery.resume_turn.is_some() && let Some(command)=recovery.command {journal.finish(&command.to_string(),"complete").await?;}
        }
        transition(journal, run, Event::SessionReady { session:session.id }).await?;
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    }.await;
    if let Err(error) = prepared {
        close_host(
            journal,
            run,
            host,
            Outcome::Failed {
                failure: Failure::Host,
            },
        )
        .await?;
        let detail = match settings.scanner() {
            Ok(scanner) => scanner
                .redact(&[error.to_string()], tokio::sync::watch::channel(false).1)
                .await
                .ok()
                .and_then(|mut text| text.pop()),
            Err(_) => None,
        }
        .unwrap_or_else(|| "Details withheld because the secret scan was unavailable.".into());
        return Err(
            format!("The repair host did not complete session preparation: {detail}").into(),
        );
    }
    Ok(Hosted { host, run: run.id })
}

/// A failed shutdown retains the host identity for recovery. It must never be
/// turned into an acknowledgement that the host and its children are gone.
async fn close_host(
    journal: &Journal,
    run: &mut Run,
    mut host: Host,
    outcome: Outcome,
) -> Result<()> {
    let identity = host.identity;
    let nonce: Uuid = host.nonce.parse()?;
    let (current, settings) = match journal.repair_input(run.id).await {
        Ok((run, _, settings)) => (run, settings),
        Err(error) => {
            host.shutdown().await?;
            return Err(error);
        }
    };
    let owns = current.host == Some(identity) && current.host_nonce == Some(nonce);
    let release = if owns && current.session_ready {
        let scanner = settings.scanner();
        match scanner {
            Ok(scanner) => {
                crate::runtime::release_session(
                    &mut host,
                    journal,
                    current
                        .session
                        .as_deref()
                        .ok_or("Missing repair session.")?,
                    &current.track,
                    Some(&scanner),
                )
                .await
            }
            Err(error) => Err(error),
        }
    } else {
        Ok(())
    };
    host.shutdown().await?;
    journal.project_use_closed(nonce.to_string()).await?;
    journal.secret_expired(nonce.to_string()).await?;
    *run = journal.repair_input(run.id).await?.0;
    if run.host != Some(identity) || run.host_nonce != Some(nonce) {
        return Err("The closed host does not own the current repair Run.".into());
    }
    let outcome = if release.is_err() {
        Outcome::Failed {
            failure: Failure::Protocol,
        }
    } else if run.phase == Phase::Stopping {
        Outcome::Stopped
    } else {
        outcome
    };
    transition(journal, run, Event::HostClosed { outcome }).await?;
    release.map_err(|_| "The host closed, but durable session release was not confirmed.".into())
}
