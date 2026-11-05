export interface PickupState { opId: string; trackId: string; sessionId: string; phase: 'stopping' | 'capturing' | 'publishing' | 'summarizing' | 'starting' | 'awaiting-receipt' | 'ready' | 'cancelling' | 'cancelled'; snapshotSha: string | null }
import type { IntegrationJob, IntegrationRequest, DiffRequest, Diff } from './commit/types';
import type { RepairRequest, RepairRun, RepairRefreshRequest, RepairResponse } from './commit/repair';
export type InputTarget = { mode: 'none' | 'new' } | { mode: 'lock'; trackId: string };
export type Stops = 'irreversible-only' | 'every-approval';
export interface Effect { id: string; trackId: string; sessionId: string; itemId: string; turnId: string; clipId: string | null; kind: string; target: string; ruleId: string; outcome: 'done' | 'failed' | 'unknown'; running: boolean; at: number; reason: string | null }
export interface TimelineClip { id: string; trackId: string; repairRunId: string | null; ordinal: number; sessionId: string; turnId: string | null; category: 'step' | 'shell' | 'snapshot'; effectCount?: number; label: string; kind: 'read' | 'think' | 'edit' | 'shell'; start: number; end: number; running: boolean; generationEnd: number | null; approximate: boolean; toolTicks: { itemId: string; tool: string; status: string }[] }
export interface TimelineMark { id: string; trackId: string; repairRunId: string | null; at: number; inputId: string | null; text: string }
export interface TimelineState { clock: { epochMs: number; monoMs: number; bootId: string; timelineMs: number }; summaries: { trackId: string; sessionId: string; repairRunId: string | null; boundaryCursor: string }[] }
export interface OutputResult { content?: string; encoding?: string; byteLen?: number; eof?: boolean; unavailable?: boolean; _intercatTruncated?: string[] }
export interface StepItem { itemId: string; kind: string; revision: number; status: string; tool?: string; args?: string; commandText?: string; text?: string; visibleOutput?: string; failureReason?: string; recordedAt?: string; retracted?: boolean; truncated?: boolean; _intercatTruncated?: string[]; patchRef?: { id: string }; outputRef?: { id: string }; outputs: { patchRef?: OutputResult; outputRef?: OutputResult } }
export interface StepDetail { clip: TimelineClip; items: StepItem[]; deletionSnapshots?: DeletionSnapshot[]; effects?: Effect[] }
export type InputResolution = { action: 'retry'; target: InputTarget } | { action: 'hold' };
export interface InputDecision { kind: 'extend' | 'fork' | 'schedule' | 'trim' | 'park' | 'ask' | 'plan' | 'command'; trackId: string | null; boundary: string | null; command: string | null; confidence: number; inferenceMs: number; timedOut: boolean }
export interface Attachment { id: string; name: string; byteLen: number; sha256: string }
export interface Utterance { id: string; text: string; attachments: Attachment[]; createdAt: number; state: 'staged' | 'reading' | 'placed' | 'asked' | 'held' | 'parked' | 'cut' | 'redirecting' | 'starting' | 'sent' | 'landed' | 'orphaned'; replyTo?: { id: string; revision: number } | null; revision: number; target: InputTarget; provisionalTrackId?: string; decision: InputDecision | null; reason: string | null }
export interface LiveTrack { id: string; label: string; title: string; status: string }
export interface EditorStatus { pid?: number | null; phase: 'not_configured' | 'starting' | 'ready' | 'unavailable' | 'stopped' | 'downloading' | 'download_paused'; process: boolean; accepting: boolean; modelLoaded: boolean; generation: number; restart: number; canRetry: boolean; detail?: string; download?: { received: number; total: number; phase: string; file: string | null } | null }
export interface Project { name: string; path: string; root: string; commonDir: string; base: string; baseBranch: string | null }
export interface Session { id: string; slug: string; name: string; createdAt: number; bound?: boolean }
export interface Workspace { trackId: string; path: string; reference: string; base: string; attachmentRoots: string[]; state: 'pending' | 'preparing' | 'branching' | 'recovering' | 'ready' | 'failed'; error: string | null }
export interface BranchRequest { opId: string; parentTrackId: string; turnId: string }
export interface BranchState { request: BranchRequest; trackId: string; parentSessionId: string; sessionId: string | null; workspace: Workspace; checkpoint: NonNullable<WorkerState['checkpoint']>; phase: 'preparing' | 'awaiting-receipt' | 'ready' }
export interface DeletionSnapshot { restoreOperationId?: string; id: string; trackId: string; repairRunId: string | null; itemId: string; sha: string; reference: string; paths: string[]; absent: string[]; directories: string[]; uncovered: string[]; complete: boolean; purpose: 'before-delete' | 'before-restore' }
export interface Approval { deletion?: { id: string; sha: string; files: number; complete: boolean; uncovered: number } | null; id: string; requirement: unknown; subject: string; reason: { kind: string; message: string }; canAllow: boolean; canDeny: boolean }
export interface InputQuestion { id: string; header: string; question: string; selection: { mode: 'single' | 'multiple'; minSelections?: number; maxSelections?: number }; options: { label: string; description?: string; preview?: { format: string; content: string } }[] }
export interface Prompt { request: { sessionId: string; userInputId: string; turnId: string; questions: InputQuestion[]; autoResolutionMs?: number }; revision: string; phase: 'open' | 'sending' | 'awaiting-settlement'; operation: string | null }
export interface PromptAnswer { questionId: string; selectedLabel?: string; selectedLabels?: string[]; freeText?: string; note?: string }
export type PromptResponse = { action: 'answer' | 'stop'; answers: PromptAnswer[] } | { action: 'explain'; content: string };
export interface ReportQuestion { id: string; reportId: string; trackId: string; sessionId: string; turnId: string; revision: number; kind: 'question' | 'review'; text: string; state: 'open' | 'queued' | 'held'; replyUtteranceId: string | null }
export type ReportAnswer = { action: 'reply'; text: string } | { action: 'continue' | 'hold' };
export interface ProjectSecretState { projectId: string; revision: number; names: string[]; allNames: string[]; ready: boolean }
export interface ProjectSecretSettings { project: ProjectSecretState; savedRevision: number; savedNames: string[]; issue: string | null }
export interface ProcessIdentity { pid: number; started_seconds: number; started_micros: number }
export interface ProjectSecretUsage { nonce: string; trackId: string; repairRunId: string | null; revision: number; names: string[]; processes: { identity: ProcessIdentity; host: boolean; kept: boolean; canStop: boolean }[] }
export type SecretQuestion = { kind: 'msp'; id: string; revision: string } | { kind: 'report'; id: string; revision: number };
export interface SecretEntry { scope: 'once' | 'project'; opId: string; fingerprintOpId: string; name: string; value: string }
export type SecretOwner = { kind: 'work' } | { kind: 'repair'; runId: string };
export interface SecretInputState { owner: SecretOwner; inputId: string; opId: string; trackId: string; name: string; state: 'prepared' | 'stopping' | 'settled' | 'restarting' | 'ready' | 'expired' | 'failed'; detail: string | null }
export interface WorkerState { trackId: string; phase: 'starting' | 'waiting-secrets' | 'reconfiguring' | 'installing-secret' | 'reconnecting' | 'lost' | 'after-connection-loss' | 'ready' | 'failed' | 'stopped' | 'needs-recovery' | 'retry' | 'working' | 'held' | 'stopping' | 'restoring' | 'approval' | 'question' | 'checkpoint'; sessionId: string | null; identity: { pid: number } | null; detail: string | null; activeTurn: string | null; output: string | null; checkpointPending: boolean; prompts: Prompt[]; checkpoint: { turnId: string; sha: string; reference: string; unmergedPaths: string[]; liveProcesses: boolean } | null; approvals: Approval[] }
export interface OutputPreview { sessionId: string; itemId: string; turnId: string; text: string; truncated: boolean }
export interface OutputFlow { preview?: OutputPreview | null; trackId?: string; repairRunId: string | null; queuedBytes: number; oldestQueuedMs: number; droppedDeltas: number; lagging: boolean; interrupted: boolean }
export interface RedirectState { inputId: string; revision: number; trackId: string; sessionId: string; turnId: string; phase: 'stopping' | 'ready' | 'sent' | 'done' | 'held' }
export interface BackupApply { opId: string; phase: 'capturing' | 'awaiting-receipt' | 'complete' | 'cancelled'; acknowledgeUnverified: boolean }
export interface FingerprintRecoveryState { sessions: { sessionId: string; trackId: string; reason: 'missing' | 'changed' | 'unexpected' }[]; projects: { projectId: string; reason: 'missing' | 'changed' | 'unexpected' }[]; rebuiltProjects?: string[]; historyReviewRequired?: boolean; pendingApply?: BackupApply }
export type BackupReviewRequest = { action: 'inspect' } | { action: 'reviewFiles'; opId: string; trackId: string } | { action: 'chooseSession'; opId: string; sessionId: string; choice: 'pick-up' | 'discard-history'; reviewId: string | null; acknowledgeUnread: boolean } | { action: 'stop'; opId: string; hostId: string; root: ProcessIdentity };
export interface BackupSessionReview { sessionId: string; trackId: string; reason: string; canPickUp: boolean; choice: { opId: string; choice: 'pick-up' | 'discard-history'; reviewId: string | null; acknowledgeUnread: boolean; needsRecheck: boolean; stale: boolean } | null }
export interface BackupFileReview { opId: string; trackId: string; baseline: string; seal: string; files: number; directories: number; changes: { path: string; kind: string }[]; protected: string[]; unsupported: string[]; attachments: { originals: number; verifiedOriginals: number; missingOriginals: string[]; invalidOriginals: string[]; unrecordedOriginals: string[]; copies: { path: string | null; attachmentId: string | null; state: string }[]; complete: boolean }; complete: boolean; stale: boolean }
export interface BackupReview {
  historyPaused: true; pendingOperations: number; missingProcessRecords: string[];
  sessions: BackupSessionReview[];
  apply?: BackupApply;
  fileReview?: BackupFileReview;
  workspaces: { trackId: string; sessionId: string | null; path: string; savedReference: { reference?: string; expected?: string; actual?: string | null; objectAvailable?: boolean; matches: boolean; issue?: string }; filesReviewed: boolean; attachmentsReviewed: boolean; lastReviewId: string | null }[];
  hosts: { id: string; workspace: string | null; root: ProcessIdentity | null; processes: number; groups: number; knownToLog: boolean; issue: string | null }[];
}
export interface UnloggedControl { opId: string; trackId?: string; paused?: boolean; unlogged: boolean }
export interface DaemonStatus { logUnavailable?: boolean; unloggedControls?: UnloggedControl[]; dispatchPaused?: boolean; runningProcesses?: number; processIssues?: string[]; fingerprintRecovery?: FingerprintRecoveryState | null; branches?: BranchState[]; pickups?: PickupState[]; redirects?: RedirectState[]; projectSecretIssue?: string | null; projectSecrets?: ProjectSecretState | null; secretInputs?: SecretInputState[]; outputFlows?: OutputFlow[]; repairRuns?: RepairRun[]; integration?: IntegrationJob | null; effects?: Effect[]; stops?: Stops; timeline?: TimelineState; connected: boolean; mode?: 'preview' | 'unavailable'; message?: string; daemonPid?: number; sequence?: number; editor?: EditorStatus; input?: { running: boolean; detail: string | null }; utterances?: Utterance[]; tracks?: LiveTrack[]; project?: Project | null; session?: Session | null; workspaces?: Workspace[]; preparation?: { phase: string; detail: string | null }; workerConfigured?: boolean; workers?: WorkerState[]; reportQuestions?: ReportQuestion[]; reports?: { id: string; trackId: string; pending: boolean }[] }
declare global {
  interface Window {
    intercat: {
      reviewExternal(request: { kind: 'image' | 'link'; url: string }): Promise<{ id: string; kind: 'image' | 'link'; url: string }>;
      confirmExternal(request: { id: string; kind: 'image' | 'link' }): Promise<{ dataUrl: string } | { opened: true }>;
      cancelExternal(id: string): Promise<void>;
      getStatus(): Promise<DaemonStatus>;
      reviewBackup(request: BackupReviewRequest): Promise<BackupReview>;
      applyBackup(request: { opId: string; acknowledgeUnverified: boolean }): Promise<DaemonStatus>;
      cancelBackup(request: { opId: string }): Promise<DaemonStatus>;
      retryConnection(): Promise<DaemonStatus>;
      rebuildFingerprints(request: { opId: string; projectId: string }): Promise<DaemonStatus>;
      getOutputFlows(): Promise<OutputFlow[]>;
      integrationCommand(request: IntegrationRequest): Promise<IntegrationJob>;
      startRepair(request: RepairRequest): Promise<import('./commit/repair').RepairResponse>;
      followUpRepair(request: import('./commit/repair').RepairFollowUp): Promise<RepairResponse>;
      refreshRepair(request: RepairRefreshRequest): Promise<RepairResponse>;
      controlRepair(request: { opId: string; id: string; revision: number; action: 'resume' | 'discard' }): Promise<RepairRun>;
      stopRepair(request: { opId: string; id: string }): Promise<void>;
      answerRepair(request: { opId: string; id: string; approvalId: string; requirement: unknown; allow: boolean }): Promise<void>;
      respondToRepair(request: { opId: string; id: string; promptId: string; revision: string; response: PromptResponse }): Promise<void>;
      getIntegrationDiff(request: DiffRequest): Promise<Diff>;
      restoreDeletion(request: { opId: string; trackId: string; repairRunId: string | null; snapshotId: string }): Promise<{ snapshotId: string; backupId: string; files: number }>;
      pauseDispatch(request: {opId:string; paused:boolean}): Promise<{paused:boolean}>;
      getQuitPreference(): Promise<'ask'|'background'|'stop'>;
      setQuitPreference(value:'ask'|'background'|'stop'): Promise<'ask'|'background'|'stop'>;
      setStops(request: { opId: string; stops: Stops }): Promise<{ stops: Stops }>;
      getTimelineWindow(request: import('./timeline/useWindow').TimelineBounds & { since: number | null }): Promise<{ revision: number; reset: boolean; clips: TimelineClip[]; removed: string[]; marks: TimelineMark[]; removedMarks: string[] }>;
      pickupFromFiles(request: { opId: string; trackId: string; sessionId: string }): Promise<PickupState>;
      cancelFileRecovery(request: { opId: string; recoveryId: string; trackId: string; sessionId: string }): Promise<PickupState>;
      branchFromCheckpoint(request: BranchRequest): Promise<BranchState>;
      getClipDetail(clipId: string): Promise<StepDetail>;
      createOperationId(): Promise<string>;
      installEditor(): Promise<EditorStatus | null>;
      retryEditor(opId: string): Promise<EditorStatus>;
      downloadEditor(opId: string): Promise<EditorStatus>;
      pauseEditorDownload(): Promise<EditorStatus>;
      configureProject(): Promise<{ project: Project | null; error?: string }>;
      configureWorkers(): Promise<{ configured: boolean; error?: string }>;
      startWorker(request: { opId: string; trackId: string }): Promise<void>;
      holdWorker(request: { opId: string; trackId: string }): Promise<void>;
      startHeldUtterance(request: { opId: string; utteranceId: string; revision: number }): Promise<Utterance>;
      stopWorker(request: { opId: string; trackId: string; turnId: string | null }): Promise<void>;
      answerWorker(request: { opId: string; trackId: string; approvalId: string; requirement: unknown; allow: boolean }): Promise<void>;
      getProjectSecretUsage(): Promise<ProjectSecretUsage[]>;
      getProjectSecretSettings(): Promise<ProjectSecretSettings>;
      retryProjectSecrets(): Promise<ProjectSecretSettings>;
      removeProjectSecret(request: { opId: string; name: string; revision: number }): Promise<{ removed: boolean }>;
      applyProjectSecrets(revision: number): Promise<void>;
      decideProjectSecretProcess(request: { nonce: string; identity: ProcessIdentity; keep: boolean }): Promise<void>;
      provideWorkerSecret(request: SecretEntry & { owner: SecretOwner; trackId: string; sessionId: string; turnId: string; question: SecretQuestion }): Promise<{ accepted: boolean }>;
      respondToWorker(request: { opId: string; trackId: string; promptId: string; revision: string; response: PromptResponse }): Promise<void>;
      respondToReport(request: { opId: string; questionId: string; revision: number; answer: ReportAnswer }): Promise<Utterance | null>;
      retryWorker(request: { opId: string; trackId: string }): Promise<void>;
      retryPreparation(opId: string): Promise<void>;
      attachReference(reselect: boolean): Promise<Attachment | null>;
      stageUtterance(request: { opId: string; text: string; attachmentIds: string[]; target: InputTarget; selectedTrackId: string | null }): Promise<Utterance>;
      cutUtterance(id: string): Promise<Utterance>;
      resolveUtterance(request: { opId: string; utteranceId: string; revision: number; resolution: InputResolution }): Promise<Utterance>;
    };
  }
}
