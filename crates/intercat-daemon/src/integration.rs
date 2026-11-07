//! Frozen Commit inputs and object-only integration. No worker checkout is read
//! and no project code, hooks, or filters are executed while planning a merge.
use crate::{Result, checkpoint::Checkpoint, journal::Journal, project::Project};
use intercat_git::{Merge, ObjectId, Repository, SavedFile};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "action",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Action {
    Open {
        tracks: Option<Vec<String>>,
    },
    Refresh {
        id: Uuid,
        revision: u64,
    },
    Select {
        id: Uuid,
        revision: u64,
        track: String,
        included: bool,
    },
    Order {
        id: Uuid,
        revision: u64,
        tracks: Vec<String>,
    },
    Discard {
        id: Uuid,
        revision: u64,
    },
    Prepare {
        id: Uuid,
        revision: u64,
    },
    Recheck {
        id: Uuid,
        revision: u64,
    },
    SkipHooks {
        id: Uuid,
        revision: u64,
    },
    EditMessage {
        id: Uuid,
        revision: u64,
        track: String,
        message: String,
    },
    Commit {
        id: Uuid,
        revision: u64,
        head: String,
        tree: String,
        settings_hash: String,
    },
    RetryCommit {
        id: Uuid,
        revision: u64,
    },
    RetryAssumptions {
        id: Uuid,
        revision: u64,
    },
    KeepAssumption {
        id: Uuid,
        revision: u64,
        assumption: Uuid,
    },
    KeepSecret {
        id: Uuid,
        revision: u64,
        finding: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Command {
    pub op_id: Uuid,
    pub action: Action,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DiffRequest {
    pub id: Uuid,
    pub revision: u64,
    pub track: String,
    pub path: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diff {
    pub path: String,
    pub before: String,
    pub after: String,
    pub redacted: bool,
    pub binary: bool,
    pub too_large: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    Preparing,
    Ordering,
    Open,
    MergeConflict,
    Failed,
    Discarded,
    Building,
    Staging,
    Checked,
    Committing,
    CommitBlocked,
    Done,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub id: String,
    pub label: String,
    pub title: String,
    pub initial_base: String,
    pub checkpoint: Option<Checkpoint>,
    pub checkpoint_count: u64,
    pub last_clip: Option<i64>,
    pub included: bool,
    pub integration_count: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Job {
    pub id: Uuid,
    pub revision: u64,
    pub status: Status,
    pub opened_at: i64,
    pub session_id: String,
    pub slug: String,
    pub session_name: String,
    pub project: Project,
    pub tracks: Vec<Track>,
    pub plan: Option<Plan>,
    pub error: Option<String>,
    pub candidate: Option<crate::candidate::Candidate>,
    pub messages: BTreeMap<String, String>,
    pub publication: Option<crate::publication::Publication>,
    pub kept_secrets: std::collections::BTreeSet<String>,
    pub contributions: BTreeMap<String, crate::candidate::Contribution>,
    // These relations are hydrated by the journal and projected explicitly for the UI.
    #[serde(skip)]
    pub assumptions: Vec<crate::integration_assumptions::Review>,
    #[serde(skip)]
    pub assumptions_changed: bool,
    #[serde(skip)]
    pub assumptions_pending: usize,
    #[serde(skip)]
    pub assumptions_failed: usize,
    #[serde(skip)]
    pub order_review: Option<crate::integration_order::Review>,
}

pub(crate) fn project(job: &Job) -> Result<serde_json::Value> {
    let mut value = crate::integration_assumptions::project(job)?;
    value["orderReview"] = serde_json::to_value(&job.order_review)?;
    Ok(value)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub target_ref: String,
    /// None means the session branch has not been created yet. The first
    /// publication must use a create-only CAS, even though target_head exists.
    pub expected_target: Option<String>,
    pub expected_staging: Option<String>,
    pub target_head: String,
    /// The combined preview tree. This is not a checked candidate commit.
    pub tree: Option<String>,
    pub rows: Vec<Row>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub track: String,
    pub base_ref: String,
    pub expected_base: Option<String>,
    pub base: String,
    pub integrated_ref: String,
    pub expected_integrated: Option<String>,
    pub state: RowState,
    pub tree: Option<String>,
    pub changes: Vec<Change>,
    pub conflicts: Vec<Conflict>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RowState {
    Included,
    NoChanges,
    Excluded,
    NoCheckpoint,
    Conflict,
    Waiting,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Change {
    pub path: String,
    pub before: Option<File>,
    pub after: Option<File>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct File {
    pub object: String,
    pub mode: u16,
}
impl From<&SavedFile> for File {
    fn from(file: &SavedFile) -> Self {
        Self {
            object: file.object.to_string(),
            mode: file.mode,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    pub path: String,
    /// Earlier selected tracks that changed this path. An empty list means
    /// the conflict is with changes already in the session branch.
    pub with_tracks: Vec<String>,
}

pub(crate) fn read_diff(root: std::path::PathBuf, change: Change) -> Result<Diff> {
    let path = change.path.clone();
    let repository = Repository::open(root)?;
    let mut too_large = false;
    let mut binary = false;
    let mut read = |file: Option<File>| -> Result<String> {
        let Some(file) = file else {
            return Ok(String::new());
        };
        let saved = SavedFile {
            object: file.object.parse()?,
            mode: file.mode,
        };
        if repository.saved_size(&saved)? > 1024 * 1024 {
            too_large = true;
            return Ok(String::new());
        }
        let bytes = repository.saved_bytes(&saved)?;
        if bytes.contains(&0) {
            binary = true;
            return Ok(String::new());
        }
        match String::from_utf8(bytes) {
            Ok(text) => Ok(text),
            Err(_) => {
                binary = true;
                Ok(String::new())
            }
        }
    };
    let before = read(change.before)?;
    let after = read(change.after)?;
    Ok(Diff {
        path,
        before,
        after,
        redacted: false,
        binary,
        too_large,
    })
}

pub(crate) fn changes(repo: &Repository, before: ObjectId, after: ObjectId) -> Result<Vec<Change>> {
    let before = repo.tree_files(before)?;
    let after = repo.tree_files(after)?;
    let paths: std::collections::BTreeSet<_> = before.keys().chain(after.keys()).collect();
    Ok(paths
        .into_iter()
        .filter_map(|path| {
            let old = before.get(path);
            let new = after.get(path);
            (old != new).then(|| Change {
                path: path.clone(),
                before: old.map(File::from),
                after: new.map(File::from),
            })
        })
        .collect())
}

pub(crate) fn repository(job: &Job) -> Result<Repository> {
    let source = Repository::worktree_source(&job.project.root, Some(job.project.base.parse()?))?;
    if source.root != job.project.root || source.common_dir != job.project.common_dir {
        return Err("The configured project has moved or changed repository.".into());
    }
    let repo = Repository::open(&job.project.root)?;
    if !repo.owns_session(&job.slug, &job.session_id)? {
        return Err("This session's Git reservation changed.".into());
    }
    Ok(repo)
}

/// Capture Git inputs separately so the coordinator can persist them before
/// starting a merge. Resuming a calculation must never read new base refs.
pub fn freeze(job: &Job) -> Result<Plan> {
    let repo = repository(job)?;
    let target_ref = format!("refs/heads/intercat/{}", job.slug);
    let expected_target = repo.reference_target(&target_ref)?;
    let target_head = expected_target.unwrap_or(job.project.base.parse()?);
    let mut roots = vec![target_head];
    let mut rows = Vec::new();
    for track in &job.tracks {
        let base_ref = format!("refs/intercat/{}/base/{}", job.slug, track.label);
        let integrated_ref = format!("refs/intercat/{}/integrated/{}", job.slug, track.label);
        let expected_base = repo.reference_target(&base_ref)?;
        let expected_integrated = repo.reference_target(&integrated_ref)?;
        let base = expected_base.unwrap_or(track.initial_base.parse()?);
        roots.push(base);
        if let Some(checkpoint) = &track.checkpoint {
            roots.push(checkpoint.sha.parse()?);
        }
        rows.push(Row {
            track: track.id.clone(),
            base_ref,
            expected_base: expected_base.map(|v| v.to_string()),
            base: base.to_string(),
            integrated_ref,
            expected_integrated: expected_integrated.map(|v| v.to_string()),
            state: RowState::Waiting,
            tree: None,
            changes: Vec::new(),
            conflicts: Vec::new(),
        });
    }
    repo.sync_objects(&roots)?;
    Ok(Plan {
        target_ref,
        expected_target: expected_target.map(|id| id.to_string()),
        expected_staging: repo
            .reference_target(&format!("refs/intercat/{}/staging", job.slug))?
            .map(|id| id.to_string()),
        target_head: target_head.to_string(),
        tree: None,
        rows,
    })
}

pub fn plan(job: &Job) -> Result<Plan> {
    let repo = repository(job)?;
    let frozen = job
        .plan
        .as_ref()
        .ok_or("Integration inputs must be saved before merging.")?;
    let mut tree = repo.tree_id(frozen.target_head.parse()?)?;
    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut blocked = false;
    let mut rows = Vec::new();
    let mut roots = vec![tree];
    for track in &job.tracks {
        let mut row = frozen
            .rows
            .iter()
            .find(|row| row.track == track.id)
            .ok_or("Missing frozen track base.")?
            .clone();
        let base = row.base.parse()?;
        row.state = RowState::Excluded;
        row.tree = None;
        row.changes.clear();
        row.conflicts.clear();
        if let Some(checkpoint) = &track.checkpoint {
            let checkpoint_id = checkpoint.sha.parse()?;
            if repo.reference_target(&checkpoint.reference)? != Some(checkpoint_id) {
                return Err(
                    format!("{}'s frozen checkpoint reference changed.", track.label).into(),
                );
            }
            // The checkpoint records unmerged index entries even when its
            // worktree happens to contain text. Never publish that as resolved.
            if track.included && !checkpoint.unmerged_paths.is_empty() {
                row.state = RowState::Conflict;
                row.conflicts = checkpoint
                    .unmerged_paths
                    .iter()
                    .map(|path| Conflict {
                        path: path.clone(),
                        with_tracks: vec![],
                    })
                    .collect();
                blocked = true;
            } else if track.included && blocked {
                row.state = RowState::Waiting;
            } else if track.included {
                let base_tree = repo.tree_id(base)?;
                let checkpoint_tree = repo.tree_id(checkpoint_id)?;
                match repo.merge_trees(base_tree, tree, checkpoint_tree)? {
                    Merge::Clean(merged) => {
                        row.changes = changes(&repo, tree, merged)?;
                        row.state = if merged == tree {
                            RowState::NoChanges
                        } else {
                            RowState::Included
                        };
                        for change in &row.changes {
                            owners
                                .entry(change.path.clone())
                                .or_default()
                                .push(track.id.clone());
                        }
                        tree = merged;
                        roots.push(tree);
                        row.tree = Some(tree.to_string());
                    }
                    Merge::Conflicts(paths) => {
                        blocked = true;
                        row.state = RowState::Conflict;
                        for path in paths {
                            let path = std::str::from_utf8(&path)?.to_string();
                            row.conflicts.push(Conflict {
                                with_tracks: owners.get(&path).cloned().unwrap_or_default(),
                                path,
                            });
                        }
                    }
                }
            }
        } else {
            row.state = RowState::NoCheckpoint;
        }
        rows.push(row);
    }
    // Preview trees are durable before a journal row refers to them. No ref
    // (including the user's HEAD or index) is changed by this stage.
    repo.sync_objects(&roots)?;
    Ok(Plan {
        target_ref: frozen.target_ref.clone(),
        expected_target: frozen.expected_target.clone(),
        expected_staging: frozen.expected_staging.clone(),
        target_head: frozen.target_head.clone(),
        tree: (!blocked).then(|| tree.to_string()),
        rows,
    })
}

/// One coordinator is shared by all RPC clients of this daemon. Journal CAS
/// also rejects stale results, so a disconnected window cannot overwrite a
/// refreshed plan. Closing a window does not discard its integration job.
#[derive(Clone)]
pub struct Service {
    journal: Journal,
    state: Arc<Mutex<ServiceState>>,
    root: std::path::PathBuf,
}
#[derive(Default)]
struct ServiceState {
    running: Option<Running>,
}
struct Running {
    publishing: bool,
    id: Uuid,
    revision: u64,
    cancel: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}
#[derive(Debug)]
pub(crate) struct StagingBusy;
impl std::fmt::Display for StagingBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Candidate persistence is finishing.")
    }
}
impl std::error::Error for StagingBusy {}
impl Service {
    pub fn new(journal: Journal, root: std::path::PathBuf) -> Self {
        Self {
            journal,
            state: Arc::new(Mutex::new(ServiceState::default())),
            root,
        }
    }
    pub async fn diff(&self, request: DiffRequest) -> Result<Diff> {
        let job: Job =
            serde_json::from_value(self.journal.snapshot().await?["integration"].clone())?;
        if job.id != request.id || job.revision != request.revision {
            return Err("The sheet changed · review again".into());
        }
        let settings = self
            .journal
            .integration_settings(job.id, job.revision)
            .await?
            .ok_or("Missing Check settings.")?;
        let mut root = job.project.root.clone();
        let changes = if let Some(commit) = job
            .candidate
            .as_ref()
            .and_then(|candidate| candidate.blocked_commit.as_ref())
            .filter(|commit| commit.track == request.track)
        {
            root = self
                .root
                .join(job.id.to_string())
                .join(job.candidate.as_ref().unwrap().generation.to_string())
                .join("repo");
            &commit.changes
        } else if let Some(commit) = job.candidate.as_ref().and_then(|candidate| {
            candidate
                .commits
                .iter()
                .find(|commit| commit.track == request.track)
        }) {
            &commit.changes
        } else {
            &job.plan
                .as_ref()
                .ok_or("Missing plan.")?
                .rows
                .iter()
                .find(|row| row.track == request.track)
                .ok_or("Unknown track.")?
                .changes
        };
        let change = changes
            .iter()
            .find(|change| change.path == request.path)
            .ok_or("The file is not in this candidate.")?
            .clone();
        let mut diff = tokio::task::spawn_blocking(move || read_diff(root, change)).await??;
        let scanner = settings.scanner()?;
        let safe = scanner
            .redact(
                &[diff.before.clone(), diff.after.clone()],
                tokio::sync::watch::channel(false).1,
            )
            .await?;
        diff.redacted = safe[0] != diff.before || safe[1] != diff.after;
        diff.before = safe[0].clone();
        diff.after = safe[1].clone();
        let current = self.journal.snapshot().await?;
        if current["integration"]["id"] != job.id.to_string()
            || current["integration"]["revision"] != job.revision
        {
            return Err("The sheet changed · review again".into());
        }
        Ok(diff)
    }
    pub async fn command(&self, command: Command) -> Result<Job> {
        let mut state = self.state.lock().await;
        if state
            .running
            .as_ref()
            .is_some_and(|run| run.task.is_finished())
        {
            let run = state.running.take().unwrap();
            run.task.await?;
        }
        if let Action::Recheck { id, revision } = &command.action {
            if self.journal.operation(command.op_id).await?.is_some() {
                let job = self.journal.integration_command(command).await?;
                return self.advance(&mut state, job).await;
            }
            let job: Job =
                serde_json::from_value(self.journal.snapshot().await?["integration"].clone())?;
            if job.id != *id || job.revision != *revision {
                return Err("The sheet changed · review again".into());
            }
            let environment = self.journal.check_environment().await?;
            let settings = tokio::task::spawn_blocking(move || {
                crate::candidate::Settings::capture(&job, environment)
            })
            .await??;
            let job = self.journal.recheck_integration(command, settings).await?;
            return self.advance(&mut state, job).await;
        }
        if let Action::EditMessage {
            id,
            revision,
            message,
            ..
        } = &command.action
        {
            if message.trim().is_empty() || message.len() > 16 * 1024 || message.contains('\0') {
                return Err("A commit message must be nonempty and at most 16 KiB.".into());
            }
            let settings = self
                .journal
                .integration_settings(*id, *revision)
                .await?
                .ok_or("Missing Check settings.")?;
            let scanner = settings.scanner()?;
            let safe = scanner
                .redact(
                    std::slice::from_ref(message),
                    tokio::sync::watch::channel(false).1,
                )
                .await?;
            if safe[0] != *message {
                return Err("Secret found in the commit message. Remove it before saving.".into());
            }
        }
        // Validate/replay the durable command before interrupting anything.
        // A stale window or a repeated operation must not cancel a newer Check.
        let job = match self.journal.integration_command(command.clone()).await {
            Err(error) if error.is::<StagingBusy>() => {
                if let Some(run) = state.running.take() {
                    run.task.await?;
                }
                self.journal.integration_command(command).await?
            }
            result => result?,
        };
        self.advance(&mut state, job).await
    }
    pub async fn finish_repair_refresh(&self, operation: Uuid) -> Result<()> {
        let mut state = self.state.lock().await;
        if let Some(job) = self.journal.repair_finish_refresh(operation).await? {
            let job = self.advance(&mut state, job).await?;
            if job.status == Status::Open
                && job.plan.as_ref().is_some_and(|plan| plan.tree.is_some())
            {
                let building = self
                    .journal
                    .integration_command(Command {
                        op_id: Uuid::new_v5(&operation, b"build-refreshed-candidate"),
                        action: Action::Prepare {
                            id: job.id,
                            revision: job.revision,
                        },
                    })
                    .await?;
                self.advance(&mut state, building).await?;
            }
            self.journal.repair_complete_refresh(operation).await?;
        }
        Ok(())
    }
    async fn advance(&self, state: &mut ServiceState, mut job: Job) -> Result<Job> {
        if state.running.as_ref().is_some_and(|run| {
            run.id == job.id && (run.revision != job.revision || job.status == Status::Discarded)
        }) {
            let run = state.running.take().unwrap();
            let _ = run.cancel.send(true);
            run.task.await?;
        }
        if job.status == Status::Discarded {
            crate::integration_refs::discard(&job, &self.journal).await?;
        }
        // A persisted candidate/result can be observed before its producing
        // task returns. Join that task before handing the slot to publication
        // or to the next integration job; never strand the accepted operation.
        if state.running.as_ref().is_some_and(|run| {
            run.id != job.id || (job.status == Status::Committing && !run.publishing)
        }) {
            let run = state.running.take().unwrap();
            if !run.publishing {
                let _ = run.cancel.send(true);
            }
            run.task.await?;
        }
        if job.status == Status::Committing {
            if state.running.is_none() {
                let input = job.clone();
                let journal = self.journal.clone();
                let (cancel, _) = tokio::sync::watch::channel(false);
                let task = tokio::spawn(async move {
                    let _ = crate::publication::publish(input, journal).await;
                });
                state.running = Some(Running {
                    publishing: true,
                    id: job.id,
                    revision: job.revision,
                    cancel,
                    task,
                });
            }
            return Ok(job);
        }
        if job.status == Status::Building {
            if state.running.is_none() {
                let settings = self
                    .journal
                    .integration_settings(job.id, job.revision)
                    .await?
                    .ok_or("Missing frozen Check settings.")?;
                let (cancel, receive) = tokio::sync::watch::channel(false);
                let input = job.clone();
                let staging_job = job.clone();
                let journal = self.journal.clone();
                let root = self.root.clone();
                let task = tokio::spawn(async move {
                    let id = input.id;
                    let revision = input.revision;
                    // gix repositories are thread-local. Keep Git and file IO
                    // off the async reducer, while subprocess IO still uses
                    // the runtime reactor through this handle.
                    let result = tokio::task::spawn_blocking(move || {
                        tokio::runtime::Handle::current().block_on(async {
                            let parent = root.join(id.to_string());
                            std::fs::create_dir_all(&parent)?;
                            crate::candidate::build(
                                &input,
                                &parent.join(
                                    input
                                        .candidate
                                        .as_ref()
                                        .filter(|candidate| candidate.blocked_commit.is_some())
                                        .map_or(revision, |candidate| candidate.generation)
                                        .to_string(),
                                ),
                                &root.join("downloads"),
                                &settings,
                                &input.messages,
                                receive,
                            )
                            .await
                        })
                    })
                    .await
                    .unwrap_or_else(|error| Err(error.into()));
                    let result = match result {
                        Ok(candidate) => {
                            if !journal
                                .integration_stage_begin(id, revision)
                                .await
                                .unwrap_or(false)
                            {
                                return;
                            }
                            match crate::integration_refs::stage(&staging_job, &candidate, &journal)
                                .await
                            {
                                Ok(()) => Ok(candidate),
                                Err(error) => Err(error),
                            }
                        }
                        Err(error) => Err(error),
                    };
                    let _ = journal
                        .integration_candidate(
                            id,
                            revision,
                            result.map_err(|error: Box<dyn std::error::Error + Send + Sync>| {
                                error.to_string()
                            }),
                        )
                        .await;
                });
                state.running = Some(Running {
                    publishing: false,
                    id: job.id,
                    revision: job.revision,
                    cancel,
                    task,
                });
            }
            return Ok(job);
        }
        if job.status != Status::Preparing {
            return Ok(job);
        }
        if self
            .journal
            .integration_settings(job.id, job.revision)
            .await?
            .is_none()
        {
            let input = job.clone();
            let environment = self.journal.check_environment().await?;
            let settings = tokio::task::spawn_blocking(move || {
                crate::candidate::Settings::capture(&input, environment)
            })
            .await??;
            self.journal
                .integration_save_settings(job.id, job.revision, settings)
                .await?;
        }
        if job.plan.is_none() {
            let input = job.clone();
            let result = tokio::task::spawn_blocking(move || freeze(&input)).await?;
            match result {
                Ok(frozen) => {
                    job = self
                        .journal
                        .integration_frozen(job.id, job.revision, frozen)
                        .await?;
                }
                Err(error) => {
                    return self
                        .journal
                        .integration_planned(job.id, job.revision, Err(error.to_string()))
                        .await;
                }
            }
        }
        if job.order_review.is_none() {
            let input = job.clone();
            let review =
                tokio::task::spawn_blocking(move || crate::integration_order::inspect(&input))
                    .await?;
            match review {
                Ok(review) => {
                    job = self
                        .journal
                        .integration_order_review(job.id, job.revision, review)
                        .await?
                }
                Err(error) => {
                    return self
                        .journal
                        .integration_planned(job.id, job.revision, Err(error.to_string()))
                        .await;
                }
            }
        }
        if job.status == Status::Ordering {
            return Ok(job);
        }
        let input = job.clone();
        let result = tokio::task::spawn_blocking(move || plan(&input)).await?;
        self.journal
            .integration_planned(job.id, job.revision, result.map_err(|e| e.to_string()))
            .await
    }
    pub async fn recover(&self) -> Result<()> {
        let _state = self.state.lock().await;
        for job in self.journal.pending_publications().await? {
            crate::publication::publish(job, self.journal.clone()).await?;
        }
        Ok(())
    }
    pub async fn shutdown(&self) {
        let mut state = self.state.lock().await;
        if let Some(run) = state.running.take() {
            let _ = run.cancel.send(true);
            let _ = run.task.await;
        }
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        if Arc::strong_count(&self.state) == 1
            && let Ok(state) = self.state.try_lock()
            && let Some(run) = &state.running
        {
            let _ = run.cancel.send(true);
        }
    }
}
