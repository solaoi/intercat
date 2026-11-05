//! Build the actual per-track commits and check their final tree in isolation.
//! Publishing refs is a separate operation; this module never moves a ref in
//! the original repository or changes a worker checkout.
use crate::{
    Result,
    checkout::{Checkout, Configuration},
    command::Output,
    hooks,
    integration::{Job, RowState},
};
use intercat_git::{Merge, ObjectId, Repository};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "mode", content = "command", rename_all = "camelCase")]
pub enum CheckCommand {
    Auto,
    Disabled,
    Run(String),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub scanner: crate::secret_scan::Engine,
    pub command: CheckCommand,
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
    pub fn capture(job: &Job) -> Result<Self> {
        let source =
            Repository::worktree_source(&job.project.root, Some(job.project.base.parse()?))?;
        let path = std::env::var("PATH")?;
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut toolchain = Vec::new();
        for directory in
            std::env::split_paths(&path).filter(|dir| dir.is_absolute() && dir.is_dir())
        {
            let directory = directory.canonicalize()?;
            if !source.root.starts_with(&directory)
                && !home
                    .as_ref()
                    .is_some_and(|home| home.starts_with(&directory))
            {
                toolchain.push(directory);
            }
        }
        if let Some(node) = std::env::split_paths(&path)
            .map(|dir| dir.join("node"))
            .find(|path| path.is_file())
        {
            let node = node.canonicalize()?;
            if let Some(root) = node.parent().and_then(Path::parent) {
                toolchain.push(root.to_path_buf());
            }
        }
        let npm = crate::dependencies::NpmTool::discover(&path)?;
        if let Some(npm) = &npm
            && let Some(root) = npm.cli.parent().and_then(Path::parent)
        {
            toolchain.push(root.to_path_buf());
        }
        toolchain.sort();
        toolchain.dedup();
        Ok(Self {
            scanner: crate::secret_scan::Engine::capture()?,
            command: CheckCommand::Auto,
            npm,
            registry_origins: vec!["https://registry.npmjs.org/".into()],
            timeout_ms: 300_000,
            path,
            toolchain,
            checkout: Configuration::capture(&source)?,
            skip_hooks: false,
        })
    }
}

fn check_command(
    repo: &Repository,
    tree: ObjectId,
    choice: &CheckCommand,
) -> Result<Option<String>> {
    match choice {
        CheckCommand::Disabled => Ok(None),
        CheckCommand::Run(command) => Ok(Some(command.clone())),
        CheckCommand::Auto => {
            let files = repo.tree_files(tree)?;
            if let Some(file) = files.get("package.json") {
                let manifest: serde_json::Value = serde_json::from_slice(&repo.saved_bytes(file)?)?;
                if manifest["scripts"]["test"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
                {
                    return Ok(Some("npm test".into()));
                }
            }
            Ok(None)
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum State {
    Checked,
    Conflict,
    EnvFailed,
    HooksOutside,
    HookRejected,
    DirtyBeforeCheck,
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
pub struct Check {
    pub tree: String,
    pub settings_hash: String,
    pub command: Option<String>,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
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
    pub check: Option<Check>,
    pub reason: Option<String>,
    pub failed_track: Option<String>,
    pub left_out: Vec<String>,
    pub hooks_skipped: bool,
    pub dependency_runs: Vec<crate::dependencies::Run>,
    pub dependency_error: Option<crate::dependencies::Failure>,
    pub secret_scans: Vec<crate::secret_scan::Scan>,
    pub generation: u64,
    pub blocked_commit: Option<Commit>,
    pub repair: Option<crate::repair::Plan>,
    pub contributions: BTreeMap<String, Contribution>,
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
    fn fail(mut self, state: State, reason: impl Into<String>, track: Option<&str>) -> Self {
        self.state = state;
        self.reason = Some(reason.into());
        self.failed_track = track.map(str::to_owned);
        self
    }
}

fn output_record(tree: &str, hash: &str, command: Option<String>, output: Output) -> Check {
    Check {
        tree: tree.into(),
        settings_hash: hash.into(),
        command,
        code: output.code,
        stdout: output.stdout_text(),
        stderr: output.stderr_text(),
        truncated: output.truncated,
    }
}

fn repair_boundary_changed(candidate: &Candidate, track: Option<&str>) -> bool {
    candidate.repair.as_ref().is_some_and(|repair| {
        repair
            .steps
            .last()
            .is_some_and(|step| Some(step.original.track.as_str()) == track)
            && candidate.tree != repair.tree
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
            repair: None,
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

struct Inputs<'a> {
    messages: &'a BTreeMap<String, String>,
    repair: Option<RepairInput<'a>>,
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
            repair: Some(repair),
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
    let scanner = settings.scanner.start(&[])?;
    let result = build_inner(job, root, cache_root, settings, inputs, cancel, &scanner).await;
    // Error messages can contain hook output as well as ordinary Check output.
    // Only the redacted representation is allowed into a result/event.
    match result {
        Ok(mut candidate) => {
            let mut fields: Vec<&mut String> = Vec::new();
            if let Some(reason) = &mut candidate.reason {
                fields.push(reason);
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

async fn build_inner(
    job: &Job,
    root: &Path,
    cache_root: &Path,
    settings: &Settings,
    inputs: Inputs<'_>,
    cancel: watch::Receiver<bool>,
    scanner: &crate::secret_scan::Scanner,
) -> Result<Candidate> {
    let plan = job.plan.as_ref().ok_or("No frozen integration plan.")?;
    let _merged_tree: ObjectId = plan
        .tree
        .as_ref()
        .ok_or("Resolve the integration conflict first.")?
        .parse()?;
    let repository = Repository::open(&job.project.root)?;
    let hash = settings.hash()?;
    let retain_contributions = inputs.repair.is_some() || !job.contributions.is_empty();
    let mut candidate = Candidate {
        job_id: job.id,
        revision: job.revision,
        state: State::Checked,
        head: plan.target_head.clone(),
        tree: repository.tree_id(plan.target_head.parse()?)?.to_string(),
        settings_hash: hash,
        commits: vec![],
        check: None,
        reason: None,
        failed_track: None,
        left_out: vec![],
        hooks_skipped: settings.skip_hooks,
        dependency_runs: Vec::new(),
        dependency_error: None,
        secret_scans: Vec::new(),
        generation: job.revision,
        blocked_commit: None,
        repair: None,
        contributions: job.contributions.clone(),
    };
    if cache_root.starts_with(&job.project.root) {
        return Err("The dependency cache must be outside the source checkout.".into());
    }
    let cache =
        crate::dependencies::Cache::new(cache_root.to_path_buf(), &settings.registry_origins)?;
    let mut dependencies = crate::dependencies::Preparer::new(cache, settings.npm.clone());
    let source = job.project.source().await?;
    let mut start_at = 0;
    let checkout = if let Some(saved) = job
        .candidate
        .as_ref()
        .filter(|candidate| candidate.blocked_commit.is_some() && inputs.repair.is_none())
    {
        if saved.state != State::SecretFound
            || saved.settings_hash != candidate.settings_hash
            || saved.job_id != job.id
        {
            return Err("The suspended candidate changed.".into());
        }
        candidate = saved.clone();
        candidate.revision = job.revision;
        let pending = candidate.blocked_commit.take().unwrap();
        let scan = candidate
            .secret_scans
            .last()
            .ok_or("Missing suspended secret scan.")?;
        if scan.binding.commit != pending.sha || !scan.accepted(&job.kept_secrets) {
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
        if String::from_utf8(head.stdout)?.trim() != pending.sha {
            return Err("The suspended checkout changed. Refresh to rebuild.".into());
        }
        let actual = Repository::open(&checkout.workdir)?.commit_details(pending.sha.parse()?)?;
        if actual.parents != vec![candidate.head.parse::<ObjectId>()?]
            || actual.tree != pending.tree
        {
            return Err("The suspended candidate no longer matches its scan.".into());
        }
        start_at = job
            .tracks
            .iter()
            .position(|track| track.id == pending.track)
            .ok_or("Missing suspended track.")?
            + 1;
        checkout.import_commit(pending.sha.parse()?)?;
        candidate.head = pending.sha.clone();
        candidate.tree = pending.tree.clone();
        candidate.commits.push(pending);
        candidate.state = State::Checked;
        candidate.reason = None;
        candidate.failed_track = None;
        checkout
    } else {
        let checkout = Checkout::create(
            root,
            source.clone(),
            plan.target_head.parse()?,
            settings.checkout.clone(),
            OsString::from(&settings.path),
            settings.toolchain.clone(),
            cancel.clone(),
        )
        .await?;
        checkout
            .deploy(
                repository.tree_id(plan.target_head.parse()?)?,
                cancel.clone(),
            )
            .await?;
        if let Some(repair) = &inputs.repair {
            repair.plan.validate(job, settings)?;
            let private = Repository::open(&checkout.workdir)?;
            for step in &repair.plan.steps {
                private.import_tree(repair.repository, step.tree.parse()?)?;
            }
            let mut base = private.tree_id(plan.target_head.parse()?)?.to_string();
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
        }
        checkout
    };
    if repair_boundary_changed(
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
        if row.state == RowState::Conflict {
            return Ok(candidate.fail(
                State::Conflict,
                "Resolve the integration conflict first.",
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
        let merged = if let Some((repair, index)) = repair_step {
            let base = if index == 0 {
                repository.tree_id(plan.target_head.parse()?)?
            } else {
                repair.steps[index - 1].tree.parse()?
            };
            // A newly executed hook can change the previous commit. Replay
            // the next delta onto that actual tree instead of overwriting it
            // with a precomputed snapshot and accidentally hiding the change.
            Repository::open(&checkout.workdir)?.merge_trees(
                base,
                current_tree,
                repair.steps[index].tree.parse()?,
            )?
        } else if let Some(contribution) = job.contributions.get(&track.id) {
            let private = Repository::open(&checkout.workdir)?;
            let base = contribution.base.parse()?;
            let tree = contribution.tree.parse()?;
            private.import_tree(&contribution.source, base)?;
            private.import_tree(&contribution.source, tree)?;
            private.merge_trees(base, current_tree, tree)?
        } else {
            repository.merge_trees(
                repository.tree_id(row.base.parse()?)?,
                current_tree,
                repository.tree_id(checkpoint.sha.parse()?)?,
            )?
        };
        let tree = match merged {
            Merge::Clean(tree) => tree,
            Merge::Conflicts(paths) => {
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
            if repair_boundary_changed(&candidate, Some(&track.id)) {
                return Ok(candidate.fail(
                    State::RepairChanged,
                    "The rebuilt commits differ from the repaired tree.",
                    None,
                ));
            }
            continue;
        }
        checkout.deploy(tree, cancel.clone()).await?;
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
        candidate.left_out.extend(
            checkout
                .stage(plan.target_head.parse()?, cancel.clone())
                .await?,
        );
        let hooks = if settings.skip_hooks {
            checkout.root.join("control/empty-hooks")
        } else {
            match hooks::prepare(&checkout, &source, tree) {
                Ok(hooks) => hooks.directory,
                Err(error) => {
                    return Ok(candidate.fail(
                        State::HooksOutside,
                        error.to_string(),
                        Some(&track.id),
                    ));
                }
            }
        };
        let address = track
            .last_clip
            .map(|clip| format!("{}:{clip}", track.label))
            .unwrap_or_else(|| format!("{}@{}", track.label, track.checkpoint_count));
        let body = inputs.messages.get(&track.id).unwrap_or(&track.title);
        let message = format!(
            "{}\n\nIntercat: {} · {}/{}\n",
            body.trim_end(),
            job.session_name,
            job.slug,
            address
        );
        let message_path = checkout
            .root
            .join("control")
            .join(format!("message-{}", track.id));
        std::fs::write(&message_path, &message)?;
        let output = checkout
            .git(
                &[
                    "-c".into(),
                    format!("core.hooksPath={}", hooks.display()).into(),
                    "-c".into(),
                    "user.name=Intercat".into(),
                    "-c".into(),
                    "user.email=intercat@localhost".into(),
                    "-c".into(),
                    "committer.name=Intercat".into(),
                    "-c".into(),
                    "committer.email=intercat@localhost".into(),
                    "commit".into(),
                    "--quiet".into(),
                    "--cleanup=verbatim".into(),
                    format!(
                        "--author=Worker {} <intercat+{}-{}@localhost>",
                        track.label, job.slug, track.label
                    )
                    .into(),
                    "--file".into(),
                    message_path.into_os_string(),
                ],
                true,
                cancel.clone(),
            )
            .await?;
        if !output.success() {
            let state = if output.cancelled {
                State::Cancelled
            } else {
                State::HookRejected
            };
            return Ok(candidate.fail(
                state,
                format!(
                    "Commit failed (exit {:?}): {}",
                    output.code,
                    output.stderr_text()
                ),
                Some(&track.id),
            ));
        }
        let sha = checkout
            .git(&["rev-parse".into(), "HEAD".into()], false, cancel.clone())
            .await?
            .require_success("Read candidate")?;
        let raw_sha: ObjectId = String::from_utf8(sha.stdout)?.trim().parse()?;
        checkout.verify_storage()?;
        let repository = Repository::open(&checkout.workdir)?;
        let (sha, removed) = repository.commit_without_attachments(
            raw_sha,
            plan.target_head.parse()?,
            &checkout.attachment_roots,
        )?;
        if sha != raw_sha {
            repository.sync_objects(&[sha])?;
            checkout
                .git(
                    &[
                        "update-ref".into(),
                        "HEAD".into(),
                        sha.to_string().into(),
                        raw_sha.to_string().into(),
                    ],
                    true,
                    cancel.clone(),
                )
                .await?
                .require_success("Exclude attachment copies from candidate")?;
            checkout
                .git(
                    &["read-tree".into(), sha.to_string().into()],
                    true,
                    cancel.clone(),
                )
                .await?
                .require_success("Update candidate index after excluding attachments")?;
        }
        candidate.left_out.extend(removed);
        let details = repository.commit_details(sha)?;
        if details.parents != vec![candidate.head.parse::<ObjectId>()?] {
            return Ok(candidate.fail(
                State::EnvFailed,
                "A hook changed the candidate's parent.",
                Some(&track.id),
            ));
        }
        let scan = scanner
            .commit(
                &Repository::open(&checkout.workdir)?,
                candidate.head.parse()?,
                sha,
                cancel.clone(),
            )
            .await?;
        let found = !scan.accepted(&job.kept_secrets);
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
        if found {
            candidate.blocked_commit = Some(commit);
            return Ok(candidate.fail(
                State::SecretFound,
                "Secret found in the diff or final commit message.",
                Some(&track.id),
            ));
        }
        checkout.import_commit(sha)?;
        candidate.commits.push(commit);
        candidate.head = sha.to_string();
        candidate.tree = details.tree.to_string();
        if repair_boundary_changed(&candidate, Some(&track.id)) {
            return Ok(candidate.fail(
                State::RepairChanged,
                "A commit hook changed the repaired tree. Review the new candidate.",
                None,
            ));
        }
    }
    let command = check_command(&repository, candidate.tree.parse()?, &settings.command)?;
    checkout
        .git(&["clean".into(), "-ffdx".into()], true, cancel.clone())
        .await?
        .require_success("Clean generated files before Check")?;
    checkout.ensure_attachments()?;
    match dependencies
        .prepare(
            &checkout,
            &Repository::open(&checkout.workdir)?,
            candidate.tree.parse()?,
            true,
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
            return Ok(candidate.fail(state, message, None));
        }
    }
    let actual = checkout
        .tree_on_disk(candidate.tree.parse()?, cancel.clone())
        .await?;
    if actual != candidate.tree {
        return Ok(candidate.fail(
            State::DirtyBeforeCheck,
            "The checkout differs from the candidate before Check.",
            None,
        ));
    }
    let output = if let Some(command) = &command {
        let output = checkout
            .run_check(
                command,
                cancel.clone(),
                Duration::from_millis(settings.timeout_ms),
            )
            .await?;
        let success = output.success();
        let cancelled = output.cancelled;
        candidate.check = Some(output_record(
            &candidate.tree,
            &candidate.settings_hash,
            Some(command.clone()),
            output,
        ));
        (success, cancelled)
    } else {
        candidate.check = Some(Check {
            tree: candidate.tree.clone(),
            settings_hash: candidate.settings_hash.clone(),
            command: None,
            code: None,
            stdout: String::new(),
            stderr: String::new(),
            truncated: false,
        });
        (true, false)
    };
    if output.1 {
        return Ok(candidate.fail(State::Cancelled, "Check cancelled.", None));
    }
    let after = checkout
        .tree_on_disk(candidate.tree.parse()?, cancel)
        .await?;
    if after != candidate.tree {
        return Ok(candidate.fail(
            State::CheckMutated,
            "Check changed files in the candidate.",
            None,
        ));
    }
    if !output.0 {
        return Ok(candidate.fail(State::CheckFailed, "The merged Check failed.", None));
    }
    candidate.left_out.sort();
    candidate.left_out.dedup();
    Ok(candidate)
}
