import type {PullJob,PullSelection} from './pull/types';
import PublicationActions from './push/PublicationActions';
import type {PushJob,PushSelection} from './push/types';
import { folderChoiceCards } from './desk/FolderChoice';
import ProjectSelectionCard from './desk/ProjectSelection';
import type {ProjectSetupResult} from './bridge';
import { reviewFixCards } from './desk/ReviewConflict';
import { queuedTracks } from './scheduling';
import { Plans } from './plans/Plans';
import { ContinuationContext } from './plans/ContinuationContext';
import type { Plan, PlanRequest } from './plans/types';
import { usePlanStart } from './plans/usePlanStart';
import { PlanOnlyButton } from './plans/PlanOnlyButton';
import { PrivacyEnabled, PrivacySummary, PrivacyText, PrivacyRevision, privacyLabel, usePrivacyLabels, usePrivacyText } from './desk/PrivacyText';
import { ApprovalDraftStore, ApprovalReplyRecovery } from './desk/ApprovalDrafts';
import { StageDivider, type DeskLayout } from './desk/StageDivider';
import { StagePanels } from './desk/StagePanels';
import { useApplicationCommands } from './desk/useApplicationCommands';
import { BranchDialog, BranchRecovery, branchInstructionCards, type BranchSelection } from './timeline/BranchDialog';
import type { BranchPreview } from './timeline/branchPointLayout';
import { useBranchPoints } from './timeline/useBranchPoints';
import { PickupButton } from './desk/PickupButton';
import { RequestTarget } from './desk/RequestTarget';
import React, { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import '@fontsource/inter/400.css';
import '@fontsource/inter/500.css';
import '@fontsource/inter/600.css';
import '@fontsource/jetbrains-mono/400.css';
import './style.css';
import { Timeline } from './timeline/Timeline';
import { FreshDialog, freshCards } from './fresh/FreshDialog';
import { TwoWaysDialog,twoWaysCards } from './twoWays/TwoWaysDialog';
import { preparing as preparingTwoWays,type TwoWaysCommand,type TwoWaysJob } from './twoWays/types';
import { currentWriter, writingTarget, type FreshJob, type FreshRequest, type FreshCommand } from './fresh/types';
import { advanceView, cutPlannedTurn, initialView, sampleTracks, type Clip, type Track } from './timeline/model';
import type { Approval, WorkerState, DaemonStatus, InputResolution, Utterance, ReportQuestion, ReportAnswer, Attachment, PlanDependencyLocation } from './bridge';
import { liveTimeline } from './timeline/state';
import { useTimelineWindow, type TimelineBounds } from './timeline/useWindow';
import { useTimelineObservation } from './timeline/useObservation';
import { mergeStatus } from './status';
import { ConfirmationNotice } from './desk/Confirmation';
import { Requests, requestCards } from './desk/Requests';
import type { AttentionCard } from './desk/NeedsYou';
import { realignmentCards } from './commit/Realignment';
import {realignMergeOwner,type RealignJob} from './commit/realign';
import { integrationFailure } from './commit/IntegrationFailure';
import { useDrafts, type DraftEntry, type DraftSubmission, type EditLease } from './desk/drafts';
import { RequestEditor } from './desk/RequestEditor';
import { useDraftEditing } from './desk/useDraftEditing';
import { usePlanEditing } from './plans/usePlanEditing';
import { useTimelineHistory } from './desk/useTimelineHistory';
import { HistoryControls } from './desk/HistoryControls';
import { PastedSources } from './desk/PastedSources';
import { Workers, workerCards } from './desk/Workers';
import { inheritanceIssueCards } from './desk/InheritanceIssue';
import { checkpointIssueCards } from './desk/CheckpointIssue';
import { runProcessCards } from './desk/RunProcesses';
import { WorkerModel } from './desk/WorkerModel';
import { DispatchControls } from './desk/DispatchControls';
import { assumptionCards } from './desk/AssumptionCard';
import { planAssumptionCards } from './plans/PlanAssumptions';
import { proposalCards } from './desk/ProposalCard';
import { ReportProgress, reportCards } from './desk/ReportQuestions';
import { EditorLine } from './desk/EditorLine';
import { EditorLog } from './desk/EditorLog';
import { useEditorPresentation } from './desk/useEditorPresentation';
import { EditorStatus } from './desk/EditorStatus';
import { PolicySettings } from './desk/PolicySettings';
import { FingerprintRecovery } from './desk/FingerprintRecovery';
import { contextState, reanchor, type CommitContext } from './commit/context';
import type {Retirement,RestoreSelection} from './restore/types';
import {RestorePicker,savedRestoreEntry} from './restore/RestorePicker';
import {restoreCards} from './restore/cards';

const Diagnostics = lazy(() => import('./preview/Diagnostics'));
const DocumentPreview = lazy(() => import('./preview/DocumentPreview'));
const StepInspector = lazy(() => import('./timeline/StepInspector'));
const CommitSheet = lazy(() => import('./commit/CommitSheet'));
const PushSheet=lazy(()=>import('./push/PushSheet'));
const PullSheet=lazy(()=>import('./pull/PullSheet'));
const RealignMergeSheet=lazy(()=>import('./commit/RealignMergeSheet'));
const RestoreSheet=lazy(()=>import('./restore/RestoreSheet'));
const isPlanStatus = (text: string) => /^(starts in [1-5] s|held|editing|queued|waiting for your go)$/.test(text);

function App() {
  const [previewTracks, setPreviewTracks] = useState(sampleTracks);
  const [selected, setSelected] = useState('A');
  const [selectedRepair, setSelectedRepair] = useState<string | null>(null);
  const [view, setView] = useState(initialView);
  const [playing, setPlaying] = useState(false);
  const [notice, setNotice] = useState('');
  const [editingRequest, setEditingRequest] = useState<{ word: Utterance; editText: boolean; lease: EditLease | null } | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [deskLayout, setDeskLayout] = useState<DeskLayout>({ mode: 'normal', height: null });
  const stageMode = deskLayout.mode === 'stage';
  const stageElement = useRef<HTMLElement>(null);
  const deskElement = useRef<HTMLElement>(null);
  function setStageMode(enabled: boolean) {
    const restoreFocus = document.activeElement instanceof HTMLElement && !!document.activeElement.closest('.needs-you,.stage-panel-controls');
    setDeskLayout({ mode: enabled ? 'stage' : 'normal', height: null });
    if (restoreFocus) requestAnimationFrame(() => document.querySelector<HTMLElement>(enabled ? '.stage-needs-you:not(:disabled), [aria-label="Request draft"]' : '[aria-label="Request draft"]')?.focus());
  }
  const [commitOpen, setCommitOpen] = useState(false);
  const [realignSelection,setRealignSelection]=useState<string|null>(null);
  const [restoreSelection,setRestoreSelection]=useState<RestoreSelection|null>(null);
  const [pushSelection,setPushSelection]=useState<PushSelection|null>(null);
  const [pullSelection,setPullSelection]=useState<PullSelection|null>(null);
  const integrationOpen=commitOpen||realignSelection!==null||restoreSelection!==null||pushSelection!==null||pullSelection!==null;
  const observePull=(job:PullJob)=>setStatus(current=>({...current,pullRequests:[...(current.pullRequests??[]).filter(old=>old.id!==job.id||old.revision>job.revision),...((current.pullRequests??[]).some(old=>old.id===job.id&&old.revision>job.revision)?[]:[job])]}));
  const observePush=(job:PushJob)=>setStatus(current=>({...current,pushes:[...(current.pushes??[]).filter(old=>old.id!==job.id||old.revision>job.revision),...((current.pushes??[]).some(old=>old.id===job.id&&old.revision>job.revision)?[]:[job])]}));
  const [commitJobId,setCommitJobId] = useState<string|undefined>();
  const [branchSelection, setBranchSelection] = useState<BranchSelection | null>(null);
  const [branchDialogPreview, setBranchDialogPreview] = useState<BranchPreview | null>(null);
  const [commitRequest, setCommitRequest] = useState<import('./commit/types').IntegrationRequest | undefined>();
  const [clip, setClip] = useState<{ track: Track; clip: Clip; repairRunId: string | null } | null>(null);
  const [status, setStatus] = useState<DaemonStatus>({ connected: false });
  const [twoWaysSelection,setTwoWaysSelection]=useState<string|null>(null);
  const twoWaysCommands=useRef(new Map<string,TwoWaysCommand>());
  const observeTwoWays=(job:TwoWaysJob)=>setStatus(current=>{const old=current.twoWays?.find(value=>value.request.opId===job.request.opId);return old&&old.revision>job.revision?current:{...current,twoWays:[...(current.twoWays??[]).filter(value=>value.request.opId!==job.request.opId),job]};});
  const [freshSelection,setFreshSelection]=useState<FreshRequest|null>(null);
  const freshCommands=useRef(new Map<string,FreshCommand>());
  const pendingFresh=useRef(new Map<string,FreshRequest>());
  const closedWriters=useMemo(()=>new Set((status.continuations??[]).map(link=>link.source)),[status.continuations]);
  const observeFresh=(job:FreshJob)=>{pendingFresh.current.delete(`${job.request.trackId}:${job.request.sessionId}`);setStatus(current=>{
    const old=current.freshTracks?.find(value=>value.request.opId===job.request.opId);
    return old&&old.revision>job.revision?current:{...current,freshTracks:[...(current.freshTracks??[]).filter(value=>value.request.opId!==job.request.opId),job]};
  });};
  function viewTrack(id:string){if(saving||saveError)return;setSelectedPlanItem(null);openingInput.current++;setSelected(id);setSelectedRepair(null);setClip(null);}
  async function openFresh(worker:WorkerState){
    if(!worker.sessionId||closedWriters.has(worker.trackId))return;
    const key=`${worker.trackId}:${worker.sessionId}`;
    const saved=status.freshTracks?.findLast(job=>job.request.trackId===worker.trackId&&job.request.sessionId===worker.sessionId&&job.phase!=='cancelled');
    const pending=pendingFresh.current.get(key);
    try {setFreshSelection(saved?.request??(status.freshTracks?.some(job=>job.request.opId===pending?.opId)?undefined:pending)??{opId:await window.intercat.createOperationId(),trackId:worker.trackId,sessionId:worker.sessionId});}
    catch{setNotice('The fresh conversation could not be opened. Try again.');}
  }
  const retiredTracks=useMemo(()=>new Set((status.pathRetirements??[]).filter(entry=>entry.phase!=='released').map(entry=>entry.track)),[status.pathRetirements]);
  const updatePlan = useCallback((plan: Plan) => setStatus(current => ({ ...current, plans: current.plans?.map(old => old.id === plan.id && plan.rev >= old.rev ? plan : old) })), []);
  const [planRequest, setPlanRequest] = useState<PlanRequest | null>(null);
  const [selectedPlanItem, setSelectedPlanItem] = useState<string | null>(null);
  const [focusPlanItem, setFocusPlanItem] = useState<{ id: string; nonce: number } | null>(null);
  const [timelineBounds, setTimelineBounds] = useState<TimelineBounds | null>(null);
  const { clips: timelineClips, marks: timelineMarks } = useTimelineWindow(timelineBounds, status);
  const showTimeline = useCallback((bounds: TimelineBounds) => setTimelineBounds(current => JSON.stringify(current) === JSON.stringify(bounds) ? current : bounds), []);
  const draftScope = useRef<string | null>(null);
  if (status.connected || status.mode === 'preview') draftScope.current = status.session?.id ?? 'home';
  const drafts = useDrafts(draftScope.current);
  const draft = drafts.entry.text;
  const draftTarget = drafts.entry.target;
  const effectiveDraftTarget=writingTarget(draftTarget,status.continuations??[]);
  const references = drafts.entry.attachments;
  const sources = drafts.entry.sources ?? [];
  const hasDraft = !!draft.trim() || sources.length > 0;
  const sourcesPending = sources.some(source => !source.attachment);
  const editing = useDraftEditing(drafts, updated => setStatus(current => ({ ...current, utterances: current.utterances?.map(word => word.id === updated.id ? updated : word) })));
  const planEditing = usePlanEditing(drafts, updatePlan, (plan, itemId) => setPlanRequest({ id: plan.id, itemId, nonce: performance.now() }));
  const context = drafts.entry.context;
  const planReplyContext = context.kind === 'plan-reply' ? context : null;
  const planEditContext = context.kind === 'plan-item' ? context : null;
  const continuationContext = context.kind === 'continuation' ? context : null;
  const continuationPlan = status.plans?.find(plan => plan.id === continuationContext?.planId);
  const staleContinuation = !!continuationContext && (continuationPlan?.rev !== continuationContext.revision || !continuationPlan?.items.some(item => item.planItemId === continuationContext.itemId && ['running', 'succeeded', 'failed'].includes(item.state)));
  function continuePlanItem(plan: Plan, id: string) {
    const item = plan.items.find(item => item.planItemId === id);
    if (!item || !drafts.ready || saving) return;
    drafts.openContinuation({ kind: 'continuation', planId: plan.id, itemId: id, title: item.title, revision: plan.rev, trackId: item.trackRef });
    requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus());
  }
  async function openDependency(location: PlanDependencyLocation) {
    if (!location.clipId || location.atMs === null) return;
    const request = ++openingInput.current;
    try {
      const result = await window.intercat.getTimelineWindow({ tracks: [location.trackId], start: Math.max(0, location.atMs - 1), end: location.atMs + 1, since: null });
      if (request !== openingInput.current) return;
      const track = liveTimeline(status, result.clips).find(track => track.id === location.trackId);
      const selected = track?.clips.find(clip => clip.id === location.clipId);
      if (!track || !selected) throw new Error('The saved dependency step is unavailable.');
      setSelectedPlanItem(null); setSelected(track.id); setSelectedRepair(null); setClip({ track, clip: selected, repairRunId: null });
      setView(current => ({ ...current, start: Math.max(0, selected.start - current.duration * 0.2) }));
    } catch (error) { setNotice(error instanceof Error ? error.message : 'The dependency could not be opened.'); }
  }
  const editContext = context.kind === 'edit' ? context : null;
  const replyTarget = context.kind === 'reply' ? context.question : null;
  const currentReplyQuestion = status.reportQuestions?.find(question => question.id === replyTarget?.id);
  const staleReply = !!replyTarget && (!currentReplyQuestion || currentReplyQuestion.revision !== replyTarget.revision);
  const commitContext = context.kind === 'commit' ? context.context : null;
  const repairTargetId = context.kind === 'repair' ? context.id : null;
  const submission = drafts.entry.pending?.kind === 'work' ? drafts.entry.pending.request : null;
  const repairSubmission = drafts.entry.pending?.kind === 'repair' ? drafts.entry.pending.request : null;
  const commitSubmission = drafts.entry.pending?.kind === 'commit' ? drafts.entry.pending : null;
  const saveError = drafts.entry.pending !== null;
  const [attaching, setAttaching] = useState(false);
  const [attachmentError, setAttachmentError] = useState(false);
  const [composerDrop, setComposerDrop] = useState(false);
  const pendingDrops = useRef<{ files: File[]; store: ReturnType<typeof useDrafts>; draftId: string } | null>(null);
  const importingDrops = useRef(false);
  const [saving, setSaving] = useState(false);
  const [choosingProject, setChoosingProject] = useState(false);
  const [projectError, setProjectError] = useState('');
  const [workerSetup, setWorkerSetup] = useState(false);
  const [workerError, setWorkerError] = useState('');
  const heldStarts = useRef(new Map<string, { opId: string; utteranceId: string; revision: number }>());
  const interaction = useRef(0);
  const openingInput = useRef(0);
  const liveClock = useRef<{ now: number; received: number; bootId: string } | null>(null);
  const timelineNow = useCallback(() => {
    const clock = liveClock.current;
    return clock ? clock.now + Math.min(2, (performance.now() - clock.received) / 1000) : null;
  }, []);
  const [returnedAt, setReturnedAt] = useState(0);
  const presentation = useEditorPresentation(interval => {
    setReturnedAt(interval.to);
    void window.intercat.getStatus().then(value => {
      observeStatus(value);
      setView(current => {
        const now = value.timeline ? value.timeline.clock.timelineMs / 1000 : timelineNow() ?? current.now;
        const duration = (interval.to - interval.from) / 800;
        return { now, duration, start: now - duration * 0.8 };
      });
    }).catch(() => {});
  });
  const observeStatus = useCallback((value: DaemonStatus) => {
    if (value.connected && value.mode !== 'preview' && value.timeline) {
      const { timelineMs, bootId } = value.timeline.clock;
      const previous = liveClock.current;
      if (!previous || previous.bootId !== bootId || timelineMs / 1000 >= previous.now) {
        liveClock.current = { now: timelineMs / 1000, received: performance.now(), bootId };
        if (!previous || previous.bootId !== bootId) setView({ start: timelineMs / 1000 - 384, duration: 480, now: timelineMs / 1000 });
      }
    } else liveClock.current = null;
    setStatus(current => mergeStatus(current, value));
  }, []);
  const historyController = useTimelineHistory(drafts.session, status.editHistory, observeStatus, word => openRequest(word, true));
  const preview = status.mode === 'preview';
  const branchTrack = clip && !clip.repairRunId && !clip.track.reviewer && !retiredTracks.has(clip.track.id) && !clip.clip.running ? clip.track.id : null;
  const savedBranchPoints = useBranchPoints(branchTrack, Math.max(0, Math.floor(view.start * 1000)), Math.ceil((view.start + view.duration) * 1000), status.workers?.find(worker => worker.trackId === branchTrack)?.checkpoint?.turnId ?? null, status.connected && !preview);
  const branchPreview: BranchPreview | null = branchDialogPreview ?? (branchTrack ? { trackId: branchTrack, points: savedBranchPoints.points,
    selectedTurnId: savedBranchPoints.points.find(point => point.available && point.timelineAt !== null && point.timelineAt <= clip!.clip.end * 1000)?.turnId ?? null } : null);
  async function openBranchPoint(trackId: string, at: number | null) {
    if (settingsOpen || integrationOpen || branchSelection || freshSelection || twoWaysSelection || editingRequest) return;
    try { setBranchSelection({ operationId: await window.intercat.createOperationId(), trackId, at }); }
    catch { setNotice('The branch preparation could not be opened. Try again.'); }
  }
  const privacyVersion = `${status.privacyRevision ?? 0}:${status.privacy?.mode === 'full' ? `${status.editor?.generation}:${status.editor?.modelLoaded}:${status.editor?.accepting}` : 'basic'}`;
  const applicationCommand = useApplicationCommands({
    connected: status.connected,
    words: status.utterances ?? [],
    blocked: settingsOpen || integrationOpen || !!branchSelection || !!editingRequest || saving,
    previewOpen: !!clip,
    open: (word) => {
      const presentation = word.presentation!;
      if (presentation.command === 'stage-mode') setStageMode(true);
      else if (presentation.command === 'settings') setSettingsOpen(true);
      else if(presentation.command==='push'||presentation.command==='open-pr') {
        const folder=presentation.trackId?status.trackFolders?.[presentation.trackId]:null;
        const folders=presentation.trackId?(folder?[folder]:[]):(status.project?.folders??[]).filter(folder=>status.integrations?.some(job=>job.project.id===folder.id&&job.status==='done')).map(folder=>folder.id);
        setPushSelection({operationId:presentation.operationId,folders:presentation.publication?.folders??folders,through:presentation.publication?.through,...(presentation.command==='open-pr'?{intent:'pull' as const}:{})});
      }
      else if(presentation.command==='restore') {
        const candidate=presentation.restore.length===1?presentation.restore[0]:undefined;
        const entry=candidate?savedRestoreEntry(candidate,status.pathRetirements??[],status.session?.id):undefined;
        setRestoreSelection({entry:entry??null,presentation});
      }
      else if (presentation.command === 'branch-from') {
        const selectedStep = clip && !clip.repairRunId && !clip.track.reviewer ? clip : null;
        const source = presentation.trackId ?? selectedStep?.track.id ?? null;
        setBranchSelection({ operationId: presentation.operationId, trackId: source,
          at: selectedStep?.track.id === source ? timelineClips.find(item => item.id === selectedStep.clip.id)?.end ?? null : null });
      }
      else {
        setCommitJobId(undefined);
        setCommitRequest({ opId: presentation.operationId, action: { action: 'open', tracks: presentation.commitTracks ?? (presentation.trackId ? [presentation.trackId] : null) } });
        setCommitOpen(true);
      }
    },
    visible: presentation => presentation.command==='restore'?!!document.querySelector(`[data-restore-operation="${presentation.operationId}"]`):presentation.command==='push'||presentation.command==='open-pr'?pushSelection?.operationId===presentation.operationId||pullSelection?.operationId===presentation.operationId:presentation.command === 'stage-mode' ? stageMode : presentation.command === 'settings' ? settingsOpen : presentation.command === 'branch-from' ? branchSelection?.operationId === presentation.operationId : commitOpen && commitRequest?.opId === presentation.operationId,
    updated: word => setStatus(current => ({ ...current, utterances: current.utterances?.map(previous => previous.id === word.id ? word : previous) })),
  });
  const countdowns = usePlanStart(status, updatePlan);
  const tracks = useMemo(() => {
    const tracks = preview ? previewTracks : liveTimeline(status, timelineClips);
    return tracks.map(track => ({ ...track, planned: track.planned.map(item => {
      const countdown = item.planId ? countdowns[item.planId] : undefined;
      const plan = status.plans?.find(plan => plan.id === item.planId);
      return countdown && countdown.revision === plan?.rev && item.after === 'waiting for your go' ? { ...item, after: `starts in ${countdown.seconds} s` } : item;
    }) }));
  }, [preview, previewTracks, status.tracks, status.branches, status.continuations, status.workers, status.project, status.trackFolders, status.workspaces, status.pathRetirements, timelineClips, status.repairRuns, status.utterances, status.plans, status.planDependencies, countdowns]);
  const timelineTexts = useMemo(() => [...timelineMarks.map(mark => mark.text), ...tracks.flatMap(track => [track.title, ...track.clips.map(clip => clip.label), ...track.planned.flatMap(turn => isPlanStatus(turn.after) ? [turn.label] : [turn.label, turn.after]), ...track.repairs.flatMap(repair => repair.clips.map(clip => clip.label))])], [tracks, timelineMarks]);
  const privateTimelineLabel = usePrivacyLabels(timelineTexts, privacyVersion, !preview);
  const privateTracks = useMemo(() => tracks.map(track => ({ ...track, title: privateTimelineLabel(track.title), clips: track.clips.map(clip => ({ ...clip, label: privateTimelineLabel(clip.label) })), planned: track.planned.map(turn => ({ ...turn, label: privateTimelineLabel(turn.label), after: isPlanStatus(turn.after) ? turn.after : privateTimelineLabel(turn.after) })), repairs: track.repairs.map(repair => ({ ...repair, clips: repair.clips.map(clip => ({ ...clip, label: privateTimelineLabel(clip.label) })) })) })), [tracks, privateTimelineLabel]);
  const privateMarks = useMemo(() => timelineMarks.map(mark => ({ ...mark, text: privateTimelineLabel(mark.text) })), [timelineMarks, privateTimelineLabel]);
  function branchCreated(branch: import('./bridge').BranchState) {
    setBranchSelection(null); setSelected(branch.trackId); setSelectedRepair(null); setClip(null);
    if (drafts.ready && !drafts.entry.pending) { drafts.open({ kind: 'work' }); drafts.setTarget({ mode: 'lock', trackId: branch.trackId }); }
    else setNotice('Branch ready · finish saving the current request before changing its destination.');
    setStatus(current => ({ ...current, branches: [...(current.branches ?? []).filter(item => item.request.opId !== branch.request.opId), branch] }));
    void window.intercat.getStatus().then(observeStatus).catch(() => {});
    requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus());
  }
  const active = tracks.find(track => track.id === selected);
  useTimelineObservation(status, active?.reviewer ? null : active?.id ?? null, selectedRepair);
  const activeRequest = status.utterances?.find(word => word.decision?.kind === 'fork' && word.decision.trackId === selected);
  const deskContextSource = activeRequest?.text ?? active?.title ?? 'Editor decides';
  const deskPrivacy = usePrivacyText(deskContextSource, !preview && !!(activeRequest || active), privacyVersion);
  const deskContext = deskPrivacy.view?.text ?? deskPrivacy.error ?? 'Checking privacy…';
  const activeRepair = status.repairRuns?.find(run => run.id === repairTargetId);
  const missingDraftTarget = !effectiveDraftTarget || effectiveDraftTarget.mode === 'lock' && (!tracks.some(track => track.id === effectiveDraftTarget.trackId)||retiredTracks.has(effectiveDraftTarget.trackId));
  const commitContextState = commitContext ? contextState(commitContext, status.integrations?.find(job=>job.id===commitContext.jobId)) : null;
  const contextRepair = commitContext ? status.repairRuns?.find(run => run.owner.kind==='commit'&&run.owner.id === commitContext.jobId && !['done', 'discarded'].includes(run.phase)) : null;
  const modelTrackId = commitContext?.track ?? replyTarget?.trackId
    ?? (repairTargetId ? activeRepair?.track : effectiveDraftTarget?.mode === 'lock' ? effectiveDraftTarget.trackId : active?.reviewer ? undefined : active ? currentWriter(active.id,status.continuations??[])??undefined : undefined);
  const modelTrack = tracks.find(track => track.id === modelTrackId);
  const modelRepair = activeRepair ?? contextRepair;
  const requestModel = planReplyContext ? null : modelRepair ? modelRepair.model : commitContext || repairTargetId ? null
    : status.workers?.find(worker => worker.trackId === modelTrackId)?.model;
  const modelPrefix = commitContext || repairTargetId ? `Worker ${modelTrack?.label ?? '?'} · for the commit`
    : draftTarget.mode === 'lock' ? `Only ${modelTrack?.label ?? 'unavailable track'}` : active?.reviewer ? 'Editor decides' : `Worker ${modelTrack?.label ?? '?'}`;
  const canSendContext = commitContextState === 'current' && (!contextRepair || (contextRepair.track === commitContext?.track && contextRepair.phase === 'held' && !contextRepair.restoring && (!contextRepair.refresh || !!contextRepair.refresh.error)));
  const preparationError = status.preparation?.detail ?? status.workspaces?.find(workspace => workspace.state === 'failed')?.error;
  const sessionSource = status.session?.name || status.utterances?.[0]?.text || '';
  const sessionPrivacy = usePrivacyText(sessionSource, !preview && !!sessionSource, privacyVersion);
  const sessionName = preview ? 'Login hardening' : !sessionSource ? 'New session' : sessionPrivacy.view ? privacyLabel(sessionPrivacy.view.text, 30) : sessionPrivacy.error ?? 'Checking privacy…';
  useEffect(() => {
    let disposed = false, pending = false;
    const refresh = () => { if (pending) return; pending = true; void window.intercat.getStatus().then(value => { if (!disposed) observeStatus(value); }).catch(() => {}).finally(() => { pending = false; }); };
    refresh();
    const timer = setInterval(refresh, 1000);
    return () => { disposed = true; clearInterval(timer); };
  }, [observeStatus]);
  useEffect(() => {
    let disposed = false, pending = false;
    const refresh = async () => {
      if (pending) return;
      pending = true;
      try { const outputFlows = await window.intercat.getOutputFlows(); if (!disposed) setStatus(current => JSON.stringify(current.outputFlows ?? []) === JSON.stringify(outputFlows) ? current : ({ ...current, outputFlows })); }
      catch { /* Keep the latest observation while the daemon reconnects. */ }
      finally { pending = false; }
    };
    const timer = setInterval(() => { void refresh(); }, 33);
    return () => { disposed = true; clearInterval(timer); };
  }, []);
  async function retryPreparation() {
    try { await window.intercat.retryPreparation(await window.intercat.createOperationId()); }
    catch { setNotice('Preparation could not be retried. Please reconnect and try again.'); }
  }
  async function startWorker(trackId: string) {
    if(closedWriters.has(trackId)){setNotice('Use the continuation to start new work.');return;}
    if(retiredTracks.has(trackId)){setNotice('Restore this worker before starting it.');return;}
    try { await window.intercat.startWorker({ opId: await window.intercat.createOperationId(), trackId }); }
    catch { setNotice('This worker cannot start yet. Review its waiting requests.'); }
  }
  async function stopWorker(trackId: string) {
    const turnId = status.workers?.find(worker => worker.trackId === trackId)?.activeTurn ?? null;
    try { await window.intercat.stopWorker({ opId: await window.intercat.createOperationId(), trackId, turnId }); }
    catch { setNotice('Stop could not be confirmed. Check the worker connection and log.'); }
  }
  async function pauseDispatch(paused: boolean) {
    try {
      const result=await window.intercat.pauseDispatch({opId:await window.intercat.createOperationId(),paused});
      setStatus(current=>({...current,dispatchPaused:result.paused}));
    } catch { setNotice(paused ? 'Stop could not be confirmed. The emergency interrupt was requested.' : 'Resume could not be confirmed. Work remains held.'); }
  }
  async function holdWorker(trackId: string) {
    try { await window.intercat.holdWorker({ opId: await window.intercat.createOperationId(), trackId }); }
    catch { setNotice('The worker could not hold its next requests.'); }
  }
  async function startWord(word: Utterance) {
    if(word.decision?.kind!=='command'&&word.decision?.trackId&&retiredTracks.has(word.decision.trackId)){setNotice('Restore this worker before starting its requests.');return;}
    if (!word.review && word.decision && (['fork', 'extend', 'schedule'].includes(word.decision.kind)
      || word.decision.kind === 'command' && ['commit', 'push', 'open-pr', 'restore', 'settings', 'stage-mode', 'branch-from'].includes(word.decision.command ?? ''))) {
      try {
        let request = heldStarts.current.get(word.id);
        if (!request) {
          request = { opId: await window.intercat.createOperationId(), utteranceId: word.id, revision: word.revision };
          heldStarts.current.set(word.id, request);
        }
        const updated = await window.intercat.startHeldUtterance(request);
        heldStarts.current.delete(word.id);
        setStatus(current => ({ ...current, utterances: current.utterances?.map(item => item.id === updated.id ? updated : item) }));
      } catch { setNotice('This request could not start. Review its current state and try again.'); }
    } else await resolveWord(word, { action: 'retry', target: word.decision?.trackId ? { mode: 'lock', trackId: word.decision.trackId } : word.target });
  }
  async function stopRepair(id: string) {
    try { await window.intercat.stopRepair({ opId: await window.intercat.createOperationId(), id }); }
    catch { setNotice('The repair Stop could not be delivered. Check its connection.'); }
  }
  async function answerWorker(opId: string, worker: WorkerState, card: Approval, response: import('./bridge').ApprovalResponse) {
    await window.intercat.answerWorker({ opId, trackId: worker.trackId, approvalId: card.id, requirement: card.requirement, response });
  }
  async function retryWorker(trackId: string) {
    try { await window.intercat.retryWorker({ opId: await window.intercat.createOperationId(), trackId }); }
    catch { setNotice('The worker could not be restarted. Check its state and try again.'); }
  }
  async function setupWorkers() {
    setWorkerSetup(true); setWorkerError('');
    try {
      const result = await window.intercat.configureWorkers();
      if (result.error) setWorkerError(result.error);
      if (result.configured) setStatus(current => ({ ...current, workerConfigured: true }));
    } catch { setWorkerError('The Meta worker could not be configured.'); }
    finally { setWorkerSetup(false); }
  }
  function projectSelected(result:ProjectSetupResult) {
    setStatus(current=>({...current,project:result.project??current.project,projectSelection:result.selection??null}));
    if (result.error) setProjectError(result.error);
  }
  async function setupProject() {
    setChoosingProject(true); setProjectError('');
    try {
      const result = await window.intercat.configureProject();
      projectSelected(result);
    } catch { setProjectError('The project could not be configured. Please try again.'); }
    finally { setChoosingProject(false); }
  }
  async function sendCommitInstruction(context: CommitContext, current: DaemonStatus) {
    let prepared: DraftSubmission | null = commitSubmission;
    if (!prepared) {
      if (contextState(context, current.integrations?.find(job=>job.id===context.jobId)) !== 'current') { setNotice('The commit changed. Review the selected lines before sending.'); return; }
      const run = current.repairRuns?.find(run => run.owner.kind==='commit'&&run.owner.id === context.jobId && !['done', 'discarded'].includes(run.phase));
      if (run && (run.track !== context.track || run.phase !== 'held' || run.restoring || (run.refresh && !run.refresh.error))) { setNotice('Stop the current repair before sending another instruction.'); return; }
      const opId = await window.intercat.createOperationId();
      prepared = run ? { kind: 'commit', method: 'followup', request: { opId, id: run.id, revision: run.revision, text: draft, selection: context.selection } }
        : { kind: 'commit', method: 'start', request: { opId, owner:{kind:'commit',id:context.jobId,revision:context.revision}, track: context.track, purpose: { kind: 'edit', selection: context.selection, text: draft } } };
    }
    const pending = await drafts.prepare(prepared);
    if (pending.kind !== 'commit') throw new Error('A different draft operation is pending.');
    const result = pending.method === 'start' ? await window.intercat.startRepair(pending.request) : await window.intercat.followUpRepair(pending.request);
    if (result.status === 'rejected') { drafts.clearPending(); setNotice(result.reason); return; }
    drafts.finish(); setNotice('Instruction sent for this commit.');
    void window.intercat.getStatus().then(observeStatus).catch(() => {});
  }
  async function resendCommitInstruction() {
    if (!commitContext) return;
    openingInput.current++;
    setSaving(true);
    try {
      const current = await window.intercat.getStatus();
      const job = current.integrations?.find(job=>job.id===commitContext.jobId);
      if (!job || job.id !== commitContext.jobId || ['done', 'discarded'].includes(job.status)) { setStatus(current); setNotice('The commit is gone. Choose whether to send this as ordinary work.'); return; }
      const diff = await window.intercat.getIntegrationDiff({ id: job.id, revision: job.revision, owner:{kind:'worker',track:commitContext.track}, path: commitContext.selection.path });
      const updated = reanchor(commitContext, job, diff);
      if (!updated) { setStatus(current); setNotice('Those lines could not be found uniquely. Select them again in the current diff.'); return; }
      drafts.replaceContext({ kind: 'commit', context: updated }); setStatus(current);
      await sendCommitInstruction(updated, current);
    } catch { setNotice(drafts.entry.pending?.kind === 'commit' ? 'Save could not be confirmed. Retry uses the same operation ID.' : 'The current diff could not be loaded. Your draft has been kept.'); }
    finally { setSaving(false); }
  }
  async function attachReference(reselect = false) {
    if (importingDrops.current) return;
    if (reselect) pendingDrops.current = null;
    if (pendingDrops.current?.files.length) { await importDroppedFiles(); return; }
    setAttaching(true); setAttachmentError(false);
    const draftId = drafts.book.active;
    try {
      const reference = await window.intercat.attachReference(reselect);
      if (reference) {
        if (references.length >= 128 || references.reduce((sum, file) => sum + file.byteLen, reference.byteLen) > 100 * 1024 * 1024) { setNotice('Reference files can total up to 100 MiB per request. Remove a file before adding another.'); return; }
        drafts.setAttachments(files => files.some(file => file.id === reference.id) ? files : [...files, reference], draftId);
      }
    } catch { setAttachmentError(true); setNotice('The reference could not be saved. Retry keeps the same file, or choose another file.'); }
    finally { setAttaching(false); }
  }
  async function importDroppedFiles() {
    const pending = pendingDrops.current;
    if (importingDrops.current || !pending) return;
    importingDrops.current = true;
    setAttaching(true);
    setAttachmentError(false);
    const { store, draftId, files } = pending;
    let bytes = (store.book.entries[draftId]?.attachments ?? []).reduce((sum, file) => sum + file.byteLen, 0);
    try {
      while (files.length) {
        if (store.session !== draftScope.current || !store.book.entries[draftId]) throw new Error('Original draft unavailable');
        const reference = await window.intercat.attachDroppedReference(files[0]);
        if (store.session !== draftScope.current || !store.book.entries[draftId]) throw new Error('Original draft unavailable');
        if (bytes + reference.byteLen > 100 * 1024 * 1024) throw new Error('Reference size limit');
        bytes += reference.byteLen;
        store.setAttachments(saved => saved.some(file => file.id === reference.id) ? saved : [...saved, reference], draftId);
        files.shift();
      }
      pendingDrops.current = null;
    } catch {
      setAttachmentError(true);
      setNotice(store.session !== draftScope.current ? 'Return to the original session to retry these reference files.' : 'The reference could not be saved. Retry keeps the same file, or choose another file.');
    } finally { importingDrops.current = false; setAttaching(false); }
  }
  function dropFiles(files: File[], trackId?: string) {
    setComposerDrop(false);
    if (!files.length || !canDropFiles || importingDrops.current) return;
    if(trackId&&closedWriters.has(trackId)){setNotice('Attach the reference to the continuation.');return;}
    if(trackId&&retiredTracks.has(trackId)){setNotice('Restore this worker before adding requests.');return;}
    if (trackId && continuationContext && trackId !== continuationContext.trackId) { setNotice('This instruction keeps its selected plan item. Attach the reference in the input area.'); return; }
    if (trackId && !status.tracks?.some(track => track.id === trackId)) { setNotice('That worker is no longer available.'); return; }
    const total = references.reduce((sum, file) => sum + file.byteLen, 0) + files.reduce((sum, file) => sum + file.size, 0);
    if (references.length + files.length > 128 || total > 100 * 1024 * 1024 || files.some(file => file.size > (file.type.startsWith('image/') ? 10 : 25) * 1024 * 1024)) {
      setNotice('Reference files can total up to 100 MiB, with 25 MiB per file and 10 MiB per image.');
      return;
    }
    if (trackId) drafts.setTarget({ mode: 'lock', trackId });
    pendingDrops.current = { files, store: drafts, draftId: drafts.book.active };
    void importDroppedFiles();
  }
  const canDropFiles = drafts.ready && status.connected && !preview && !saving && !saveError && !attaching && !replyTarget && !repairTargetId && !commitContext && !editContext && !planEditContext && !planReplyContext;
  function pasteDraft(event: React.ClipboardEvent<HTMLTextAreaElement>, segment: string | null = null) {
    const text = event.clipboardData.getData('text/plain');
    if (new TextEncoder().encode(text).byteLength > 2 * 1024 * 1024) {
      event.preventDefault();
      setNotice('This paste exceeds 2 MiB. Shorten it and try again.');
      return;
    }
    let characters = 0;
    for (const _ of text) { if (++characters > 600) break; }
    if (characters <= 600 || (context.kind !== 'work' && context.kind !== 'continuation') || preview) return;
    event.preventDefault();
    if (sources.length + references.length >= 128 || attaching || saving || saveError) {
      setNotice('Finish saving this request before adding another pasted block.'); return;
    }
    const field = event.currentTarget;
    const before = field.value;
    const start = field.selectionStart;
    const end = field.selectionEnd;
    const draftId = drafts.book.active;
    setAttaching(true);
    void (async () => {
      try {
        const pasted = await window.intercat.stagePastedText({ opId: await window.intercat.createOperationId(), text });
        drafts.insertPaste(pasted.id, pasted.words, segment, before, start, end, draftId);
        await drafts.flush();
      } catch (error) { setNotice(error instanceof Error ? error.message : 'The pasted text could not be saved.'); }
      finally { setAttaching(false); }
    })();
  }
  function composerKeyDown(event: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (event.key === 'Escape' && planReplyContext && !event.nativeEvent.isComposing && !saving && !saveError) { event.preventDefault(); drafts.back(); return; }
    if (event.key === 'Escape' && continuationContext && !event.nativeEvent.isComposing && !saving && !saveError) { event.preventDefault(); drafts.back(); return; }
    if (event.key === 'Escape' && planEditContext && !event.nativeEvent.isComposing && !saving && !saveError) { event.preventDefault(); void planEditing.cancel(); return; }
    if (event.key === 'Escape' && editContext && !event.nativeEvent.isComposing && !saving && !saveError) { event.preventDefault(); void editing.cancel(); return; }
    if (event.key === 'Escape' && commitContext && !saving && !saveError) { event.preventDefault(); drafts.replaceContext({ kind: 'work' }); return; }
    if (event.metaKey && event.shiftKey && event.code === 'KeyP' && !event.nativeEvent.isComposing) { event.preventDefault(); void saveDraft(false, false, true); return; }
    if (event.metaKey && event.key === 'Enter' && hasDraft && !saving && !attaching && !sourcesPending && status.connected) { event.preventDefault(); void saveDraft(event.shiftKey); }
  }
  async function saveDraft(newTrack = false, asWork = false, planOnly = false) {
    if (!drafts.ready) return;
    if (planReplyContext && new TextEncoder().encode(draft).byteLength > 32 * 1024) { setNotice('Plan replies can contain up to 32 KiB. Shorten this reply before sending.'); return; }
    if (planReplyContext && (newTrack || planOnly)) { setNotice('This reply belongs to the selected plan question.'); return; }
    if (continuationContext && (newTrack || planOnly)) { setNotice('This instruction belongs to the selected plan item.'); return; }
    if (continuationContext && !submission && staleContinuation) { setNotice('The plan changed. Review the item before sending.'); return; }
    if (continuationContext && !submission && !draft.trim() && sources.every(source => source.role === 'reference' && !source.after.trim())) { setNotice('Write an instruction for this plan item.'); return; }
    if (planOnly && (replyTarget || repairTargetId || commitContext || editContext || planEditContext || planReplyContext || !hasDraft || saving || saveError || attaching || !status.connected)) return;
    if (sourcesPending) { setNotice('The pasted text is still being saved.'); return; }
    const referenceBytes = references.reduce((sum, file) => sum + file.byteLen, 0) + sources.reduce((sum, source) => sum + (source.attachment?.byteLen ?? 0), 0);
    if (references.length + sources.length > 128 || referenceBytes > 100 * 1024 * 1024) {
      setNotice('Reference files and pasted blocks can total up to 100 MiB per request. Remove a reference before sending.'); return;
    }
    if (!submission && !newTrack && !asWork && !replyTarget && !repairTargetId && !commitContext && !editContext && !planEditContext && !planReplyContext && missingDraftTarget) { setNotice('Choose another track before sending.'); return; }
    if (attaching || (references.length > 0 && !asWork && (replyTarget || repairTargetId || commitContext))) { setNotice('Send these reference files with an ordinary request, or remove them before replying.'); return; }
    openingInput.current++;
    setSaving(true);
    try {
      if (planReplyContext && !asWork) {
        const pending = await drafts.prepare({ kind: 'plan-reply', request: { opId: await window.intercat.createOperationId(), planId: planReplyContext.planId, revision: planReplyContext.revision, questionId: planReplyContext.questionId, question: planReplyContext.question, text: draft } });
        if (pending.kind !== 'plan-reply') throw new Error('A different draft operation is pending.');
        const result = await window.intercat.replyToPlan(pending.request);
        drafts.finish();
        setNotice(result.state === 'orphaned' ? 'The plan changed. Your reply is kept for review.' : 'Reply saved · revising the plan.');
        void window.intercat.getStatus().then(observeStatus).catch(() => {}); return;
      }
      if (planEditContext && !asWork && !submission) { await planEditing.submit(); return; }
      if (editContext && !asWork && !submission) {
        if (!await editing.submit()) setNotice('Already sent. Choose whether to send a new instruction.');
        return;
      }
      if (commitContext && !submission && !asWork) { await sendCommitInstruction(commitContext, status); return; }
      if ((repairTargetId || repairSubmission) && !submission && !asWork) {
        if (!repairSubmission && (activeRepair?.phase !== 'held' || !!activeRepair.restoring || (!!activeRepair.refresh && !activeRepair.refresh.error))) {
          setNotice('Stop this repair before sending another instruction.'); return;
        }
        const pending = await drafts.prepare({ kind: 'repair', request: repairSubmission ?? { opId: await window.intercat.createOperationId(), id: repairTargetId!, revision: activeRepair!.revision, text: draft } });
        if (pending.kind !== 'repair') throw new Error('A different draft operation is pending.');
        const result = await window.intercat.followUpRepair(pending.request);
        if (result.status === 'rejected') { drafts.clearPending(); setNotice(result.reason); return; }
        drafts.finish(); setNotice('Instruction sent to repair.');
        void window.intercat.getStatus().then(observeStatus).catch(() => {}); return;
      }
      const pending = await drafts.prepare({ kind: 'work', request: submission ?? { opId: await window.intercat.createOperationId(), text: draft, executeAllowed: !planOnly, attachmentIds: references.map(file => file.id),
        ...(sources.length ? { sources: sources.map(source => ({ attachmentId: source.id, role: source.role, after: source.after })) } : {}),
        ...(continuationContext ? { planItem: { planId: continuationContext.planId, itemId: continuationContext.itemId, revision: continuationContext.revision } } : {}),
        target: continuationContext ? { mode: 'lock', trackId: continuationContext.trackId } : newTrack ? { mode: 'new' } : asWork && commitContext ? { mode: 'lock', trackId: commitContext.track } : effectiveDraftTarget!,
        selectedTrackId: continuationContext ? continuationContext.trackId : asWork && commitContext ? commitContext.track : active?.reviewer ? null : active ? currentWriter(active.id,status.continuations??[]) : null, ...(replyTarget && !asWork ? { replyTo: { id: replyTarget.id, revision: replyTarget.revision } } : {}) } });
      if (pending.kind !== 'work') throw new Error('A different draft operation is pending.');
      const request = pending.request;
      const saved = request.replyTo ? await window.intercat.respondToReport({ opId: request.opId, questionId: request.replyTo.id, revision: request.replyTo.revision, answer: { action: 'reply', text: request.text } }) : await window.intercat.stageUtterance(request);
      if (!saved) throw new Error('Reply was not saved');
      setStatus(current => ({ ...current, utterances: [...(current.utterances ?? []).filter(item => item.id !== saved.id), saved] }));
      drafts.finish();
    } catch { setNotice('Save could not be confirmed. Retry uses the same operation ID.'); }
    finally { setSaving(false); }
  }
  function draftLabel(entry: DraftEntry) {
    const context = entry.context;
    const target = entry.target;
    if (context.kind === 'plan-reply') return `reply to plan · ${context.label}`;
    if (context.kind === 'continuation') return `instruction for ${context.title}`;
    if (context.kind === 'plan-item') return `revision of ${context.title}`;
    if (context.kind === 'edit') return `edit of ${context.originalText.slice(0, 30)}`;
    if (context.kind === 'reply') return `Reply to Worker ${status.tracks?.find(track => track.id === context.question.trackId)?.label ?? '?'}`;
    if (entry.context.kind === 'commit') return `draft for this commit · ${entry.context.context.label}`;
    if (entry.context.kind === 'repair') return 'draft for the repair';
    if (target.mode === 'lock') return `draft to Worker ${status.tracks?.find(track => track.id === target.trackId)?.label ?? '?'}`;
    return 'draft to Editor';
  }
  async function respondReport(question: ReportQuestion, answer: ReportAnswer) {
    try { await window.intercat.respondToReport({ opId: await window.intercat.createOperationId(), questionId: question.id, revision: question.revision, answer }); }
    catch { setNotice('The question changed. Refresh before answering.'); }
  }
  async function openTimelineInput(id: string) {
    if (saving || saveError) return;
    const reading = ++openingInput.current;
    try {
      const word = await window.intercat.getUtterance(id);
      if (openingInput.current !== reading) return;
      if (word.reviewFix) {
        const detail=await window.intercat.getClipDetail(word.reviewFix.clipId);
        if (openingInput.current !== reading) return;
        const track=liveTimeline(status,[detail.clip]).find(value=>value.id===detail.clip.trackId);
        const chosen=track?.clips.find(value=>value.id===detail.clip.id);
        if (track && chosen) {setSelected(track.id);setSelectedRepair(null);setClip({track,clip:chosen,repairRunId:null});}
        return;
      }
      if (word.planId && !word.planContinuation) { setPlanRequest({ id: word.planId, itemId: word.planItemId, nonce: performance.now() }); return; }
      if (word.decision?.trackId) { setSelected(word.decision.trackId); setSelectedRepair(null); setClip(null); }
      openRequest(word, false);
    } catch { if (openingInput.current === reading) setNotice('This request could not be loaded. Try again.'); }
  }
  const reviewStops = useRef(new Map<string,{opId:string;reviewId:string;revision:number}>());
  async function stopReviews(jobs: {id:string;revision:number}[]) {
    for (const job of jobs) {
      try {
        const key=`${job.id}:${job.revision}`;
        let request=reviewStops.current.get(key);
        if(!request){request={opId:await window.intercat.createOperationId(),reviewId:job.id,revision:job.revision};reviewStops.current.set(key,request);}
        await window.intercat.stopReview(request);reviewStops.current.delete(key);
      } catch(error){setNotice(error instanceof Error?error.message:'The review could not be stopped.');}
    }
  }
  async function openReviewedStep(id: string) {
    const reading=++openingInput.current;
    try {
      const detail=await window.intercat.getClipDetail(id);
      if(openingInput.current!==reading)return;
      const track=liveTimeline(status,[detail.clip]).find(value=>value.id===detail.clip.trackId);
      const chosen=track?.clips.find(value=>value.id===id);
      if(track&&chosen){setSelected(track.id);setSelectedRepair(null);setClip({track,clip:chosen,repairRunId:null});}
    } catch(error){setNotice(error instanceof Error?error.message:'The reviewed step is unavailable.');}
  }
  function openRequest(word: Utterance, editText: boolean) {
    const paired=word.twoWays&&status.twoWays?.find(job=>job.request.opId===word.twoWays?.request.opId)||word.twoWays;
    if(paired&&preparingTwoWays(paired)){setTwoWaysSelection(paired.request.opId);return;}
    openingInput.current++;
    const id = `edit:${word.id}`;
    const entry = drafts.book.entries[id];
    if (entry?.pending) {
      if (drafts.book.active !== id) drafts.resume(id);
      setNotice('Retry the saved change before editing this request again.');
      requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus());
      return;
    }
    const context = entry?.context;
    setEditingRequest({ word, editText, lease: context?.kind === 'edit' ? context.lease : null });
  }
  function observeRequest(word: Utterance) {
    editing.observe(word);
    if(word.twoWays&&preparingTwoWays(word.twoWays)){observeTwoWays(word.twoWays);setEditingRequest(null);setTwoWaysSelection(word.twoWays.request.opId);return;}
    setEditingRequest(current => current?.word.id === word.id ? { ...current, lease: word.editHold ?? null } : current);
  }
  const editingTokens = Object.values(drafts.book.entries).flatMap(entry => entry.context.kind === 'edit' && entry.context.lease ? [entry.context.lease.token] : []);
  if (editingRequest?.lease) editingTokens.push(editingRequest.lease.token);
  async function cutWord(word: Utterance) {
    try { const updated = await window.intercat.cutUtterance(word.id); setStatus(current => ({ ...current, utterances: current.utterances?.map(item => item.id === updated.id ? updated : item) })); return true; }
    catch { setNotice('This request could not be cut. Refresh and try again.'); return false; }
  }
  async function resolveWord(word: Utterance, resolution: InputResolution) {
    try {
      const updated = await window.intercat.resolveUtterance({ opId: await window.intercat.createOperationId(), utteranceId: word.id, revision: word.revision, resolution });
      setStatus(current => ({ ...current, utterances: current.utterances?.map(item => item.id === updated.id ? updated : item) }));
    } catch { setNotice('The request changed or could not be saved. Refresh before answering.'); }
  }
  async function cutFuture(id: string) {
    const plan = status.plans?.find(plan => plan.items.some(item => item.planItemId === id));
    if (plan) {
      setPlanRequest({ id: plan.id, itemId: id, mode: 'cut', nonce: performance.now() });
      return;
    }
    if (preview) { setPreviewTracks(current => cutPlannedTurn(current, id)); setNotice('Planned turn cut. The current turn is unchanged.'); }
    else { const word = status.utterances?.find(word => word.id === id); if (word) await cutWord(word); }
  }
  useEffect(() => {
    if (preview) { liveClock.current = null; setView(initialView); }
  }, [preview]);
  useEffect(() => {
    if (preview && !playing) return;
    const timer = setInterval(() => setView(current => {
      const now = timelineNow();
      const next = advanceView(current, preview ? 1 : now === null ? 0 : Math.max(0, now - current.now), Date.now() - interaction.current < 1500);
      return preview || next.start !== current.start ? next : current;
    }), 1000);
    return () => clearInterval(timer);
  }, [playing, preview, timelineNow]);
  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(''), 4500);
    return () => clearTimeout(timer);
  }, [notice]);
  useEffect(() => {
    const toggleStage = (event: KeyboardEvent) => {
      if (!event.metaKey || !event.shiftKey || event.code !== 'KeyT' || event.isComposing || document.querySelector('dialog[open]')) return;
      event.preventDefault();
      setStageMode(!stageMode);
    };
    window.addEventListener('keydown', toggleStage);
    return () => window.removeEventListener('keydown', toggleStage);
  }, [stageMode]);
  useEffect(() => {
    const planOnly = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.isComposing || !event.metaKey || !event.shiftKey || event.altKey || event.code !== 'KeyP' || document.querySelector('dialog[open]')) return;
      event.preventDefault(); void saveDraft(false, false, true);
    };
    window.addEventListener('keydown', planOnly);
    return () => window.removeEventListener('keydown', planOnly);
  });
  useEffect(() => {
    const branch = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.isComposing || !event.metaKey || event.ctrlKey || event.shiftKey || event.altKey || event.code !== 'KeyB'
        || !branchTrack || !status.connected || preview || document.querySelector('dialog[open]') || (event.target instanceof Element && event.target.closest('.monaco-editor'))) return;
      event.preventDefault(); void openBranchPoint(branchTrack, clip!.clip.end * 1000);
    };
    window.addEventListener('keydown', branch);
    return () => window.removeEventListener('keydown', branch);
  });
  function cycleRegion(event: React.KeyboardEvent) {
    if (event.metaKey && (event.code === 'Period' || event.key === '.')) {
      if(!event.altKey&&!event.shiftKey&&event.target instanceof Element&&event.target.closest('[data-commit-review]'))return;
      event.preventDefault();
      if (event.altKey) {
        void pauseDispatch(true);
      } else if (restoreSelection&&!event.shiftKey) {
        const job=status.pathRestores?.find(job=>job.request.retirementId===restoreSelection.entry?.id);
        const run=status.repairRuns?.find(run=>run.owner.kind==='restore'&&run.owner.id===job?.request.opId&&!['done','discarded'].includes(run.phase));
        if(run)void stopRepair(run.id);
      } else if (active?.reviewer) {
        void stopReviews((status.reviewerLane?.jobs ?? []).filter(job=>!event.shiftKey || job.state==='queued'));
      } else if (event.shiftKey) {
        const worker = active?.reviewer ? undefined : (status.workers ?? []).find(worker => worker.trackId === active?.id) ?? status.workers?.[0];
        if (worker) void holdWorker(worker.trackId);
      } else if (realignSelection) {
        const job=status.realignments?.find(job=>job.request.opId===realignSelection),owner=job?realignMergeOwner(job):null;
        const run=status.repairRuns?.find(run=>owner&&run.owner.kind===owner.kind&&run.owner.id===owner.id&&['queued','preparing','running','stopping','manual','held'].includes(run.phase));
        if(run)void stopRepair(run.id);
      } else if (selectedRepair) void stopRepair(selectedRepair);
      else { const worker = active?.reviewer ? undefined : (status.workers ?? []).find(worker => worker.trackId === active?.id) ?? status.workers?.[0]; if (worker) void stopWorker(worker.trackId); }
      return;
    }
    if (!event.ctrlKey || event.key !== 'Tab') return;
    event.preventDefault();
    const regions = [...document.querySelectorAll<HTMLElement>('[data-focus-region]')].filter(region => region.getClientRects().length > 0);
    const index = regions.findIndex(region => region.contains(document.activeElement));
    regions[(index + (event.shiftKey ? -1 : 1) + regions.length) % regions.length]?.focus();
  }
  const editorNeedsAction = status.connected && status.editor && (status.editor.canRetry || ['not_configured', 'download_paused'].includes(status.editor.phase));
  function observeRealignment(job:RealignJob) {
    setStatus(current=>({...current,realignments:[...(current.realignments??[]).filter(item=>item.request.opId!==job.request.opId),job]}));
  }
  async function refreshRestore(){const value=await window.intercat.getStatus();observeStatus(value);if(!value.connected)throw new Error('Restore could not be read. Reconnect and retry the saved choice.');}
  function openRestore(entry:Retirement){if(saving||saveError||integrationOpen)return;setRestoreSelection({entry,presentation:null});}
  const attention: AttentionCard[] = [
    ...restoreCards({entries:status.pathRetirements??[],jobs:status.pathRestores??[],followups:status.restoreFollowups??[],realignments:status.realignments??[],workers:status.workers??[],disabled:!status.connected||saving||saveError||integrationOpen,open:openRestore,changed:refreshRestore,queued:observeRealignment}),
    ...realignmentCards(status.realignments??[],status.pendingRealignments??[],status.tracks??[],status.workers??[],!status.connected||saving||saveError||integrationOpen,observeRealignment,id=>{setCommitOpen(false);setRealignSelection(id);}),
    ...(status.integrations??[]).flatMap(job=>integrationFailure(job,status.repairRuns??[],integrationOpen,()=>{setCommitJobId(job.id);setCommitRequest(undefined);setCommitOpen(true);})),
    ...reviewFixCards(status.reviewFixes ?? [], status.tracks ?? [], !status.connected || saving || saveError || integrationOpen, id=>{void openReviewedStep(id);}),
    ...proposalCards(status.proposal, !status.connected || saving || saveError || integrationOpen, response => {
      if(response.realignment)observeRealignment(response.realignment);
      setStatus(current => ({ ...current, proposal: current.proposal?.id === response.proposalId ? null : current.proposal }));
    }, proposal => {
      drafts.open({ kind: 'plan-reply', planId: proposal.planId, questionId: proposal.id, revision: proposal.planRevision, question: `${proposal.text}\n${proposal.detail}`, label: proposal.label });
      requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus());
    }),
    ...planAssumptionCards(status.planAssumptions ?? [], !status.connected || saving || saveError || integrationOpen, value => {
      drafts.open({ kind: 'plan-reply', planId: value.planId, questionId: value.id, revision: value.planRevision, question: `${value.text}\n${value.detail}`, label: value.label });
      requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus());
    }),
    ...assumptionCards(status.assumptions ?? [], active?.id ?? null, status.tracks ?? []),
    ...requestCards({ onReviewPlan: id => setPlanRequest({ id, nonce: performance.now() }), drafts, onRephrased: observeRequest, words: status.utterances ?? [], tracks: status.tracks ?? [],
      onResolve: (word, resolution) => { void resolveWord(word, resolution); },
      onEdit: word => { openRequest(word, true); }, onCut: word => { void cutWord(word); },
    }),
    ...reportCards({ status,
      provideSecret: async (question, entry) => { await window.intercat.provideWorkerSecret({ ...entry, owner: { kind: 'work' }, trackId: question.trackId, sessionId: question.sessionId, turnId: question.turnId, question: { kind: 'report', id: question.id, revision: question.revision } }); },
      stop: track => { void stopWorker(track); },
      reply: question => { if (saving || saveError) return; setSelectedRepair(null); drafts.open({ kind: 'reply', question }); document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus(); },
      respond: (question, answer) => { void respondReport(question, answer); },
      takeBack: word => { void cutWord(word); },
      retry: word => { void resolveWord(word, { action: 'retry', target: word.target }); },
    }),
    ...folderChoiceCards(status),
    ...checkpointIssueCards(status),
    ...freshCards(status,setFreshSelection),
    ...twoWaysCards(status,setTwoWaysSelection),
    ...runProcessCards(status.repairRuns ?? [], !status.connected, request => window.intercat.decideRepairProcesses(request)),
    ...branchInstructionCards(status),
    ...workerCards({ workers: status.workers ?? [], tracks: status.tracks ?? [],
      pendingInheritance: (status.pendingInheritance ?? []).filter(pending => !status.inheritanceIssues?.some(issue => issue.applicationId === pending.operationId)),
      finishInheritance: async pending => { await window.intercat.finishInheritance({ opId: await window.intercat.createOperationId(), trackId: pending.trackId, applicationId: pending.operationId }); },
      provideSecret: async (worker, prompt, entry) => { await window.intercat.provideWorkerSecret({ ...entry, owner: { kind: 'work' }, trackId: worker.trackId, sessionId: prompt.request.sessionId, turnId: prompt.request.turnId, question: { kind: 'msp', id: prompt.request.userInputId, revision: prompt.revision } }); },
      answer: answerWorker,
      respond: async (worker, prompt, response) => { await window.intercat.respondToWorker({ opId: await window.intercat.createOperationId(), trackId: worker.trackId, promptId: prompt.request.userInputId, revision: prompt.revision, response }); },
    }),
    ...inheritanceIssueCards({ issues: status.inheritanceIssues ?? [], workers: status.workers ?? [], tracks: status.tracks ?? [],
      command: async (issue, action) => { await window.intercat.resolveInheritanceIssue({ opId: await window.intercat.createOperationId(), issueId: issue.id, action }); },
      finish: async issue => { if (issue.applicationId) await window.intercat.finishInheritance({ opId: await window.intercat.createOperationId(), trackId: issue.trackId, applicationId: issue.applicationId }); },
      review: issue => { if (issue.planId) setPlanRequest({ id: issue.planId, itemId: issue.itemId, nonce: performance.now() }); },
    }),
  ];
  if (editorNeedsAction && status.editor) {
    attention.push({
      id: 'editor', kind: 'blocked', title: 'Local Editor needs attention',
      content: <EditorStatus value={status.editor} update={editor => setStatus(current => ({ ...current, editor }))} />,
    });
  }
  if (status.mode === 'unavailable' && status.message) {
    attention.push({
      id: 'connection', kind: 'blocked', title: 'Connection needs attention',
      content: <div className="desk-card" role="alert"><div>
        <p>{status.message}</p>
        {status.fingerprintRecovery && <FingerprintRecovery value={status.fingerprintRecovery}
          cancel={async request => { setStatus(await window.intercat.cancelBackup(request)); }}
          apply={async request => { setStatus(await window.intercat.applyBackup(request)); }}
          rebuild={async request => { setStatus(await window.intercat.rebuildFingerprints(request)); }} />}
      </div></div>,
    });
  }
  if (status.connected && status.project && !status.workerConfigured) {
    attention.push({
      id: 'worker-setup', kind: 'blocked', title: 'Connect your Meta worker',
      content: <div className="desk-card desk-action-card worker-setup">
        <span className="card-mark">◇</span>
        <div className="desk-copy">
          <strong>Connect your Meta worker</strong>
          <p>Workers use your Muse sign-in. Your requests and project files are sent to Meta when work runs.</p>
          {workerError && <p role="alert">{workerError}</p>}
        </div>
        <div className="desk-actions" data-card-actions>
          <button disabled={workerSetup} onClick={() => { void setupWorkers(); }}>{workerSetup ? 'Connecting…' : 'Connect workers'}</button>
        </div>
      </div>,
    });
  }
  if (preparationError) {
    attention.push({
      id: 'preparation', kind: 'blocked', title: 'Work could not be prepared',
      content: <div className="desk-card desk-action-card" role="alert">
        <span className="card-mark">◇</span>
        <div className="desk-copy"><strong>Work could not be prepared</strong><p>{preparationError}</p></div>
        <div className="desk-actions" data-card-actions><button onClick={() => { void retryPreparation(); }}>Retry preparation</button></div>
      </div>,
    });
  }
  if (status.projectSelection && (status.projectSelection.issue || status.projectSelection.error)) {
    const selection = status.projectSelection;
    attention.push({id:`project-selection-${selection.id}`,kind:'blocked',title:'Project folder selection',version:JSON.stringify(selection),
      content:<ProjectSelectionCard key={selection.id} selection={selection} changed={projectSelected}/>});
  }
  if (projectError) {
    attention.push({
      id: 'project', kind: 'blocked', title: 'Project needs attention',
      content: <div className="desk-card desk-action-card" role="alert">
        <span className="card-mark">◇</span>
        <div className="desk-copy"><strong>Project needs attention</strong><p>{projectError}</p></div>
        <div className="desk-actions" data-card-actions><button onClick={() => setProjectError('')}>Dismiss</button></div>
      </div>,
    });
  }
  for (const worker of status.workers?.filter(worker => worker.detail && ['failed', 'needs-recovery', 'question', 'lost', 'after-connection-loss'].includes(worker.phase)) ?? []) {
    const recovering = status.pickups?.some(pickup => pickup.trackId === worker.trackId && pickup.sessionId === worker.sessionId && !['ready', 'cancelled'].includes(pickup.phase));
    attention.push({
      id: `worker-${worker.trackId}`, kind: 'blocked',
      title: `Worker ${status.tracks?.find(track => track.id === worker.trackId)?.label ?? ''} needs attention`,
      content: <div className="desk-card desk-action-card" role="alert">
        <span className="card-mark">◇</span>
        <div className="desk-copy">
          <strong>{worker.phase === 'lost' ? 'Worker lost its connection' : 'Worker needs attention'}</strong>
          <p>{worker.detail}</p>
        </div>
        <div className="desk-actions" data-card-actions>
          {['failed', 'lost'].includes(worker.phase) && !recovering && <button onClick={() => { void retryWorker(worker.trackId); }}>
            {worker.phase === 'lost' ? 'Retry connection' : 'Retry worker'}
          </button>}
          {['lost', 'needs-recovery'].includes(worker.phase) && !recovering && <button onClick={() => { void stopWorker(worker.trackId); }}>Stop here</button>}
          {['lost', 'needs-recovery', 'failed', 'after-connection-loss'].includes(worker.phase) && worker.sessionId && <PickupButton worker={worker}
            pending={status.pickups?.filter(pickup => pickup.trackId === worker.trackId && pickup.sessionId === worker.sessionId && pickup.phase !== 'ready').at(-1)} />}
        </div>
      </div>,
    });
  }
  for (const flow of status.outputFlows?.filter(flow => flow.lagging || flow.interrupted) ?? []) {
    const repair = flow.repairRunId ? status.repairRuns?.find(run => run.id === flow.repairRunId) : null;
    const track = status.tracks?.find(track => track.id === (repair?.track ?? flow.trackId));
    attention.push({
      id: `output-${flow.repairRunId ?? flow.trackId}`, kind: 'blocked', title: 'Output needs attention',
      content: <div className="desk-card desk-action-card" role="status">
        <div className="desk-copy">
          <strong>{flow.interrupted ? 'output too fast to record' : flow.oldestQueuedMs >= 2000 ? 'New work paused · can’t save to the log' : 'log lagging'} · Worker {track?.label ?? '?'}{flow.repairRunId ? ' · for the commit' : ''}</strong>
          <p>{flow.interrupted ? 'This worker was interrupted. Review its state before restarting.' : `${Math.ceil(flow.queuedBytes / 1024)} KiB waiting to be saved. Stop still works.`}</p>
        </div>
        <div className="desk-actions" data-card-actions><button onClick={() => {
          if (flow.repairRunId) void stopRepair(flow.repairRunId);
          else if (flow.trackId) void stopWorker(flow.trackId);
        }}>Stop this {flow.repairRunId ? 'repair' : 'worker'}</button></div>
      </div>,
    });
  }
  const commitDisabled=!!realignSelection||!status.connected||preview||!(status.integrations?.length||status.workspaces?.some(workspace=>workspace.state==='ready'));
  const projectFolders=status.project?.folders??[];
  async function openCommitFolder(folderId?:string){
    setPushSelection(null);
    if(!folderId){setCommitJobId(undefined);setCommitRequest(undefined);setCommitOpen(true);return;}
    const saved=status.integrations?.find(job=>job.project.id===folderId&&!['discarded','done'].includes(job.status));
    setCommitJobId(saved?.id);
    setCommitRequest(saved?undefined:{opId:await window.intercat.createOperationId(),action:{action:'open',tracks:(status.tracks??[]).filter(track=>status.trackFolders?.[track.id]===folderId&&status.workspaces?.some(workspace=>workspace.trackId===track.id&&workspace.state==='ready')).map(track=>track.id)}});
    setCommitOpen(true);
  }
  const commitButton=<PublicationActions status={status} preview={preview} disabled={commitDisabled} commit={folder=>{void openCommitFolder(folder);}} push={selection=>{setCommitOpen(false);setRealignSelection(null);setPullSelection(null);setPushSelection(selection);}} pull={selection=>{setCommitOpen(false);setRealignSelection(null);setPushSelection(null);setPullSelection(selection);}}/>;

  for (const branch of status.branches?.filter(branch => branch.phase !== 'ready'&&!status.twoWays?.some(job=>job.branchId===branch.request.opId)) ?? []) {
    attention.push({ id: `branch-${branch.request.opId}`, kind: 'blocked', version: branch.phase,
      title: 'Branch preparation needs attention', content: <BranchRecovery branch={branch} connected={status.connected} created={branchCreated} /> });
  }
  return <PrivacyEnabled.Provider value={!preview}><PrivacyRevision.Provider value={privacyVersion}><ApprovalDraftStore value={drafts}><div className={`app${integrationOpen ? ' has-commit' : ''}${stageMode ? ' stage-mode' : ''}${deskLayout.height !== null ? ' desk-layout-adjusted' : ''}`} onKeyDownCapture={cycleRegion}>
    <aside className="sidebar" tabIndex={-1} data-focus-region aria-label="Sessions">
      <div className="window-chrome"><span className="app-wordmark">intercat</span><span className="sidebar-icon">◧</span></div>
      <button className="new-session" disabled>
        <span className="new-session-icon" aria-hidden="true">
          <svg viewBox="0 0 20 20" width="14" height="14">
            <path d="M10 4v12M4 10h12" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
          </svg>
        </span>
        New session
      </button>
      <div className="nav-label">WORKSPACE</div>
      <div className="project-label">{preview ? 'Sample project' : status.project?.name ?? 'Local workspace'}</div>
      <button className="session active"><span className="session-dot" />{sessionName}<span>↗</span></button>
      <div className="sidebar-note">A place for parallel work.<br />A timeline you can steer.</div>
      <button className="settings-button" disabled={!status.connected} onClick={() => setSettingsOpen(true)}>Settings</button>
      <div className="sidebar-bottom"><span className="local-dot" />{preview ? 'Local preview' : 'Local editor'}<span className="version">0.1</span></div>
    </aside>
    <main className="main">
      <header className="session-header"><div><div className="eyebrow">{preview ? 'SAMPLE SESSION' : 'LOCAL SESSION'}</div><h1 title={sessionName}>{sessionName}</h1></div>{commitButton}</header>
      <div className="session-subtitle">{preview ? <><span className="folder-chip">▱ sample-web</span><span className="folder-chip">▱ sample-api</span><span className="preview-label">Preview data</span></> : <>{status.project ? status.project.folders.map(folder => <span key={folder.id} className="folder-chip" title={folder.root}>▱ {folder.name}</span>) : <span className="folder-chip">No project folder</span>}{!status.project && <button className="folder-chip" disabled={!status.connected || choosingProject} onClick={() => { void setupProject(); }}>{choosingProject ? 'Checking folder…' : 'Choose project folder'}</button>}</>}</div>
      {status.projectSecretIssue && <div className="project-secret-notice" role="status"><span>{status.projectSecretIssue}</span><button onClick={() => setSettingsOpen(true)}>Review saved secrets</button></div>}
      <section ref={stageElement} className="stage" style={deskLayout.height === null ? undefined : { flex: '0 0 auto', height: deskLayout.height }} tabIndex={-1} data-focus-region aria-label="Stage">
        <Timeline onRestoreAside={id=>{const entry=status.pathRetirements?.find(entry=>entry.id===id);if(entry)openRestore(entry);}} onPushCommit={commit=>{if(saving||saveError||integrationOpen)return;setCommitOpen(false);setRealignSelection(null);setPushSelection({folders:[commit.folderId],through:{[commit.folderId]:commit.sha}});}} branchPreview={branchPreview} branchError={savedBranchPoints.error} onBranchPoint={(trackId, at) => { void openBranchPoint(trackId, at); }} onReviewStop={jobs=>{void stopReviews(jobs);}} focusPlanItem={focusPlanItem} selectedPlanItem={selectedPlanItem} onPlanControl={(id, itemId, mode, point) => { setSelectedPlanItem(itemId); setPlanRequest({ id, itemId, mode, point, nonce: performance.now() }); }} onPlan={(id, itemId, point) => { setSelectedPlanItem(itemId ?? null); setPlanRequest({ id, itemId, point, nonce: performance.now() }); }} onInput={id => { void openTimelineInput(id); }} onViewport={showTimeline} liveNow={timelineNow} advancing={preview ? playing : !!status.timeline} preview={preview} tracks={privateTracks} marks={privateMarks} selected={selected} selectedRepair={selectedRepair} view={view} onSelect={id => { if (saving || saveError) return; setSelectedPlanItem(null); openingInput.current++; setSelected(id); setSelectedRepair(null); setClip(null); }} onRepairSelect={(track, id) => { if (saving || saveError) return; openingInput.current++; setSelected(track.id); if (!commitContext) { setSelectedRepair(id); drafts.open({ kind: 'repair', id }, true); } setClip(null); }} onRepairStop={id => { void stopRepair(id); }} onClip={(track, chosen, repairRunId) => { setSelectedPlanItem(null); setClip({ track, clip: chosen, repairRunId }); }}
          onOpenTrack={id => {
            if (saving || saveError) return;
            setDeskLayout(current => current.mode === 'normal' ? current : { mode: 'normal', height: null });
            requestAnimationFrame(() => {
              const section = document.querySelector<HTMLElement>(`[data-worker-card="${CSS.escape(id)}"]`);
              const scroll = section?.closest<HTMLElement>('.desk-scroll');
              if (section && scroll) scroll.scrollTop += section.getBoundingClientRect().top - scroll.getBoundingClientRect().top;
            });
          }}
          canDropFiles={canDropFiles} onDropFiles={dropFiles} targetDisabled={!drafts.ready || !status.connected || saving || saveError || attaching} onSayTo={id => { if (saving || saveError || attaching) return; if (continuationContext) drafts.open({ kind: 'work' }); else drafts.replaceContext({ kind: 'work' }); drafts.setTarget({ mode: 'lock', trackId: id }); setSelectedRepair(null); document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus(); }} onCut={id => { void cutFuture(id); }} onInteraction={() => { interaction.current = Date.now(); }} />
        <div className="stage-footer"><div><span className="legend-mark">◇</span> waiting <span className="legend-mark">✓</span> done <span className="legend-edit" /> edit</div><div><span className="key-hint">↑ ↓ select · 1–9 jump</span>{preview && <button className="play-button" aria-pressed={playing} onClick={() => setPlaying(!playing)}>{playing ? 'Ⅱ Pause preview' : '▷ Play preview'}</button>}</div></div>
        {!preview && status.timeline?.summaries.some(summary => summary.trackId === selected && summary.repairRunId === selectedRepair) && <p className="history-summary" role="note">Earlier history was summarized by the worker.</p>}
        {clip ? <section className="clip-inspector"><div className="inspector-heading"><span>{clip.track.label}:{clip.clip.ordinal ?? clip.clip.id}{clip.repairRunId ? ' · for the commit' : ''} / {clip.clip.label}</span><button onClick={() => setClip(null)} aria-label="Close step preview">×</button></div><Suspense fallback={<p>Loading step…</p>}>{preview ? <DocumentPreview /> : <StepInspector clipId={clip.clip.id} sequence={status.sequence} openClip={id=>{void openReviewedStep(id);}} />}</Suspense></section> : null}
      </section>
      <StageDivider stage={stageElement} desk={deskElement} layout={deskLayout} change={setDeskLayout} />
      <section ref={deskElement} className="desk" tabIndex={-1} data-focus-region aria-label="Desk">
        <div className="desk-heading"><span>DESK</span><span className="desk-selection" title={deskContext}>{deskContext}</span><span className="shortcut">⌃Tab to move between areas</span></div>
        <div className="desk-scroll">
        <ConfirmationNotice value={status.confirmation} />
        {applicationCommand.error && <div className="application-command-error" role="alert"><span>{applicationCommand.error}</span><button onClick={applicationCommand.retry}>Retry opening action</button></div>}
        <div className="desk-activity">
          {status.connected && status.editor && !editorNeedsAction && <EditorStatus value={status.editor} update={editor => setStatus(current => ({ ...current, editor }))} />}
          <ReportProgress status={status} />
        </div>

        <StagePanels finish={commitButton} compact={stageMode} folded={deskLayout.mode !== 'normal'} workerLabel={active && !active.reviewer ? `Worker ${active.label}` : null} cards={attention} returnedAt={returnedAt} canDropFiles={canDropFiles} dropFiles={dropFiles} expandWorkers={() => {
          setStageMode(false);
          requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus());
        }} workers={<Workers freshJobs={status.freshTracks??[]} continuations={status.continuations??[]} closedWriters={closedWriters} connected={status.connected} openFresh={worker=>{void openFresh(worker);}} openTrack={viewTrack} retiredTracks={retiredTracks} realignments={status.realignments??[]} pendingRealignments={status.pendingRealignments??[]} pendingInheritance={status.pendingInheritance} queued={queuedTracks(status)} unloggedControls={status.unloggedControls} secretInputs={status.secretInputs ?? []} flows={status.outputFlows ?? []} effects={status.effects ?? []} workers={status.workers ?? []} tracks={status.tracks ?? []} start={id => { void startWorker(id); }} stop={id => { void stopWorker(id); }} canDropFiles={canDropFiles} dropFiles={dropFiles} />} />
        {!stageMode && deskLayout.mode === 'normal' && !preview && active && !active.reviewer && !selectedRepair && <EditorLog key={active.id} trackId={active.id} label={active.label} sequence={status.sequence} />}
        <HistoryControls history={status.editHistory} controller={historyController} disabled={!status.connected || !drafts.ready || saving || saveError} />
        <Requests retiredTracks={retiredTracks} scheduling={status.scheduling} hostMemory={status.hostMemory} confirmation={status.confirmation} onConfirmationChanged={confirmation => setStatus(current => ({ ...current, confirmation }))} onConfirmed={word => setStatus(current => ({ ...current, utterances: current.utterances?.map(item => item.id === word.id ? word : item) }))} editingTokens={editingTokens} onStart={word => { void startWord(word); }} redirects={status.redirects ?? []} words={status.utterances ?? []} tracks={status.tracks ?? []} onSelect={word => { openRequest(word, false); }} onCut={word => { void cutWord(word); }} />
        </div>
        <Plans openDependency={location => { void openDependency(location); }} continueItem={continuePlanItem} countdowns={countdowns} edit={(plan, id) => { void planEditing.open(plan, id).catch(error => setNotice(error instanceof Error ? error.message : 'This item could not be opened for revision.')); }} status={status} moved={(plan, id, focus) => { setSelectedPlanItem(id); if (focus) setFocusPlanItem(previous => ({ id, nonce: (previous?.nonce ?? 0) + 1 })); const item = plan.items.find(item => item.planItemId === id); if (item) setSelected(item.trackRef.startsWith('new:') ? `${plan.id}/${item.trackRef}` : item.trackRef); }} request={planRequest} updated={updatePlan} />
        {presentation.error && <p role="status" className="presentation-error">{presentation.error}</p>}
        <EditorLine character={presentation.state?.character ?? null} hidden={presentation.state?.hidden ?? true} away={presentation.away} entries={status.editorEntries ?? []} waitingTracks={(status.utterances ?? []).filter(word => ['placed', 'redirecting', 'starting'].includes(word.state)).flatMap(word => word.decision?.trackId ? [word.decision.trackId] : [])} history={status.editHistory} undo={() => historyController.step('undo')} disabled={!status.connected || !drafts.ready || saving || saveError || historyController.busy || historyController.pending} />
        <div className={`composer${composerDrop ? ' file-drop-active' : ''}`} onDragOver={event => { if (canDropFiles && event.dataTransfer.types.includes('Files')) { event.preventDefault(); event.dataTransfer.dropEffect = 'copy'; setComposerDrop(true); } }} onDragLeave={event => { if (!(event.relatedTarget instanceof Node) || !event.currentTarget.contains(event.relatedTarget)) setComposerDrop(false); }} onDrop={event => { event.preventDefault(); dropFiles(Array.from(event.dataTransfer.files)); }}>
          {!drafts.ready && !drafts.issue && <span className="sr-only" role="status">Loading saved draft…</span>}
          {drafts.issue && <div className="draft-save-issue" role="alert"><span>{drafts.issue}</span>
            {drafts.conflict ? <><button disabled={!drafts.ready} onClick={() => { void drafts.resolve(true).catch(() => {}); }}>Keep this draft</button><button disabled={!drafts.ready} onClick={() => { void drafts.resolve(false).catch(() => {}); }}>Reload saved draft</button></>
              : <><button onClick={() => { void (drafts.ready ? drafts.flush() : drafts.load()).catch(() => {}); }}>Retry draft save</button>{drafts.ready && <button onClick={() => { void drafts.resolve(false).catch(() => {}); }}>Reload saved draft</button>}</>}
          </div>}
          <ApprovalReplyRecovery status={status} repairVisible={integrationOpen} />
          {Object.keys(drafts.book.entries).filter(id => { const entry = drafts.book.entries[id]; return id !== drafts.book.active && entry.context.kind !== 'approval' && entry.context.kind !== 'rephrase' && (entry.text || entry.sources?.length || entry.attachments.length || entry.pending); }).length > 0 && <div className="draft-stack" aria-label="Saved drafts">{Object.keys(drafts.book.entries).filter(id => id !== drafts.book.active && drafts.book.entries[id].context.kind !== 'approval').reverse().map(id => {
            const entry = drafts.book.entries[id];
            return entry && (entry.text || entry.sources?.length || entry.attachments.length || entry.pending) ? <button key={id} disabled={saving} onClick={() => drafts.resume(id)}>{draftLabel(entry)} · Resume</button> : null;
          })}</div>}
          {context.kind==='work'&&draftTarget.mode==='lock'&&effectiveDraftTarget?.mode==='lock'&&draftTarget.trackId!==effectiveDraftTarget.trackId&&<p className="fresh-draft-destination" role="status">{tracks.find(track=>track.id===draftTarget.trackId)?.label??'The original'} continued as {tracks.find(track=>track.id===effectiveDraftTarget.trackId)?.label??'a new conversation'} · this request goes there</p>}
          {context.kind==='work'&&!effectiveDraftTarget&&<p className="fresh-draft-destination" role="alert">The continuation is unavailable. Choose a destination before sending.</p>}
          {composerDrop && <div className="file-drop-hint">Drop here to attach it to your next words</div>}{continuationContext && <ContinuationContext context={continuationContext} label={status.tracks?.find(track => track.id === continuationContext.trackId)?.label ?? '?'} stale={staleContinuation} busy={saving} pending={saveError} close={() => drafts.back()} review={() => setPlanRequest({ id: continuationContext.planId, itemId: continuationContext.itemId, nonce: performance.now() })} />}{planEditContext && <div className="reply-target edit-context" aria-label="Plan item being revised"><span>about · <PrivacySummary text={planEditContext.title} limit={60} /></span><button disabled={saving} onClick={() => { void planEditing.cancel(); }}>Cancel revision</button>{planEditing.error && <div role="status">{planEditing.error}<button disabled={saving} onClick={() => { void planEditing.reopen(); }}>Reopen item for editing</button></div>}</div>}{editContext && <div className="reply-target edit-context" aria-label="Request being edited"><span>about · <PrivacySummary text={editContext.originalText} limit={60} /></span><button disabled={saving} onClick={() => { void editing.cancel(); }}>Cancel edit</button>{editContext.state === 'sent' ? <div role="status">Already sent<button disabled={saving} onClick={() => { void saveDraft(false, true); }}>Send as a new instruction</button></div> : editing.error && <div role="status">{editing.error}<button disabled={saving} onClick={() => { void editing.ensure(drafts.book.active).catch(() => {}); }}>Retry editing</button></div>}</div>}{commitContext && <div className="reply-target commit-context" aria-label="Commit context"><span title={commitContext.selection.path}>{commitContext.selection.path}:{commitContext.selection.startLine}{commitContext.selection.endLine !== commitContext.selection.startLine ? `–${commitContext.selection.endLine}` : ''} @ {commitContext.selection.tree.slice(0, 7)}</span><span>fix for this commit · {commitContext.label}</span><button disabled={saving || saveError} onClick={() => drafts.replaceContext({ kind: 'work' })}>Clear commit context</button>{commitContextState !== 'current' && <div className="commit-context-changed" role="status"><span>{commitContextState === 'gone' ? 'the commit is gone' : 'the commit changed'}</span>{commitContextState === 'changed' ? <button disabled={saving || saveError || !draft.trim()} onClick={() => { void resendCommitInstruction(); }}>Resend on the new version</button> : <button disabled={saving || saveError || !draft.trim()} onClick={() => { void saveDraft(false, true); }}>Send to {commitContext.label}</button>}<button disabled={saving || saveError} onClick={() => drafts.replaceContext({ kind: 'work' })}>Take back</button></div>}</div>}{repairTargetId && <div className="reply-target">{active?.label} · for the commit · {activeRepair?.phase === 'held' ? 'Add an instruction' : activeRepair?.phase ?? 'Recorded repair'}<button disabled={saving || saveError} onClick={() => { setSelectedRepair(null); drafts.replaceContext({ kind: 'work' }); }}>Clear repair target</button></div>}{planReplyContext && <div className="reply-target reply-context">Reply to plan · <PrivacyText text={planReplyContext.label} /><button disabled={saving || saveError} onClick={() => drafts.back()}>Cancel reply</button><details><summary>Original plan question</summary><p><PrivacyText text={planReplyContext.question} /></p></details><small>The current plan is checked again when you send.</small></div>}{replyTarget && <div className="reply-target reply-context">Reply to Worker {status.tracks?.find(track => track.id === replyTarget.trackId)?.label}<button disabled={saving} onClick={() => { drafts.back(); }}>Cancel reply</button>{staleReply && <div role="status">This question is no longer current. Keeping this reply will ask you where to send it.</div>}<details><summary>Original question</summary><p><PrivacyText text={replyTarget.text} /></p></details></div>}{(references.length > 0 || attachmentError) && <div className="reference-files">{attachmentError && <button disabled={attaching || saving || saveError} onClick={() => { void attachReference(true); }}>Choose another file</button>}{references.map(file => <span className="reference-chip" key={file.id}><span title={file.name}>📎 {file.name}</span><button disabled={saving || saveError || (!!editContext || !!planEditContext || !!planReplyContext)} aria-label={`Remove reference ${file.name}`} onClick={() => drafts.setAttachments(files => files.filter(item => item.id !== file.id))}>×</button></span>)}</div>}<textarea data-focus-region aria-label="Request draft" aria-busy={!drafts.ready && !drafts.issue} placeholder={!drafts.ready && !drafts.issue ? 'Loading saved draft…' : 'What would you like to work on?'} rows={stageMode ? 1 : 2} value={draft} disabled={!drafts.ready || saving || saveError || attaching} onChange={event => drafts.setText(event.target.value)} onPaste={pasteDraft} onKeyDown={composerKeyDown} /><PastedSources drafts={drafts} disabled={!drafts.ready || saving || saveError || attaching} readOnly={context.kind !== 'work' && context.kind !== 'edit' && context.kind !== 'continuation'} paste={pasteDraft} submit={composerKeyDown} /><div className="composer-bottom"><div className="composer-tools"><button className="attach-reference" aria-label={attachmentError ? 'Retry reference save' : 'Attach reference file'} title={attaching ? 'Saving reference…' : attachmentError ? 'Retry reference save' : 'Attach reference file'} aria-busy={attaching} disabled={!drafts.ready || !status.connected || preview || saving || saveError || attaching || !!replyTarget || !!repairTargetId || !!commitContext || (!!editContext || !!planEditContext || !!planReplyContext)} onClick={() => { void attachReference(); }}><svg viewBox="0 0 20 20" width="18" height="18" aria-hidden="true"><path d="M10 4v12M4 10h12" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" /></svg></button>{continuationContext || commitContext || repairTargetId || replyTarget || editContext || planEditContext || planReplyContext ? <span>{planReplyContext ? 'Reply to a plan question' : continuationContext ? 'Instruction for this plan item' : commitContext ? `fix for this commit · ${commitContext.label}` : repairTargetId ? `${active?.label} · for the commit` : planEditContext ? 'Revising a plan item' : editContext ? 'Editing a previous request' : 'Reply to question'}</span> : <RequestTarget target={effectiveDraftTarget??draftTarget} tracks={tracks.filter(track=>!track.reviewer&&!retiredTracks.has(track.id)&&!closedWriters.has(track.id))} disabled={!drafts.ready || !status.connected || saving || saveError || attaching} onChange={target => drafts.setTarget(target)} />}</div><div className="composer-actions"><button disabled={!drafts.ready || !status.connected || !hasDraft || saving || sourcesPending || saveError || attaching || !!replyTarget || !!repairTargetId || !!commitContext || !!continuationContext || (!!editContext || !!planEditContext || !!planReplyContext)} onClick={() => { void saveDraft(true); }} title="New track · ⇧⌘↵">New track</button><button disabled={!drafts.ready || !status.connected || !hasDraft || saving || sourcesPending || attaching || (staleContinuation && !saveError) || (missingDraftTarget && !saveError && !replyTarget && !repairTargetId && !commitContext && !editContext && !planEditContext && !planReplyContext) || (editContext?.state === 'sent' && !saveError) || (!!commitContext && !canSendContext && !saveError) || (!!repairTargetId && (activeRepair?.phase !== 'held' || !!activeRepair.restoring || (!!activeRepair.refresh && !activeRepair.refresh.error)) && !saveError)} onClick={() => { void saveDraft(); }} title="Put request · ⌘↵">{saving ? 'Saving…' : saveError ? 'Retry save' : planReplyContext ? 'Send reply' : continuationContext ? 'Send to item' : commitContext ? 'Send to repair' : repairTargetId ? 'Send to repair' : replyTarget ? staleReply ? 'Keep reply for review' : 'Send reply' : planEditContext ? 'Revise item' : editContext ? 'Update request' : 'Put'} <span aria-hidden="true">↵</span></button>{!continuationContext && !replyTarget && !repairTargetId && !commitContext && !editContext && !planEditContext && !planReplyContext && <PlanOnlyButton disabled={!drafts.ready || !status.connected || !hasDraft || saving || sourcesPending || saveError || attaching || missingDraftTarget} submit={() => { void saveDraft(false, false, true); }} />}</div></div></div>
        <div className="model-status" data-inferred={!modelTrackId} aria-label="Request model">{modelTrackId ? <WorkerModel model={requestModel} prefix={modelPrefix} /> : <span>Editor picks per track</span>}</div>
        <footer className="status-bar">{status.logUnavailable && <span role="status">Can’t save to the log · Stop still works</span>}<DispatchControls status={status} change={paused=>{void pauseDispatch(paused);}} /><span><i />{status.connected ? 'Daemon connected' : 'Daemon not connected'}</span><span>Local editor · {status.editor?.phase === 'ready' ? 'ready' : status.editor?.phase === 'starting' ? 'starting' : 'unavailable'}</span>{status.mode === 'unavailable' && <button onClick={() => { void window.intercat.retryConnection().then(setStatus); }}>Reconnect</button>}<span className="status-end">{status.workers?.length ? `Workers · ${status.workers.filter(worker => worker.phase === 'working' || worker.phase === 'approval').length} active / ${status.workers.filter(worker => worker.phase === 'ready').length} ready` : status.workerConfigured ? 'Workers connecting' : 'No work is being sent'}</span></footer>
      </section>
    </main>
    {restoreSelection&&(()=>{
      const command=restoreSelection.presentation;
      if(!restoreSelection.entry)return command&&<RestorePicker command={command} entries={status.pathRetirements??[]} session={status.session?.id} connected={status.connected} choose={entry=>setRestoreSelection(current=>current&&({...current,entry}))} close={()=>setRestoreSelection(null)}/>;
      const selectedEntry=restoreSelection.entry;
      const current=status.pathRetirements?.find(entry=>entry.id===selectedEntry.id&&entry.track===selectedEntry.track&&entry.folder===selectedEntry.folder&&entry.session===selectedEntry.session);
      const entry=current??selectedEntry;
      const available=!!current&&current.kind==='aside'&&current.phase==='aside'&&current.session===status.session?.id;
      const job=status.pathRestores?.find(job=>job.request.retirementId===entry.id);
      const run=status.repairRuns?.find(run=>run.owner.kind==='restore'&&run.owner.id===job?.request.opId&&!['done','discarded'].includes(run.phase));
      return <Suspense fallback={<aside className="commit-sheet" data-restore-operation={command?.operationId}>Opening Restore…</aside>}><RestoreSheet key={entry.id} entry={entry} job={job} run={run} available={available} operationId={command?.operationId} connected={status.connected&&entry.session===status.session?.id} flows={status.outputFlows??[]} secrets={status.secretInputs??[]} close={()=>setRestoreSelection(null)} changed={refreshRestore}/></Suspense>;
    })()}
    {realignSelection&&(()=>{const job=status.realignments?.find(job=>job.request.opId===realignSelection);if(!job)return null;const owner=realignMergeOwner(job);const run=status.repairRuns?.find(run=>run.owner.kind===owner.kind&&run.owner.id===owner.id&&!['done','discarded'].includes(run.phase));return <Suspense fallback={<aside className="commit-sheet">Opening realignment merge…</aside>}><RealignMergeSheet key={job.request.opId} job={job} run={run} label={status.tracks?.find(track=>track.id===job.request.trackId)?.label??'?'} flows={status.outputFlows??[]} secrets={status.secretInputs??[]} close={()=>setRealignSelection(null)} changed={async()=>{const value=await window.intercat.getStatus();observeStatus(value);if(!value.connected)throw new Error('The realignment could not be read. Reconnect and retry the saved choice.');}}/></Suspense>;})()}
    {pullSelection&&<Suspense fallback={<aside className="commit-sheet">Opening PR review…</aside>}><PullSheet key={JSON.stringify(pullSelection)} selection={pullSelection} jobs={status.pullRequests??[]} connected={status.connected} changed={observePull} close={()=>setPullSelection(null)}/></Suspense>}
    {pushSelection&&<Suspense fallback={<aside className="commit-sheet">Opening Push review…</aside>}><PushSheet key={JSON.stringify(pushSelection)} folders={projectFolders.filter(folder=>status.integrations?.some(job=>job.status==='done'&&job.project.id===folder.id)).map(folder=>({id:folder.id,name:folder.name}))} selection={pushSelection} jobs={status.pushes??[]} connected={status.connected} changed={observePush} openPull={review=>{setPullSelection({review,operationId:pushSelection.operationId});setPushSelection(null);}} close={()=>setPushSelection(null)}/></Suspense>}
    {commitOpen && <Suspense fallback={<aside className="commit-sheet">Opening review…</aside>}><CommitSheet comparisons={status.comparisons??[]} connected={status.connected} realignments={status.realignments??[]} workers={status.workers??[]} realignChanged={observeRealignment} hostMemory={status.hostMemory} folders={(status.project?.folders ?? []).map(folder => ({ id: folder.id, name: folder.name, tracks: (status.tracks ?? []).filter(track => status.trackFolders?.[track.id] === folder.id && status.workspaces?.some(workspace => workspace.trackId === track.id && workspace.state === 'ready')).map(track => track.id) }))} jobs={status.integrations??[]} groups={status.commitGroups??[]} initialJobId={commitJobId} initialRequest={commitRequest} secretInputs={status.secretInputs ?? []} selectContext={context => { if (saving || saveError) return; drafts.open({ kind: 'commit', context }, true); setSelectedRepair(null); }} effects={status.effects ?? []} flows={status.outputFlows ?? []} repairs={status.repairRuns ?? []} close={() => setCommitOpen(false)} update={async () => { const value = await window.intercat.getStatus(); observeStatus(value); if(!value.connected)throw new Error('The current review could not be read. Reconnect and retry the saved choice.'); }} /></Suspense>}
    {editingRequest && <RequestEditor key={editingRequest.word.id} word={editingRequest.word} clips={timelineClips} plans={status.plans ?? []} initialEditing={editingRequest.editText} initialLease={editingRequest.lease} compose={(word, lease) => { drafts.openEdit(word, lease); requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus()); }} tracks={status.tracks ?? []} workers={status.workers ?? []} close={() => setEditingRequest(null)} update={observeRequest} />}
    {branchSelection && <BranchDialog previewChanged={setBranchDialogPreview} key={branchSelection.operationId} selection={branchSelection} tracks={status.tracks ?? []} sourcePaths={Object.fromEntries((status.tracks ?? []).flatMap(track => { const folder = status.project?.folders.find(folder => folder.id === status.trackFolders?.[track.id]); return folder ? [[track.id, folder.root]] : []; }))} connected={status.connected} blockedReason={!status.workerConfigured ? 'Connect workers before creating a branch.' : !drafts.ready || saveError ? 'Finish saving the current request before creating a branch.' : null} close={() => setBranchSelection(null)} created={branchCreated} />}
    {twoWaysSelection&&<TwoWaysDialog key={twoWaysSelection} id={twoWaysSelection} status={status} commands={twoWaysCommands.current} close={()=>setTwoWaysSelection(null)} updated={observeTwoWays} openTrack={viewTrack}/>}
    {freshSelection&&<FreshDialog key={freshSelection.opId} commands={freshCommands.current} wasAttempted={pendingFresh.current.has(`${freshSelection.trackId}:${freshSelection.sessionId}`)} request={freshSelection} status={status} close={()=>setFreshSelection(null)} updated={observeFresh} attempted={request=>pendingFresh.current.set(`${request.trackId}:${request.sessionId}`,request)} openTrack={viewTrack}/>}
    <div className="toast" role="status">{notice}</div>
    <PolicySettings presentation={presentation.state} characterChanged={presentation.setCharacter} privacy={status.privacy ?? { revision: 0, mode: 'basic' }} privacyEnabled={status.connected && !preview} localReady={!!status.editor?.modelLoaded && !!status.editor?.accepting} privacySaved={privacy => setStatus(current => privacy.revision < (current.privacy?.revision ?? 0) ? current : ({ ...current, privacy, privacyRevision: (current.privacyRevision ?? 0) + privacy.revision - (current.privacy?.revision ?? 0) }))} confirmation={status.confirmation} confirmationSaved={confirmation => setStatus(current => ({ ...current, confirmation }))} project={status.projectSecrets} tracks={status.tracks ?? []} open={settingsOpen} stops={status.stops ?? 'irreversible-only'} close={() => setSettingsOpen(false)} saved={stops => setStatus(current => ({ ...current, stops }))} />
  </div></ApprovalDraftStore></PrivacyRevision.Provider></PrivacyEnabled.Provider>;
}

createRoot(document.getElementById('root')!).render(new URLSearchParams(location.search).get('view') === 'diagnostics' ? <Suspense fallback={<p>Loading diagnostics…</p>}><Diagnostics /></Suspense> : <App />);
