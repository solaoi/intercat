//! Ordered scan, offline hook, LFS and exact-ref send for one confirmed recipient.
use crate::{
    Result,
    checkout::{Checkout, Configuration},
    git_tool::GitTool,
    journal::Journal,
    push_auth::Authentication,
    push_connection::Connection,
    push_inspect::Inspector,
    push_job::{Job, Phase, Progress},
    push_lfs::{Binding, Endpoint, Tool},
    push_target::Destination,
    push_transfer::Snapshot,
    push_wire::Receipt,
};
use intercat_git::{RemoteAddress, Repository, Transport};
use std::{
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::watch;
#[derive(Clone)]
pub(crate) struct Driver {
    pub journal: Journal,
    pub git: GitTool,
    pub inspector: Arc<Inspector>,
    pub lfs: Arc<Tool>,
    pub root: PathBuf,
    pub path: OsString,
    pub agent: Option<PathBuf>,
}
struct Cancellation {
    flag: Arc<AtomicBool>,
    observer: tokio::task::JoinHandle<()>,
}
impl Cancellation {
    fn new(mut cancel: watch::Receiver<bool>) -> Self {
        let flag = Arc::new(AtomicBool::new(*cancel.borrow()));
        let observed = flag.clone();
        let observer = tokio::spawn(async move {
            loop {
                if *cancel.borrow() {
                    observed.store(true, Ordering::Release);
                    break;
                }
                if cancel.changed().await.is_err() {
                    break;
                }
            }
        });
        Self { flag, observer }
    }
}
impl Drop for Cancellation {
    fn drop(&mut self) {
        self.observer.abort();
    }
}
#[derive(Debug)]
struct Changed;
impl std::fmt::Display for Changed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Destination changed since you reviewed · Review")
    }
}
impl std::error::Error for Changed {}
impl Driver {
    async fn advance(
        &self,
        job: &Job,
        index: usize,
        phase: Phase,
        scans: Option<Vec<crate::secret_scan::Scan>>,
        receipt: Option<Receipt>,
    ) -> Result<Job> {
        self.journal
            .push_progress(Progress {
                id: job.id,
                recipient: index,
                attempt: job.recipients[index].attempt,
                phase,
                scans,
                receipt,
                reason: None,
            })
            .await
    }
    async fn resolve(
        &self,
        member: &crate::push_review::Member,
        cancel: watch::Receiver<bool>,
    ) -> Result<Vec<Endpoint>> {
        self.inspector
            .freeze(
                &member.folder,
                &member.target.source_ref,
                &member.target.remote,
                &member.target.target_ref,
                Some(member.target.sha.parse()?),
                cancel.clone(),
            )
            .await?;
        let urls = self
            .inspector
            .destinations(&member.folder, &member.target.remote, cancel.clone())
            .await?;
        member.target.verify_destinations(
            &member.target.remote,
            &member.target.target_ref,
            &urls,
        )?;
        let endpoints = self
            .lfs
            .resolve(
                &self.inspector,
                &member.folder,
                &member.target,
                &urls,
                cancel.clone(),
            )
            .await?;
        Binding::verify(&member.lfs, &endpoints)?;
        let mut ssh = std::collections::BTreeMap::new();
        for url in urls
            .iter()
            .chain(endpoints.iter().map(|endpoint| &endpoint.upload_url))
        {
            if RemoteAddress::parse(url)?.transport == Transport::Ssh {
                let binding = crate::push_ssh_config::Configuration::inspect(
                    &self.inspector,
                    &member.folder,
                    url,
                    cancel.clone(),
                )
                .await?
                .binding(url)?;
                ssh.insert(binding.destination.fingerprint.clone(), binding);
            }
        }
        if ssh.into_values().collect::<Vec<_>>() != member.ssh {
            return Err(Changed.into());
        }
        Ok(endpoints)
    }
    pub async fn run(&self, id: uuid::Uuid, cancel: watch::Receiver<bool>) -> Result<()> {
        let stopping = Cancellation::new(cancel.clone());
        let initial = self.journal.push_job(id).await?;
        for index in 0..initial.recipients.len() {
            let job = self.journal.push_job(id).await?;
            if job.recipients[index].phase != Phase::Queued {
                continue;
            }
            if let Err(error) = self.one(job, index, cancel.clone(), &stopping.flag).await {
                let job = self.journal.push_job(id).await?;
                let recipient = &job.recipients[index];
                if !recipient.phase.active() {
                    continue;
                }
                let cancelled = *cancel.borrow() || job.stopping;
                let phase = if recipient.phase == Phase::Sending {
                    Phase::Uncertain
                } else if cancelled {
                    Phase::Cancelled
                } else if error.is::<Changed>() {
                    Phase::NeedsReview
                } else {
                    Phase::Failed
                };
                let reason = if phase == Phase::Uncertain {
                    "The send result is unknown. Retry checks this destination before sending again."
                } else if cancelled {
                    "Push cancelled. Finished destinations are retained."
                } else {
                    "Push stopped before sending this ref. Review its scan, hook, authentication and destination settings, then Retry."
                };
                self.journal
                    .push_progress(Progress {
                        id,
                        recipient: index,
                        attempt: recipient.attempt,
                        phase,
                        scans: None,
                        receipt: None,
                        reason: Some(reason),
                    })
                    .await?;
            }
        }
        Ok(())
    }
    async fn one(
        &self,
        job: Job,
        index: usize,
        cancel: watch::Receiver<bool>,
        stopped: &AtomicBool,
    ) -> Result<()> {
        let job = self
            .advance(&job, index, Phase::Preparing, None, None)
            .await?;
        let recipient = &job.recipients[index];
        let member = job
            .review
            .members
            .iter()
            .find(|member| member.folder.id == recipient.folder_id)
            .ok_or("Missing Push folder.")?;
        let endpoints = match self.resolve(member, cancel.clone()).await {
            Ok(endpoints) => endpoints,
            Err(_) => {
                let phase = if *cancel.borrow() {
                    Phase::Cancelled
                } else {
                    Phase::NeedsReview
                };
                self.journal.push_progress(Progress{id:job.id,recipient:index,attempt:recipient.attempt,phase,scans:None,receipt:None,reason:Some("Destination settings could not be matched to this confirmation. Open a new Push review.")}).await?;
                return Ok(());
            }
        };
        let endpoint = endpoints
            .iter()
            .find(|endpoint| {
                Destination::capture(&endpoint.git_url)
                    .is_ok_and(|destination| destination == recipient.destination)
            })
            .ok_or("Missing confirmed Push destination.")?;
        let job_root = self.root.join(job.id.to_string());
        fs::create_dir_all(&job_root)?;
        fs::set_permissions(&job_root, fs::Permissions::from_mode(0o700))?;
        let snapshot = Snapshot::prepare(
            &job.review,
            &member.folder.id,
            &job_root.join(&member.folder.id),
            stopped,
        )?;
        let connection = Connection::open(
            &snapshot,
            self.git.clone(),
            &self.inspector,
            &endpoint.git_url,
            &self.path,
            self.agent.as_deref(),
            cancel.clone(),
        )
        .await?;
        let advertised = connection.advertise(cancel.clone()).await?;
        let remote_head = advertised
            .iter()
            .find(|row| row.reference == member.target.target_ref)
            .map(|row| row.sha);
        if remote_head == Some(member.target.sha.parse()?) {
            self.advance(
                &job,
                index,
                Phase::Done,
                None,
                Some(Receipt {
                    sha: member.target.sha.clone(),
                    reference: member.target.target_ref.clone(),
                    destination: recipient.destination.clone(),
                    changed: false,
                }),
            )
            .await?;
            return Ok(());
        }
        let repo = Repository::open(&snapshot.git_directory)?;
        let tips = advertised.iter().map(|row| row.sha).collect::<Vec<_>>();
        let range = repo.push_commits(member.target.sha.parse()?, &tips, stopped)?;
        let source = intercat_git::WorktreeSource {
            root: member.folder.root.clone(),
            common_dir: member.folder.common_dir.clone(),
            head: member.target.sha.parse()?,
        };
        let mut configuration = Configuration::capture(&source)?;
        let payloads = snapshot.copy_lfs_range(&range, &configuration.lfs_directory, stopped)?;
        let environment =
            crate::check_environment::Frozen::capture(self.journal.check_environment().await?)?;
        if let Some(path) = &environment.selection.file {
            crate::check_environment::validate_file(path, &member.folder)?;
        }
        let values = environment.values()?;
        let scanner = crate::secret_scan::Engine::capture()?
            .start(&values.values().cloned().collect::<Vec<_>>())?;
        let approvals = self
            .journal
            .push_approvals(job.review.session_id.clone(), member.folder.id.clone())
            .await?;
        let job = self
            .advance(&job, index, Phase::Scanning, None, None)
            .await?;
        let mut scans = Vec::new();
        let mut approved = true;
        for commit in range {
            let scan = scanner
                .push_commit(
                    &repo,
                    commit,
                    &snapshot.git_directory.join("lfs"),
                    cancel.clone(),
                )
                .await?;
            approved &= approvals.iter().any(|proof| proof.accepts(&scan));
            scans.push(scan);
        }
        if !approved {
            self.journal.push_progress(Progress{id:job.id,recipient:index,attempt:job.recipients[index].attempt,phase:Phase::Failed,scans:Some(scans),receipt:None,reason:Some("Unchecked commit in the Push range. Complete its Commit review with the current secret-scan settings before sending.")}).await?;
            return Ok(());
        }
        let job = self
            .advance(&job, index, Phase::Hooks, Some(scans), None)
            .await?;
        configuration.denied_roots.push(snapshot.root.clone());
        let hook_parent = tempfile::Builder::new()
            .prefix("hooks-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(&job_root)?;
        let path = self.path.to_str().ok_or("Push needs a UTF-8 tool path.")?;
        let toolchain = crate::checkout::toolchain_paths(path, &source.root)?;
        let checkout = Checkout::create(
            &hook_parent.path().join("checkout"),
            source,
            member.target.sha.parse()?,
            configuration,
            path.into(),
            toolchain,
            cancel.clone(),
        )
        .await?;
        if let Some(output) = crate::push_hook::run(
            &checkout,
            &snapshot,
            &endpoint.git_url,
            remote_head,
            cancel.clone(),
        )
        .await?
            && (!output.success() || output.truncated)
        {
            return Err("The offline pre-push hook did not finish successfully.".into());
        }
        let job = self.advance(&job, index, Phase::Lfs, None, None).await?;
        self.resolve(member, cancel.clone())
            .await
            .map_err(|_| Box::new(Changed) as Box<dyn std::error::Error + Send + Sync>)?;
        environment.values()?;
        if !payloads.entries().is_empty() {
            if RemoteAddress::parse(&endpoint.api_url)?.transport == Transport::File {
                self.lfs
                    .push_file(&snapshot, endpoint, &payloads, cancel.clone())
                    .await?;
            } else {
                let auth = Authentication::capture(
                    &self.inspector,
                    &member.folder,
                    &self.git,
                    &endpoint.api_url,
                    &self.path,
                    self.agent.as_deref(),
                    cancel.clone(),
                )
                .await?;
                self.lfs
                    .push_network(&snapshot, endpoint, &payloads, auth, cancel.clone())
                    .await?;
            }
        }
        self.resolve(member, cancel.clone())
            .await
            .map_err(|_| Box::new(Changed) as Box<dyn std::error::Error + Send + Sync>)?;
        environment.values()?;
        let job = self
            .advance(&job, index, Phase::Sending, None, None)
            .await?;
        let receipt = connection.send(cancel).await?;
        self.advance(&job, index, Phase::Done, None, Some(receipt))
            .await?;
        Ok(())
    }
}
