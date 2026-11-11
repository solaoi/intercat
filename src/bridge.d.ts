export interface CheckpointIssue { id: string; trackId: string; sessionId: string; turnId: string; revision: number; path: string }
export interface ExecutionLimitsSetting { revision: number; parallelWorkers: number; plannerReviewerTurns: number }
export interface PendingInheritance { operationId: string; trackId: string; sourceInputId: string; sourceState: string | null; state: 'prepared' | 'dispatched' }
export interface InheritanceIssue { id: string; input: string; trackId: string; sessionId: string | null; revision: number; parents: string[]; applicationId: string | null; blocker: { kind: 'processes' } | { kind: 'conflict'; paths: string[]; base: boolean } | { kind: 'other'; message: string }; kept: boolean; planId?: string; itemId?: string }
import type { Plan, PlanCommand } from './plans/types';
export interface ExecutionLimitsChange { opId: string; revision: number; parallelWorkers: number; plannerReviewerTurns: number }
export interface SchedulingStatus { limit: number; active: number; waiting: { id: string; priority: 'continuation' | 'user' | 'plan'; position: number }[] }
export interface PrivacyState { revision: number; mode: 'basic' | 'full' }
export interface PrivacyChange { opId: string; revision: number; mode: PrivacyState['mode'] }
export interface PrivacyView { text: string; parts: ({ kind: 'text'; text: string } | { kind: 'redaction'; id: number; category: 'api-key' | 'token' | 'email' | 'phone' | 'password' | 'name' | 'address' })[] }
export interface PickupState { opId: string; trackId: string; sessionId: string; phase: 'stopping' | 'capturing' | 'publishing' | 'summarizing' | 'starting' | 'awaiting-receipt' | 'ready' | 'cancelling' | 'cancelled'; snapshotSha: string | null }
import type { IntegrationJob, IntegrationRequest, DiffRequest, Diff } from './commit/types';
import type { RepairRequest, RepairRun, RepairRefreshRequest, RepairResponse } from './commit/repair';
export type InputTarget = { mode: 'none' | 'new' } | { mode: 'lock'; trackId: string };
export type WaitSource = { kind: 'turn'; value: StartTurn } | { kind: 'clip'; value: { id: string; ordinal: number; turn: StartTurn } } | { kind: 'plan-item'; value: { planId: string; itemId: string; title: string; label: string } };
export type WaitTarget = { kind: 'clip'; clipId: string } | { kind: 'plan-item'; planId: string; itemId: string; revision: number };
export type RequestEdit = {kind:'tryTwoWays';opId:string;selection:import('./twoWays/types').TwoWaysSelection} | { kind: 'cancel' | 'takeBack' } | { kind: 'text'; text: string; sources?: RequestSource[] } | { kind: 'waitFor'; source: WaitTarget } | { kind: 'readAs'; trackId: string; boundary: 'now' | 'after-turn' | 'when-told'; turn: { sessionId: string; turnId: string } | null };
export type EditAction = { kind: 'begin'; token: string; utteranceId: string; revision: number } | { kind: 'renew'; token: string } | { kind: 'apply'; opId: string; token: string; revision: number; edit: RequestEdit } | { kind: 'end'; token: string; revision: number; edit: RequestEdit };
export interface TimelineEdit { entryId: string; label: string; text: string; at: number; blocked: string | null }
export interface EditHistory { undo: TimelineEdit | null; redo: TimelineEdit | null; takenBack: { id: string; text: string; revision: number }[] }
export type HistoryAction = { kind: 'undo' | 'redo'; entryId: string } | { kind: 'restore' | 'restoreDraft'; utteranceId: string; revision: number };
export interface HistoryRequest { opId: string; action: HistoryAction }
export type Stops = 'irreversible-only' | 'every-approval';
export interface Effect { id: string; trackId: string; sessionId: string; itemId: string; turnId: string; clipId: string | null; kind: string; target: string; ruleId: string; outcome: 'done' | 'failed' | 'unknown'; running: boolean; at: number; reason: string | null }
export interface ClipWarning { kind: 'duplicate-title' | 'same-file' | 'file-comparison-unavailable'; text: string }
export interface TimelineClip { planId?: string; planItemId?: string; warnings?: ClipWarning[]; assumptionTimes?: number[]; assumptionCount?: number; assumptionIds?: string[]; title?: string; summary?: string; id: string; trackId: string; repairRunId: string | null; ordinal: number; sessionId: string; turnId: string | null; category: 'step' | 'shell' | 'snapshot' | 'review'; effectCount?: number; label: string; kind: 'read' | 'think' | 'edit' | 'shell'; start: number; end: number; running: boolean; generationEnd: number | null; approximate: boolean; toolTicks: { itemId: string; tool: string; status: string }[] }
export interface TimelineMark { commit?:{folderId:string;sha:string;jobId:string;pushed:boolean}; operation?: InputDecision['kind'] | null; command?: string | null; state?: Utterance['state']; attached?: boolean; id: string; trackId: string | null; repairRunId: string | null; at: number; inputId: string | null; text: string }
export interface TimelineState { clock: { epochMs: number; monoMs: number; bootId: string; timelineMs: number }; summaries: { trackId: string; sessionId: string; repairRunId: string | null; boundaryCursor: string }[] }
export interface OutputResult { content?: string; encoding?: string; byteLen?: number; eof?: boolean; unavailable?: boolean; _intercatTruncated?: string[] }
export interface StepItem { itemId: string; kind: string; revision: number; status: string; tool?: string; args?: string; commandText?: string; text?: string; visibleOutput?: string; failureReason?: string; recordedAt?: string; retracted?: boolean; truncated?: boolean; _intercatTruncated?: string[]; patchRef?: { id: string }; outputRef?: { id: string }; outputs: { patchRef?: OutputResult; outputRef?: OutputResult } }
export interface SessionModel { modelId: string | null; providerId: string | null; reasoningEffort: string | null }
export interface ReviewSummary { retryable?: boolean; id: string; revision: number; state: 'queued' | 'running' | 'complete' | 'failed' | 'held'; summary: string | null; detail: string | null }
export interface ReviewNote { fix?: { state: ReviewFix['state']; detail: string | null; inputId: string | null; inputRevision: number | null }; id: string; reviewId: string; targetSha: string; checkedSha: string; outdated: boolean; kind: 'issue' | 'suggestion' | 'question' | 'nitpick'; blocking: boolean; clipId: string; path: string; startLine: number; endLine: number; text: string }
export interface StepDetail { reviewedClips?: {id: string; label: string}[]; model?: SessionModel | null; assumptions?: Assumption[]; clip: TimelineClip; items: StepItem[]; deletionSnapshots?: DeletionSnapshot[]; effects?: Effect[]; reviews?: ReviewSummary[]; reviewNotes?: ReviewNote[] }
export interface StartTurn { trackId: string; label: string; sessionId: string; turnId: string }
export type InputChoice = { kind: 'instruction' | 'stop-and-send' | 'after-turn' | 'when-told' | 'now' } | { kind: 'command'; command: string } | { kind: 'after'; source: WaitSource };
export type WaitingRequest = { kind: 'input'; inputId: string; revision: number } | { kind: 'plan'; planId: string; revision: number };
interface CommandPresentationBase { operationId: string; receiptId: string; trackId: string | null; commitTracks?: string[] | null; publication?:{folders:string[];through:Record<string,string>}|null }
export type CommandPresentation = CommandPresentationBase & ({command:'commit'|'push'|'open-pr'|'settings'|'stage-mode'|'branch-from';restore?:never}|{command:'restore';restore:import('./restore/types').RestoreCandidate[]});
export type InputResolution = { action: 'retry'; target: InputTarget } | { action: 'hold' } | { action: 'confirm'; choice: InputChoice } | { action: 'startWaiting'; choice: WaitingRequest } | { action: 'presented' };
export interface InputReview { stage: 'operation' | 'lane' | 'trim' | 'boundary' | 'command' | 'stale-turn' | 'start-turn'; turn: { sessionId: string; turnId: string } | null }
export interface InputDecision { description?: { detail: string; line: string; title: string; createdAt: number }; kind: 'extend' | 'fork' | 'schedule' | 'trim' | 'park' | 'ask' | 'plan' | 'command'; trackId: string | null; boundary: string | null; command: string | null; confidence: number; inferenceMs: number; timedOut: boolean; timings?: { roundTripMs: number; inferenceMs: number }[] }
export interface Attachment { id: string; name: string; byteLen: number; sha256: string }
export interface RequestSource { attachmentId: string; role: 'request' | 'reference'; after: string }
export interface RephraseSubmission { opId: string; utteranceId: string; revision: number; text: string }
export interface ReviewFix { id: string; revision: number; noteId: string; inputId: string | null; trackId: string; state: 'planned' | 'held' | 'reserved' | 'running' | 'awaiting-review' | 'unresolved' | 'superseded' | 'resolved' | 'outdated' | 'cut' | 'blocked' | 'kept'; path: string; clipId: string; text: string; reason: string | null; fileAttempts: number; trackAttempts: number; attempts: { inputId: string; state: ReviewFix['state']; reason: string | null }[] }
export interface Utterance { twoWays?:import('./twoWays/types').TwoWaysJob|null; reviewFix?: { id: string; state: ReviewFix['state']; path: string; clipId: string } | null; reviewFixRunnable?: boolean; dependencyRetry?: { clipId: string; title: string }; dependencyRetryRunnable?: boolean; planManualStart?: boolean; planContinuation?: { title: string }; planId?: string; planItemId?: string; planRunnable?: boolean; executeAllowed?: boolean; reasonReview?: { label: string; reportId: string; reportRevision: number; turnId: string; checkpoint: string } | null; rephrase?: { state: string; proposal: string | null } | null; deadline?: { trackId: string | null; recommendation: { trackId: string | null } | null } | null; presentation?: CommandPresentation | null; startWaiting?: { candidates: (WaitingRequest & { text: string; label: string | null; available: boolean; reason: string | null })[]; started: WaitingRequest | null } | null; startCondition?: { source: WaitSource | null; candidates: WaitSource[]; ready: boolean; detail?: string | null } | null; confirmation?: { decisionId: string; turn: { sessionId: string; turnId: string } | null } | null; correction?: { explicit: number; inferred: number; misread: boolean } | null; reading?: { index: number; kind: 'input' | 'park'; state: 'queued' | 'running' | 'late' | 'complete' | 'stale' } | null; sourceText?: string; sources?: RequestSource[]; interruptStarted?: boolean; editHold?: { owner: string; token: string; revision: number } | null; id: string; text: string; attachments: Attachment[]; createdAt: number; state: 'staged' | 'reading' | 'placed' | 'asked' | 'held' | 'cut' | 'redirecting' | 'starting' | 'sent' | 'landed' | 'orphaned'; planReply?: { planId: string; question: string; state: 'landed' | 'orphaned' } | null; replyTo?: { id: string; revision: number; question?: ReportQuestion | null } | null; revision: number; target: InputTarget; provisionalTrackId?: string; decision: InputDecision | null; review?: InputReview | null; reason: string | null }
export interface TrackCompletion { sessionId: string; turnId: string; result: { itemId: string; revision: number; kind: 'done' | 'partial' | 'failed'; reason: string } | null }
export interface LiveTrack { id: string; label: string; title: string; status: string; completion?: TrackCompletion | null }
export interface EditorStatus { pid?: number | null; phase: 'not_configured' | 'starting' | 'ready' | 'unavailable' | 'stopped' | 'downloading' | 'download_paused'; process: boolean; accepting: boolean; modelLoaded: boolean; generation: number; restart: number; canRetry: boolean; detail?: string; download?: { received: number; total: number; phase: string; file: string | null } | null }
export interface Folder { id: string; name: string; path: string; root: string; commonDir: string; base: string; baseBranch: string | null }
export interface Project { id: string; name: string; folders: Folder[] }
export interface Session { id: string; slug: string; name: string; createdAt: number; bound?: boolean }
export interface Workspace { writerClosed?:boolean; deniedRoots: string[]; trackId: string; path: string; reference: string; base: string; attachmentRoots: string[]; state: 'pending' | 'preparing' | 'branching' | 'recovering' | 'fresh-publishing' | 'cancelled' | 'ready' | 'failed'; error: string | null }
export interface BranchRequest { opId: string; parentTrackId: string; point: { kind: 'turn'; turnId: string } | { kind: 'initial'; twoWaysId: string }; instructions: 'current' | 'checkpoint' }
export interface BranchOrigin { endedAt: number | null; timelineAt: number | null; turnNumber: number }
export interface BranchPoint extends BranchOrigin { id: number; turnId: string; checkpoint: string; shellWasRunning: boolean; available: boolean }
export interface BranchPoints { points: BranchPoint[]; before: number | null }
export type BranchStart = { kind: 'turn'; checkpoint: NonNullable<WorkerState['checkpoint']> } | { kind: 'initial'; snapshot: {operation: string; sha: string; reference: string} };
export interface BranchState { request: BranchRequest; trackId: string; parentSessionId: string; sessionId: string | null; workspace: Workspace; point: BranchStart; instructions: { reference: string; sha: string; changed: string[] } | null; origin: BranchOrigin; phase: 'preparing' | 'awaiting-receipt' | 'ready' | 'cancelled' }
export interface DeletionSnapshot { restoreOperationId?: string; id: string; trackId: string; repairRunId: string | null; itemId: string; sha: string; reference: string; paths: string[]; absent: string[]; directories: string[]; uncovered: string[]; complete: boolean; purpose: 'before-delete' | 'before-restore' }
export type ApprovalResponse = { action: 'allow' | 'deny' } | { action: 'reply'; text: string; mode: 'feedback' | 'restart' };
export interface Approval { replyMode?: 'feedback' | 'restart' | null; deletion?: { id: string; sha: string; files: number; complete: boolean; uncovered: number } | null; id: string; requirement: unknown; subject: string; reason: { kind: string; message: string }; canAllow: boolean; canDeny: boolean }
export interface InputQuestion { id: string; header: string; question: string; selection: { mode: 'single' | 'multiple'; minSelections?: number; maxSelections?: number }; options: { label: string; description?: string; preview?: { format: string; content: string } }[] }
export interface Prompt { request: { sessionId: string; userInputId: string; turnId: string; questions: InputQuestion[]; autoResolutionMs?: number }; revision: string; phase: 'open' | 'sending' | 'awaiting-settlement'; operation: string | null }
export interface PromptAnswer { questionId: string; selectedLabel?: string; selectedLabels?: string[]; freeText?: string; note?: string }
export type PromptResponse = { action: 'answer' | 'stop'; answers: PromptAnswer[] } | { action: 'explain'; content: string };
export interface Assumption {
  id: string;
  source: string;
  sourceId: string;
  trackId: string;
  clipId: string;
  questionId?: string | null;
  question: string;
  options: string[];
  text: string;
  evidence: string;
  revision: number;
  observedAt?: { epochMs: number; monoMs: number; bootId: string; timelineMs: number } | null;
  state: 'open' | 'kept' | 'changing' | 'changed' | 'superseded';
  keptAt: number | null;
  changedAt: number | null;
  changeInputId: string | null;
}
export interface AssumptionCommand {
  opId: string;
  assumptionId: string;
  revision: number;
  answer: { action: 'keep' } | { action: 'change'; text: string };
}
export type ProposalAction = 'commit' | 'continue' | 'later' | 'keep-plan' | 'parallel-plan' | 'realign' | 'open-pr';
export type ProjectSelectionIssue = {kind:'duplicate';roots:string[]} | {kind:'repositoryRoot';root:string;roots:string[]} | {kind:'ignored';path:string;root:string} | {kind:'submodules';path:string};
export interface ProjectSelection {id:string;issue:ProjectSelectionIssue|null;error:string|null;mayHaveSaved:boolean}
export interface ProjectSetupResult {project:Project|null;selection?:ProjectSelection;error?:string}
export type Proposal = { id: string; label: string; createdAt: number; text: string; detail: string; options: { action: ProposalAction; text: string; disabledReason?: string | null }[] } & ({ kind: 'work-completed' | 'quiet-changes' | 'work-overlap' | 'integration-fix'; trackId: string } | {kind:'publication'} | { kind: 'plan-conflict'; planId: string; planRevision: number; replyBlockedReason: string | null });
export interface PlanReplyRequest { opId: string; planId: string; revision: number; questionId: string; question: string; text: string }
export interface PlanReplyResult { inputId: string; planId: string; state: 'landed' | 'orphaned' }
export interface ProposalCommand { opId: string; proposalId: string; action: ProposalAction }
export interface ProposalResponse { realignment?:import('./commit/realign').RealignJob; proposalId: string; action: ProposalAction; trackId?: string; planId?: string; planRevision?: number; inputId?: string }
export interface ReportQuestion { planId?: string; planItemId?: string; id: string; reportId: string; trackId: string; sessionId: string; turnId: string; revision: number; kind: 'question' | 'review'; text: string; state: 'open' | 'queued' | 'held' | 'answered' | 'superseded'; replyUtteranceId: string | null; askedBeforeLastMessage?: boolean }
export type ReportAnswer = { action: 'reply'; text: string } | { action: 'continue' | 'hold' };
export interface ProjectSecretState { projectId: string; revision: number; names: string[]; allNames: string[]; ready: boolean }
export interface ProjectSecretSettings { project: ProjectSecretState; savedRevision: number; savedNames: string[]; issue: string | null }
export interface ProcessIdentity { pid: number; started_seconds: number; started_micros: number }
export interface ProjectSecretUsage { nonce: string; trackId: string; repairRunId: string | null; revision: number; names: string[]; processes: { identity: ProcessIdentity; host: boolean; kept: boolean; canStop: boolean }[] }
export type SecretQuestion = { kind: 'msp'; id: string; revision: string } | { kind: 'report'; id: string; revision: number };
export interface SecretEntry { scope: 'once' | 'project'; opId: string; fingerprintOpId: string; name: string; value: string }
export type SecretOwner = { kind: 'work' } | { kind: 'repair'; runId: string };
export interface SecretInputState { owner: SecretOwner; inputId: string; opId: string; trackId: string; name: string; state: 'prepared' | 'stopping' | 'settled' | 'restarting' | 'ready' | 'expired' | 'failed'; detail: string | null }
export interface WorkerState { pairedInstruction?: import('./twoWays/types').PairedInstruction | null; model?: SessionModel | null; trackId: string; phase: 'done' | 'fresh-releasing' | 'waiting-memory' | 'sleeping' | 'reviewing-fix' | 'review-conflict' | 'starting' | 'waiting-secrets' | 'reconfiguring' | 'installing-secret' | 'reconnecting' | 'lost' | 'after-connection-loss' | 'ready' | 'failed' | 'stopped' | 'needs-recovery' | 'retry' | 'working' | 'held' | 'stopping' | 'restoring' | 'stopping-processes' | 'finishing-inheritance' | 'restoring-path' | 'realigning' | 'approval' | 'question' | 'checkpoint-blocked' | 'checkpoint'; sessionId: string | null; identity: { pid: number } | null; detail: string | null; activeTurn: string | null; output: string | null; checkpointPending: boolean; prompts: Prompt[]; checkpoint: { turnId: string; sha: string; reference: string; unmergedPaths: string[]; liveProcesses: boolean } | null; approvals: Approval[] }
export interface OutputPreview { sessionId: string; itemId: string; turnId: string; text: string; truncated: boolean }
export interface OutputFlow { preview?: OutputPreview | null; trackId?: string; repairRunId: string | null; queuedBytes: number; oldestQueuedMs: number; droppedDeltas: number; lagging: boolean; interrupted: boolean }
export interface RedirectState { inputId: string; revision: number; trackId: string; sessionId: string; turnId: string; phase: 'stopping' | 'interrupting' | 'ready' | 'sent' | 'done' | 'held' }
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
export interface EditorEntry { id: number; trackId: string | null; scene: string; inputId: string | null; title: string; line: string; detail: string; createdAt: number; expiresAt: number | null }
export interface EditorReturn { id: string; from: number; to: number }
export interface EditorPresentation { character: 'speak' | 'mute'; hidden: boolean; returned: EditorReturn | null }
export interface EditorAwayPage { updates: number; effects: number; clips: number; waiting: string[]; entries: EditorEntry[]; before: number | null }
export interface EditorNotice extends EditorEntry { away?: { interval: EditorReturn; page: EditorAwayPage } }
export interface EditorLogPage { entries: EditorEntry[]; before: number | null }
export interface PlanDependencyLocation { unavailable?: boolean; id: string; planId: string | null; itemId: string | null; retryable: boolean; trackId: string; title: string; state: import('./plans/types').PlanItem['state']; unclear: boolean; clipId: string | null; atMs: number | null }
export interface HostMemoryStatus { sample: { freeBytes: number; pressured: boolean } | null; error: string | null; estimatedHostBytes: number; hosts: number; waiting: { owner: string; position: number }[] }
export interface DaemonStatus { twoWays?:import('./twoWays/types').TwoWaysJob[]; freshTracks?:import('./fresh/types').FreshJob[]; continuations?:import('./fresh/types').ConversationContinuation[]; comparisons?:import('./comparison/types').Comparison[]; pathRetirements?:import('./restore/types').Retirement[];pathRestores?:import('./restore/types').RestoreJob[];restoreFollowups?:import('./restore/types').RestoreFollowup[];asideProcessStops?:import('./restore/types').AsideStop[]; pullRequests?:import('./pull/types').PullJob[]; pushes?:import('./push/types').PushJob[]; commitGroups?:import('./commit/group').CommitGroup[]; projectSelection?: ProjectSelection|null; realignments?: import('./commit/realign').RealignJob[]; pendingRealignments?: import('./commit/realign').PendingRealignment[]; checkpointIssues?: CheckpointIssue[]; hostMemory?: HostMemoryStatus; reviewerLane?: {id: string; label: string; title: string; status: 'working' | 'waiting' | 'done' | 'stopped'; active: number; queued: number; jobs?: {id: string; revision: number; state: 'queued'|'running'}[]} | null; reviewFixes?: ReviewFix[]; planAssumptions?: import('./plans/types').PlanAssumptionCard[]; planDependencies?: Record<string, PlanDependencyLocation>; trackFolders?: Record<string,string>; sessionFolders?: { sessionId: string; folderId: string; slug: string }[]; inheritanceIssues?: InheritanceIssue[]; pendingInheritance?: PendingInheritance[]; plans?: Plan[]; scheduling?: SchedulingStatus; plannerScheduling?: SchedulingStatus; proposal?: Proposal | null; editorEntries?: EditorEntry[]; privacy?: PrivacyState; privacyRevision?: number; confirmation?: ConfirmationStatus; assumptions?: Assumption[]; editHistory?: EditHistory; logUnavailable?: boolean; unloggedControls?: UnloggedControl[]; dispatchPaused?: boolean; runningProcesses?: number; processIssues?: string[]; fingerprintRecovery?: FingerprintRecoveryState | null; branches?: BranchState[]; pickups?: PickupState[]; redirects?: RedirectState[]; projectSecretIssue?: string | null; projectSecrets?: ProjectSecretState | null; secretInputs?: SecretInputState[]; outputFlows?: OutputFlow[]; repairRuns?: RepairRun[]; integrations?: IntegrationJob[]; effects?: Effect[]; stops?: Stops; timeline?: TimelineState; connected: boolean; mode?: 'preview' | 'unavailable'; message?: string; daemonPid?: number; sequence?: number; editor?: EditorStatus; input?: { running: boolean; detail: string | null }; utterances?: Utterance[]; tracks?: LiveTrack[]; project?: Project | null; session?: Session | null; workspaces?: Workspace[]; preparation?: { phase: string; detail: string | null }; workerConfigured?: boolean; workers?: WorkerState[]; reportQuestions?: ReportQuestion[]; reports?: { id: string; trackId: string; pending: boolean }[] }
declare global {
  interface Window {
    intercat: {
      reviewExternal(request: { kind: 'image' | 'link'; url: string }): Promise<{ id: string; kind: 'image' | 'link'; url: string }>;
      confirmExternal(request: { id: string; kind: 'image' | 'link' }): Promise<{ dataUrl: string } | { opened: true }>;
      cancelExternal(id: string): Promise<void>;
      getStatus(): Promise<DaemonStatus>;
      readDrafts(session: string): Promise<import('./desk/drafts').SavedDrafts>;
      discardDrafts(session: string): Promise<import('./desk/drafts').SavedDrafts>;
      writeDrafts(request: import('./desk/drafts').DraftSave): Promise<import('./desk/drafts').DraftSaveResult>;
      reviewBackup(request: BackupReviewRequest): Promise<BackupReview>;
      applyBackup(request: { opId: string; acknowledgeUnverified: boolean }): Promise<DaemonStatus>;
      cancelBackup(request: { opId: string }): Promise<DaemonStatus>;
      retryConnection(): Promise<DaemonStatus>;
      rebuildFingerprints(request: { opId: string; projectId: string }): Promise<DaemonStatus>;
      getOutputFlows(): Promise<OutputFlow[]>;
      openComparison(request:import('./comparison/types').Open):Promise<import('./comparison/types').Comparison>;
      controlComparison(request:import('./comparison/types').Control):Promise<import('./comparison/types').Comparison>;
      readComparison(request:{id:string}):Promise<import('./comparison/types').Comparison>;
      comparisonEvidence(request:{id:string}):Promise<import('./comparison/types').Evidence>;
      comparisonDiff(request:import('./comparison/types').DiffRequest):Promise<import('./commit/types').Diff>;
      openPullRequest(request:import('./pull/types').PullOpen):Promise<import('./pull/types').PullJob>;
      pullCommand(request:import('./pull/types').PullCommand):Promise<import('./pull/types').PullJob>;
      readPullRequest(request:{id:string}):Promise<import('./pull/types').PullJob>;
      openPullOnGitHub(request:{id:string;folderId:string;kind:'pull'|'branch'}):Promise<void>;
      getPushRemotes(request:{folderId:string}):Promise<string[]>;
      openPushReview(request:import('./push/types').PushRequest):Promise<import('./push/types').PushPresentation>;
      getPushPresentation(request:{id:string}):Promise<import('./push/types').PushPresentation>;
      pushCommand(request:import('./push/types').PushCommand):Promise<import('./push/types').PushJob>;
      integrationCommand(request: IntegrationRequest): Promise<IntegrationJob>;
      openCommitGroup(request:{opId:string;tracks:string[]|null}):Promise<import('./commit/group').CommitGroup>;
      commitGroup(request:import('./commit/group').CommitGroupRequest):Promise<import('./commit/group').CommitGroup>;
      discardCommitGroup(request:{opId:string;id:string;revision:number}):Promise<import('./commit/group').CommitGroup>;
      requestRealign(request: import('./commit/realign').RealignRequest): Promise<import('./commit/realign').RealignJob>;
      restorePath(request:import('./restore/types').RestoreRequest):Promise<import('./restore/types').RestoreJob>;
      controlPathRestore(request:import('./restore/types').RestoreControl):Promise<import('./restore/types').RestoreJob>;
      approvePathRestoreAccess(request:import('./restore/types').RestoreAccess):Promise<import('./restore/types').RestoreJob>;
      getAsideProcesses(request:{retirementId:string}):Promise<import('./restore/types').AsideInfo>;
      stopAsideProcesses(request:import('./restore/types').RestoreRequest):Promise<import('./restore/types').AsideStop>;
      controlRealign(request: import('./commit/realign').RealignControl): Promise<import('./commit/realign').RealignJob>;
      startRepair(request: RepairRequest): Promise<import('./commit/repair').RepairResponse>;
      beginManualMerge(request: import('./commit/manual').Begin): Promise<RepairRun>;
      startManualMerge(request: RepairRequest): Promise<RepairRun>;
      readManualMerge(request: {id:string;revision:number;path:string|null}): Promise<import('./commit/manual').View>;
      applyManualMerge(request: import('./commit/manual').Apply): Promise<RepairRun>;
      openManualMerge(request: import('./commit/manual').Begin & {path:string}): Promise<void>;
      returnManualMerge(request: import('./commit/manual').Begin): Promise<RepairRun>;
      followUpRepair(request: import('./commit/repair').RepairFollowUp): Promise<RepairResponse>;
      refreshRepair(request: RepairRefreshRequest): Promise<RepairResponse>;
      decideRepairProcesses(request: import('./commit/repair').RunProcessCommand): Promise<void>;
      controlRepair(request: { opId: string; id: string; revision: number; action: 'resume' | 'discard' }): Promise<RepairRun>;
      stopRepair(request: { opId: string; id: string }): Promise<void>;
      answerRepair(request: { opId: string; id: string; approvalId: string; requirement: unknown; response: ApprovalResponse }): Promise<void>;
      respondToRepair(request: { opId: string; id: string; promptId: string; revision: string; response: PromptResponse }): Promise<void>;
      getIntegrationDiff(request: DiffRequest): Promise<Diff>;
      restoreDeletion(request: { opId: string; trackId: string; repairRunId: string | null; snapshotId: string }): Promise<{ snapshotId: string; backupId: string; files: number }>;
      pauseDispatch(request: {opId:string; paused:boolean}): Promise<{paused:boolean}>;
      getQuitPreference(): Promise<'ask'|'background'|'stop'>;
      setQuitPreference(value:'ask'|'background'|'stop'): Promise<'ask'|'background'|'stop'>;
      setStops(request: { opId: string; stops: Stops }): Promise<{ stops: Stops }>;
      getUtterance(id: string): Promise<Utterance>;
      getRequestOrigin(id: string): Promise<RequestOrigin | null>;
      getRequestChanges(identity: RequestTurnIdentity): Promise<RequestTurnChanges>;
      getRequestDiff(request: { identity: RequestTurnIdentity; checkpoint: string; path: string }): Promise<import('./commit/types').Diff>;
      stopRequestTurn(request: { opId: string; trackId: string; identity: RequestTurnIdentity }): Promise<void>;
      changeConfirmation(request: ConfirmationChange): Promise<ConfirmationStatus>;
      confirmUtterance(request: { opId: string; utteranceId: string; revision: number; decisionId: string }): Promise<Utterance>;
      markMisread(request: { opId: string; utteranceId: string; revision: number }): Promise<Utterance>;
      getEditorLog(query: { trackId: string; before: number | null }): Promise<EditorLogPage>;
      getEditorPresentation(): Promise<EditorPresentation>;
      setEditorCharacter(value: 'speak' | 'mute'): Promise<'speak' | 'mute'>;
      onEditorPresentation(listener: (state: EditorPresentation) => void): () => void;
      acknowledgeEditorReturn(id: string): Promise<void>;
      getEditorAway(query: { from: number; to: number; before: number | null }): Promise<EditorAwayPage>;
      getTimelineWindow(request: import('./timeline/useWindow').TimelineBounds & { since: number | null }): Promise<{ revision: number; reset: boolean; clips: TimelineClip[]; removed: string[]; marks: TimelineMark[]; removedMarks: string[] }>;
      observeTimeline(view: { trackId: string; repairRunId: string | null } | null): Promise<void>;
    readTwoWaysOptions(request:{inputId:string;revision:number}):Promise<import('./twoWays/types').TwoWaysOptions>;
    readTwoWays(request:{id:string}):Promise<import('./twoWays/types').TwoWaysJob>;
    commandTwoWays(request:import('./twoWays/types').TwoWaysCommand):Promise<import('./twoWays/types').TwoWaysJob>;
    finishTwoWays(request:{id:string}):Promise<import('./twoWays/types').TwoWaysJob>;
      prepareFresh(request: import('./fresh/types').FreshRequest): Promise<import('./fresh/types').FreshJob>;
      commandFresh(request: import('./fresh/types').FreshCommand): Promise<import('./fresh/types').FreshJob>;
      readFresh(request: {id:string}): Promise<import('./fresh/types').FreshJob>;
      pickupFromFiles(request: { opId: string; trackId: string; sessionId: string }): Promise<PickupState>;
      cancelFileRecovery(request: { opId: string; recoveryId: string; trackId: string; sessionId: string }): Promise<PickupState>;
      branchFromCheckpoint(request: BranchRequest): Promise<BranchState>;
      getBranchPoints(request: { trackId: string; before: number | null; range: { start: number; end: number } | null }): Promise<BranchPoints>;
      getClipDetail(clipId: string): Promise<StepDetail>;
      commandReviewFix(request: {opId: string; fixId: string; revision: number; action: 'fixOnceMore' | 'keepIt'}): Promise<unknown>;
      stopReview(request: {opId: string; reviewId: string; revision: number}): Promise<ReviewSummary>;
      retryReview(request: {opId: string; reviewId: string; revision: number}): Promise<ReviewSummary>;
      createOperationId(): Promise<string>;
      editHistory(request: HistoryRequest): Promise<DaemonStatus>;
      onHistoryShortcut(listener: (direction: 'undo' | 'redo') => void): () => void;
      nativeHistory(direction: 'undo' | 'redo'): Promise<void>;
      installEditor(): Promise<EditorStatus | null>;
      retryEditor(opId: string): Promise<EditorStatus>;
      downloadEditor(opId: string): Promise<EditorStatus>;
      pauseEditorDownload(): Promise<EditorStatus>;
      assignFolder(request: { opId: string; trackId: string; folderId: string }): Promise<void>;
      configureProject(): Promise<ProjectSetupResult>;
      resolveProjectSelection(request:{id:string;action:'useRoot'|'retry'|'cancel'}):Promise<ProjectSetupResult>;
      configureWorkers(): Promise<{ configured: boolean; error?: string }>;
      startWorker(request: { opId: string; trackId: string }): Promise<void>;
      excludeCheckpointRepository(request: { opId: string; issueId: string; revision: number }): Promise<void>;
      finishInheritance(request: { opId: string; trackId: string; applicationId: string }): Promise<void>;
      resolveInheritanceIssue(request: { opId: string; issueId: string; action: 'keep' | 'stop' }): Promise<void>;
      holdWorker(request: { opId: string; trackId: string }): Promise<void>;
      startHeldUtterance(request: { opId: string; utteranceId: string; revision: number }): Promise<Utterance>;
      stopWorker(request: { opId: string; trackId: string; turnId: string | null }): Promise<void>;
      answerWorker(request: { opId: string; trackId: string; approvalId: string; requirement: unknown; response: ApprovalResponse }): Promise<void>;
      getProjectSecretUsage(): Promise<ProjectSecretUsage[]>;
      getProjectSecretSettings(): Promise<ProjectSecretSettings>;
      replyToPlan(request: PlanReplyRequest): Promise<PlanReplyResult>;
      commandPlan(request: PlanCommand): Promise<Plan>;
      editPlan(action: import('./plans/types').PlanRewriteAction): Promise<Plan | null>;
      getExecutionLimits(): Promise<ExecutionLimitsSetting>;
      getPlansStart(): Promise<import('./plans/types').PlansStartSetting>;
      changePlansStart(request: import('./plans/types').PlansStartChange): Promise<import('./plans/types').PlansStartSetting>;
      observePlans(request: import('./plans/types').PlanObservation): Promise<import('./plans/types').PlanStartUpdate>;
      changeExecutionLimits(request: ExecutionLimitsChange): Promise<ExecutionLimitsSetting>;
      getChecks(): Promise<ChecksState>;
      changeChecks(request: ChecksChange): Promise<ChecksSetting>;
      getCheckEnvironment(): Promise<CheckEnvironmentState>;
      chooseCheckEnvironment(): Promise<string | null>;
      changeCheckEnvironment(request: CheckEnvironmentChange): Promise<CheckEnvironmentSelection>;
      retryProjectSecrets(): Promise<ProjectSecretSettings>;
      removeProjectSecret(request: { opId: string; name: string; revision: number }): Promise<{ removed: boolean }>;
      applyProjectSecrets(revision: number): Promise<void>;
      decideProjectSecretProcess(request: { nonce: string; identity: ProcessIdentity; keep: boolean }): Promise<void>;
      provideWorkerSecret(request: SecretEntry & { owner: SecretOwner; trackId: string; sessionId: string; turnId: string; question: SecretQuestion }): Promise<{ accepted: boolean }>;
      respondToWorker(request: { opId: string; trackId: string; promptId: string; revision: string; response: PromptResponse }): Promise<void>;
      answerProposal(request: ProposalCommand): Promise<ProposalResponse>;
      answerAssumption(request: AssumptionCommand): Promise<Assumption>;
      respondToReport(request: { opId: string; questionId: string; revision: number; answer: ReportAnswer }): Promise<Utterance | null>;
      retryWorker(request: { opId: string; trackId: string }): Promise<void>;
      retryPreparation(opId: string): Promise<void>;
      stagePastedText(request: { opId: string; text: string }): Promise<{ id: string; words: number }>;
      finishPastedText(id: string): Promise<Attachment>;
      changePrivacy(request: PrivacyChange): Promise<PrivacyState>;
      previewPrivacy(texts: string[]): Promise<PrivacyView[]>;
      readPrivacyOriginal(id: number): Promise<string>;
      readPastedText(id: string): Promise<string>;
      refreshPastedText(request: { opId: string; attachmentId: string }): Promise<{ attachmentId: string; sha256: string; byteLen: number; changed: boolean }>;
      readPastedReferences(request: { trackId: string; after: string | null }): Promise<{ items: Attachment[]; next: string | null }>;
      attachReference(reselect: boolean): Promise<Attachment | null>;
      attachDroppedReference(file: File): Promise<Attachment>;
      stageUtterance(request: { opId: string; text: string; executeAllowed?: boolean; planItem?: { planId: string; itemId: string; revision: number }; attachmentIds: string[]; sources?: RequestSource[]; target: InputTarget; selectedTrackId: string | null }): Promise<Utterance>;
      cutUtterance(id: string): Promise<Utterance>;
      rephraseUtterance(request: RephraseSubmission): Promise<Utterance>;
      resolveUtterance(request: { opId: string; utteranceId: string; revision: number; resolution: InputResolution }): Promise<Utterance>;
      editUtterance(action: EditAction): Promise<Utterance>;
    };
  }
}

export interface RequestTurnIdentity { utteranceId: string; sessionId: string; turnId: string }
export interface RequestOrigin { identity: RequestTurnIdentity; trackId: string; label: string; phase: 'running' | 'stopping' | 'saving' | 'finished' | 'moved-on' | 'unavailable'; checkpoint: string | null }
export interface RequestTurnChanges { identity: RequestTurnIdentity; label: string; checkpoint: string; files: string[] }

export type ConfirmationPolicy = 'automatic' | 'always' | 'off';
export interface ConfirmationStatus { revision: number; policy: ConfirmationPolicy; active: boolean; dismissedThrough: number; suggested: boolean; suggestedFor: string | null; notice: string; rates: { requests: number; explicit: number; inferred: number } }
export interface ConfirmationChange { opId: string; revision: number; action: { kind: 'policy'; policy: ConfirmationPolicy } | { kind: 'back-to-automatic' } }

export type CheckCommand = { mode: 'auto' | 'disabled' } | { mode: 'run'; command: string };
export interface FolderChecks { revision: number; command: CheckCommand; history: boolean }
export interface ChecksSetting { revision: number; folders: Record<string, FolderChecks> }
export interface ChecksState { setting: ChecksSetting; folders: Folder[] }
export interface ChecksChange { opId: string; revision: number; folder: string; command: CheckCommand; history: boolean }
export interface CheckEnvironmentSelection { revision: number; file: string | null }
export interface CheckEnvironmentChange extends CheckEnvironmentSelection { opId: string }
export interface CheckEnvironmentState { selection: CheckEnvironmentSelection; candidates: string[]; available: boolean }
