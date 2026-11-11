//! Frozen Commit inputs and object-only integration. No worker checkout is read
//! and no project code, hooks, or filters are executed while planning a merge.
use crate::{Result, checkpoint::Checkpoint, journal::Journal, project::Folder};
use intercat_git::{Merge, ObjectId, Repository, SavedFile};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use tokio::sync::Mutex;
use uuid::Uuid;

#[path = "commit_group_service.rs"]
mod groups;

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
    ChoosePath {
        id: Uuid,
        revision: u64,
        track: String,
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
    TreatCheckAsFailure {
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

impl Action {
    fn job_id(&self) -> Option<Uuid> {
        match self {
            Self::Open { .. } => None,
            Self::Refresh { id, .. }
            | Self::Select { id, .. }
            | Self::ChoosePath { id, .. }
            | Self::Order { id, .. }
            | Self::Discard { id, .. }
            | Self::Prepare { id, .. }
            | Self::Recheck { id, .. }
            | Self::TreatCheckAsFailure { id, .. }
            | Self::SkipHooks { id, .. }
            | Self::EditMessage { id, .. }
            | Self::Commit { id, .. }
            | Self::RetryCommit { id, .. }
            | Self::RetryAssumptions { id, .. }
            | Self::KeepAssumption { id, .. }
            | Self::KeepSecret { id, .. } => Some(*id),
        }
    }
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
    pub owner: DiffOwner,
    pub path: String,
}
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum DiffOwner {
    Worker { track: String },
    Undo,
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
    Choosing,
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
    pub basis: crate::lineage::Basis,
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
    pub project: Folder,
    pub tracks: Vec<Track>,
    pub lineage: crate::lineage::Frozen,
    pub omitted_conflicts: std::collections::BTreeSet<String>,
    pub plan: Option<Plan>,
    pub undo: Option<crate::replacement_undo::Plan>,
    pub path_review: crate::replacement_review::Review,
    pub error: Option<String>,
    pub candidate: Option<crate::candidate::Candidate>,
    pub messages: BTreeMap<String, String>,
    pub publication: Option<crate::publication::Publication>,
    pub kept_secrets: std::collections::BTreeSet<String>,
    pub contributions: BTreeMap<String, crate::candidate::Contribution>,
    pub fixes: BTreeMap<String, std::collections::BTreeSet<String>>,
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
    #[serde(skip)]
    pub path_state: Option<crate::replacement_review::Evaluation>,
    #[serde(skip)]
    pub retirements: Vec<crate::path_retirement::Entry>,
}

pub(crate) fn project(job: &Job) -> Result<serde_json::Value> {
    let mut value = crate::integration_assumptions::project(job)?;
    value["pathReview"]["families"] = serde_json::to_value(
        job.path_review
            .families
            .iter()
            .filter(|family| family.has_alternatives())
            .collect::<Vec<_>>(),
    )?;
    value["orderReview"] = serde_json::to_value(&job.order_review)?;
    value["exclusions"] = serde_json::to_value(crate::integration_exclusions::project(job)?)?;
    value["pathState"] = job
        .path_state
        .as_ref()
        .map(|state| {
            serde_json::json!({
                "pending":state.pending,"retired":state.retired,"undoRecords":state.records.len(),
                "mixed":state.mixed,"descendants":state.descendants,"ready":state.ready(),
            })
        })
        .unwrap_or(serde_json::Value::Null);
    value["retirements"] = serde_json::to_value(&job.retirements)?;
    Ok(value)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    /// The journal binds this to target_head before any merge is planned.
    pub coverage: Option<crate::lineage::Ancestors>,
    pub undo_conflict: Option<crate::replacement_undo::Conflict>,
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
    pub delta_base: Option<String>,
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
        let base = track.basis.base.parse()?;
        if expected_base.is_some_and(|expected| expected != base) {
            return Err("The track base changed. Refresh before preparing the Commit.".into());
        }
        roots.push(base);
        if let Some(checkpoint) = &track.checkpoint {
            roots.push(checkpoint.sha.parse()?);
        }
        rows.push(Row {
            track: track.id.clone(),
            base_ref,
            expected_base: expected_base.map(|v| v.to_string()),
            base: base.to_string(),
            delta_base: None,
            integrated_ref,
            expected_integrated: expected_integrated.map(|v| v.to_string()),
            state: RowState::Waiting,
            tree: None,
            changes: Vec::new(),
            conflicts: Vec::new(),
        });
    }
    roots.extend(job.lineage.roots()?);
    repo.sync_objects(&roots)?;
    Ok(Plan {
        coverage: None,
        undo_conflict: None,
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
    if job.path_state.as_ref().is_some_and(|state| !state.ready()) {
        return Err(
            "Choose a path and review its affected dependencies before preparing Commit.".into(),
        );
    }
    crate::integration_exclusions::validate(job)?;
    let repo = repository(job)?;
    let frozen = job
        .plan
        .as_ref()
        .ok_or("Integration inputs must be saved before merging.")?;
    let mut tree = repo.tree_id(frozen.target_head.parse()?)?;
    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut blocked = false;
    let mut undo_conflict = None;
    let mut rows = Vec::new();
    let mut roots = vec![tree];
    let mut integrated = frozen
        .coverage
        .clone()
        .ok_or("Freeze the published checkpoint coverage before comparing changes.")?;
    if let Some(undo) = &job.undo {
        undo.validate(job)?;
        undo.verify_records(&repo)?;
        crate::replacement_delta::rewind(&undo.records, &mut integrated);
        let mut merges = crate::merge_choices::Merges::new(&repo, &undo.resolutions)?;
        let reversed = crate::replacement_delta::reverse(&mut merges, tree, &undo.records)?;
        merges.finish()?;
        match reversed.result {
            Merge::Clean(next) => {
                tree = next;
                roots.push(tree);
            }
            Merge::Conflicts {
                base,
                target,
                incoming,
                paths,
            } => {
                blocked = true;
                let applied = reversed.applied;
                let remaining = undo
                    .records
                    .iter()
                    .filter(|record| !applied.contains(&record.id))
                    .map(|record| record.id)
                    .collect();
                undo_conflict = Some(crate::replacement_undo::Conflict {
                    track: undo.records[applied.len()].track_id.clone(),
                    applied,
                    remaining,
                    base: base.to_string(),
                    target: target.to_string(),
                    incoming: incoming.to_string(),
                    paths: paths
                        .into_iter()
                        .map(|path| String::from_utf8(path.into()))
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                });
            }
        }
    }
    for track in &job.tracks {
        let mut row = frozen
            .rows
            .iter()
            .find(|row| row.track == track.id)
            .ok_or("Missing frozen track base.")?
            .clone();
        row.state = RowState::Excluded;
        row.tree = None;
        row.changes.clear();
        row.conflicts.clear();
        row.delta_base = None;
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
                let base_tree = match crate::integration_baseline::calculate(
                    job,
                    track,
                    &integrated,
                    &mut crate::merge_choices::Merges::new(&repo, &[])?,
                )? {
                    Merge::Clean(tree) => tree,
                    Merge::Conflicts { paths, .. } => {
                        row.state = RowState::Conflict;
                        row.conflicts = paths
                            .into_iter()
                            .map(|path| Conflict {
                                path: path.to_string(),
                                with_tracks: vec![],
                            })
                            .collect();
                        blocked = true;
                        rows.push(row);
                        continue;
                    }
                };
                roots.push(base_tree);
                row.delta_base = Some(base_tree.to_string());
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
                        job.lineage.incorporate(
                            &mut integrated,
                            job.lineage.by_sha(&track.id, &checkpoint.sha)?,
                        )?;
                    }
                    Merge::Conflicts { paths, .. } => {
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
        coverage: frozen.coverage.clone(),
        undo_conflict,
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
    running: BTreeMap<Uuid, Running>,
}
struct Running {
    publishing: bool,
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
        let job = self.journal.integration_job(request.id).await?;
        if job.id != request.id || job.revision != request.revision {
            return Err("The sheet changed · review again".into());
        }
        let settings = self
            .journal
            .integration_settings(job.id, job.revision)
            .await?
            .ok_or("Missing Check settings.")?;
        let mut root = job.project.root.clone();
        let changes = match &request.owner {
            DiffOwner::Undo => {
                let candidate = job
                    .candidate
                    .as_ref()
                    .ok_or("This review has no candidate.")?;
                if candidate.pending_undo() {
                    root = candidate.workspace.join("repo");
                }
                &candidate
                    .undo
                    .as_ref()
                    .ok_or("This review has no undo commit.")?
                    .changes
            }
            DiffOwner::Worker { track } => {
                if let Some(commit) = job
                    .candidate
                    .as_ref()
                    .and_then(|candidate| candidate.blocked_commit.as_ref())
                    .filter(|commit| &commit.track == track)
                {
                    root = job.candidate.as_ref().unwrap().workspace.join("repo");
                    &commit.changes
                } else if let Some(commit) = job.candidate.as_ref().and_then(|candidate| {
                    candidate
                        .commits
                        .iter()
                        .find(|commit| &commit.track == track)
                }) {
                    &commit.changes
                } else {
                    &job.plan
                        .as_ref()
                        .ok_or("Missing plan.")?
                        .rows
                        .iter()
                        .find(|row| &row.track == track)
                        .ok_or("Unknown track.")?
                        .changes
                }
            }
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
        let current = self.journal.integration_job(job.id).await?;
        if current.revision != job.revision || current.status == Status::Discarded {
            return Err("The sheet changed · review again".into());
        }
        Ok(diff)
    }
    async fn reap_finished(&self, state: &mut ServiceState) -> Result<()> {
        let completed = state
            .running
            .iter()
            .filter(|(_, run)| run.task.is_finished())
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        for id in completed {
            state.running.remove(&id).unwrap().task.await?;
        }
        Ok(())
    }
    pub async fn command(&self, command: Command) -> Result<Job> {
        let mut state = self.state.lock().await;
        self.reap_finished(&mut state).await?;
        if let Action::Recheck { id, revision } = &command.action {
            if self.journal.operation(command.op_id).await?.is_some() {
                let job = self.journal.integration_command(command).await?;
                return self.advance(&mut state, job).await;
            }
            let job = self.journal.integration_job(*id).await?;
            if job.id != *id || job.revision != *revision {
                return Err("The sheet changed · review again".into());
            }
            let environment = self.journal.check_environment().await?;
            let checks = self.journal.check_settings().await?;
            let settings = tokio::task::spawn_blocking(move || {
                crate::candidate::Settings::capture(&job.project, environment, checks)
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
                if let Some(id) = command.action.job_id()
                    && let Some(run) = state.running.remove(&id)
                {
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
    pub(crate) async fn finish_undo_merge_result(&self, operation: Uuid) -> Result<()> {
        let mut state = self.state.lock().await;
        if let Some(job) = self
            .journal
            .change_undo_merge(crate::undo_merge_store::Change::Dispatch(operation))
            .await?
        {
            let job = self.advance(&mut state, job).await?;
            if job.status == Status::Open && job.plan.as_ref().is_some_and(|p| p.tree.is_some())
                || job.status == Status::MergeConflict
                    && job.plan.as_ref().is_some_and(|p| {
                        p.undo_conflict.is_some()
                            || p.rows.iter().any(|r| r.state == RowState::Conflict)
                    })
            {
                let building = self
                    .journal
                    .integration_command(Command {
                        op_id: Uuid::new_v5(&operation, b"build-undo-resolution"),
                        action: Action::Prepare {
                            id: job.id,
                            revision: job.revision,
                        },
                    })
                    .await?;
                self.advance(&mut state, building).await?;
            }
        }
        self.journal
            .change_undo_merge(crate::undo_merge_store::Change::Complete(operation))
            .await?;
        Ok(())
    }
    async fn advance(&self, state: &mut ServiceState, mut job: Job) -> Result<Job> {
        self.watch_groups(state).await?;
        if state
            .running
            .get(&job.id)
            .is_some_and(|run| run.task.is_finished())
        {
            state.running.remove(&job.id).unwrap().task.await?;
        }
        if state
            .running
            .get(&job.id)
            .is_some_and(|run| run.revision != job.revision || job.status == Status::Discarded)
        {
            let run = state.running.remove(&job.id).unwrap();
            let _ = run.cancel.send(true);
            run.task.await?;
        }
        if job.status == Status::Discarded {
            crate::integration_refs::discard(&job, &self.journal).await?;
        }
        // A persisted candidate/result can be observed before its producing
        // task returns. Join that folder's task before publication. Other
        // folders keep their own Check and publication tasks.
        if state
            .running
            .get(&job.id)
            .is_some_and(|run| job.status == Status::Committing && !run.publishing)
        {
            let run = state.running.remove(&job.id).unwrap();
            if !run.publishing {
                let _ = run.cancel.send(true);
            }
            run.task.await?;
        }
        if job.status == Status::Committing {
            if let std::collections::btree_map::Entry::Vacant(slot) = state.running.entry(job.id) {
                let input = job.clone();
                let journal = self.journal.clone();
                let (cancel, cancelling) = tokio::sync::watch::channel(false);
                let task = tokio::spawn(async move {
                    let _ = crate::publication::publish(input, journal, cancelling).await;
                });
                slot.insert(Running {
                    publishing: true,
                    revision: job.revision,
                    cancel,
                    task,
                });
            }
            return Ok(job);
        }
        if job.status == Status::Building {
            if let std::collections::btree_map::Entry::Vacant(slot) = state.running.entry(job.id) {
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
                            let workspace = input
                                .candidate
                                .as_ref()
                                .filter(|candidate| {
                                    candidate.blocked_commit.is_some() || candidate.pending_undo()
                                })
                                .map_or_else(
                                    || parent.join(revision.to_string()),
                                    |candidate| candidate.workspace.clone(),
                                );
                            crate::candidate::build(
                                &input,
                                &workspace,
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
                slot.insert(Running {
                    publishing: false,
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
            let checks = self.journal.check_settings().await?;
            let settings = tokio::task::spawn_blocking(move || {
                crate::candidate::Settings::capture(&input.project, environment, checks)
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
        if job.status == Status::Choosing {
            return Ok(job);
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
        let mut state = self.state.lock().await;
        for job in self.journal.pending_publications().await? {
            self.advance(&mut state, job).await?;
        }
        self.watch_groups(&mut state).await
    }
    pub async fn shutdown(&self) {
        let mut state = self.state.lock().await;
        let running = std::mem::take(&mut state.running);
        for run in running.values() {
            let _ = run.cancel.send(true);
        }
        for (_, run) in running {
            let _ = run.task.await;
        }
        let _ = self.reconcile_groups().await;
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        if Arc::strong_count(&self.state) == 1
            && let Ok(state) = self.state.try_lock()
        {
            for run in state.running.values() {
                let _ = run.cancel.send(true);
            }
        }
    }
}
