//! Build the actual per-track commits and check their final tree in isolation.
//! Publishing refs is a separate operation; this module never moves a ref in
//! the original repository or changes a worker checkout.
use crate::{
    Result,
    check_run::{CheckCommand, Record},
    checkout::{Checkout, Configuration},
    integration::{Job, RowState},
};
use intercat_git::{Merge, ObjectId, Repository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};
use tokio::sync::watch;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub environment: Option<crate::check_environment::Frozen>,
    pub scanner: crate::secret_scan::Engine,
    pub command: CheckCommand,
    pub checks_revision: u64,
    pub history: bool,
    pub npm: Option<crate::dependencies::NpmTool>,
    pub registry_origins: Vec<String>,
    pub timeout_ms: u64,
    pub path: String,
    pub toolchain: Vec<PathBuf>,
    pub checkout: Configuration,
    pub skip_hooks: bool,
}
impl Settings {
    pub fn hash(&self) -> Result<String> {
        Ok(
            Sha256::digest(serde_json::to_vec(&(self, crate::secret_scan::ruleset()))?)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        )
    }
    pub fn capture(
        project: &crate::project::Folder,
        environment: crate::check_environment::Selection,
        checks: crate::check_settings::Setting,
    ) -> Result<Self> {
        let choice = checks.folder(&project.id);
        if let Some(file) = &environment.file {
            crate::check_environment::validate_file(file, project)?;
        }
        let source = Repository::worktree_source(&project.root, Some(project.base.parse()?))?;
        let path = std::env::var("PATH")?;
        let mut toolchain = crate::checkout::toolchain_paths(&path, &source.root)?;
        let npm = crate::dependencies::NpmTool::discover(&path)?;
        if let Some(npm) = &npm
            && let Some(root) = npm.cli.parent().and_then(Path::parent)
        {
            toolchain.push(root.to_path_buf());
        }
        toolchain.sort();
        toolchain.dedup();
        Ok(Self {
            environment: Some(crate::check_environment::Frozen::capture(environment)?),
            scanner: crate::secret_scan::Engine::capture()?,
            command: choice.command,
            checks_revision: choice.revision,
            history: choice.history,
            npm,
            registry_origins: vec!["https://registry.npmjs.org/".into()],
            timeout_ms: 300_000,
            path,
            toolchain,
            checkout: Configuration::capture(&source)?,
            skip_hooks: false,
        })
    }
    pub fn environment_values(&self) -> Result<BTreeMap<String, String>> {
        self.environment
            .as_ref()
            .map(crate::check_environment::Frozen::values)
            .transpose()
            .map(Option::unwrap_or_default)
    }
    pub fn scanner(&self) -> Result<crate::secret_scan::Scanner> {
        self.scanner
            .start(&self.environment_values()?.into_values().collect::<Vec<_>>())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Rechecking,
    Checked,
    Conflict,
    EnvFailed,
    HooksOutside,
    HookRejected,
    DirtyBeforeCheck,
    HistoryRequired,
    CheckFailed,
    CheckMutated,
    Cancelled,
    SecretFound,
    RepairChanged,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Commit {
    pub track: String,
    pub sha: String,
    pub parent: String,
    pub tree: String,
    pub message: String,
    pub author: String,
    pub changes: Vec<crate::integration::Change>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub job_id: uuid::Uuid,
    pub revision: u64,
    pub state: State,
    pub head: String,
    pub tree: String,
    pub settings_hash: String,
    pub commits: Vec<Commit>,
    pub undo: Option<crate::replacement_undo::Commit>,
    pub undo_conflict: Option<crate::replacement_undo::Conflict>,
    pub check: Option<Record>,
    pub reason: Option<String>,
    pub failed_track: Option<String>,
    pub left_out: Vec<String>,
    pub hooks_skipped: bool,
    pub dependency_runs: Vec<crate::dependencies::Run>,
    pub dependency_error: Option<crate::dependencies::Failure>,
    pub secret_scans: Vec<crate::secret_scan::Scan>,
    pub build_revision: u64,
    pub workspace: PathBuf,
    pub blocked_commit: Option<Commit>,
    pub repair: Option<crate::repair::Plan>,
    pub resolution: Option<crate::merge_resolution::Plan>,
    pub conflict: Option<crate::merge_resolution::Conflict>,
    pub contributions: BTreeMap<String, Contribution>,
    pub fixes: BTreeMap<String, std::collections::BTreeSet<String>>,
}

/// A frozen per-track delta after a repair. Objects remain in the private
/// candidate repository, including any tree still blocked by the secret scan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Contribution {
    pub base: String,
    pub tree: String,
    pub source: PathBuf,
}
impl Candidate {
    pub fn scans(&self) -> impl DoubleEndedIterator<Item = &crate::secret_scan::Scan> {
        self.undo
            .iter()
            .map(|undo| &undo.scan)
            .chain(self.secret_scans.iter())
    }
    pub fn pending_undo(&self) -> bool {
        self.commits.is_empty() && self.undo.as_ref().is_some_and(|undo| undo.sha != self.head)
    }
    fn fail(mut self, state: State, reason: impl Into<String>, track: Option<&str>) -> Self {
        self.state = state;
        self.reason = Some(reason.into());
        self.failed_track = track.map(str::to_owned);
        self
    }
}

fn edit_boundary_changed(candidate: &Candidate, track: Option<&str>) -> bool {
    candidate.repair.as_ref().is_some_and(|repair| {
        repair
            .steps
            .last()
            .is_some_and(|step| Some(step.original.track.as_str()) == track)
            && candidate.tree != repair.tree
    }) || candidate.resolution.as_ref().is_some_and(|resolution| {
        Some(resolution.basis.track.as_str()) == track && candidate.tree != resolution.tree
    })
}

pub async fn build(
    job: &Job,
    root: &Path,
    cache_root: &Path,
    settings: &Settings,
    messages: &BTreeMap<String, String>,
    cancel: watch::Receiver<bool>,
) -> Result<Candidate> {
    build_from(
        job,
        root,
        cache_root,
        settings,
        Inputs {
            messages,
            edit: None,
        },
        cancel,
    )
    .await
}

pub(crate) struct RepairInput<'a> {
    pub plan: &'a crate::repair::Plan,
    pub repository: &'a Path,
    pub original: &'a Path,
}

pub(crate) struct MergeInput<'a> {
    pub plan: &'a crate::merge_resolution::Plan,
    pub repository: &'a Path,
    pub original: &'a Path,
}
enum Edit<'a> {
    Fix(RepairInput<'a>),
    Merge(MergeInput<'a>),
}
struct Inputs<'a> {
    messages: &'a BTreeMap<String, String>,
    edit: Option<Edit<'a>>,
}

pub(crate) async fn build_repaired(
    job: &Job,
    root: &Path,
    cache_root: &Path,
    settings: &Settings,
    repair: RepairInput<'_>,
    cancel: watch::Receiver<bool>,
) -> Result<Candidate> {
    build_from(
        job,
        root,
        cache_root,
        settings,
        Inputs {
            messages: &job.messages,
            edit: Some(Edit::Fix(repair)),
        },
        cancel,
    )
    .await
}

pub(crate) async fn build_resolved(
    job: &Job,
    root: &Path,
    cache_root: &Path,
    settings: &Settings,
    resolution: MergeInput<'_>,
    cancel: watch::Receiver<bool>,
) -> Result<Candidate> {
    build_from(
        job,
        root,
        cache_root,
        settings,
        Inputs {
            messages: &job.messages,
            edit: Some(Edit::Merge(resolution)),
        },
        cancel,
    )
    .await
}

async fn build_from(
    job: &Job,
    root: &Path,
    cache_root: &Path,
    settings: &Settings,
    inputs: Inputs<'_>,
    cancel: watch::Receiver<bool>,
) -> Result<Candidate> {
    let environment = settings.environment_values()?;
    let scanner = settings
        .scanner
        .start(&environment.values().cloned().collect::<Vec<_>>())?;
    let result = if job
        .candidate
        .as_ref()
        .is_some_and(|saved| saved.state == State::Rechecking)
    {
        recheck(job, cache_root, settings, cancel, &scanner, &environment).await
    } else {
        build_inner(
            job,
            root,
            cache_root,
            settings,
            inputs,
            cancel,
            &scanner,
            &environment,
        )
        .await
    };
    // Error messages can contain hook output as well as ordinary Check output.
    // Only the redacted representation is allowed into a result/event.
    match result {
        Ok(mut candidate) => {
            let mut fields: Vec<&mut String> = Vec::new();
            if let Some(reason) = &mut candidate.reason {
                fields.push(reason);
            }
            if let Some(undo) = &mut candidate.undo {
                fields.push(&mut undo.message);
            }
            for commit in &mut candidate.commits {
                fields.push(&mut commit.message);
            }
            if let Some(commit) = &mut candidate.blocked_commit {
                fields.push(&mut commit.message);
            }
            if let Some(check) = &mut candidate.check {
                if let Some(command) = &mut check.command {
                    fields.push(command);
                }
                fields.push(&mut check.stdout);
                fields.push(&mut check.stderr);
            }
            for run in &mut candidate.dependency_runs {
                fields.push(&mut run.stdout);
                fields.push(&mut run.stderr);
            }
            if let Some(error) = &mut candidate.dependency_error {
                fields.push(&mut error.message);
                if let Some(output) = &mut error.output {
                    fields.push(output);
                }
            }
            let texts = fields
                .iter()
                .map(|text| (**text).clone())
                .collect::<Vec<_>>();
            match scanner.redact(&texts, watch::channel(false).1).await {
                Ok(texts) => {
                    for (field, text) in fields.into_iter().zip(texts) {
                        *field = text;
                    }
                }
                Err(_) => {
                    for field in fields {
                        *field = "[output withheld: secret scan unavailable]".into();
                    }
                    candidate.state = State::EnvFailed;
                    candidate.reason = Some("Output secret scan did not complete.".into());
                }
            }
            Ok(candidate)
        }
        Err(error) => {
            let safe = scanner
                .redact(&[error.to_string()], watch::channel(false).1)
                .await
                .ok()
                .and_then(|mut texts| texts.pop())
                .unwrap_or_else(|| {
                    "Candidate failed; output withheld because the secret scan did not complete."
                        .into()
                });
            Err(safe.into())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn build_inner(
    job: &Job,
    root: &Path,
    cache_root: &Path,
    settings: &Settings,
    inputs: Inputs<'_>,
    cancel: watch::Receiver<bool>,
    scanner: &crate::secret_scan::Scanner,
    environment: &BTreeMap<String, String>,
) -> Result<Candidate> {
    let plan = job.plan.as_ref().ok_or("No frozen integration plan.")?;
    if let Some(tree) = &plan.tree {
        let _: ObjectId = tree.parse()?;
    } else if job.undo.is_none() && !plan.rows.iter().any(|row| row.state == RowState::Conflict) {
        return Err("The integration plan has no tree or conflict to build.".into());
    }
    let repository = Repository::open(&job.project.root)?;
    let hash = settings.hash()?;
    let retain_contributions = inputs.edit.is_some() || !job.contributions.is_empty();
    let mut candidate = Candidate {
        job_id: job.id,
        revision: job.revision,
        state: State::Checked,
        head: plan.target_head.clone(),
        tree: repository.tree_id(plan.target_head.parse()?)?.to_string(),
        settings_hash: hash,
        commits: vec![],
        undo: None,
        undo_conflict: None,
        check: None,
        reason: None,
        failed_track: None,
        left_out: vec![],
        hooks_skipped: settings.skip_hooks,
        dependency_runs: Vec::new(),
        dependency_error: None,
        secret_scans: Vec::new(),
        build_revision: job.revision,
        workspace: root.to_path_buf(),
        blocked_commit: None,
        repair: None,
        resolution: None,
        conflict: None,
        contributions: job.contributions.clone(),
        fixes: job.fixes.clone(),
    };
    if cache_root.starts_with(&job.project.root) {
        return Err("The dependency cache must be outside the source checkout.".into());
    }
    let cache =
        crate::dependencies::Cache::new(cache_root.to_path_buf(), &settings.registry_origins)?;
    let mut dependencies = crate::dependencies::Preparer::new(cache, settings.npm.clone());
    let source = job.project.source().await?;
    let mut start_at = 0;
    let checkout = if let Some(saved) = job.candidate.as_ref().filter(|candidate| {
        (candidate.blocked_commit.is_some() || candidate.pending_undo()) && inputs.edit.is_none()
    }) {
        if saved.state != State::SecretFound
            || saved.settings_hash != candidate.settings_hash
            || saved.job_id != job.id
        {
            return Err("The suspended candidate changed.".into());
        }
        candidate = saved.clone();
        candidate.revision = job.revision;
        let pending = candidate.blocked_commit.take();
        let (sha, tree, scan) = if let Some(pending) = &pending {
            (
                pending.sha.clone(),
                pending.tree.clone(),
                candidate
                    .secret_scans
                    .last()
                    .ok_or("Missing suspended secret scan.")?,
            )
        } else {
            let undo = candidate.undo.as_ref().ok_or("Missing suspended undo.")?;
            (undo.sha.clone(), undo.tree.clone(), &undo.scan)
        };
        if scan.binding.commit != sha || !scan.accepted(&job.kept_secrets) {
            return Err("The secret findings still need a decision.".into());
        }
        let checkout = Checkout::resume(
            root,
            source.clone(),
            settings.checkout.clone(),
            OsString::from(&settings.path),
            settings.toolchain.clone(),
        )
        .await?;
        let head = checkout
            .git(&["rev-parse".into(), "HEAD".into()], false, cancel.clone())
            .await?
            .require_success("Read suspended candidate")?;
        if String::from_utf8(head.stdout)?.trim() != sha {
            return Err("The suspended checkout changed. Refresh to rebuild.".into());
        }
        let actual = Repository::open(&checkout.workdir)?.commit_details(sha.parse()?)?;
        if actual.parents != vec![candidate.head.parse::<ObjectId>()?] || actual.tree != tree {
            return Err("The suspended candidate no longer matches its scan.".into());
        }
        if pending.is_none() {
            candidate
                .undo
                .as_ref()
                .ok_or("Missing suspended undo.")?
                .verify(
                    &Repository::open(&checkout.workdir)?,
                    job.undo.as_ref().ok_or("Missing frozen undo plan.")?,
                    settings,
                )?;
        }
        checkout.import_commit(sha.parse()?)?;
        candidate.head = sha;
        candidate.tree = tree;
        if let Some(pending) = pending {
            start_at = job
                .tracks
                .iter()
                .position(|track| track.id == pending.track)
                .ok_or("Missing suspended track.")?
                + 1;
            candidate.commits.push(pending);
        }
        candidate.state = State::Checked;
        candidate.reason = None;
        candidate.failed_track = None;
        checkout
    } else {
        let checkout = match Checkout::create(
            root,
            source.clone(),
            plan.target_head.parse()?,
            settings.checkout.clone(),
            OsString::from(&settings.path),
            settings.toolchain.clone(),
            cancel.clone(),
        )
        .await
        {
            Ok(checkout) => checkout,
            Err(error) => {
                return Ok(candidate.fail(
                    State::EnvFailed,
                    crate::checkout::failure(error.as_ref()),
                    None,
                ));
            }
        };
        if settings.history
            && let Err(error) = checkout
                .set_history(true, plan.target_head.parse()?, cancel.clone())
                .await
        {
            return Ok(candidate.fail(
                State::EnvFailed,
                crate::checkout::failure(error.as_ref()),
                None,
            ));
        }
        if let Err(error) = checkout
            .deploy(
                repository.tree_id(plan.target_head.parse()?)?,
                cancel.clone(),
            )
            .await
        {
            return Ok(candidate.fail(
                State::EnvFailed,
                crate::checkout::failure(error.as_ref()),
                None,
            ));
        }
        if let Some(undo_plan) = &job.undo {
            if inputs.edit.is_some() {
                let saved = job
                    .candidate
                    .as_ref()
                    .ok_or("Missing candidate undo prefix.")?;
                let undo = saved.undo.as_ref().ok_or("Missing saved undo prefix.")?;
                let private = Repository::open(&checkout.workdir)?;
                private.import_commit(
                    &Repository::open(saved.workspace.join("repo"))?,
                    undo.sha.parse()?,
                )?;
                // The saved candidate's commit chain and LFS store are copied,
                // never rebuilt by running its hooks again during a repair.
                crate::lfs_store::transfer(
                    &saved.workspace.join("repo/.git/lfs"),
                    &checkout.workdir.join(".git/lfs"),
                    &private.lfs_entries(undo.tree.parse()?)?,
                )?;
                undo.verify(&private, undo_plan, settings)?;
                candidate.undo = Some(undo.clone());
                candidate.head = undo.sha.clone();
                candidate.tree = undo.tree.clone();
            } else {
                match crate::replacement_undo::build(
                    job,
                    &checkout,
                    undo_plan,
                    settings,
                    &mut dependencies,
                    scanner,
                    cancel.clone(),
                )
                .await?
                {
                    crate::replacement_undo::Outcome::Built(built) => {
                        candidate.dependency_runs.extend(built.dependency_runs);
                        candidate.left_out.extend(built.left_out);
                        let review = built.commit.scan.review_reason(
                            &job.kept_secrets,
                            "Secret found in the replacement undo diff or final message.",
                        );
                        candidate.undo = Some(built.commit);
                        if let Some(reason) = review {
                            return Ok(candidate.fail(State::SecretFound, reason, None));
                        }
                        let undo = candidate.undo.as_ref().unwrap();
                        checkout.import_commit(undo.sha.parse()?)?;
                        candidate.head = undo.sha.clone();
                        candidate.tree = undo.tree.clone();
                    }
                    crate::replacement_undo::Outcome::Conflict(conflict) => {
                        candidate.undo_conflict = Some(conflict);
                        return Ok(candidate.fail(
                            State::Conflict,
                            "The replacement undo conflicts with a later edit.",
                            None,
                        ));
                    }
                    crate::replacement_undo::Outcome::Rejected(rejected) => {
                        candidate.dependency_error = rejected.dependency_error;
                        candidate.left_out.extend(rejected.left_out);
                        return Ok(candidate.fail(rejected.state, rejected.reason, None));
                    }
                }
            }
        }
        if let Some(Edit::Fix(repair)) = &inputs.edit {
            repair.plan.validate(job, settings)?;
            let private = Repository::open(&checkout.workdir)?;
            for step in &repair.plan.steps {
                let tree = step.tree.parse()?;
                private.import_tree(repair.repository, tree)?;
                crate::lfs_store::transfer(
                    &repair.repository.join(".git/lfs"),
                    &checkout.workdir.join(".git/lfs"),
                    &private.lfs_entries(tree)?,
                )?;
            }
            let mut base = candidate.tree.clone();
            for step in &repair.plan.steps {
                candidate.contributions.insert(
                    step.original.track.clone(),
                    Contribution {
                        base,
                        tree: step.tree.clone(),
                        source: checkout.workdir.clone(),
                    },
                );
                base = step.tree.clone();
            }
            let original = Repository::open(repair.original)?;
            let prefix = repair
                .plan
                .steps
                .iter()
                .position(|step| step.original.track == repair.plan.applied_track)
                .ok_or("Missing repair attribution.")?;
            let saved = job.candidate.as_ref().ok_or("Missing failed candidate.")?;
            for commit in saved.commits.iter().take(prefix) {
                private.import_commit(&original, commit.sha.parse()?)?;
                candidate.head = commit.sha.clone();
                candidate.tree = commit.tree.clone();
                candidate.commits.push(commit.clone());
            }
            candidate.secret_scans = saved.secret_scans.iter().take(prefix).cloned().collect();
            candidate.left_out = repair.plan.left_out.clone();
            candidate
                .fixes
                .entry(repair.plan.applied_track.clone())
                .or_default()
                .extend(
                    crate::integration::changes(
                        &Repository::open(repair.repository)?,
                        repair.plan.basis.tree.parse()?,
                        repair.plan.tree.parse()?,
                    )?
                    .into_iter()
                    .map(|change| change.path),
                );
            candidate.repair = Some(repair.plan.clone());
            start_at = job
                .tracks
                .iter()
                .position(|track| track.id == repair.plan.applied_track)
                .ok_or("Missing repair owner.")?;
            checkout
                .git(
                    &[
                        "checkout".into(),
                        "--quiet".into(),
                        "--detach".into(),
                        candidate.head.clone().into(),
                    ],
                    true,
                    cancel.clone(),
                )
                .await?
                .require_success("Restore unchanged candidate prefix")?;
        } else if let Some(Edit::Merge(resolution)) = &inputs.edit {
            resolution.plan.validate(job, settings)?;
            let basis = &resolution.plan.basis;
            let saved = job
                .candidate
                .as_ref()
                .ok_or("Missing conflicted candidate.")?;
            let private = Repository::open(&checkout.workdir)?;
            let original = Repository::open(resolution.original)?;
            for tree in [basis.tree.parse()?, resolution.plan.tree.parse()?] {
                private.import_tree(resolution.repository, tree)?;
                crate::lfs_store::transfer(
                    &resolution.repository.join(".git/lfs"),
                    &checkout.workdir.join(".git/lfs"),
                    &private.lfs_entries(tree)?,
                )?;
            }
            for commit in &saved.commits {
                private.import_commit(&original, commit.sha.parse()?)?;
                crate::lfs_store::transfer(
                    &resolution.original.join(".git/lfs"),
                    &checkout.workdir.join(".git/lfs"),
                    &private.lfs_entries(commit.tree.parse()?)?,
                )?;
                candidate.head = commit.sha.clone();
                candidate.tree = commit.tree.clone();
                candidate.commits.push(commit.clone());
            }
            candidate.secret_scans = saved.secret_scans.clone();
            candidate.contributions.insert(
                basis.track.clone(),
                Contribution {
                    base: basis.tree.clone(),
                    tree: resolution.plan.tree.clone(),
                    source: checkout.workdir.clone(),
                },
            );
            candidate.left_out = resolution.plan.left_out.clone();
            candidate
                .fixes
                .entry(basis.track.clone())
                .or_default()
                .extend(basis.conflicts.iter().cloned());
            candidate.resolution = Some(resolution.plan.clone());
            start_at = job
                .tracks
                .iter()
                .position(|track| track.id == basis.track)
                .ok_or("Missing merge owner.")?;
            checkout
                .git(
                    &[
                        "checkout".into(),
                        "--quiet".into(),
                        "--detach".into(),
                        candidate.head.clone().into(),
                    ],
                    true,
                    cancel.clone(),
                )
                .await?
                .require_success("Restore unchanged conflict prefix")?;
        }
        checkout
    };
    if edit_boundary_changed(
        &candidate,
        start_at
            .checked_sub(1)
            .and_then(|i| job.tracks.get(i))
            .map(|t| t.id.as_str()),
    ) {
        return Ok(candidate.fail(
            State::RepairChanged,
            "A commit hook changed the repaired tree. Review the new candidate.",
            None,
        ));
    }
    let messages = crate::integration_assumptions::messages(job, inputs.messages);
    for track in job.tracks.iter().skip(start_at).filter(|t| t.included) {
        if *cancel.borrow() {
            return Ok(candidate.fail(State::Cancelled, "Candidate build cancelled.", None));
        }
        let Some(checkpoint) = &track.checkpoint else {
            continue;
        };
        let row = plan
            .rows
            .iter()
            .find(|row| row.track == track.id)
            .ok_or("Missing track base.")?;
        if !checkpoint.unmerged_paths.is_empty() {
            candidate.conflict = Some(crate::merge_resolution::Conflict::SavedIndex {
                paths: checkpoint.unmerged_paths.clone(),
            });
            return Ok(candidate.fail(
                State::Conflict,
                "Resolve the saved checkpoint's conflicts first.",
                Some(&track.id),
            ));
        }
        let current_tree = candidate.tree.parse()?;
        let repair_step = candidate.repair.as_ref().and_then(|repair| {
            repair
                .steps
                .iter()
                .position(|step| step.original.track == track.id)
                .map(|index| (repair, index))
        });
        let merge_inputs;
        let merged = if let Some((repair, index)) = repair_step {
            let base = if index == 0 {
                candidate
                    .undo
                    .as_ref()
                    .map(|undo| undo.tree.parse())
                    .transpose()?
                    .unwrap_or(repository.tree_id(plan.target_head.parse()?)?)
            } else {
                repair.steps[index - 1].tree.parse()?
            };
            merge_inputs = (base, repair.steps[index].tree.parse()?, None);
            // A newly executed hook can change the previous commit. Replay
            // the next delta onto that actual tree instead of overwriting it
            // with a precomputed snapshot and accidentally hiding the change.
            Repository::open(&checkout.workdir)?.merge_trees(
                base,
                current_tree,
                repair.steps[index].tree.parse()?,
            )?
        } else if let Some(contribution) = candidate.contributions.get(&track.id) {
            let private = Repository::open(&checkout.workdir)?;
            let base = contribution.base.parse()?;
            let tree = contribution.tree.parse()?;
            private.import_tree(&contribution.source, base)?;
            private.import_tree(&contribution.source, tree)?;
            for input in [base, tree] {
                crate::lfs_store::transfer(
                    &contribution.source.join(".git/lfs"),
                    &checkout.workdir.join(".git/lfs"),
                    &private.lfs_entries(input)?,
                )?;
            }
            merge_inputs = (base, tree, None);
            private.merge_trees(base, current_tree, tree)?
        } else {
            let base = if let Some(base) = &row.delta_base {
                base.parse()?
            } else {
                let mut integrated = plan
                    .coverage
                    .clone()
                    .ok_or("Missing frozen published coverage.")?;
                if let Some(undo) = &job.undo {
                    crate::replacement_delta::rewind(&undo.records, &mut integrated);
                }
                for previous in job
                    .tracks
                    .iter()
                    .take_while(|item| item.id != track.id)
                    .filter(|item| item.included)
                {
                    if let Some(point) = &previous.checkpoint {
                        job.lineage.incorporate(
                            &mut integrated,
                            job.lineage.by_sha(&previous.id, &point.sha)?,
                        )?;
                    }
                }
                match crate::integration_baseline::calculate(
                    job,
                    track,
                    &integrated,
                    &mut crate::merge_choices::Merges::new(&repository, &[])?,
                )? {
                    Merge::Clean(base) => base,
                    Merge::Conflicts { paths, .. } => {
                        candidate.conflict = Some(crate::merge_resolution::Conflict::Baseline {
                            paths: paths.into_iter().map(|path| path.to_string()).collect(),
                        });
                        return Ok(candidate.fail(
                            State::Conflict,
                            "The inherited baseline has a conflict.",
                            Some(&track.id),
                        ));
                    }
                }
            };
            let incoming = repository.tree_id(checkpoint.sha.parse()?)?;
            merge_inputs = (base, incoming, Some(&job.project.root));
            repository.merge_trees(base, current_tree, incoming)?
        };
        let tree = match merged {
            Merge::Clean(tree) => tree,
            Merge::Conflicts { paths, .. } => {
                let (base, incoming, source) = merge_inputs;
                let private = Repository::open(&checkout.workdir)?;
                for tree in [base, incoming] {
                    if let Some(source) = source {
                        private.import_tree(source, tree)?;
                    }
                    crate::lfs_store::transfer(
                        &settings.checkout.lfs_directory,
                        &checkout.workdir.join(".git/lfs"),
                        &private.lfs_entries(tree)?,
                    )?;
                }
                private.sync_objects(&[base, incoming])?;
                candidate.conflict = Some(crate::merge_resolution::Conflict::Overlay {
                    base: base.to_string(),
                    incoming: incoming.to_string(),
                    paths: paths.iter().map(|path| path.to_string()).collect(),
                });
                return Ok(candidate.fail(
                    State::Conflict,
                    format!(
                        "Integration conflict in {}",
                        paths
                            .iter()
                            .map(|p| String::from_utf8_lossy(p).into_owned())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    Some(&track.id),
                ));
            }
        };
        if tree == current_tree {
            if edit_boundary_changed(&candidate, Some(&track.id)) {
                return Ok(candidate.fail(
                    State::RepairChanged,
                    "The rebuilt commits differ from the repaired tree.",
                    None,
                ));
            }
            continue;
        }
        if let Err(error) = checkout.deploy(tree, cancel.clone()).await {
            return Ok(candidate.fail(
                State::EnvFailed,
                crate::checkout::failure(error.as_ref()),
                Some(&track.id),
            ));
        }
        if retain_contributions {
            candidate.contributions.insert(
                track.id.clone(),
                Contribution {
                    base: current_tree.to_string(),
                    tree: tree.to_string(),
                    source: checkout.workdir.clone(),
                },
            );
        }
        match dependencies
            .prepare(
                &checkout,
                &Repository::open(&checkout.workdir)?,
                tree,
                false,
                cancel.clone(),
            )
            .await
        {
            Ok(runs) => candidate.dependency_runs.extend(runs),
            Err(error) => {
                let state = if error.kind == crate::dependencies::FailureKind::Cancelled {
                    State::Cancelled
                } else {
                    State::EnvFailed
                };
                let message = error.message.clone();
                candidate.dependency_error = Some(error);
                return Ok(candidate.fail(state, message, Some(&track.id)));
            }
        }
        let address = track
            .last_clip
            .map(|clip| format!("{}:{clip}", track.label))
            .unwrap_or_else(|| format!("{}@{}", track.label, track.checkpoint_count));
        let body = messages.get(&track.id).unwrap_or(&track.title);
        let message = format!(
            "{}\n\nIntercat: {} · {}/{}\n",
            body.trim_end(),
            job.session_name,
            job.slug,
            address
        );
        let author = format!(
            "Worker {} <intercat+{}-{}@localhost>",
            track.label, job.slug, track.label
        );
        let made = match crate::candidate_commit::create(
            &checkout,
            settings,
            crate::candidate_commit::Request {
                tree,
                parent: candidate.head.parse()?,
                attachment_base: plan.target_head.parse()?,
                author: &author,
                message: &message,
                allow_empty: false,
            },
            scanner,
            cancel.clone(),
        )
        .await?
        {
            crate::candidate_commit::Outcome::Made(made) => made,
            crate::candidate_commit::Outcome::Rejected(rejected) => {
                candidate.left_out.extend(rejected.left_out);
                return Ok(candidate.fail(rejected.state, rejected.reason, Some(&track.id)));
            }
        };
        candidate.left_out.extend(made.left_out);
        let sha = made.sha;
        let details = made.details;
        let scan = made.scan;
        let review = scan.review_reason(
            &job.kept_secrets,
            "Secret found in the diff or final commit message.",
        );
        candidate.secret_scans.push(scan);
        let commit = Commit {
            track: track.id.clone(),
            sha: sha.to_string(),
            parent: candidate.head.clone(),
            tree: details.tree.to_string(),
            message: details.message,
            author: details.author_name,
            changes: crate::integration::changes(
                &Repository::open(&checkout.workdir)?,
                current_tree,
                details.tree,
            )?,
        };
        if retain_contributions {
            candidate.contributions.insert(
                track.id.clone(),
                Contribution {
                    base: current_tree.to_string(),
                    tree: details.tree.to_string(),
                    source: checkout.workdir.clone(),
                },
            );
        }
        if let Some(reason) = review {
            candidate.blocked_commit = Some(commit);
            return Ok(candidate.fail(State::SecretFound, reason, Some(&track.id)));
        }
        checkout.import_commit(sha)?;
        candidate.commits.push(commit);
        candidate.head = sha.to_string();
        candidate.tree = details.tree.to_string();
        if edit_boundary_changed(&candidate, Some(&track.id)) {
            return Ok(candidate.fail(
                State::RepairChanged,
                "A commit hook changed the repaired tree. Review the new candidate.",
                None,
            ));
        }
    }
    check_final(
        candidate,
        checkout,
        cache_root,
        settings,
        cancel,
        environment,
        crate::check_run::Deployment::KeepPrepared,
    )
    .await
}

async fn recheck(
    job: &Job,
    cache_root: &Path,
    settings: &Settings,
    cancel: watch::Receiver<bool>,
    scanner: &crate::secret_scan::Scanner,
    environment: &BTreeMap<String, String>,
) -> Result<Candidate> {
    let mut candidate = job
        .candidate
        .clone()
        .ok_or("Missing candidate to re-check.")?;
    if candidate.job_id != job.id
        || candidate.revision != job.revision
        || candidate.blocked_commit.is_some()
        || candidate.pending_undo()
    {
        return Err("The candidate changed before re-checking.".into());
    }
    let checkout = Checkout::resume(
        &candidate.workspace,
        job.project.source().await?,
        settings.checkout.clone(),
        OsString::from(&settings.path),
        settings.toolchain.clone(),
    )
    .await?;
    let repo = Repository::open(&checkout.workdir)?;
    let details = repo.commit_details(candidate.head.parse()?)?;
    if details.tree != candidate.tree {
        return Err("The candidate tree changed before re-checking.".into());
    }
    candidate.settings_hash = settings.hash()?;
    candidate.state = State::Checked;
    candidate.check = None;
    candidate.reason = None;
    candidate.failed_track = None;
    candidate.dependency_runs.clear();
    candidate.dependency_error = None;
    candidate.secret_scans.clear();
    if let Some(undo) = &mut candidate.undo {
        undo.scan = scanner
            .commit(
                &repo,
                undo.parent.parse()?,
                undo.sha.parse()?,
                &checkout.workdir.join(".git/lfs"),
                cancel.clone(),
            )
            .await?;
        if let Some(reason) = undo.scan.review_reason(
            &job.kept_secrets,
            "Secret found in the replacement undo with the current Check settings.",
        ) {
            return Ok(candidate.fail(State::SecretFound, reason, None));
        }
    }
    for commit in &candidate.commits {
        let scan = scanner
            .commit(
                &repo,
                commit.parent.parse()?,
                commit.sha.parse()?,
                &checkout.workdir.join(".git/lfs"),
                cancel.clone(),
            )
            .await?;
        let review = scan.review_reason(
            &job.kept_secrets,
            "Secret found with the current Check settings.",
        );
        candidate.secret_scans.push(scan);
        if let Some(reason) = review {
            let track = commit.track.clone();
            return Ok(candidate.fail(State::SecretFound, reason, Some(&track)));
        }
    }
    check_final(
        candidate,
        checkout,
        cache_root,
        settings,
        cancel,
        environment,
        crate::check_run::Deployment::RestoreSnapshot,
    )
    .await
}

async fn check_final(
    mut candidate: Candidate,
    checkout: Checkout,
    cache_root: &Path,
    settings: &Settings,
    cancel: watch::Receiver<bool>,
    environment: &BTreeMap<String, String>,
    deployment: crate::check_run::Deployment,
) -> Result<Candidate> {
    use crate::check_run;
    let report = check_run::execute(
        checkout,
        check_run::Snapshot {
            head: candidate.head.parse()?,
            tree: candidate.tree.parse()?,
            settings_hash: &candidate.settings_hash,
        },
        cache_root,
        check_run::Settings {
            command: &settings.command,
            history: settings.history,
            npm: settings.npm.clone(),
            registry_origins: &settings.registry_origins,
            timeout_ms: settings.timeout_ms,
        },
        environment,
        cancel,
        deployment,
    )
    .await?;
    candidate.workspace = report.workspace;
    candidate.check = report.check;
    candidate.dependency_runs.extend(report.dependency_runs);
    candidate.dependency_error = report.dependency_error;
    let state = match report.state {
        check_run::State::Passed => None,
        check_run::State::EnvFailed => Some(State::EnvFailed),
        check_run::State::DirtyBeforeCheck => Some(State::DirtyBeforeCheck),
        check_run::State::Cancelled => Some(State::Cancelled),
        check_run::State::HistoryRequired => Some(State::HistoryRequired),
        check_run::State::Failed => Some(State::CheckFailed),
        check_run::State::Mutated => Some(State::CheckMutated),
    };
    if let Some(state) = state {
        let reason = if state == State::CheckFailed {
            "The merged Check failed.".to_owned()
        } else {
            report.reason.ok_or("The failed Check has no reason.")?
        };
        return Ok(candidate.fail(state, reason, None));
    }
    candidate.left_out.sort();
    candidate.left_out.dedup();
    Ok(candidate)
}
