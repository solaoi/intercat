//! Serial, cancellable Checks of frozen branch points, independent of Commit.
use crate::{
    Result,
    candidate::Settings,
    check_run,
    checkout::Checkout,
    comparison::{Action, Control, DiffRequest, Job, Open, Outcome, Phase},
    comparison_store::Change,
    journal::Journal,
};
use intercat_git::Repository;
use std::{
    collections::BTreeMap,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
};
use tokio::sync::{Mutex, watch};
use uuid::Uuid;
#[derive(Clone)]
pub struct Service {
    journal: Journal,
    root: PathBuf,
    queue: Arc<Mutex<()>>,
    running: Arc<Mutex<BTreeMap<Uuid, Running>>>,
}
struct Running {
    cancel: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        self.task.abort();
    }
}
impl Service {
    pub async fn new(journal: Journal, root: PathBuf) -> Result<Self> {
        journal.recover_comparisons().await?;
        Ok(Self {
            journal,
            root,
            queue: Arc::new(Mutex::new(())),
            running: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }
    pub async fn open(&self, request: Open) -> Result<serde_json::Value> {
        request.validate()?;
        let mut running = self.running.lock().await;
        running.retain(|_, task| !task.task.is_finished());
        let job = if let Some(job) = self.journal.comparison_job(request.op_id).await? {
            if job.request != request {
                return Err("Comparison operation ID was reused.".into());
            }
            job
        } else {
            let capture = self.journal.capture_comparison(request).await?;
            let input = capture.clone();
            let mut settings = tokio::task::spawn_blocking(move || {
                Settings::capture(&input.folder, input.environment, input.checks)
            })
            .await??;
            settings
                .checkout
                .denied_roots
                .extend(capture.denied_roots.iter().cloned());
            settings.checkout.denied_roots.sort();
            settings.checkout.denied_roots.dedup();
            let folder = capture.folder.clone();
            let slug = capture.slug.clone();
            let session = capture.session.clone();
            let base = capture.base.clone();
            let sources = capture.sources.clone();
            let id = capture.request.op_id;
            // Object/ref writes precede SQLite intent. These owned immutable refs
            // are safe to repeat, and never move any worker's ref or index.
            let (trees, changes) = tokio::task::spawn_blocking(move || -> Result<_> {
                let repo = Repository::open(&folder.root)?;
                if !repo.owns_session(&slug, &session)? {
                    return Err("This session’s Git reservation changed.".into());
                }
                let mut trees = [String::new(), String::new()];
                let mut changes = [Vec::new(), Vec::new()];
                let base_sha = base.sha().parse()?;
                let base_tree = repo.tree_id(base_sha)?;
                for (name, sha, point_ref) in [
                    ("base", base.sha(), base.reference()),
                    (
                        "parent",
                        &sources[0].checkpoint.sha,
                        &sources[0].checkpoint.reference,
                    ),
                    (
                        "child",
                        &sources[1].checkpoint.sha,
                        &sources[1].checkpoint.reference,
                    ),
                ] {
                    let sha = sha.parse()?;
                    if repo.reference_target(point_ref)? != Some(sha) {
                        return Err("The protected comparison checkpoint changed.".into());
                    }
                    repo.sync_objects(&[sha])?;
                    let reference = format!("refs/intercat/{slug}/comparisons/{id}/{name}");
                    repo.compare_and_swap(&reference, None, sha)?;
                    repo.sync_ref(&reference)?;
                }
                for (index, source) in sources.iter().enumerate() {
                    let tree = repo.tree_id(source.point.sha.parse()?)?;
                    trees[index] = tree.to_string();
                    changes[index] = crate::integration::changes(&repo, base_tree, tree)?;
                }
                Ok((trees, changes))
            })
            .await??;
            self.journal
                .change_comparison(Change::Install {
                    capture: Box::new(capture),
                    settings: Box::new(settings),
                    trees,
                    changes,
                })
                .await?
        };
        self.start(&mut running, &job);
        job.project()
    }
    fn start(&self, running: &mut BTreeMap<Uuid, Running>, job: &Job) {
        if job.phase != Phase::Ready || running.contains_key(&job.id) {
            return;
        }
        let journal = self.journal.clone();
        let root = self.root.clone();
        let queue = self.queue.clone();
        let id = job.id;
        let revision = job.revision;
        let (cancel, mut stopping) = watch::channel(false);
        let task = tokio::spawn(async move {
            let slot = tokio::select! {slot=queue.lock()=>slot,_=stopping.changed()=>return};
            if *stopping.borrow() {
                return;
            }
            if let Ok(job) = journal
                .change_comparison(Change::Begin { id, revision })
                .await
            {
                let attempt = job.attempt;
                if run(&journal, &root, job, stopping).await.is_err() {
                    let _ = journal
                        .change_comparison(Change::Interrupted { id, attempt })
                        .await;
                }
            }
            drop(slot);
        });
        running.insert(id, Running { cancel, task });
    }
    pub async fn control(&self, request: Control) -> Result<serde_json::Value> {
        let mut running = self.running.lock().await;
        running.retain(|_, task| !task.task.is_finished());
        let stopping = request.action == Action::Hold;
        let job = self.journal.control_comparison(request).await?;
        if stopping {
            if job.phase == Phase::Held
                && let Some(mut task) = running.remove(&job.id)
            {
                let _ = task.cancel.send(true);
                let _ = (&mut task.task).await;
            }
        } else if job.phase == Phase::Ready {
            if let Some(mut task) = running.remove(&job.id) {
                let _ = (&mut task.task).await;
            }
            self.start(&mut running, &job);
        }
        self.read(job.id).await
    }
    pub async fn read(&self, id: Uuid) -> Result<serde_json::Value> {
        self.journal
            .comparison_job(id)
            .await?
            .ok_or("Unknown comparison.")?
            .project()
    }
    pub async fn evidence(&self, id: Uuid) -> Result<crate::comparison_evidence::Evidence> {
        let job = self
            .journal
            .comparison_job(id)
            .await?
            .ok_or("Unknown comparison.")?;
        let settings = self.journal.comparison_settings(id).await?;
        tokio::task::spawn_blocking(move || {
            crate::comparison_evidence::read(&job, &settings.command)
        })
        .await?
    }
    pub async fn diff(&self, request: DiffRequest) -> Result<crate::integration::Diff> {
        let job = self
            .journal
            .comparison_job(request.id)
            .await?
            .ok_or("Unknown comparison.")?;
        let input = job.clone();
        let path = request.path.clone();
        let mut diff = tokio::task::spawn_blocking(move || -> Result<crate::integration::Diff> {
            let changes = match request.basis {
                crate::comparison::DiffBasis::Parent => input.sides[0].changes.clone(),
                crate::comparison::DiffBasis::Child => input.sides[1].changes.clone(),
                crate::comparison::DiffBasis::Paths => {
                    let repo = Repository::open(&input.folder.root)?;
                    crate::integration::changes(
                        &repo,
                        input.sides[0].tree.parse()?,
                        input.sides[1].tree.parse()?,
                    )?
                }
            };
            let change = changes
                .into_iter()
                .find(|change| change.path == path)
                .ok_or("This file is not in the saved paths' diff.")?;
            crate::integration::read_diff(input.folder.root, change)
        })
        .await??;
        let settings = self.journal.comparison_settings(job.id).await?;
        let scanner = settings.scanner()?;
        let safe = scanner
            .redact(
                &[diff.before.clone(), diff.after.clone()],
                watch::channel(false).1,
            )
            .await?;
        diff.redacted = safe[0] != diff.before || safe[1] != diff.after;
        diff.before = safe[0].clone();
        diff.after = safe[1].clone();
        Ok(diff)
    }
    pub async fn shutdown(&self) {
        let mut running = self.running.lock().await;
        for task in running.values() {
            let _ = task.cancel.send(true);
        }
        for (id, mut task) in std::mem::take(&mut *running) {
            let _ = (&mut task.task).await;
            if let Ok(Some(job)) = self.journal.comparison_job(id).await
                && matches!(job.phase, Phase::Running | Phase::Ready)
            {
                let _ = self
                    .journal
                    .control_comparison(Control {
                        op_id: Uuid::now_v7(),
                        id,
                        revision: job.revision,
                        attempt: job.attempt,
                        action: Action::Hold,
                    })
                    .await;
            }
        }
    }
}
async fn run(
    journal: &Journal,
    root: &std::path::Path,
    mut job: Job,
    stopping: watch::Receiver<bool>,
) -> Result<()> {
    let settings = journal.comparison_settings(job.id).await?;
    for side in 0..2 {
        if job.phase != Phase::Running {
            return Ok(());
        }
        if job.sides[side].outcome.is_some() {
            continue;
        }
        if *stopping.borrow() {
            journal
                .change_comparison(Change::Interrupted {
                    id: job.id,
                    attempt: job.attempt,
                })
                .await?;
            return Ok(());
        }
        let location = root
            .join(job.id.to_string())
            .join(format!("attempt-{}-{side}", job.attempt));
        let input = job.clone();
        let captured = settings.clone();
        let cancel = stopping.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current()
                .block_on(check(&input, side, &captured, &location, cancel))
        })
        .await?;
        job = journal
            .change_comparison(Change::Finish {
                id: job.id,
                attempt: job.attempt,
                side,
                outcome,
            })
            .await?;
    }
    Ok(())
}
async fn check(
    job: &Job,
    side: usize,
    settings: &Settings,
    root: &std::path::Path,
    cancel: watch::Receiver<bool>,
) -> Outcome {
    let mut scanner = None;
    let result = async {
        scanner = Some(settings.scanner()?);
        let parent = root.parent().ok_or("Missing comparison directory.")?;
        std::fs::create_dir_all(parent)?;
        let metadata = std::fs::symlink_metadata(parent)?;
        if !metadata.is_dir()
            || metadata.file_type().is_symlink()
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err("The comparison directory is not private.".into());
        }
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        let head = job.sides[side].source.point.sha.parse()?;
        let checkout = Checkout::create(
            root,
            job.folder.source().await?,
            head,
            settings.checkout.clone(),
            settings.path.clone().into(),
            settings.toolchain.clone(),
            cancel.clone(),
        )
        .await?;
        check_run::execute(
            checkout,
            check_run::Snapshot {
                head,
                tree: job.sides[side].tree.parse()?,
                settings_hash: &job.sides[side].settings_hash,
            },
            &root
                .parent()
                .ok_or("Missing comparison directory.")?
                .join("downloads"),
            check_run::Settings {
                command: &settings.command,
                history: settings.history,
                npm: settings.npm.clone(),
                registry_origins: &settings.registry_origins,
                timeout_ms: settings.timeout_ms,
            },
            &settings.environment_values()?,
            cancel,
            check_run::Deployment::RestoreSnapshot,
        )
        .await
    }
    .await;
    let mut outcome = match result {
        Ok(report) => Outcome {
            state: report.state,
            check: report.check,
            reason: report.reason,
            dependency_runs: report.dependency_runs,
            dependency_error: report.dependency_error,
        },
        Err(error) => Outcome {
            state: check_run::State::EnvFailed,
            check: None,
            reason: Some(error.to_string()),
            dependency_runs: vec![],
            dependency_error: None,
        },
    };
    let mut fields: Vec<&mut String> = Vec::new();
    if let Some(reason) = &mut outcome.reason {
        fields.push(reason);
    }
    if let Some(check) = &mut outcome.check {
        if let Some(command) = &mut check.command {
            fields.push(command);
        }
        fields.push(&mut check.stdout);
        fields.push(&mut check.stderr);
    }
    for run in &mut outcome.dependency_runs {
        fields.push(&mut run.stdout);
        fields.push(&mut run.stderr);
    }
    if let Some(error) = &mut outcome.dependency_error {
        fields.push(&mut error.message);
        if let Some(output) = &mut error.output {
            fields.push(output);
        }
    }
    let texts = fields
        .iter()
        .map(|text| (**text).clone())
        .collect::<Vec<_>>();
    let safe = if let Some(scanner) = scanner {
        scanner.redact(&texts, watch::channel(false).1).await.ok()
    } else {
        None
    };
    if let Some(safe) = safe {
        for (field, text) in fields.into_iter().zip(safe) {
            *field = text;
        }
    } else {
        for field in fields {
            *field = "[output withheld: secret scan unavailable]".into();
        }
        outcome.state = check_run::State::EnvFailed;
        outcome.reason = Some("Output secret scan did not complete.".into());
    }
    outcome
}
