//! One SQLite writer thread. Await acknowledgements outside the state reducer.

use crate::Result;
use crate::decision::{Candidate, Decision, Kind, Outcome, PendingInput, Resolution, Target};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    thread,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{mpsc, oneshot, watch};

pub const SCHEMA_VERSION: i64 = 53;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    New,
    Replay(String),
}
#[derive(Debug)]
pub struct StartSuperseded;
impl std::fmt::Display for StartSuperseded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("The request changed before dispatch.")
    }
}
impl std::error::Error for StartSuperseded {}

#[derive(Clone)]
pub struct RawFrame {
    pub stamp: crate::clock::Stamp,
    pub frame: std::sync::Arc<Value>,
    pub lease: Option<crate::output_flow::Lease>,
}

enum Job {
    #[cfg(all(feature = "fault-injection", debug_assertions))]
    TestFault {
        request: crate::fault_injection::Request,
        reply: oneshot::Sender<Result<Value>>,
    },
    DispatchPause {
        id: String,
        paused: bool,
        queued_at: std::time::Instant,
        reply: oneshot::Sender<Result<()>>,
    },
    PauseHistory {
        reply: oneshot::Sender<Result<Vec<(uuid::Uuid, bool)>>>,
    },
    BackupApplyRead {
        id: Option<uuid::Uuid>,
        reply: oneshot::Sender<Result<Option<crate::backup_apply::State>>>,
    },
    BackupApplyChange {
        change: crate::backup_apply::Change,
        reply: oneshot::Sender<Result<crate::backup_apply::State>>,
    },
    BackupResume {
        track: String,
        reply: oneshot::Sender<Result<Option<(crate::session_retirement::Resume, bool)>>>,
    },
    BackupResumeOpened {
        track: String,
        session: String,
        reply: oneshot::Sender<Result<()>>,
    },
    SaveBackupChoice {
        receipt: crate::fingerprint_receipt::Receipt,
        record: crate::backup_choices::Record,
        reply: oneshot::Sender<Result<()>>,
    },
    SaveBackupFiles {
        id: uuid::Uuid,
        payload: Value,
        reply: oneshot::Sender<Result<()>>,
    },
    BackupReview {
        receipt: crate::fingerprint_receipt::Receipt,
        reply: oneshot::Sender<Result<crate::backup_review::Plan>>,
    },
    SummaryTasks {
        id: uuid::Uuid,
        reply: oneshot::Sender<Result<Vec<uuid::Uuid>>>,
    },
    CancelPickup {
        request: crate::pickup::CancelRequest,
        reply: oneshot::Sender<Result<crate::pickup::Recovery>>,
    },
    SummaryPlan {
        id: uuid::Uuid,
        input: Value,
        reply: oneshot::Sender<Result<()>>,
    },
    SummaryNext {
        id: uuid::Uuid,
        reply: oneshot::Sender<Result<crate::handoff::Next>>,
    },
    SummaryChange {
        change: crate::handoff::Change,
        reply: oneshot::Sender<Result<crate::handoff::Task>>,
    },
    Pickup {
        change: crate::pickup::Change,
        reply: oneshot::Sender<Result<crate::pickup::Recovery>>,
    },
    PickupCheckpoint {
        track: String,
        session: String,
        checkpoint: uuid::Uuid,
        reply: oneshot::Sender<Result<bool>>,
    },
    RebuildFingerprints {
        request: crate::fingerprint_recovery::Request,
        reply: oneshot::Sender<Result<Value>>,
    },
    FingerprintRecovery {
        receipt: Option<crate::fingerprint_receipt::Receipt>,
        reply: oneshot::Sender<Result<Option<Value>>>,
    },
    ProjectSecretsCheck {
        id: uuid::Uuid,
        manifest: crate::project_secrets::Manifest,
        reply: oneshot::Sender<Result<()>>,
    },
    ProjectSecretsHold {
        reply: oneshot::Sender<Result<()>>,
    },
    ProjectHostUse {
        host: crate::project_secret_usage::HostUse,
        reply: oneshot::Sender<Result<()>>,
    },
    ProjectUses {
        reply: oneshot::Sender<Result<Vec<crate::project_secret_usage::Record>>>,
    },
    ProjectUseClosed {
        nonce: String,
        reply: oneshot::Sender<Result<()>>,
    },
    ProjectUseKept {
        decision: crate::project_secret_usage::Decision,
        reply: oneshot::Sender<Result<()>>,
    },
    ProjectSecretsPrepare {
        id: uuid::Uuid,
        manifest: crate::project_secrets::Manifest,
        reply: oneshot::Sender<Result<crate::secrets::Catalog>>,
    },
    ProjectSecretsActivate {
        manifest: crate::project_secrets::Manifest,
        reply: oneshot::Sender<Result<()>>,
    },
    SecretSettled {
        id: uuid::Uuid,
        reply: oneshot::Sender<Result<()>>,
    },
    SecretAccepted {
        id: uuid::Uuid,
        reply: oneshot::Sender<Result<()>>,
    },
    PrepareSecret {
        target: crate::secret_input::Target,
        reply: oneshot::Sender<Result<bool>>,
    },
    SecretRestarting {
        id: uuid::Uuid,
        reply: oneshot::Sender<Result<()>>,
    },
    QueueRepairSecret {
        target: crate::secret_input::Target,
        checkpoint: crate::checkpoint::Checkpoint,
        reply: oneshot::Sender<Result<()>>,
    },
    SecretReady {
        target: crate::secret_input::Target,
        host: String,
        reply: oneshot::Sender<Result<()>>,
    },
    SecretFailed {
        id: uuid::Uuid,
        reply: oneshot::Sender<Result<()>>,
    },
    SecretExpired {
        host: String,
        reply: oneshot::Sender<Result<()>>,
    },
    FingerprintCatalog {
        reply: oneshot::Sender<Result<crate::secrets::Catalog>>,
    },
    Branch {
        change: crate::branch::Change,
        reply: oneshot::Sender<Result<crate::branch::Branch>>,
    },
    ConfirmFingerprints {
        seal: crate::secrets::Seal,
        reply: oneshot::Sender<Result<()>>,
    },
    RegisterFingerprint {
        request: crate::secrets::Registration,
        fingerprint: crate::progress::Fingerprint,
        reply: oneshot::Sender<Result<crate::secrets::Catalog>>,
    },
    ReadFingerprints {
        track: String,
        session: String,
        reply: oneshot::Sender<Result<Vec<crate::progress::Fingerprint>>>,
    },
    RepairStart {
        request: crate::repair_run::Request,
        reply: oneshot::Sender<Result<crate::repair_run::Run>>,
    },
    RepairControl {
        command: crate::repair_run::Command,
        reply: oneshot::Sender<Result<crate::repair_run::Run>>,
    },
    RepairFollowup {
        request: crate::repair_run::FollowUp,
        reply: oneshot::Sender<Result<crate::repair_run::Run>>,
    },
    RepairRefresh {
        request: crate::repair_run::RefreshRequest,
        reply: oneshot::Sender<Result<crate::repair_run::Run>>,
    },
    RepairFinishRefresh {
        operation: uuid::Uuid,
        reply: oneshot::Sender<Result<Option<crate::integration::Job>>>,
    },
    RepairCompleteRefresh {
        operation: uuid::Uuid,
        reply: oneshot::Sender<Result<()>>,
    },
    RepairBeginRestore {
        operation: uuid::Uuid,
        id: uuid::Uuid,
        track: String,
        snapshot: uuid::Uuid,
        reply: oneshot::Sender<Result<crate::repair_run::Run>>,
    },
    RepairFinishRestore {
        operation: uuid::Uuid,
        result: crate::deletion::Restored,
        reply: oneshot::Sender<Result<()>>,
    },
    RepairTransition {
        op: uuid::Uuid,
        id: uuid::Uuid,
        revision: u64,
        event: crate::repair_run::Event,
        reply: oneshot::Sender<Result<crate::repair_run::Run>>,
    },
    RepairInput {
        id: uuid::Uuid,
        reply: oneshot::Sender<
            Result<(
                crate::repair_run::Run,
                crate::integration::Job,
                crate::candidate::Settings,
            )>,
        >,
    },
    RepairBeginApply {
        id: uuid::Uuid,
        revision: u64,
        reply: oneshot::Sender<
            Result<(
                crate::repair_run::Run,
                crate::integration::Job,
                crate::candidate::Settings,
            )>,
        >,
    },
    RepairApplied {
        id: uuid::Uuid,
        revision: u64,
        result: Box<std::result::Result<crate::candidate::Candidate, String>>,
        reply: oneshot::Sender<Result<crate::repair_run::Run>>,
    },
    PendingPublications {
        reply: oneshot::Sender<Result<Vec<crate::integration::Job>>>,
    },
    IntegrationPublished {
        id: uuid::Uuid,
        revision: u64,
        outcome: crate::publication::Outcome,
        reply: oneshot::Sender<Result<crate::integration::Job>>,
    },
    IntegrationStageBegin {
        id: uuid::Uuid,
        revision: u64,
        reply: oneshot::Sender<Result<bool>>,
    },
    IntegrationSettings {
        id: uuid::Uuid,
        revision: u64,
        reply: oneshot::Sender<Result<Option<crate::candidate::Settings>>>,
    },
    SaveIntegrationSettings {
        id: uuid::Uuid,
        revision: u64,
        settings: crate::candidate::Settings,
        reply: oneshot::Sender<Result<()>>,
    },
    IntegrationCandidate {
        id: uuid::Uuid,
        revision: u64,
        result: Box<std::result::Result<crate::candidate::Candidate, String>>,
        reply: oneshot::Sender<Result<crate::integration::Job>>,
    },
    IntegrationFrozen {
        id: uuid::Uuid,
        revision: u64,
        plan: crate::integration::Plan,
        reply: oneshot::Sender<Result<crate::integration::Job>>,
    },
    IntegrationCommand {
        command: crate::integration::Command,
        reply: oneshot::Sender<Result<crate::integration::Job>>,
    },
    IntegrationPlanned {
        id: uuid::Uuid,
        revision: u64,
        result: std::result::Result<crate::integration::Plan, String>,
        reply: oneshot::Sender<Result<crate::integration::Job>>,
    },
    RebuildEffects {
        reply: oneshot::Sender<Result<()>>,
    },
    InterruptEffects {
        session: String,
        reply: oneshot::Sender<Result<()>>,
    },
    ReadWorkspace {
        track: String,
        reply: oneshot::Sender<Result<crate::preparation::Workspace>>,
    },
    SaveDeletion {
        snapshot: crate::deletion::Snapshot,
        reply: oneshot::Sender<Result<()>>,
    },
    ReadDeletion {
        id: uuid::Uuid,
        reply: oneshot::Sender<Result<crate::deletion::Snapshot>>,
    },
    Stops {
        id: String,
        stops: crate::policy::Stops,
        reply: oneshot::Sender<Result<crate::policy::Stops>>,
    },
    Policy {
        track: String,
        workspace: std::path::PathBuf,
        hosts: Vec<String>,
        request: Value,
        deletion: Option<crate::deletion::Snapshot>,
        reply: oneshot::Sender<Result<crate::policy::Verdict>>,
    },
    Msp {
        track: String,
        session: String,
        stamp: crate::clock::Stamp,
        frame: Value,
        lease: Option<crate::output_flow::Lease>,
        source: Option<i64>,
        reply: oneshot::Sender<Result<i64>>,
    },
    MspRaw {
        track: String,
        session: String,
        frames: Vec<RawFrame>,
        reply: oneshot::Sender<Result<Vec<i64>>>,
    },
    ViewCursor {
        track: String,
        session: String,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    ViewSeen {
        session: String,
        cursor: String,
        reply: oneshot::Sender<Result<bool>>,
    },
    ViewFloor {
        session: String,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    ViewGap {
        track: String,
        session: String,
        reply: oneshot::Sender<Result<Option<crate::history::Gap>>>,
    },
    GapFilled {
        track: String,
        session: String,
        through: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    ViewCompacted {
        track: String,
        session: String,
        boundary: String,
        source: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    ViewSnapshot {
        track: String,
        session: String,
        source: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    OpeningSnapshot {
        track: String,
        session: String,
        source: i64,
        covered_gap: Option<i64>,
        reply: oneshot::Sender<Result<bool>>,
    },
    ReconcileLocal {
        session: String,
        reply: oneshot::Sender<Result<crate::reconcile::Local>>,
    },
    ReconcileRecord {
        session: String,
        summary: Value,
        retired: Vec<uuid::Uuid>,
        reply: oneshot::Sender<Result<()>>,
    },
    ReconcileReplay {
        session: String,
        turn: uuid::Uuid,
        reply: oneshot::Sender<Result<Vec<(Value, i64)>>>,
    },
    ClipDetail {
        id: String,
        reply: oneshot::Sender<Result<Value>>,
    },
    TimelineWindow {
        request: crate::timeline::Window,
        reply: oneshot::Sender<Result<Value>>,
    },
    Output {
        session: String,
        item: String,
        reference: String,
        value: Value,
        reply: oneshot::Sender<Result<()>>,
    },
    ObserveReport {
        report: crate::reports::Report,
        reply: oneshot::Sender<Result<crate::reports::Observation>>,
    },
    AnalyzeReport {
        id: String,
        revision: i64,
        analysis: crate::reports::Analysis,
        reply: oneshot::Sender<Result<i64>>,
    },
    RespondReport {
        id: uuid::Uuid,
        question: String,
        revision: i64,
        answer: crate::reports::Answer,
        reply: oneshot::Sender<Result<Value>>,
    },
    ReserveTurn {
        id: String,
        input: String,
        revision: i64,
        track: String,
        params: Value,
        reply: oneshot::Sender<Result<Admission>>,
    },
    InputExecution {
        input: String,
        revision: i64,
        state: String,
        reply: oneshot::Sender<Result<()>>,
    },
    StopLostWorker {
        id: uuid::Uuid,
        state: crate::runtime::WorkerState,
        turn: Option<uuid::Uuid>,
        reply: oneshot::Sender<Result<()>>,
    },
    RetryWorker {
        id: String,
        track: String,
        reply: oneshot::Sender<Result<Admission>>,
    },
    WorkerConfig {
        id: String,
        config: crate::runtime::Config,
        reply: oneshot::Sender<Result<Admission>>,
    },
    ReadWorkerConfig {
        reply: oneshot::Sender<Result<Option<crate::runtime::Config>>>,
    },
    WorkerState {
        state: crate::runtime::WorkerState,
        reply: oneshot::Sender<Result<()>>,
    },
    BindSession {
        id: String,
        slug: String,
        reply: oneshot::Sender<Result<()>>,
    },
    WorkspaceResult {
        workspace: crate::preparation::Workspace,
        reply: oneshot::Sender<Result<()>>,
    },
    RetryPreparation {
        id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    ReadOperation {
        id: String,
        reply: oneshot::Sender<Result<Option<Value>>>,
    },
    ProjectConfig {
        id: String,
        project: crate::project::Project,
        reply: oneshot::Sender<Result<Value>>,
    },
    PrepareProject {
        id: String,
        project: crate::project::Project,
        reply: oneshot::Sender<Result<Admission>>,
    },
    PendingProject {
        reply: oneshot::Sender<Result<Option<(uuid::Uuid, crate::project::Project)>>>,
    },
    EditorConfig {
        id: String,
        config: Value,
        reply: oneshot::Sender<Result<Admission>>,
    },
    ReadEditorConfig {
        reply: oneshot::Sender<Result<Option<Value>>>,
    },
    StageUtterance {
        id: String,
        text: String,
        target: Target,
        selected: Option<String>,
        attachments: Vec<uuid::Uuid>,
        reply: oneshot::Sender<Result<Value>>,
    },
    SaveAttachment {
        stored: crate::attachment_store::Stored,
        reply: oneshot::Sender<Result<()>>,
    },
    InputAttachments {
        input: String,
        track: String,
        session: String,
        reply: oneshot::Sender<Result<Option<crate::attachment_store::CopyPlan>>>,
    },
    FrozenAttachments {
        owner: uuid::Uuid,
        workspace: crate::preparation::Workspace,
        reply: oneshot::Sender<Result<Option<crate::attachment_store::CopyPlan>>>,
    },
    SessionAttachments {
        session: String,
        track: String,
        reply: oneshot::Sender<Result<Option<crate::attachment_store::CopyPlan>>>,
    },
    ClaimInput {
        reply: oneshot::Sender<Result<Option<PendingInput>>>,
    },
    ApplyInput {
        id: String,
        revision: i64,
        outcome: Outcome,
        reply: oneshot::Sender<Result<()>>,
    },
    CutInput {
        id: String,
        input: String,
        reply: oneshot::Sender<Result<Value>>,
    },
    HoldInputs {
        id: String,
        track: String,
        queued_at: std::time::Instant,
        reply: oneshot::Sender<Result<()>>,
    },
    StartHeldInput {
        id: String,
        input: String,
        revision: i64,
        reply: oneshot::Sender<Result<Value>>,
    },
    RecoverInputs {
        reply: oneshot::Sender<Result<()>>,
    },
    ResolveInput {
        id: String,
        input: String,
        revision: i64,
        resolution: Resolution,
        reply: oneshot::Sender<Result<Value>>,
    },
    Snapshot {
        reply: oneshot::Sender<Result<Value>>,
    },
    Intent {
        id: String,
        kind: String,
        payload: Value,
        reply: oneshot::Sender<Result<Admission>>,
    },
    Append {
        kind: String,
        payload: Value,
        reply: oneshot::Sender<Result<i64>>,
    },
    Finish {
        id: String,
        state: String,
        reply: oneshot::Sender<Result<()>>,
    },
}

impl Job {
    fn allowed_while_control_is_unsaved(&self) -> bool {
        #[cfg(all(feature = "fault-injection", debug_assertions))]
        if matches!(self, Self::TestFault { .. }) {
            return true;
        }
        matches!(
            self,
            Self::HoldInputs { .. }
                | Self::DispatchPause { .. }
                | Self::Snapshot { .. }
                | Self::PauseHistory { .. }
                | Self::ReadOperation { .. }
                | Self::ReadWorkerConfig { .. }
                | Self::ReadEditorConfig { .. }
                | Self::ReadWorkspace { .. }
                | Self::ReadDeletion { .. }
                | Self::PendingProject { .. }
                | Self::ProjectUses { .. }
                | Self::FingerprintCatalog { .. }
                | Self::ReadFingerprints { .. }
                | Self::FingerprintRecovery { .. }
                | Self::BackupReview { .. }
                | Self::BackupApplyRead { .. }
                | Self::BackupResume { .. }
                | Self::SummaryTasks { .. }
                | Self::PickupCheckpoint { .. }
                | Self::RepairInput { .. }
                | Self::PendingPublications { .. }
                | Self::IntegrationSettings { .. }
                | Self::ViewCursor { .. }
                | Self::ViewSeen { .. }
                | Self::ViewFloor { .. }
                | Self::ViewGap { .. }
                | Self::ReconcileLocal { .. }
                | Self::ReconcileReplay { .. }
                | Self::ClipDetail { .. }
                | Self::TimelineWindow { .. }
        )
    }
}

fn persist_control(connection: &mut Connection, job: Job, unlogged: bool) -> Option<Job> {
    let queued_at = match &job {
        Job::HoldInputs { queued_at, .. } | Job::DispatchPause { queued_at, .. } => queued_at,
        _ => unreachable!("only stop controls may be deferred"),
    };
    // The control already took effect in memory. Make a long writer stall
    // visible even if SQLite accepts the first eventual write.
    let unlogged = unlogged || queued_at.elapsed() >= std::time::Duration::from_secs(1);
    let result = match &job {
        Job::HoldInputs { id, track, .. } => hold_inputs(connection, id, track, unlogged),
        Job::DispatchPause { id, paused, .. } => {
            crate::dispatch_control::set(connection, id, *paused, unlogged)
        }
        _ => unreachable!("only stop controls may be deferred"),
    };
    if result.as_ref().err().is_some_and(|error| matches!(error.downcast_ref::<rusqlite::Error>(),Some(rusqlite::Error::SqliteFailure(code,_)) if code.code==rusqlite::ErrorCode::DiskFull)) {
        return Some(job);
    }
    match job {
        Job::HoldInputs { reply, .. } | Job::DispatchPause { reply, .. } => {
            let _ = reply.send(result);
        }
        _ => unreachable!(),
    }
    None
}
fn flush_controls(connection: &mut Connection, pending: &mut std::collections::VecDeque<Job>) {
    while let Some(job) = pending.pop_front() {
        if let Some(job) = persist_control(connection, job, true) {
            pending.push_front(job);
            break;
        }
    }
}

#[derive(Clone)]
pub struct Journal {
    secret_key: std::sync::Arc<crate::secrets::Key>,
    attachment_root: std::sync::Arc<PathBuf>,
    jobs: mpsc::Sender<Job>,
    changes: watch::Receiver<i64>,
}

impl Journal {
    pub(crate) fn write_unavailable(&self, error: &(dyn std::error::Error + 'static)) -> bool {
        !self.jobs.is_closed()
            && (error.is::<oneshot::error::RecvError>()
                || matches!(error.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::SqliteFailure(code, _))
                if matches!(code.code, rusqlite::ErrorCode::DiskFull | rusqlite::ErrorCode::SystemIoFailure | rusqlite::ErrorCode::ReadOnly | rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)))
    }

    pub async fn import_attachment(
        &self,
        id: uuid::Uuid,
        path: PathBuf,
    ) -> Result<crate::attachment_store::Attachment> {
        let store = crate::attachment_store::Store {
            root: (*self.attachment_root).clone(),
            key: self.secret_key.clone(),
        };
        let stored = tokio::task::spawn_blocking(move || store.import(id, &path)).await??;
        let attachment = stored.attachment.clone();
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::SaveAttachment { stored, reply })
            .await?;
        result.await??;
        Ok(attachment)
    }
    pub async fn materialize_input_attachments(
        &self,
        input: String,
        track: String,
        session: String,
        path: PathBuf,
    ) -> Result<crate::attachment_store::ReferenceContext> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::InputAttachments {
                input,
                track,
                session,
                reply,
            })
            .await?;
        let Some(plan) = result.await?? else {
            return Ok(crate::attachment_store::ReferenceContext {
                files: vec![],
                directory: None,
            });
        };
        let crate::attachment_store::CopyPlan {
            workspace,
            originals: records,
        } = plan;
        let store = crate::attachment_store::Store {
            root: (*self.attachment_root).clone(),
            key: self.secret_key.clone(),
        };
        tokio::task::spawn_blocking(move || {
            if workspace.path.canonicalize()? != path.canonicalize()? {
                return Err("The worker has a different attachment workspace.".into());
            }
            let references = store.materialize(&workspace, &records)?;
            Ok(crate::attachment_store::ReferenceContext {
                files: references,
                directory: workspace
                    .attachment_roots
                    .first()
                    .map(|root| workspace.path.join(root)),
            })
        })
        .await?
    }
    pub async fn restore_frozen_attachments(
        &self,
        owner: uuid::Uuid,
        workspace: crate::preparation::Workspace,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::FrozenAttachments {
                owner,
                workspace,
                reply,
            })
            .await?;
        self.restore_attachment_plan(result.await??, None).await
    }
    pub async fn restore_session_attachments(
        &self,
        session: String,
        track: String,
        path: PathBuf,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::SessionAttachments {
                session,
                track,
                reply,
            })
            .await?;
        self.restore_attachment_plan(result.await??, Some(path))
            .await
    }
    async fn restore_attachment_plan(
        &self,
        plan: Option<crate::attachment_store::CopyPlan>,
        path: Option<PathBuf>,
    ) -> Result<()> {
        let Some(plan) = plan else {
            return Ok(());
        };
        let store = crate::attachment_store::Store {
            root: (*self.attachment_root).clone(),
            key: self.secret_key.clone(),
        };
        tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(path) = path
                && plan.workspace.path.canonicalize()? != path.canonicalize()?
            {
                return Err("The attachment workspace changed.".into());
            }
            store.materialize(&plan.workspace, &plan.originals)?;
            Ok(())
        })
        .await?
    }
    pub(crate) async fn backup_apply(
        &self,
        id: Option<uuid::Uuid>,
    ) -> Result<Option<crate::backup_apply::State>> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::BackupApplyRead { id, reply }).await?;
        result.await?
    }
    pub(crate) async fn change_backup_apply(
        &self,
        change: crate::backup_apply::Change,
    ) -> Result<crate::backup_apply::State> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::BackupApplyChange { change, reply })
            .await?;
        result.await?
    }
    pub async fn backup_apply_recovery(
        &self,
        receipt: Option<crate::fingerprint_receipt::Receipt>,
    ) -> Result<Option<serde_json::Value>> {
        let Some(state) = self.backup_apply(None).await? else {
            return Ok(None);
        };
        if !receipt.as_ref().is_some_and(|r| state.accepts(r)) {
            return Err("The backup recovery confirmation changed.".into());
        }
        Ok(Some(state.recovery()))
    }
    pub(crate) async fn backup_resume(
        &self,
        track: &str,
    ) -> Result<Option<(crate::session_retirement::Resume, bool)>> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::BackupResume {
                track: track.into(),
                reply,
            })
            .await?;
        result.await?
    }
    pub(crate) async fn backup_resume_opened(&self, track: &str, session: &str) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::BackupResumeOpened {
                track: track.into(),
                session: session.into(),
                reply,
            })
            .await?;
        result.await?
    }
    pub(crate) async fn save_backup_choice(
        &self,
        receipt: crate::fingerprint_receipt::Receipt,
        record: crate::backup_choices::Record,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::SaveBackupChoice {
                receipt,
                record,
                reply,
            })
            .await?;
        result.await?
    }
    pub(crate) async fn save_backup_files(&self, id: uuid::Uuid, payload: Value) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::SaveBackupFiles { id, payload, reply })
            .await?;
        result.await?
    }
    pub(crate) fn attachment_store(&self) -> crate::attachment_store::Store {
        crate::attachment_store::Store {
            root: (*self.attachment_root).clone(),
            key: self.secret_key.clone(),
        }
    }
    pub(crate) fn backup_file_tag(&self, material: &[u8]) -> String {
        let mut bytes = b"intercat.backup-files.v1\0".to_vec();
        bytes.extend_from_slice(material);
        self.secret_key
            .tag(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
    pub async fn backup_review(
        &self,
        receipt: crate::fingerprint_receipt::Receipt,
    ) -> Result<crate::backup_review::Plan> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::BackupReview { receipt, reply }).await?;
        result.await?
    }
    pub async fn summary_tasks(&self, id: uuid::Uuid) -> Result<Vec<uuid::Uuid>> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::SummaryTasks { id, reply }).await?;
        result.await?
    }
    pub async fn cancel_pickup(
        &self,
        request: crate::pickup::CancelRequest,
    ) -> Result<crate::pickup::Recovery> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::CancelPickup { request, reply }).await?;
        result.await?
    }
    pub async fn summary_plan(&self, id: uuid::Uuid, input: Value) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::SummaryPlan { id, input, reply })
            .await?;
        result.await?
    }
    pub async fn summary_next(&self, id: uuid::Uuid) -> Result<crate::handoff::Next> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::SummaryNext { id, reply }).await?;
        result.await?
    }
    pub async fn summary_change(
        &self,
        change: crate::handoff::Change,
    ) -> Result<crate::handoff::Task> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::SummaryChange { change, reply }).await?;
        result.await?
    }
    pub async fn pickup(&self, change: crate::pickup::Change) -> Result<crate::pickup::Recovery> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::Pickup { change, reply }).await?;
        result.await?
    }
    pub async fn pickup_checkpoint(
        &self,
        track: String,
        session: String,
        checkpoint: uuid::Uuid,
    ) -> Result<bool> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::PickupCheckpoint {
                track,
                session,
                checkpoint,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn check_project_secrets(
        &self,
        request: crate::project_secrets::Prepare,
    ) -> Result<()> {
        let (id, manifest) = request.metadata(&self.secret_key)?;
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ProjectSecretsCheck {
                id,
                manifest,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn hold_project_secrets(&self) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::ProjectSecretsHold { reply }).await?;
        result.await?
    }
    pub async fn project_host_use(&self, host: crate::project_secret_usage::HostUse) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::ProjectHostUse { host, reply }).await?;
        result.await?
    }
    pub async fn project_uses(&self) -> Result<Vec<crate::project_secret_usage::Record>> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::ProjectUses { reply }).await?;
        result.await?
    }
    pub async fn project_use_closed(&self, nonce: String) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ProjectUseClosed { nonce, reply })
            .await?;
        result.await?
    }
    pub async fn project_use_kept(
        &self,
        decision: crate::project_secret_usage::Decision,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ProjectUseKept { decision, reply })
            .await?;
        result.await?
    }
    pub async fn prepare_project_secrets(
        &self,
        request: crate::project_secrets::Prepare,
    ) -> Result<crate::secrets::Catalog> {
        let (id, manifest) = request.metadata(&self.secret_key)?;
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ProjectSecretsPrepare {
                id,
                manifest,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn activate_project_secrets(
        &self,
        request: crate::project_secrets::Load,
    ) -> Result<crate::project_secrets::Config> {
        let config = request.prepare(&self.secret_key)?;
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ProjectSecretsActivate {
                manifest: config.manifest.clone(),
                reply,
            })
            .await?;
        result.await??;
        Ok(config)
    }
    pub async fn secret_settled(&self, id: uuid::Uuid) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::SecretSettled { id, reply }).await?;
        result.await?
    }
    pub async fn secret_accepted(&self, id: uuid::Uuid) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::SecretAccepted { id, reply }).await?;
        result.await?
    }

    pub async fn prepare_secret(
        &self,
        request: crate::secret_input::Request,
    ) -> Result<Option<crate::secret_input::Install>> {
        let install = request.prepare(&self.secret_key)?;
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::PrepareSecret {
                target: install.target.clone(),
                reply,
            })
            .await?;
        Ok(result.await??.then_some(install))
    }
    pub async fn secret_restarting(&self, id: uuid::Uuid) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::SecretRestarting { id, reply }).await?;
        result.await?
    }
    pub async fn queue_repair_secret(
        &self,
        target: crate::secret_input::Target,
        checkpoint: crate::checkpoint::Checkpoint,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::QueueRepairSecret {
                target,
                checkpoint,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn secret_ready(
        &self,
        target: crate::secret_input::Target,
        host: String,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::SecretReady {
                target,
                host,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn secret_failed(&self, id: uuid::Uuid) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::SecretFailed { id, reply }).await?;
        result.await?
    }
    pub async fn secret_expired(&self, host: String) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::SecretExpired { host, reply }).await?;
        result.await?
    }

    pub async fn fingerprint_catalog(&self) -> Result<crate::secrets::Catalog> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::FingerprintCatalog { reply }).await?;
        result.await?
    }
    pub async fn rebuild_fingerprints(
        &self,
        request: crate::fingerprint_recovery::Request,
    ) -> Result<Value> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RebuildFingerprints { request, reply })
            .await?;
        result.await?
    }
    pub async fn fingerprint_recovery(
        &self,
        receipt: Option<crate::fingerprint_receipt::Receipt>,
    ) -> Result<Option<Value>> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::FingerprintRecovery { receipt, reply })
            .await?;
        result.await?
    }
    pub async fn branch(&self, change: crate::branch::Change) -> Result<crate::branch::Branch> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::Branch { change, reply }).await?;
        result.await?
    }
    pub async fn confirm_fingerprints(&self, seal: crate::secrets::Seal) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ConfirmFingerprints { seal, reply })
            .await?;
        result.await?
    }

    pub async fn register_fingerprint(
        &self,
        request: crate::secrets::Registration,
    ) -> Result<crate::secrets::Catalog> {
        let fingerprint = request.fingerprint(&self.secret_key)?;
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RegisterFingerprint {
                request,
                fingerprint,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn session_mask(&self, track: &str, session: &str) -> Result<crate::progress::Mask> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ReadFingerprints {
                track: track.into(),
                session: session.into(),
                reply,
            })
            .await?;
        self.secret_key.mask(result.await??)
    }

    pub async fn pending_publications(&self) -> Result<Vec<crate::integration::Job>> {
        let (reply, received) = oneshot::channel();
        self.jobs.send(Job::PendingPublications { reply }).await?;
        received.await?
    }
    pub async fn integration_published(
        &self,
        id: uuid::Uuid,
        revision: u64,
        outcome: crate::publication::Outcome,
    ) -> Result<crate::integration::Job> {
        let (reply, received) = oneshot::channel();
        self.jobs
            .send(Job::IntegrationPublished {
                id,
                revision,
                outcome,
                reply,
            })
            .await?;
        received.await?
    }
    pub async fn integration_stage_begin(&self, id: uuid::Uuid, revision: u64) -> Result<bool> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::IntegrationStageBegin {
                id,
                revision,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn repair_start(
        &self,
        request: crate::repair_run::Request,
    ) -> Result<crate::repair_run::Run> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::RepairStart { request, reply }).await?;
        result.await?
    }
    pub async fn repair_control(
        &self,
        command: crate::repair_run::Command,
    ) -> Result<crate::repair_run::Run> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RepairControl { command, reply })
            .await?;
        result.await?
    }
    pub async fn repair_followup(
        &self,
        request: crate::repair_run::FollowUp,
    ) -> Result<crate::repair_run::Run> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RepairFollowup { request, reply })
            .await?;
        result.await?
    }
    pub async fn repair_refresh(
        &self,
        request: crate::repair_run::RefreshRequest,
    ) -> Result<crate::repair_run::Run> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RepairRefresh { request, reply })
            .await?;
        result.await?
    }
    pub async fn repair_finish_refresh(
        &self,
        operation: uuid::Uuid,
    ) -> Result<Option<crate::integration::Job>> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RepairFinishRefresh { operation, reply })
            .await?;
        result.await?
    }
    pub async fn repair_complete_refresh(&self, operation: uuid::Uuid) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RepairCompleteRefresh { operation, reply })
            .await?;
        result.await?
    }
    pub async fn repair_begin_restore(
        &self,
        operation: uuid::Uuid,
        id: uuid::Uuid,
        track: String,
        snapshot: uuid::Uuid,
    ) -> Result<crate::repair_run::Run> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RepairBeginRestore {
                operation,
                id,
                track,
                snapshot,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn repair_finish_restore(
        &self,
        operation: uuid::Uuid,
        restored: crate::deletion::Restored,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RepairFinishRestore {
                operation,
                result: restored,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn repair_transition(
        &self,
        op: uuid::Uuid,
        id: uuid::Uuid,
        revision: u64,
        event: crate::repair_run::Event,
    ) -> Result<crate::repair_run::Run> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RepairTransition {
                op,
                id,
                revision,
                event,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn repair_input(
        &self,
        id: uuid::Uuid,
    ) -> Result<(
        crate::repair_run::Run,
        crate::integration::Job,
        crate::candidate::Settings,
    )> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::RepairInput { id, reply }).await?;
        result.await?
    }
    pub async fn repair_begin_apply(
        &self,
        id: uuid::Uuid,
        revision: u64,
    ) -> Result<(
        crate::repair_run::Run,
        crate::integration::Job,
        crate::candidate::Settings,
    )> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RepairBeginApply {
                id,
                revision,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn repair_applied(
        &self,
        id: uuid::Uuid,
        revision: u64,
        candidate: std::result::Result<crate::candidate::Candidate, String>,
    ) -> Result<crate::repair_run::Run> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RepairApplied {
                id,
                revision,
                result: Box::new(candidate),
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn integration_settings(
        &self,
        id: uuid::Uuid,
        revision: u64,
    ) -> Result<Option<crate::candidate::Settings>> {
        let (reply, received) = oneshot::channel();
        self.jobs
            .send(Job::IntegrationSettings {
                id,
                revision,
                reply,
            })
            .await?;
        received.await?
    }
    pub async fn integration_save_settings(
        &self,
        id: uuid::Uuid,
        revision: u64,
        settings: crate::candidate::Settings,
    ) -> Result<()> {
        let (reply, received) = oneshot::channel();
        self.jobs
            .send(Job::SaveIntegrationSettings {
                id,
                revision,
                settings,
                reply,
            })
            .await?;
        received.await?
    }
    pub async fn integration_candidate(
        &self,
        id: uuid::Uuid,
        revision: u64,
        result: std::result::Result<crate::candidate::Candidate, String>,
    ) -> Result<crate::integration::Job> {
        let (reply, received) = oneshot::channel();
        self.jobs
            .send(Job::IntegrationCandidate {
                id,
                revision,
                result: Box::new(result),
                reply,
            })
            .await?;
        received.await?
    }
    pub async fn integration_frozen(
        &self,
        id: uuid::Uuid,
        revision: u64,
        plan: crate::integration::Plan,
    ) -> Result<crate::integration::Job> {
        let (reply, received) = oneshot::channel();
        self.jobs
            .send(Job::IntegrationFrozen {
                id,
                revision,
                plan,
                reply,
            })
            .await?;
        received.await?
    }
    pub async fn integration_command(
        &self,
        command: crate::integration::Command,
    ) -> Result<crate::integration::Job> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::IntegrationCommand { command, reply })
            .await?;
        result.await?
    }
    pub async fn integration_planned(
        &self,
        id: uuid::Uuid,
        revision: u64,
        result: std::result::Result<crate::integration::Plan, String>,
    ) -> Result<crate::integration::Job> {
        let (reply, received) = oneshot::channel();
        self.jobs
            .send(Job::IntegrationPlanned {
                id,
                revision,
                result,
                reply,
            })
            .await?;
        received.await?
    }
    pub async fn rebuild_effects(&self) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::RebuildEffects { reply }).await?;
        result.await?
    }
    pub async fn interrupt_effects(&self, session: String) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::InterruptEffects { session, reply })
            .await?;
        result.await?
    }
    pub async fn workspace(&self, track: String) -> Result<crate::preparation::Workspace> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::ReadWorkspace { track, reply }).await?;
        result.await?
    }
    pub async fn save_deletion(&self, snapshot: crate::deletion::Snapshot) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::SaveDeletion { snapshot, reply })
            .await?;
        result.await?
    }
    pub async fn deletion(&self, id: uuid::Uuid) -> Result<crate::deletion::Snapshot> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::ReadDeletion { id, reply }).await?;
        result.await?
    }
    pub async fn set_stops(
        &self,
        id: uuid::Uuid,
        stops: crate::policy::Stops,
    ) -> Result<crate::policy::Stops> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::Stops {
                id: id.to_string(),
                stops,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn evaluate_approval(
        &self,
        track: String,
        workspace: std::path::PathBuf,
        hosts: Vec<String>,
        request: Value,
        deletion: Option<crate::deletion::Snapshot>,
    ) -> Result<crate::policy::Verdict> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::Policy {
                track,
                workspace,
                hosts,
                request,
                deletion,
                reply,
            })
            .await?;
        result.await?
    }
    pub fn try_msp(
        &self,
        track: String,
        session: String,
        stamp: crate::clock::Stamp,
        frame: Value,
        lease: Option<crate::output_flow::Lease>,
        source: Option<i64>,
    ) -> Result<oneshot::Receiver<Result<i64>>> {
        let (reply, result) = oneshot::channel();
        self.jobs.try_send(Job::Msp {
            track,
            session,
            stamp,
            frame,
            lease,
            source,
            reply,
        })?;
        Ok(result)
    }
    pub async fn msp_raw(
        &self,
        track: String,
        session: String,
        stamp: crate::clock::Stamp,
        frame: std::sync::Arc<Value>,
        lease: Option<crate::output_flow::Lease>,
    ) -> Result<i64> {
        self.msp_raw_batch(
            track,
            session,
            vec![RawFrame {
                stamp,
                frame,
                lease,
            }],
        )
        .await?
        .pop()
        .ok_or_else(|| "The raw receipt was not saved.".into())
    }
    pub async fn msp_raw_batch(
        &self,
        track: String,
        session: String,
        frames: Vec<RawFrame>,
    ) -> Result<Vec<i64>> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::MspRaw {
                track,
                session,
                frames,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn view_cursor(&self, track: String, session: String) -> Result<Option<String>> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ViewCursor {
                track,
                session,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn view_seen(&self, session: String, cursor: String) -> Result<bool> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ViewSeen {
                session,
                cursor,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn view_floor(&self, session: String) -> Result<Option<String>> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::ViewFloor { session, reply }).await?;
        result.await?
    }
    pub async fn view_gap(
        &self,
        track: String,
        session: String,
    ) -> Result<Option<crate::history::Gap>> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ViewGap {
                track,
                session,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn gap_filled(&self, track: String, session: String, through: i64) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::GapFilled {
                track,
                session,
                through,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn view_compacted(
        &self,
        track: String,
        session: String,
        boundary: String,
        source: i64,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ViewCompacted {
                track,
                session,
                boundary,
                source,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn view_snapshot(&self, track: String, session: String, source: i64) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ViewSnapshot {
                track,
                session,
                source,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn opening_snapshot(
        &self,
        track: String,
        session: String,
        source: i64,
        covered_gap: Option<i64>,
    ) -> Result<bool> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::OpeningSnapshot {
                track,
                session,
                source,
                covered_gap,
                reply,
            })
            .await?;
        result.await?
    }

    pub async fn reconcile_local(&self, session: String) -> Result<crate::reconcile::Local> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ReconcileLocal { session, reply })
            .await?;
        result.await?
    }
    pub async fn reconcile_record(
        &self,
        session: String,
        summary: Value,
        retired: Vec<uuid::Uuid>,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ReconcileRecord {
                session,
                summary,
                retired,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn reconcile_replay(
        &self,
        session: String,
        turn: uuid::Uuid,
    ) -> Result<Vec<(Value, i64)>> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ReconcileReplay {
                session,
                turn,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn clip_detail(&self, id: String) -> Result<Value> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::ClipDetail { id, reply }).await?;
        result.await?
    }
    pub async fn timeline_window(&self, request: crate::timeline::Window) -> Result<Value> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::TimelineWindow { request, reply })
            .await?;
        result.await?
    }
    pub async fn cache_output(
        &self,
        session: String,
        item: String,
        reference: String,
        value: Value,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::Output {
                session,
                item,
                reference,
                value,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn observe_report(
        &self,
        report: crate::reports::Report,
    ) -> Result<crate::reports::Observation> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::ObserveReport { report, reply }).await?;
        result.await?
    }
    pub async fn analyze_report(
        &self,
        id: String,
        revision: i64,
        analysis: crate::reports::Analysis,
    ) -> Result<i64> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::AnalyzeReport {
                id,
                revision,
                analysis,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn respond_report(
        &self,
        id: uuid::Uuid,
        question: String,
        revision: i64,
        answer: crate::reports::Answer,
    ) -> Result<Value> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RespondReport {
                id,
                question,
                revision,
                answer,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn reserve_turn(
        &self,
        id: uuid::Uuid,
        input: String,
        revision: i64,
        track: String,
        params: Value,
    ) -> Result<Admission> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ReserveTurn {
                id: id.to_string(),
                input,
                revision,
                track,
                params,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn input_execution(&self, input: String, revision: i64, state: &str) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::InputExecution {
                input,
                revision,
                state: state.into(),
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn stop_lost_worker(
        &self,
        id: uuid::Uuid,
        state: crate::runtime::WorkerState,
        turn: Option<uuid::Uuid>,
    ) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::StopLostWorker {
                id,
                state,
                turn,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn retry_worker(&self, id: uuid::Uuid, track: String) -> Result<Admission> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RetryWorker {
                id: id.to_string(),
                track,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn worker_config(&self) -> Result<Option<crate::runtime::Config>> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::ReadWorkerConfig { reply }).await?;
        result.await?
    }
    pub async fn set_worker_config(
        &self,
        id: uuid::Uuid,
        config: crate::runtime::Config,
    ) -> Result<Admission> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::WorkerConfig {
                id: id.to_string(),
                config,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn worker_state(&self, state: crate::runtime::WorkerState) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::WorkerState { state, reply }).await?;
        result.await?
    }
    #[cfg(all(feature = "fault-injection", debug_assertions))]
    pub async fn test_fault(&self, request: crate::fault_injection::Request) -> Result<Value> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::TestFault { request, reply }).await?;
        result.await?
    }
    pub async fn pause_dispatch(&self, id: uuid::Uuid, paused: bool) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::DispatchPause {
                id: id.to_string(),
                paused,
                queued_at: std::time::Instant::now(),
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn pause_history(&self) -> Result<Vec<(uuid::Uuid, bool)>> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::PauseHistory { reply }).await?;
        result.await?
    }
    pub fn try_worker_state(
        &self,
        state: crate::runtime::WorkerState,
    ) -> Result<oneshot::Receiver<Result<()>>> {
        let (reply, result) = oneshot::channel();
        self.jobs.try_send(Job::WorkerState { state, reply })?;
        Ok(result)
    }
    pub fn subscribe(&self) -> watch::Receiver<i64> {
        self.changes.clone()
    }
    pub async fn bind_session(&self, id: String, slug: String) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::BindSession { id, slug, reply }).await?;
        result.await?
    }
    pub async fn workspace_result(&self, workspace: crate::preparation::Workspace) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::WorkspaceResult { workspace, reply })
            .await?;
        result.await?
    }
    pub async fn retry_preparation(&self, id: uuid::Uuid) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::RetryPreparation {
                id: id.to_string(),
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn operation(&self, id: uuid::Uuid) -> Result<Option<Value>> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ReadOperation {
                id: id.to_string(),
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn configure_project(
        &self,
        id: uuid::Uuid,
        project: crate::project::Project,
    ) -> Result<Value> {
        if self.prepare_project(id, project.clone()).await? == Admission::Replay("complete".into())
        {
            return Ok(serde_json::to_value(project)?);
        }
        project.pin().await?;
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ProjectConfig {
                id: id.to_string(),
                project,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn prepare_project(
        &self,
        id: uuid::Uuid,
        project: crate::project::Project,
    ) -> Result<Admission> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::PrepareProject {
                id: id.to_string(),
                project,
                reply,
            })
            .await?;
        result.await?
    }
    pub(crate) async fn pending_project(
        &self,
    ) -> Result<Option<(uuid::Uuid, crate::project::Project)>> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::PendingProject { reply }).await?;
        result.await?
    }
    pub async fn editor_config(&self) -> Result<Option<Value>> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::ReadEditorConfig { reply }).await?;
        result.await?
    }
    pub async fn set_editor_config(&self, id: uuid::Uuid, config: Value) -> Result<Admission> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::EditorConfig {
                id: id.to_string(),
                config,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn stage_utterance(
        &self,
        id: uuid::Uuid,
        text: String,
        target: Target,
        selected: Option<String>,
        attachments: Vec<uuid::Uuid>,
    ) -> Result<Value> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::StageUtterance {
                id: id.to_string(),
                text,
                target,
                selected,
                attachments,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn claim_input(&self) -> Result<Option<PendingInput>> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::ClaimInput { reply }).await?;
        result.await?
    }
    pub async fn apply_input(&self, id: String, revision: i64, outcome: Outcome) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ApplyInput {
                id,
                revision,
                outcome,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn cut_input(&self, id: uuid::Uuid, input: String) -> Result<Value> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::CutInput {
                id: id.to_string(),
                input,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn hold_inputs(&self, id: uuid::Uuid, track: String) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::HoldInputs {
                id: id.to_string(),
                track,
                queued_at: std::time::Instant::now(),
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn start_held_input(
        &self,
        id: uuid::Uuid,
        input: String,
        revision: i64,
    ) -> Result<Value> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::StartHeldInput {
                id: id.to_string(),
                input,
                revision,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn recover_inputs(&self) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::RecoverInputs { reply }).await?;
        result.await?
    }
    pub async fn resolve_input(
        &self,
        id: uuid::Uuid,
        input: String,
        revision: i64,
        resolution: Resolution,
    ) -> Result<Value> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::ResolveInput {
                id: id.to_string(),
                input,
                revision,
                resolution,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn snapshot(&self) -> Result<Value> {
        let (reply, result) = oneshot::channel();
        self.jobs.send(Job::Snapshot { reply }).await?;
        result.await?
    }
    pub fn try_append(&self, kind: &str, payload: Value) -> Result<oneshot::Receiver<Result<i64>>> {
        let (reply, result) = oneshot::channel();
        self.jobs.try_send(Job::Append {
            kind: kind.into(),
            payload,
            reply,
        })?;
        Ok(result)
    }
    pub async fn append(&self, kind: &str, payload: Value) -> Result<i64> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::Append {
                kind: kind.into(),
                payload,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn intent(&self, id: &str, kind: &str, payload: Value) -> Result<Admission> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::Intent {
                id: id.into(),
                kind: kind.into(),
                payload,
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn finish(&self, id: &str, state: &str) -> Result<()> {
        let (reply, result) = oneshot::channel();
        self.jobs
            .send(Job::Finish {
                id: id.into(),
                state: state.into(),
                reply,
            })
            .await?;
        result.await?
    }
    pub async fn open(path: impl AsRef<Path>, secret_key: crate::secrets::Key) -> Result<Self> {
        let secret_key = std::sync::Arc::new(secret_key);
        let key_check = secret_key.check();
        let pickup_key = secret_key.clone();
        let path = path.as_ref().to_path_buf();
        let attachment_root =
            std::sync::Arc::new(std::path::absolute(&path)?.with_file_name("attachments"));
        let (jobs, mut receiver) = mpsc::channel::<Job>(64);
        let (changed, changes) = watch::channel(0);
        let (ready, started) = oneshot::channel();
        thread::Builder::new()
            .name("intercat-journal".into())
            .spawn(move || {
                let mut connection = match open_connection(&path, &key_check) {
                    Ok(c) => {
                        let _ = ready.send(Ok(()));
                        c
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e));
                        return;
                    }
                };
                let mut pending_controls=std::collections::VecDeque::new();
                while let Some(job) = receiver.blocking_recv() {
                    flush_controls(&mut connection,&mut pending_controls);
                    if !pending_controls.is_empty() && !job.allowed_while_control_is_unsaved() {
                        // A pending Stop must reach durable storage before any later
                        // mutation can be accepted. Reads and recovery stay responsive.
                        match job {
                            Job::ClaimInput{reply} => {let _=reply.send(Ok(None));},
                            Job::ApplyInput{reply,..} => {let _=reply.send(Ok(()));},
                            _ => drop(job),
                        }
                        continue;
                    }
                    match job {
                        Job::BackupApplyRead{id,reply}=>{let _=reply.send(crate::backup_apply::read(&connection,id,&pickup_key));},
                        Job::BackupApplyChange{change,reply}=>{let _=reply.send(crate::backup_apply::change(&mut connection,change,&pickup_key));},
                        Job::BackupResume{track,reply}=>{let _=reply.send(crate::session_retirement::resume(&connection,&track));},
                        Job::BackupResumeOpened{track,session,reply}=>{let _=reply.send(crate::session_retirement::opened(&connection,&track,&session));},
                        Job::SaveBackupChoice{receipt,record,reply}=>{let _=reply.send(crate::backup_choices::save(&mut connection,&receipt,record,&pickup_key));},
                        Job::SaveBackupFiles{id,payload,reply}=>{let _=reply.send(crate::backup_review::save_files(&mut connection,id,payload));},
                        Job::BackupReview{receipt,reply}=>{let _=reply.send(crate::backup_review::plan(&connection,&receipt,&pickup_key));},
                        Job::SummaryTasks{id,reply}=>{let _=reply.send(crate::handoff::task_ids(&connection,id));},
                        Job::CancelPickup{request,reply}=>{let _=reply.send(crate::pickup::request_cancel(&mut connection,request));},
                        Job::SummaryPlan{id,input,reply}=>{let _=reply.send(crate::handoff::plan(&mut connection,id,input));},
                        Job::SummaryNext{id,reply}=>{let _=reply.send(crate::handoff::next(&mut connection,id));},
                        Job::SummaryChange{change,reply}=>{let _=reply.send(crate::handoff::change(&mut connection,change));},
                        Job::Pickup{change,reply}=>{let _=reply.send(crate::pickup::change(&mut connection,change,&pickup_key));},
                        Job::PickupCheckpoint{track,session,checkpoint,reply}=>{let _=reply.send(crate::pickup::inherited_checkpoint(&connection,&track,&session,checkpoint));},
                        Job::ProjectHostUse{host,reply}=>{let _=reply.send(crate::project_secret_usage::record(&connection,&host));},
                        Job::ProjectUses{reply}=>{let _=reply.send(crate::project_secret_usage::records(&connection));},
                        Job::ProjectUseClosed{nonce,reply}=>{let _=reply.send(crate::project_secret_usage::close(&connection,&nonce));},
                        Job::ProjectUseKept{decision,reply}=>{let _=reply.send(crate::project_secret_usage::keep(&connection,&decision));},
                        Job::ProjectSecretsCheck{id,manifest,reply}=>{let _=reply.send(crate::project_secrets::check(&mut connection,id,&manifest));},
                        Job::ProjectSecretsHold{reply}=>{let _=reply.send(crate::project_secrets::hold(&mut connection));},
                        Job::ProjectSecretsPrepare{id,manifest,reply}=>{let _=reply.send(crate::project_secrets::register(&mut connection,id,&manifest));},
                        Job::ProjectSecretsActivate{manifest,reply}=>{let _=reply.send(crate::project_secrets::activate(&mut connection,&manifest));},
                        Job::SecretSettled{id,reply}=>{let _=reply.send(crate::secret_input_store::settled(&mut connection,id));},
                        Job::SecretAccepted{id,reply}=>{let _=reply.send(crate::secret_input_store::accepted(&mut connection,id));},
                        Job::PrepareSecret{target,reply}=>{let _=reply.send(crate::secret_input_store::prepare(&mut connection,&target));},
                        Job::SecretRestarting{id,reply}=>{let _=reply.send(crate::secret_input_store::restarting(&connection,id));},
                        Job::QueueRepairSecret{target,checkpoint,reply}=>{let _=reply.send(crate::secret_input_store::queue_repair(&mut connection,&target,checkpoint));},
                        Job::SecretReady{target,host,reply}=>{let _=reply.send(crate::secret_input_store::ready(&mut connection,&target,&host));},
                        Job::SecretFailed{id,reply}=>{let _=reply.send(crate::secret_input_store::fail(&mut connection,id));},
                        Job::SecretExpired{host,reply}=>{let _=reply.send(crate::secret_input_store::expire(&mut connection,&host));},
                        Job::RebuildFingerprints{request,reply}=>{let _=reply.send(crate::fingerprint_recovery::rebuild(&mut connection,request));},
                        Job::FingerprintRecovery{receipt,reply}=>{let _=reply.send(crate::fingerprint_recovery::status(&connection,receipt.as_ref()));},
                        Job::FingerprintCatalog{reply}=>{let _=reply.send(crate::secret_store::catalog(&connection));},
                        Job::Branch{change,reply}=>{let _=reply.send(crate::branch::change(&mut connection,change));},
                        Job::ConfirmFingerprints{seal,reply}=>{let _=reply.send(crate::secret_store::confirm(&mut connection,&seal));},
                        Job::RegisterFingerprint{request,fingerprint,reply}=>{let _=reply.send(crate::secret_store::register(&mut connection,&request,&fingerprint));},
                        Job::ReadFingerprints{track,session,reply}=>{let _=reply.send(crate::secret_store::read(&connection,&track,&session));},
                        Job::RepairStart{request,reply}=>{let _=reply.send(crate::repair_run_store::start(&mut connection,request));},
                        Job::RepairControl{command,reply}=>{let _=reply.send(crate::repair_run_store::control(&mut connection,command));},
                        Job::RepairFollowup{request,reply}=>{let _=reply.send(crate::repair_run_store::followup(&mut connection,request));},
                        Job::RepairRefresh{request,reply}=>{let _=reply.send(crate::repair_run_store::begin_refresh(&mut connection,request));},
                        Job::RepairFinishRefresh{operation,reply}=>{let _=reply.send(crate::repair_run_store::finish_refresh(&mut connection,operation));},
                        Job::RepairCompleteRefresh{operation,reply}=>{let _=reply.send(crate::repair_run_store::complete_refresh(&mut connection,operation));},
                        Job::RepairBeginRestore{operation,id,track,snapshot,reply}=>{let _=reply.send(crate::repair_run_store::begin_restore(&mut connection,operation,id,track,snapshot));},
                        Job::RepairFinishRestore{operation,result,reply}=>{let _=reply.send(crate::repair_run_store::finish_restore(&mut connection,operation,result));},
                        Job::RepairTransition{op,id,revision,event,reply}=>{let _=reply.send(crate::repair_run_store::transition(&mut connection,op,id,revision,event));},
                        Job::RepairInput{id,reply}=>{let _=reply.send(crate::repair_run_store::input(&connection,id));},
                        Job::RepairBeginApply{id,revision,reply}=>{let _=reply.send(crate::repair_run_store::begin_apply(&mut connection,id,revision));},
                        Job::RepairApplied{id,revision,result,reply}=>{let _=reply.send(crate::repair_run_store::applied(&mut connection,id,revision,*result));},
                        Job::PendingPublications{reply}=>{let _=reply.send(crate::integration_store::pending_publications(&connection));},
                        Job::IntegrationPublished{id,revision,outcome,reply}=>{let _=reply.send(crate::integration_store::published(&mut connection,id,revision,outcome));},
                        Job::IntegrationStageBegin{id,revision,reply}=>{let _=reply.send(crate::integration_store::stage_begin(&mut connection,id,revision));},
                        Job::IntegrationSettings{id,revision,reply}=>{let _=reply.send(crate::integration_store::settings(&connection,id,revision));},
                        Job::SaveIntegrationSettings{id,revision,settings,reply}=>{let _=reply.send(crate::integration_store::save_settings(&mut connection,id,revision,settings));},
                        Job::IntegrationCandidate{id,revision,result,reply}=>{let _=reply.send(crate::integration_store::candidate(&mut connection,id,revision,*result));},
                        Job::IntegrationFrozen{id,revision,plan,reply}=>{let _=reply.send(crate::integration_store::frozen(&mut connection,id,revision,plan));},
                        Job::IntegrationCommand{command,reply}=>{let _=reply.send(crate::integration_store::command(&mut connection,&command));},
                        Job::IntegrationPlanned{id,revision,result,reply}=>{let _=reply.send(crate::integration_store::planned(&mut connection,id,revision,result));},
                        Job::RebuildEffects{reply}=>{let _=reply.send(crate::effects::rebuild(&mut connection));},
                        Job::InterruptEffects{session,reply}=>{let _=reply.send(crate::effects::interrupt(&mut connection,Some(&session)));},
                        Job::ReadWorkspace{track,reply}=>{let result=(||->Result<_>{let record:String=connection.query_row("SELECT record FROM workspaces WHERE track_id=?1",[track],|row|row.get(0))?;Ok(serde_json::from_str(&record)?)})();let _=reply.send(result);},
                        Job::SaveDeletion{snapshot,reply}=>{let _=reply.send(crate::deletion::store(&mut connection,&snapshot));},
                        Job::ReadDeletion{id,reply}=>{let _=reply.send(crate::deletion::read(&connection,id));},
                        Job::Stops{id,stops,reply}=>{let _=reply.send(crate::policy_store::set_stops(&mut connection,&id,stops));},
                        Job::Policy{track,workspace,hosts,request,deletion,reply}=>{let _=reply.send(crate::policy_store::evaluate(&mut connection,&track,&workspace,&hosts,&request,deletion.as_ref()));},
                        Job::Msp{track,session,stamp,frame,lease,source,reply}=>{let result=crate::history_store::apply(&mut connection,&track,&session,&stamp,&frame,source);drop(lease);let _=reply.send(result);},
                        Job::MspRaw{track,session,frames,reply}=>{let result=crate::history_store::raw_batch(&mut connection,&track,&session,&frames);drop(frames);let _=reply.send(result);},
                        Job::ViewCursor{track,session,reply}=>{let _=reply.send(crate::history_store::cursor(&connection,&track,&session));},
                        Job::ViewGap{track,session,reply}=>{let _=reply.send(crate::history_store::gap(&connection,&track,&session));},
                        Job::GapFilled{track,session,through,reply}=>{let _=reply.send(crate::history_store::filled(&mut connection,&track,&session,through));},
                        Job::ViewSeen{session,cursor,reply}=>{let _=reply.send(crate::history_store::seen(&connection,&session,&cursor));},
                        Job::ViewFloor{session,reply}=>{let _=reply.send(crate::history_store::floor(&connection,&session));},
                        Job::ViewCompacted{track,session,boundary,source,reply}=>{let _=reply.send(crate::history_store::compacted(&mut connection,&track,&session,&boundary,source));},
                        Job::ViewSnapshot{track,session,source,reply}=>{let _=reply.send(crate::snapshot_store::record(&mut connection,&track,&session,source));},
                        Job::OpeningSnapshot{track,session,source,covered_gap,reply}=>{let _=reply.send(crate::snapshot_store::opening(&mut connection,&track,&session,source,covered_gap));},
                        Job::ReconcileLocal{session,reply}=>{let _=reply.send(crate::reconcile_store::local(&connection,&session));},
                        Job::ReconcileRecord{session,summary,retired,reply}=>{let _=reply.send(crate::reconcile_store::record(&mut connection,&session,&summary,&retired));},
                        Job::ReconcileReplay{session,turn,reply}=>{let _=reply.send(crate::reconcile_store::replay(&connection,&session,turn));},
                        Job::ClipDetail{id,reply}=>{let _=reply.send(crate::timeline::detail(&connection,&id));},
                        Job::TimelineWindow{request,reply}=>{let _=reply.send(crate::timeline::window(&connection,request));},
                        Job::Output{session,item,reference,value,reply}=>{let _=reply.send(crate::timeline::cache_output(&mut connection,&session,&item,&reference,&value));},
                        Job::ObserveReport{report,reply}=>{let _=reply.send(crate::report_store::observe(&mut connection,report));},
                        Job::AnalyzeReport{id,revision,analysis,reply}=>{let _=reply.send(crate::report_store::analyzed(&mut connection,&id,revision,analysis));},
                        Job::RespondReport{id,question,revision,answer,reply}=>{let result=crate::report_store::respond(&mut connection,id,&question,revision,answer).and_then(|input|input.map(|input|read_input(&connection,&input)).transpose().map(|v|v.unwrap_or(Value::Null)));let _=reply.send(result);},
                        Job::ReserveTurn{id,input,revision,track,params,reply}=>{let _=reply.send(reserve_turn(&mut connection,&id,&input,revision,&track,&params));}
                        Job::InputExecution{input,revision,state,reply}=>{let _=reply.send(input_execution(&mut connection,&input,revision,&state));}
                        Job::StopLostWorker{id,state,turn,reply}=>{let _=reply.send(stop_lost_worker(&mut connection,id,&state,turn));}
                        Job::RetryWorker {id,track,reply}=>{let _=reply.send(retry_worker(&mut connection,&id,&track));}
                        Job::ReadWorkerConfig { reply } => { let result=read_setting(&connection,"worker").and_then(|value|value.map(serde_json::from_value).transpose().map_err(Into::into)); let _=reply.send(result); }
                        Job::WorkerConfig { id,config,reply }=>{let _=reply.send(set_worker_config(&mut connection,&id,&config));}
                        #[cfg(all(feature = "fault-injection", debug_assertions))]
                        Job::TestFault{request,reply}=>{
                            if let crate::fault_injection::Request::Pause{millis}=request {
                                if millis>5000 { let _=reply.send(Err("Pause exceeds five seconds.".into())); }
                                else { let _=reply.send(Ok(serde_json::json!({"paused":true})));std::thread::sleep(std::time::Duration::from_millis(millis)); }
                            } else {let _=reply.send(crate::fault_injection::apply(&mut connection,request));}
                        }
                        Job::PauseHistory{reply}=>{let _=reply.send(crate::dispatch_control::history(&connection));}
                        job @ Job::DispatchPause{..}=>{
                            if pending_controls.is_empty() {
                                if let Some(job)=persist_control(&mut connection,job,false){pending_controls.push_back(job);}
                            } else { pending_controls.push_back(job); }
                        }
                        Job::WorkerState { state,reply }=>{let _=reply.send(worker_state(&mut connection,&state));}
                        Job::BindSession { id, slug, reply } => { let _ = reply.send(bind_session(&mut connection, &id, &slug)); }
                        Job::WorkspaceResult { workspace, reply } => { let _ = reply.send(workspace_result(&mut connection, &workspace)); }
                        Job::RetryPreparation { id, reply } => { let _ = reply.send(retry_preparation(&mut connection, &id)); }
                        Job::ReadOperation { id, reply } => {
                            let result = (|| -> Result<Option<Value>> {
                                let row: Option<(String,String,String)> = connection.query_row("SELECT kind,payload,state FROM operations WHERE id=?1", [&id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
                                row.map(|(kind,payload,state)| Ok(serde_json::json!({"kind":kind,"payload":serde_json::from_str::<Value>(&payload)?,"state":state}))).transpose()
                            })();
                            let _ = reply.send(result);
                        }
                        Job::ProjectConfig { id, project, reply } => {
                            let _ = reply.send(configure_project(&mut connection, &id, &project));
                        }
                        Job::PrepareProject { id,project,reply } => {
                            let _=reply.send(prepare_project(&mut connection,&id,&project));
                        }
                        Job::PendingProject { reply } => {
                            let result=(||->Result<_>{
                                let row:Option<(String,String)>=connection.query_row("SELECT id,payload FROM operations WHERE kind='project.configure' AND state='prepared'",[],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                                row.map(|(id,payload)|Ok((id.parse()?,serde_json::from_str(&payload)?))).transpose()
                            })();
                            let _=reply.send(result);
                        }
                        Job::ReadEditorConfig { reply } => {
                            let result = (|| -> Result<Option<Value>> {
                                let value: Option<String> = connection
                                    .query_row(
                                        "SELECT value FROM settings WHERE key='editor'",
                                        [],
                                        |r| r.get(0),
                                    )
                                    .optional()?;
                                Ok(value.map(|v| serde_json::from_str(&v)).transpose()?)
                            })();
                            let _ = reply.send(result);
                        }
                        Job::EditorConfig { id, config, reply } => {
                            let _ = reply.send(set_editor_config(&mut connection, &id, &config));
                        }
                        Job::StageUtterance {
                            id,
                            text,
                            target,
                            selected,
                            attachments,
                            reply,
                        } => {
                            let _ = reply.send(stage_utterance(
                                &mut connection,
                                &id,
                                &text,
                                &target,
                                selected.as_deref(),
                                &attachments,
                            ));
                        }
                        Job::SaveAttachment {stored,reply}=>{let _=reply.send(crate::attachment_store::save(&mut connection,&stored));}
                        Job::InputAttachments {input,track,session,reply}=>{let _=reply.send(crate::attachment_store::material(&connection,&input,&track,&session));}
                        Job::FrozenAttachments {owner,workspace,reply}=>{let _=reply.send(crate::attachment_store::frozen(&connection,owner,workspace));}
                        Job::SessionAttachments {session,track,reply}=>{let _=reply.send(crate::attachment_store::session(&connection,&session,&track));}
                        Job::ClaimInput { reply } => {
                            let _ = reply.send(claim_input(&mut connection));
                        }
                        Job::ApplyInput {
                            id,
                            revision,
                            outcome,
                            reply,
                        } => {
                            let _ =
                                reply.send(apply_input(&mut connection, &id, revision, outcome));
                        }
                        Job::CutInput { id, input, reply } => {
                            let _ = reply.send(cut_input(&mut connection, &id, &input));
                        }
                        job @ Job::HoldInputs { .. } => {
                            if pending_controls.is_empty() {
                                if let Some(job)=persist_control(&mut connection,job,false){pending_controls.push_back(job);}
                            } else { pending_controls.push_back(job); }
                        }
                        Job::StartHeldInput { id, input, revision, reply } => {
                            let _ = reply.send(start_held_input(&mut connection, &id, &input, revision));
                        }
                        Job::RecoverInputs { reply } => {
                            let _ = reply.send(recover_inputs(&mut connection));
                        }
                        Job::ResolveInput {
                            id,
                            input,
                            revision,
                            resolution,
                            reply,
                        } => {
                            let _ = reply.send(resolve_input(
                                &mut connection,
                                &id,
                                &input,
                                revision,
                                &resolution,
                            ));
                        }
                        Job::Snapshot { reply } => {
                            let _ = reply.send(snapshot(&connection).map(|mut value|{value["logUnavailable"]=serde_json::json!(!pending_controls.is_empty());value}));
                        }
                        Job::Intent {
                            id,
                            kind,
                            payload,
                            reply,
                        } => {
                            let _ = reply.send(intent(&mut connection, &id, &kind, &payload));
                        }
                        Job::Append {
                            kind,
                            payload,
                            reply,
                        } => {
                            let _ = reply.send(append(&mut connection, &kind, &payload));
                        }
                        Job::Finish { id, state, reply } => {
                            let _ = reply.send(finish(&mut connection, &id, &state));
                        }
                    }
                    flush_controls(&mut connection,&mut pending_controls);
                    if let Ok(sequence) = connection.query_row("SELECT coalesce(max(seq),0) FROM events", [], |row| row.get::<_,i64>(0)) {
                        changed.send_if_modified(|old| { if *old == sequence { false } else { *old = sequence; true } });
                    }
                }
            })?;
        started.await??;
        Ok(Self {
            jobs,
            changes,
            secret_key,
            attachment_root,
        })
    }
}

fn open_connection(path: &Path, key_check: &[u8; 32]) -> Result<Connection> {
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(2))?;
    // Reserve the writer before reading: upgrading a deferred read transaction
    // can fail immediately if another connection acquires the write lock.
    connection.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON; PRAGMA foreign_keys=ON;")?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version != 0 && version != SCHEMA_VERSION {
        return Err(std::io::Error::other("unsupported database version").into());
    }
    crate::clock::register(&connection)?;
    connection.execute_batch("BEGIN IMMEDIATE;")?;
    connection.execute_batch("CREATE TABLE IF NOT EXISTS events(seq INTEGER PRIMARY KEY,ts INTEGER NOT NULL,mono INTEGER NOT NULL DEFAULT (intercat_mono()),boot TEXT NOT NULL DEFAULT (intercat_boot()),position INTEGER NOT NULL DEFAULT (intercat_position()),track_id TEXT,session_id TEXT,kind TEXT NOT NULL,payload TEXT NOT NULL,source_seq INTEGER REFERENCES events(seq));
    CREATE TABLE IF NOT EXISTS operations(id TEXT PRIMARY KEY,kind TEXT NOT NULL,payload TEXT NOT NULL,state TEXT NOT NULL CHECK(state IN ('prepared','dispatched','complete','superseded')));
    CREATE TABLE IF NOT EXISTS utterances(id TEXT PRIMARY KEY,text TEXT NOT NULL,created_at INTEGER NOT NULL,target TEXT NOT NULL,selected TEXT,state TEXT NOT NULL,revision INTEGER NOT NULL,decision TEXT,reason TEXT,provisional TEXT,question_id TEXT,question_revision INTEGER,dispatch_command TEXT);
    CREATE TABLE IF NOT EXISTS attachments(id TEXT PRIMARY KEY,record TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS input_attachments(input TEXT NOT NULL REFERENCES utterances(id),attachment TEXT NOT NULL REFERENCES attachments(id),ordinal INTEGER NOT NULL,PRIMARY KEY(input,attachment),UNIQUE(input,ordinal));
    CREATE TABLE IF NOT EXISTS tracks(id TEXT PRIMARY KEY,label TEXT NOT NULL,title TEXT NOT NULL,status TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS workspaces(track_id TEXT PRIMARY KEY REFERENCES tracks(id),record TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS workers(track_id TEXT PRIMARY KEY REFERENCES tracks(id),record TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS reports(id TEXT PRIMARY KEY,track_id TEXT NOT NULL,pending INTEGER NOT NULL,record TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS report_questions(id TEXT PRIMARY KEY,report_id TEXT NOT NULL,track_id TEXT NOT NULL,state TEXT NOT NULL,record TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS approval_policies(session TEXT NOT NULL,approval TEXT NOT NULL,requirement TEXT NOT NULL,record TEXT NOT NULL,PRIMARY KEY(session,approval,requirement));
    CREATE TABLE IF NOT EXISTS deletion_snapshots(id TEXT PRIMARY KEY,track TEXT NOT NULL,item TEXT NOT NULL,record TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS effect_gates(id TEXT PRIMARY KEY,session TEXT NOT NULL,approval TEXT NOT NULL,item TEXT NOT NULL,record TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS effect_items(session TEXT NOT NULL,item TEXT NOT NULL,record TEXT NOT NULL,PRIMARY KEY(session,item));
    CREATE TABLE IF NOT EXISTS effects(id TEXT PRIMARY KEY,track TEXT NOT NULL,session TEXT NOT NULL,item TEXT NOT NULL,clip TEXT,record TEXT NOT NULL);
    CREATE INDEX IF NOT EXISTS effects_clip ON effects(clip);
")?;
    if version == 0 {
        crate::attachment_history::install(&connection)?;
        crate::backup_choices::install(&connection)?;
        crate::backup_apply::install(&connection)?;
        crate::timeline::install(&connection)?;
        crate::history_store::install(&connection)?;
        crate::integration_store::install(&connection)?;
        crate::repair_run_store::install(&connection)?;
        crate::secret_store::install(&connection)?;
        crate::secret_input_store::install(&connection)?;
        crate::redirect::install(&connection)?;
        crate::branch::install(&connection)?;
    }
    connection.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    crate::secret_store::verify_key(&connection, key_check, version == 0)?;
    crate::secret_store::catalog(&connection)?;
    connection.execute_batch("COMMIT;")?;
    let previous: i64 =
        connection.query_row("SELECT coalesce(max(position),0) FROM events", [], |r| {
            r.get(0)
        })?;
    crate::clock::initialize(previous);
    crate::effects::interrupt(&mut connection, None)?;
    crate::integration_store::interrupt(&mut connection)?;
    crate::repair_run_store::interrupt(&mut connection)?;
    crate::secret_input_store::interrupt(&mut connection)?;
    crate::redirect::interrupt(&mut connection)?;
    connection.execute("UPDATE project_secret_state SET ready=0", [])?;
    Ok(connection)
}
pub(crate) fn timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis() as i64
}

fn local_minute(seconds: i64) -> Result<String> {
    let seconds = seconds as libc::time_t;
    let mut parts = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers refer to live storage, and success initializes tm.
    if unsafe { libc::localtime_r(&seconds, parts.as_mut_ptr()) }.is_null() {
        return Err(std::io::Error::other("invalid session time").into());
    }
    let parts = unsafe { parts.assume_init() };
    Ok(format!(
        "{:04}{:02}{:02}-{:02}{:02}",
        parts.tm_year + 1900,
        parts.tm_mon + 1,
        parts.tm_mday,
        parts.tm_hour,
        parts.tm_min
    ))
}
type InputRow = (
    String,
    String,
    i64,
    String,
    Option<String>,
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<i64>,
);
fn input_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<InputRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
    ))
}
fn input_value(row: InputRow) -> Result<Value> {
    let decision: Option<Decision> = row.7.map(|s| serde_json::from_str(&s)).transpose()?;
    let decision = decision.map(|mut d| {
        d.scores = Value::Null;
        d
    });
    Ok(
        serde_json::json!({"id":row.0,"text":row.1,"createdAt":row.2,"target":serde_json::from_str::<Target>(&row.3)?,"selectedTrackId":row.4,"state":row.5,"revision":row.6,"decision":decision,"reason":row.8,"provisionalTrackId":row.9,"replyTo":row.10.map(|id|serde_json::json!({"id":id,"revision":row.11}))}),
    )
}
fn read_input(connection: &Connection, id: &str) -> Result<Value> {
    crate::session_retirement::ensure_input(connection, id)?;
    let mut value=input_value(connection.query_row("SELECT id,text,created_at,target,selected,state,revision,decision,reason,provisional,question_id,question_revision FROM utterances WHERE id=?1",[id],input_row)?)?;
    value["attachments"] = serde_json::to_value(
        crate::attachment_store::records(connection, id)?
            .into_iter()
            .map(|s| s.attachment)
            .collect::<Vec<_>>(),
    )?;
    Ok(value)
}
fn snapshot(connection: &Connection) -> Result<Value> {
    let sequence: i64 =
        connection.query_row("SELECT coalesce(max(seq),0) FROM events", [], |r| r.get(0))?;
    let mut stmt=connection.prepare("SELECT id,text,created_at,target,selected,state,revision,decision,reason,provisional,question_id,question_revision FROM utterances WHERE state NOT IN ('landed','cut','orphaned') OR id IN (SELECT id FROM utterances ORDER BY created_at DESC,id DESC LIMIT 100) ORDER BY created_at DESC,id DESC")?;
    let rows = stmt
        .query_map([], input_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut utterances = rows
        .into_iter()
        .map(input_value)
        .collect::<Result<Vec<_>>>()?;
    for value in &mut utterances {
        value["attachments"] = serde_json::to_value(
            crate::attachment_store::records(connection, value["id"].as_str().unwrap())?
                .into_iter()
                .map(|s| s.attachment)
                .collect::<Vec<_>>(),
        )?;
    }
    utterances.reverse();
    let mut stmt = connection.prepare("SELECT id,label,title,status FROM tracks ORDER BY rowid")?;
    let tracks=stmt.query_map([],|r|Ok(serde_json::json!({"id":r.get::<_,String>(0)?,"label":r.get::<_,String>(1)?,"title":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut stmt = connection.prepare("SELECT record FROM workspaces ORDER BY rowid")?;
    let workspaces = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|s| serde_json::from_str::<Value>(&s).map_err(Into::into))
        .collect::<Result<Vec<_>>>()?;
    let mut stmt = connection.prepare("SELECT record FROM workers ORDER BY rowid")?;
    let workers = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|s| serde_json::from_str::<Value>(&s).map_err(Into::into))
        .collect::<Result<Vec<_>>>()?;
    Ok(
        serde_json::json!({"unloggedControls":crate::dispatch_control::unlogged(connection)?,"localResumes":crate::dispatch_control::local_resumes(connection)?,"dispatchPaused":crate::dispatch_control::paused(connection)?,"pickups":crate::pickup::snapshot(connection)?.iter().map(|r|r.status()).collect::<Vec<_>>(),"branches":crate::branch::snapshot(connection)?,"redirects":crate::redirect::snapshot(connection)?,"projectSecrets":crate::project_secrets::status(connection)?,"secretInputs":crate::secret_input_store::snapshot(connection)?,"daemonPid":std::process::id(),"sequence":sequence,"integration":crate::integration_store::snapshot(connection)?,"repairRuns":crate::repair_run_store::snapshot(connection)?,"timeline":crate::timeline::snapshot(connection)?,"stops":crate::policy_store::stops(connection)?,"effects":crate::effects::list(connection,None,None)?,"pendingRestores":crate::deletion::pending(connection)?,"utterances":utterances,"tracks":tracks,"workspaces":workspaces,"workers":workers,"reports":crate::report_store::pending_reports(connection)?,"reportQuestions":crate::report_store::active_questions(connection)?,"workerConfigured":read_setting(connection,"worker")?.is_some(),"project":read_setting(connection,"project")?,"session":read_setting(connection,"session")?}),
    )
}

fn reserve_turn(
    connection: &mut Connection,
    id: &str,
    input: &str,
    revision: i64,
    track: &str,
    params: &Value,
) -> Result<Admission> {
    crate::session_retirement::ensure_open(
        connection,
        params["sessionId"]
            .as_str()
            .ok_or("Missing dispatch session.")?,
    )?;
    crate::session_retirement::ensure_input(connection, input)?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let payload = serde_json::to_string(params)?;
    let previous: Option<(String, String, String)> = transaction
        .query_row(
            "SELECT kind,payload,state FROM operations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((kind, old, state)) = previous {
        if kind != "turn.start" || old != payload {
            return Err(std::io::Error::other("Operation ID reused.").into());
        }
        return Ok(Admission::Replay(state));
    }
    crate::redirect::reserve(&transaction, input, revision, track, &params["sessionId"])?;
    crate::report_store::validate_dispatch(&transaction, input, track)?;
    crate::project_secrets::validate_dispatch(&transaction, track)?;
    let changed=transaction.execute("UPDATE utterances SET state='starting',dispatch_command=?4 WHERE id=?1 AND revision=?2 AND state='placed' AND json_extract(decision,'$.trackId')=?3",params![input,revision,track,params["commandId"].as_str()])?;
    if changed != 1 {
        return Err(StartSuperseded.into());
    }
    transaction.execute(
        "INSERT INTO operations VALUES(?1,'turn.start',?2,'prepared')",
        params![id, payload],
    )?;
    crate::attachment_history::reserve(
        &transaction,
        id,
        params["sessionId"]
            .as_str()
            .ok_or("Missing attachment session.")?,
        input,
    )?;
    transaction.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'intent.turn.start',?2)",
        params![
            timestamp(),
            serde_json::to_string(
                &serde_json::json!({"operationId":id,"inputId":input,"payload":params})
            )?
        ],
    )?;
    transaction.commit()?;
    Ok(Admission::New)
}

fn input_execution(
    connection: &mut Connection,
    input: &str,
    revision: i64,
    state: &str,
) -> Result<()> {
    if !["sent", "held", "landed"].contains(&state) {
        return Err(std::io::Error::other("Invalid execution transition.").into());
    }
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let changed=transaction.execute("UPDATE utterances SET state=?3 WHERE id=?1 AND revision=?2 AND state IN ('starting','sent')",params![input,revision,state])?;
    if changed != 1 {
        if transaction.query_row("SELECT state FROM utterances WHERE id=?1", [input], |r| {
            r.get::<_, String>(0)
        })? == "landed"
        {
            return Ok(());
        }
        if state == "held"
            && transaction.query_row("SELECT state FROM utterances WHERE id=?1", [input], |r| {
                r.get::<_, String>(0)
            })? == "orphaned"
        {
            return Ok(());
        }
        return Err(std::io::Error::other("Request execution changed.").into());
    }
    crate::report_store::input_state(&transaction, input, state)?;
    crate::redirect::input_state(&transaction, input, state)?;
    transaction.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.execution',?2)",
        params![
            timestamp(),
            serde_json::to_string(&serde_json::json!({"id":input,"state":state}))?
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

fn retry_worker(connection: &mut Connection, id: &str, track: &str) -> Result<Admission> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let payload = serde_json::to_string(&serde_json::json!({"trackId":track}))?;
    let previous: Option<(String, String)> = transaction
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, old)) = previous {
        if kind != "worker.retry" || old != payload {
            return Err(std::io::Error::other("Operation ID reused.").into());
        }
        return Ok(Admission::Replay("complete".into()));
    }
    let record: String = transaction.query_row(
        "SELECT record FROM workers WHERE track_id=?1",
        [track],
        |r| r.get(0),
    )?;
    let mut state: crate::runtime::WorkerState = serde_json::from_str(&record)?;
    if !["failed", "stopped", "lost"].contains(&state.phase.as_str()) {
        return Err(std::io::Error::other("Worker is not ready for a retry.").into());
    }
    state.phase = "retry".into();
    state.detail = None;
    transaction.execute(
        "UPDATE workers SET record=?2 WHERE track_id=?1",
        params![track, serde_json::to_string(&state)?],
    )?;
    transaction.execute(
        "INSERT INTO operations VALUES(?1,'worker.retry',?2,'complete')",
        params![id, payload],
    )?;
    transaction.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'worker.retry',?2)",
        params![timestamp(), payload],
    )?;
    transaction.commit()?;
    Ok(Admission::New)
}

fn worker_state(connection: &mut Connection, state: &crate::runtime::WorkerState) -> Result<()> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    record_worker_state(&transaction, state)?;
    transaction.commit()?;
    Ok(())
}
fn record_worker_state(connection: &Connection, state: &crate::runtime::WorkerState) -> Result<()> {
    if let Some(session) = &state.session_id {
        crate::session_retirement::ensure_open(connection, session)?;
    }
    if state.phase == "needs-recovery" {
        connection.execute("UPDATE utterances SET state='held',revision=revision+1,reason='The previous worker needs recovery. Review before sending.' WHERE (json_extract(decision,'$.trackId')=?1 OR provisional=?1) AND state IN ('staged','reading','placed','starting','sent')",[&state.track_id])?;
    }
    let payload = serde_json::to_string(state)?;
    let terminal: Option<String> = if state.active_turn.is_none() && !state.checkpoint_pending {
        if let (Some(session), Some(checkpoint)) = (&state.session_id, &state.checkpoint) {
            connection
                .query_row(
                    "SELECT json_extract(payload,'$.params.terminal') FROM events
                 WHERE session_id=?1 AND kind='msp.applied'
                 AND json_extract(payload,'$.method')='turn/completed'
                 AND json_extract(payload,'$.params.turnId')=?2
                 ORDER BY seq DESC LIMIT 1",
                    params![session, checkpoint.turn_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .flatten()
        } else {
            None
        }
    } else {
        None
    };
    let track_status = if state.active_turn.is_some()
        || state.checkpoint_pending
        || ["working", "approval", "question", "stopping"].contains(&state.phase.as_str())
    {
        "working"
    } else if ["ready", "held", "stopped"].contains(&state.phase.as_str()) {
        match terminal.as_deref() {
            Some("completed") => "done",
            Some("cancelled") => "stopped",
            _ => "waiting",
        }
    } else {
        "waiting"
    };
    connection.execute(
        "UPDATE tracks SET status=?2 WHERE id=?1",
        params![state.track_id, track_status],
    )?;
    connection.execute("INSERT INTO workers VALUES(?1,?2) ON CONFLICT(track_id) DO UPDATE SET record=excluded.record",params![state.track_id,payload])?;
    connection.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'worker.state',?2)",
        params![timestamp(), payload],
    )?;
    crate::redirect::worker_changed(connection, state)?;
    Ok(())
}
fn stop_lost_worker(
    connection: &mut Connection,
    id: uuid::Uuid,
    state: &crate::runtime::WorkerState,
    turn: Option<uuid::Uuid>,
) -> Result<()> {
    if state.phase != "after-connection-loss" {
        return Err("Invalid connection-loss stop state.".into());
    }
    let payload=serde_json::json!({"opId":id,"trackId":state.track_id,"sessionId":state.session_id,"turnId":turn,"terminal":"after-connection-loss"}).to_string();
    connection.pragma_update(None, "synchronous", "FULL")?;
    let tx = connection.transaction()?;
    let previous: Option<(String, String)> = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, old)) = previous {
        if kind != "worker.stopLost" || old != payload {
            return Err("Operation ID reused.".into());
        }
        return Ok(());
    }
    tx.execute("UPDATE utterances SET state='held',revision=revision+1,reason='The worker ended after connection loss. This request is held.' WHERE (json_extract(decision,'$.trackId')=?1 OR provisional=?1) AND state IN ('placed','starting','sent')",[&state.track_id])?;
    record_worker_state(&tx, state)?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'worker.stopLost',?2,'complete')",
        params![id.to_string(), payload],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'worker.connectionStopped',?2)",
        params![timestamp(), payload],
    )?;
    tx.commit()?;
    Ok(())
}

fn set_worker_config(
    connection: &mut Connection,
    id: &str,
    config: &crate::runtime::Config,
) -> Result<Admission> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let payload = serde_json::to_string(config)?;
    let previous: Option<(String, String)> = transaction
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, old)) = previous {
        if kind != "worker.configure" || old != payload {
            return Err(std::io::Error::other("Operation ID reused.").into());
        }
        return Ok(Admission::Replay("complete".into()));
    }
    if read_setting(&transaction, "worker")?.is_some() {
        return Err(
            std::io::Error::other("Workers are already configured for this session.").into(),
        );
    }
    transaction.execute("INSERT INTO settings VALUES('worker',?1)", [&payload])?;
    transaction.execute(
        "INSERT INTO operations VALUES(?1,'worker.configure',?2,'complete')",
        params![id, payload],
    )?;
    transaction.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'worker.configured',?2)",
        params![
            timestamp(),
            serde_json::to_string(&serde_json::json!({"binary":config.binary}))?
        ],
    )?;
    transaction.commit()?;
    Ok(Admission::New)
}

fn bind_session(connection: &mut Connection, id: &str, slug: &str) -> Result<()> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let mut session = read_setting(&transaction, "session")?
        .ok_or_else(|| std::io::Error::other("No session to bind."))?;
    if session["id"] != id || (session["bound"] == true && session["slug"] != slug) {
        return Err(std::io::Error::other("Session binding changed.").into());
    }
    if session["bound"] == true {
        return Ok(());
    }
    session["slug"] = slug.into();
    session["bound"] = true.into();
    let payload = serde_json::to_string(&session)?;
    transaction.execute(
        "UPDATE settings SET value=?1 WHERE key='session'",
        [&payload],
    )?;
    transaction.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'session.bound',?2)",
        params![timestamp(), payload],
    )?;
    transaction.commit()?;
    Ok(())
}

fn workspace_result(
    connection: &mut Connection,
    workspace: &crate::preparation::Workspace,
) -> Result<()> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let payload = serde_json::to_string(workspace)?;
    transaction.execute("INSERT INTO workspaces VALUES(?1,?2) ON CONFLICT(track_id) DO UPDATE SET record=excluded.record", params![workspace.track_id,payload])?;
    transaction.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'workspace.prepared',?2)",
        params![timestamp(), payload],
    )?;
    transaction.commit()?;
    Ok(())
}

fn retry_preparation(connection: &mut Connection, id: &str) -> Result<()> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let existing: Option<String> = transaction
        .query_row("SELECT kind FROM operations WHERE id=?1", [id], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(kind) = existing {
        if kind == "preparation.retry" {
            return Ok(());
        }
        return Err(std::io::Error::other("Operation ID reused.").into());
    }
    transaction.execute("UPDATE workspaces SET record=json_set(record,'$.state','pending','$.error',NULL) WHERE json_extract(record,'$.state')='failed'", [])?;
    transaction.execute(
        "INSERT INTO operations VALUES(?1,'preparation.retry','{}','complete')",
        [id],
    )?;
    transaction.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'preparation.retry','{}')",
        [timestamp()],
    )?;
    transaction.commit()?;
    Ok(())
}

fn read_setting(connection: &Connection, key: &str) -> Result<Option<Value>> {
    let value: Option<String> = connection
        .query_row("SELECT value FROM settings WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()?;
    value
        .map(|s| serde_json::from_str(&s).map_err(Into::into))
        .transpose()
}

fn prepare_project(
    connection: &mut Connection,
    id: &str,
    project: &crate::project::Project,
) -> Result<Admission> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    if uuid::Uuid::parse_str(id)?.get_version_num() != 7 {
        return Err("Invalid project operation ID.".into());
    }
    let payload = serde_json::to_string(project)?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let old: Option<(String, String, String)> = tx
        .query_row(
            "SELECT kind,payload,state FROM operations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((kind, previous, state)) = old {
        if kind != "project.configure"
            || previous != payload
            || !["prepared", "complete"].contains(&state.as_str())
        {
            return Err("Project operation ID reused.".into());
        }
        return Ok(Admission::Replay(state));
    }
    if read_setting(&tx, "project")?.is_some() {
        return Err("This session already has a project folder.".into());
    }
    if tx.query_row("SELECT EXISTS(SELECT 1 FROM operations WHERE kind='project.configure' AND state='prepared')",[],|r|r.get::<_,bool>(0))? {
        return Err("Another project selection is still being prepared.".into());
    }
    tx.execute(
        "INSERT INTO operations VALUES(?1,'project.configure',?2,'prepared')",
        params![id, payload],
    )?;
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'intent.project.configure',?2)",
        params![
            timestamp(),
            serde_json::json!({"operationId":id,"payload":project}).to_string()
        ],
    )?;
    tx.commit()?;
    Ok(Admission::New)
}

fn configure_project(
    connection: &mut Connection,
    id: &str,
    project: &crate::project::Project,
) -> Result<Value> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction =
        connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let payload = serde_json::to_string(project)?;
    let previous: (String, String, String) = transaction.query_row(
        "SELECT kind,payload,state FROM operations WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let (kind, saved, state) = previous;
    if kind != "project.configure" || saved != payload {
        return Err("Project operation ID reused.".into());
    }
    if state == "prepared" {
        if read_setting(&transaction, "project")?.is_some() {
            return Err(std::io::Error::other("This session already has a project folder.").into());
        }
        transaction.execute("INSERT INTO settings VALUES('project',?1)", [&payload])?;
        transaction.execute("UPDATE operations SET state='complete' WHERE id=?1", [id])?;
        transaction.execute(
            "INSERT INTO events(ts,kind,payload) VALUES(?1,'project.configured',?2)",
            params![timestamp(), payload],
        )?;
    } else if state != "complete" {
        return Err("The project preparation cannot be completed.".into());
    }
    transaction.commit()?;
    Ok(serde_json::to_value(project)?)
}
fn set_editor_config(connection: &mut Connection, id: &str, config: &Value) -> Result<Admission> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let payload = serde_json::to_string(config)?;
    let previous: Option<(String, String)> = transaction
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, old)) = previous {
        if kind != "editor.configure" || old != payload {
            return Err(
                std::io::Error::other("operation ID reused with different contents").into(),
            );
        }
        return Ok(Admission::Replay("complete".into()));
    }
    transaction.execute("INSERT INTO settings VALUES('editor',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[&payload])?;
    transaction.execute(
        "INSERT INTO operations VALUES(?1,'editor.configure',?2,'complete')",
        params![id, payload],
    )?;
    transaction.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'editor.configured',?2)",
        params![timestamp(), payload],
    )?;
    transaction.commit()?;
    Ok(Admission::New)
}

fn stage_utterance(
    connection: &mut Connection,
    id: &str,
    text: &str,
    target: &Target,
    selected: Option<&str>,
    attachments: &[uuid::Uuid],
) -> Result<Value> {
    crate::session_retirement::ensure_input(connection, id)?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let payload = serde_json::to_string(
        &serde_json::json!({"text":text,"target":target,"selectedTrackId":selected,"attachmentIds":attachments}),
    )?;
    let existing: Option<(String, String)> = transaction
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, previous)) = existing {
        if kind != "utterance.create" || previous != payload {
            return Err(
                std::io::Error::other("operation ID reused with different contents").into(),
            );
        }
    } else {
        if let Target::Lock(track) = target {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)",
                [track],
                |r| r.get(0),
            )?;
            if !exists {
                return Err(std::io::Error::other("locked track is unavailable").into());
            }
        }
        let now = timestamp();
        if read_setting(&transaction, "session")?.is_none() {
            let slug = format!("s-{}", local_minute(now / 1000)?);
            let session = serde_json::json!({"id":id,"slug":slug,"name":text.chars().take(30).collect::<String>(),"createdAt":now});
            transaction.execute(
                "INSERT INTO settings VALUES('session',?1)",
                [serde_json::to_string(&session)?],
            )?;
        }
        let provisional = match target {
            Target::Lock(id) => Some(id.clone()),
            Target::New => None,
            Target::None => selected.map(str::to_owned).or(transaction.query_row("SELECT json_extract(decision,'$.trackId') FROM utterances WHERE json_extract(decision,'$.trackId') IS NOT NULL AND state='placed' ORDER BY created_at DESC LIMIT 1",[],|r|r.get::<_,Option<String>>(0)).optional()?.flatten()),
        };
        transaction.execute(
            "INSERT INTO utterances(id,text,created_at,target,selected,state,revision,decision,reason,provisional) VALUES(?1,?2,?3,?4,?5,'staged',1,NULL,NULL,?6)",
            params![id, text, now, serde_json::to_string(target)?, selected, provisional],
        )?;
        if crate::dispatch_control::paused(&transaction)? {
            transaction.execute(
                "UPDATE utterances SET state='held',reason='Turns stopped' WHERE id=?1",
                [id],
            )?;
        }
        crate::attachment_store::bind(&transaction, id, attachments)?;
        transaction.execute(
            "INSERT INTO operations VALUES(?1,'utterance.create',?2,'complete')",
            params![id, payload],
        )?;
        transaction.execute(
            "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.staged',?2)",
            params![
                now,
                serde_json::to_string(&serde_json::json!({"id":id,"text":text,"target":target}))?
            ],
        )?;
    }
    transaction.commit()?;
    read_input(connection, id)
}

fn claim_input(connection: &mut Connection) -> Result<Option<PendingInput>> {
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    let transaction = connection.transaction()?;
    let row: Option<(String,String,String,Option<String>,i64)> = transaction.query_row("SELECT id,text,target,selected,revision FROM utterances WHERE state='staged' ORDER BY created_at,id LIMIT 1",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
    let Some((id, text, target, selected, revision)) = row else {
        return Ok(None);
    };
    let mut statement = transaction.prepare("SELECT t.id,t.title,t.status FROM tracks t ORDER BY (SELECT max(u.created_at) FROM utterances u WHERE json_extract(u.decision,'$.trackId')=t.id) DESC,t.rowid")?;
    let tracks = statement
        .query_map([], |r| {
            Ok(Candidate {
                id: r.get(0)?,
                title: r.get(1)?,
                status: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);
    transaction.execute(
        "UPDATE utterances SET state='reading',reason=NULL WHERE id=?1",
        [&id],
    )?;
    transaction.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.reading',?2)",
        params![
            timestamp(),
            serde_json::to_string(&serde_json::json!({"id":id,"revision":revision}))?
        ],
    )?;
    transaction.commit()?;
    Ok(Some(PendingInput {
        id,
        text,
        target: serde_json::from_str(&target)?,
        selected,
        revision,
        tracks,
    }))
}
fn apply_input(
    connection: &mut Connection,
    id: &str,
    revision: i64,
    outcome: Outcome,
) -> Result<()> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let current: (String, i64) = transaction.query_row(
        "SELECT state,revision FROM utterances WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if current != ("reading".into(), revision) {
        return Ok(());
    }
    let (mut state, decision, mut reason) = match outcome {
        Outcome::Defer => ("staged", None, Some("editor unavailable".to_owned())),
        Outcome::Asked(reason) => ("asked", None, Some(reason)),
        Outcome::Classified(mut decision) => {
            if decision.kind == Kind::Trim {
                decision.boundary = Some("now".into());
            }
            let mut state = "placed";
            let mut reason = None;
            if decision.timed_out {
                state = "asked";
                reason = Some("The Editor took too long. Choose where this should go.".into());
            } else if decision.boundary.as_deref() == Some("after-clip") {
                state = "asked";
                reason = Some("Choose the step this request should wait for.".into());
            } else if matches!(decision.kind, Kind::Extend | Kind::Schedule | Kind::Trim)
                && decision.track_id.is_none()
            {
                state = "asked";
                reason = Some("Choose a track for this request.".into());
            } else if decision.kind == Kind::Trim {
                if crate::redirect::prepare(
                    &transaction,
                    id,
                    revision,
                    decision.track_id.as_deref().unwrap(),
                )? {
                    state = "redirecting";
                } else {
                    state = "asked";
                    reason = Some("This track has no active turn to redirect. Choose where to send the correction.".into());
                }
            } else if decision.kind == Kind::Ask {
                state = "asked";
                reason = Some("Please clarify this request.".into());
            } else if decision.kind == Kind::Park {
                state = "parked";
            } else if decision.kind == Kind::Plan {
                state = "asked";
                reason = Some("This request needs a plan.".into());
            } else if decision.kind == Kind::Fork {
                let count: i64 = transaction.query_row(
                    "SELECT count(*) FROM tracks WHERE label NOT GLOB '*[0-9]*'",
                    [],
                    |r| r.get(0),
                )?;
                let mut number = count + 1;
                let mut label = String::new();
                while number > 0 {
                    number -= 1;
                    label.insert(0, (b'A' + (number % 26) as u8) as char);
                    number /= 26;
                }
                let title: String = transaction
                    .query_row("SELECT text FROM utterances WHERE id=?1", [id], |r| {
                        r.get::<_, String>(0)
                    })?
                    .chars()
                    .take(30)
                    .collect();
                let track = uuid::Uuid::now_v7().to_string();
                transaction.execute(
                    "INSERT INTO tracks VALUES(?1,?2,?3,'waiting')",
                    params![track, label, title],
                )?;
                decision.track_id = Some(track);
            }
            (state, Some(serde_json::to_string(&decision)?), reason)
        }
    };
    if state == "placed" && crate::dispatch_control::paused(&transaction)? {
        state = "held";
        reason = Some("Turns stopped".into());
    }
    transaction.execute(
        "UPDATE utterances SET state=?2,decision=?3,reason=?4 WHERE id=?1",
        params![id, state, decision, reason],
    )?;
    transaction.execute("INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.classified',?2)",params![timestamp(),serde_json::to_string(&serde_json::json!({"id":id,"revision":revision,"state":state,"decision":decision.map(|d|serde_json::from_str::<Value>(&d)).transpose()?,"reason":reason}))?])?;
    transaction.commit()?;
    Ok(())
}
fn resolve_input(
    connection: &mut Connection,
    id: &str,
    input: &str,
    revision: i64,
    resolution: &Resolution,
) -> Result<Value> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let payload = serde_json::to_string(
        &serde_json::json!({"utteranceId":input,"revision":revision,"resolution":resolution}),
    )?;
    let previous: Option<(String, String)> = transaction
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, old)) = previous {
        if kind != "utterance.resolve" || old != payload {
            return Err(std::io::Error::other("operation ID reused").into());
        }
    } else {
        let (state, current): (String, i64) = transaction.query_row(
            "SELECT state,revision FROM utterances WHERE id=?1",
            [input],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let placed_hold = matches!(resolution, Resolution::Hold) && state == "placed";
        if current != revision || (!placed_hold && !["asked", "held"].contains(&state.as_str())) {
            return Err(std::io::Error::other("request changed; refresh before answering").into());
        }
        match resolution {
            Resolution::Hold => {
                transaction.execute("UPDATE utterances SET state='held',revision=revision+1,reason='Held by you' WHERE id=?1", [input])?;
                crate::redirect::input_state(&transaction, input, "held")?;
            }
            Resolution::Retry { target } => {
                if crate::report_store::retry_reply(&transaction, input, target)?
                    || crate::redirect::retry(&transaction, input, target)?
                {
                    // Bound replies and held corrections keep their original target.
                } else {
                    if let Target::Lock(track) = target {
                        let exists: bool = transaction.query_row(
                            "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)",
                            [track],
                            |r| r.get(0),
                        )?;
                        if !exists {
                            return Err(std::io::Error::other("chosen track is unavailable").into());
                        }
                    }
                    crate::redirect::forget(&transaction, input)?;
                    transaction.execute("UPDATE utterances SET state='staged',revision=revision+1,target=?2,provisional=?3,decision=NULL,reason=NULL WHERE id=?1", params![input,serde_json::to_string(target)?,match target { Target::Lock(id) => Some(id), _ => None }])?;
                }
            }
        }
        transaction.execute(
            "INSERT INTO operations VALUES(?1,'utterance.resolve',?2,'complete')",
            params![id, payload],
        )?;
        transaction.execute(
            "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.resolved',?2)",
            params![timestamp(), payload],
        )?;
    }
    transaction.commit()?;
    read_input(connection, input)
}

fn hold_inputs(connection: &mut Connection, id: &str, track: &str, unlogged: bool) -> Result<()> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let tx = connection.transaction()?;
    let payload = serde_json::to_string(&serde_json::json!({"trackId":track}))?;
    if let Some((kind, saved)) = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if kind != "worker.hold" || saved != payload {
            return Err("Operation ID reused.".into());
        }
        return Ok(());
    }
    if !tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM tracks WHERE id=?1)",
        [track],
        |r| r.get::<_, bool>(0),
    )? {
        return Err("The track is unavailable.".into());
    }
    tx.execute("UPDATE utterances SET state='held',revision=revision+1,reason='Held after this turn' WHERE state IN ('staged','reading','placed','held') AND (json_extract(decision,'$.trackId')=?1 OR (decision IS NULL AND provisional=?1))",[track])?;
    tx.execute(
        "INSERT INTO operations VALUES(?1,'worker.hold',?2,'complete')",
        params![id, payload],
    )?;
    let event = if unlogged {
        crate::dispatch_control::mark_uncertain(&tx, Some(track))?;
        serde_json::json!({"trackId":track,"opId":id,"unlogged":true}).to_string()
    } else {
        payload
    };
    tx.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'worker.hold',?2)",
        params![timestamp(), event],
    )?;
    tx.commit()?;
    Ok(())
}

fn start_held_input(
    connection: &mut Connection,
    id: &str,
    input: &str,
    revision: i64,
) -> Result<Value> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let tx = connection.transaction()?;
    let payload =
        serde_json::to_string(&serde_json::json!({"utteranceId":input,"revision":revision}))?;
    if let Some((kind, saved)) = tx
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
    {
        if kind != "utterance.start" || saved != payload {
            return Err("Operation ID reused.".into());
        }
    } else {
        if crate::dispatch_control::paused(&tx)? {
            return Err("Turns are stopped. Resume before starting a request.".into());
        }
        let (state, current, decision): (String, i64, Option<String>) = tx.query_row(
            "SELECT state,revision,decision FROM utterances WHERE id=?1",
            [input],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let decision: Value =
            serde_json::from_str(&decision.ok_or("This request needs a destination first.")?)?;
        if state != "held"
            || current != revision
            || !matches!(
                decision["kind"].as_str(),
                Some("fork" | "extend" | "schedule")
            )
            || !decision["trackId"].is_string()
        {
            return Err("This request changed or cannot be started here.".into());
        }
        tx.execute(
            "UPDATE utterances SET state='placed',revision=revision+1,reason=NULL,decision=json_set(decision,'$.boundary','now') WHERE id=?1",
            [input],
        )?;
        tx.execute(
            "INSERT INTO operations VALUES(?1,'utterance.start',?2,'complete')",
            params![id, payload],
        )?;
        tx.execute(
            "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.started',?2)",
            params![timestamp(), payload],
        )?;
    }
    tx.commit()?;
    read_input(connection, input)
}

fn cut_input(connection: &mut Connection, id: &str, input: &str) -> Result<Value> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let payload = serde_json::to_string(&serde_json::json!({"utteranceId":input}))?;
    let previous: Option<(String, String)> = transaction
        .query_row(
            "SELECT kind,payload FROM operations WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((kind, old)) = previous {
        if kind != "utterance.cut" || old != payload {
            return Err(std::io::Error::other("operation ID reused").into());
        }
    } else {
        let state: String =
            transaction.query_row("SELECT state FROM utterances WHERE id=?1", [input], |r| {
                r.get(0)
            })?;
        if ![
            "staged", "reading", "placed", "parked", "asked", "held", "cut", "orphaned",
        ]
        .contains(&state.as_str())
        {
            return Err(std::io::Error::other("this request already started").into());
        }
        transaction.execute(
            "UPDATE utterances SET state='cut',revision=revision+1 WHERE id=?1",
            [input],
        )?;
        crate::report_store::input_state(&transaction, input, "cut")?;
        transaction.execute(
            "INSERT INTO operations VALUES(?1,'utterance.cut',?2,'complete')",
            params![id, payload],
        )?;
        transaction.execute(
            "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.cut',?2)",
            params![timestamp(), payload],
        )?;
    }
    transaction.commit()?;
    read_input(connection, input)
}
fn recover_inputs(connection: &mut Connection) -> Result<()> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let count = transaction.execute(
        "UPDATE utterances SET state='staged',revision=revision+1 WHERE state='reading'",
        [],
    )?;
    if count > 0 {
        transaction.execute(
            "INSERT INTO events(ts,kind,payload) VALUES(?1,'utterance.recovered',?2)",
            params![
                timestamp(),
                serde_json::to_string(&serde_json::json!({"count":count}))?
            ],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn intent(connection: &mut Connection, id: &str, kind: &str, payload: &Value) -> Result<Admission> {
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let json = serde_json::to_string(payload)?;
    let existing: Option<(String, String, String)> = transaction
        .query_row(
            "SELECT kind,payload,state FROM operations WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    if let Some((old_kind, old_payload, state)) = existing {
        if old_kind != kind || old_payload != json {
            return Err(
                std::io::Error::other("operation ID reused with different contents").into(),
            );
        }
        return Ok(Admission::Replay(state));
    }
    transaction.execute(
        "INSERT INTO operations VALUES(?1,?2,?3,'prepared')",
        params![id, kind, json],
    )?;
    transaction.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,?2,?3)",
        params![
            timestamp(),
            format!("intent.{kind}"),
            serde_json::to_string(&serde_json::json!({"operationId":id,"payload":payload}))?
        ],
    )?;
    transaction.commit()?;
    Ok(Admission::New)
}

fn append(connection: &mut Connection, kind: &str, payload: &Value) -> Result<i64> {
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,?2,?3)",
        params![timestamp(), kind, serde_json::to_string(payload)?],
    )?;
    Ok(connection.last_insert_rowid())
}

fn finish(connection: &mut Connection, id: &str, state: &str) -> Result<()> {
    if !["dispatched", "complete", "superseded"].contains(&state) {
        return Err(std::io::Error::other("invalid operation state").into());
    }
    connection.pragma_update(None, "synchronous", "FULL")?;
    let transaction = connection.transaction()?;
    let previous: String =
        transaction.query_row("SELECT state FROM operations WHERE id=?1", [id], |r| {
            r.get(0)
        })?;
    if previous == state {
        return Ok(());
    }
    let valid = matches!(
        (previous.as_str(), state),
        ("prepared", "dispatched" | "superseded") | ("dispatched", "complete")
    );
    if !valid {
        return Err(std::io::Error::other("invalid operation state transition").into());
    }
    transaction.execute(
        "UPDATE operations SET state=?2 WHERE id=?1",
        params![id, state],
    )?;
    transaction.execute(
        "INSERT INTO events(ts,kind,payload) VALUES(?1,'operation.state',?2)",
        params![
            timestamp(),
            serde_json::to_string(&serde_json::json!({"operationId":id,"state":state}))?
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_transaction_prevents_lock_takeover_between_read_and_write() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("journal.sqlite");
        let mut writer = open_connection(&path, &[7; 32])?;
        let contender = Connection::open(&path)?;
        contender.busy_timeout(std::time::Duration::ZERO)?;
        let transaction = writer.transaction()?;
        let before: i64 =
            transaction.query_row("SELECT count(*) FROM events", [], |row| row.get(0))?;
        let attempted_takeover = contender.execute_batch("BEGIN IMMEDIATE");
        assert!(matches!(
            attempted_takeover,
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::DatabaseBusy
        ));
        transaction.execute(
            "INSERT INTO events(ts,kind,payload) VALUES(0,'fixture','{}')",
            [],
        )?;
        transaction.commit()?;
        let after: i64 =
            contender.query_row("SELECT count(*) FROM events", [], |row| row.get(0))?;
        assert_eq!(after, before + 1);
        Ok(())
    }
}
