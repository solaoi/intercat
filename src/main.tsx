import { PickupButton } from './desk/PickupButton';
import React, { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import '@fontsource/inter/400.css';
import '@fontsource/inter/500.css';
import '@fontsource/inter/600.css';
import '@fontsource/jetbrains-mono/400.css';
import './style.css';
import { Timeline } from './timeline/Timeline';
import { advanceView, cutPlannedTurn, initialView, sampleTracks, type Clip, type Track } from './timeline/model';
import type { Approval, WorkerState, DaemonStatus, InputTarget, InputResolution, Utterance, ReportQuestion, ReportAnswer, Attachment } from './bridge';
import { liveTimeline } from './timeline/state';
import { useTimelineWindow, type TimelineBounds } from './timeline/useWindow';
import { mergeStatus } from './status';
import { Requests } from './desk/Requests';
import { Workers } from './desk/Workers';
import { DispatchControls } from './desk/DispatchControls';
import { ReportQuestions } from './desk/ReportQuestions';
import { EditorStatus } from './desk/EditorStatus';
import { PolicySettings } from './desk/PolicySettings';
import { FingerprintRecovery } from './desk/FingerprintRecovery';
import { contextState, reanchor, type CommitContext } from './commit/context';
import type { RepairFollowUp, RepairRequest } from './commit/repair';

const Diagnostics = lazy(() => import('./preview/Diagnostics'));
const DocumentPreview = lazy(() => import('./preview/DocumentPreview'));
const StepInspector = lazy(() => import('./timeline/StepInspector'));
const CommitSheet = lazy(() => import('./commit/CommitSheet'));

function App() {
  const [previewTracks, setPreviewTracks] = useState(sampleTracks);
  const [selected, setSelected] = useState('A');
  const [selectedRepair, setSelectedRepair] = useState<string | null>(null);
  const [view, setView] = useState(initialView);
  const [playing, setPlaying] = useState(false);
  const [notice, setNotice] = useState('');
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [commitOpen, setCommitOpen] = useState(false);
  const [clip, setClip] = useState<{ track: Track; clip: Clip; repairRunId: string | null } | null>(null);
  const [status, setStatus] = useState<DaemonStatus>({ connected: false });
  const [timelineBounds, setTimelineBounds] = useState<TimelineBounds | null>(null);
  const { clips: timelineClips, marks: timelineMarks } = useTimelineWindow(timelineBounds, status);
  const showTimeline = useCallback((bounds: TimelineBounds) => setTimelineBounds(current => JSON.stringify(current) === JSON.stringify(bounds) ? current : bounds), []);
  const [draft, setDraft] = useState('');
  const [references, setReferences] = useState<Attachment[]>([]);
  const [attaching, setAttaching] = useState(false);
  const [attachmentError, setAttachmentError] = useState(false);
  const [replyTarget, setReplyTarget] = useState<ReportQuestion | null>(null);
  const [commitContext, setCommitContext] = useState<CommitContext | null>(null);
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState(false);
  const [choosingProject, setChoosingProject] = useState(false);
  const [projectError, setProjectError] = useState('');
  const [workerSetup, setWorkerSetup] = useState(false);
  const [workerError, setWorkerError] = useState('');
  const submission = useRef<{ opId: string; text: string; attachmentIds: string[]; target: InputTarget; selectedTrackId: string | null; replyTo?: { id: string; revision: number } } | null>(null);
  const heldStarts = useRef(new Map<string, { opId: string; utteranceId: string; revision: number }>());
  const repairSubmission = useRef<{ opId: string; text: string; id: string; revision: number } | null>(null);
  const commitSubmission = useRef<{ method: 'start'; request: RepairRequest } | { method: 'followup'; request: RepairFollowUp } | null>(null);
  const interaction = useRef(0);
  const liveClock = useRef<{ now: number; received: number; bootId: string } | null>(null);
  const timelineNow = useCallback(() => {
    const clock = liveClock.current;
    return clock ? clock.now + Math.min(2, (performance.now() - clock.received) / 1000) : null;
  }, []);
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
  const preview = status.mode === 'preview';
  const tracks = useMemo(() => preview ? previewTracks : liveTimeline(status, timelineClips), [preview, previewTracks, status.tracks, status.workers, status.project, status.workspaces, timelineClips, status.repairRuns, status.utterances]);
  const active = tracks.find(track => track.id === selected);
  const activeRequest = status.utterances?.find(word => word.decision?.kind === 'fork' && word.decision.trackId === selected);
  const deskContext = activeRequest?.text ?? active?.title ?? 'Editor decides';
  const activeRepair = status.repairRuns?.find(run => run.id === selectedRepair);
  const commitContextState = commitContext ? contextState(commitContext, status.integration) : null;
  const contextRepair = commitContext ? status.repairRuns?.find(run => run.jobId === commitContext.jobId && !['done', 'discarded'].includes(run.phase)) : null;
  const canSendContext = commitContextState === 'current' && (!contextRepair || (contextRepair.track === commitContext?.track && contextRepair.phase === 'held' && !contextRepair.restoring && (!contextRepair.refresh || !!contextRepair.refresh.error)));
  const preparationError = status.preparation?.detail ?? status.workspaces?.find(workspace => workspace.state === 'failed')?.error;
  const sessionName = preview ? 'Login hardening' : status.session?.name || status.utterances?.[0]?.text.slice(0, 30) || 'New session';
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
    if (word.decision && ['fork', 'extend', 'schedule'].includes(word.decision.kind)) {
      try {
        let request = heldStarts.current.get(word.id);
        if (!request) {
          request = { opId: await window.intercat.createOperationId(), utteranceId: word.id, revision: word.revision };
          heldStarts.current.set(word.id, request);
        }
        const updated = await window.intercat.startHeldUtterance(request);
        heldStarts.current.delete(word.id);
        setStatus(current => ({ ...current, utterances: current.utterances?.map(item => item.id === updated.id ? updated : item) }));
      } catch { setNotice('This request could not start. Review the worker and try again.'); }
    } else await resolveWord(word, { action: 'retry', target: word.decision?.trackId ? { mode: 'lock', trackId: word.decision.trackId } : word.target });
  }
  async function stopRepair(id: string) {
    try { await window.intercat.stopRepair({ opId: await window.intercat.createOperationId(), id }); }
    catch { setNotice('The repair Stop could not be delivered. Check its connection.'); }
  }
  async function answerWorker(worker: WorkerState, card: Approval, allow: boolean) {
    try { await window.intercat.answerWorker({ opId: await window.intercat.createOperationId(), trackId: worker.trackId, approvalId: card.id, requirement: card.requirement, allow }); }
    catch { setNotice('The approval changed or could not be saved. Review the current card.'); }
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
  async function setupProject() {
    setChoosingProject(true); setProjectError('');
    try {
      const result = await window.intercat.configureProject();
      if (result.project) setStatus(current => ({ ...current, project: result.project }));
      if (result.error) setProjectError(result.error);
    } catch { setProjectError('The project could not be configured. Please try again.'); }
    finally { setChoosingProject(false); }
  }
  async function sendCommitInstruction(context: CommitContext, current: DaemonStatus) {
    if (!commitSubmission.current) {
      if (contextState(context, current.integration) !== 'current') { setNotice('The commit changed. Review the selected lines before sending.'); return; }
      const run = current.repairRuns?.find(run => run.jobId === context.jobId && !['done', 'discarded'].includes(run.phase));
      if (run && (run.track !== context.track || run.phase !== 'held' || run.restoring || (run.refresh && !run.refresh.error))) { setNotice('Stop the current repair before sending another instruction.'); return; }
      const opId = await window.intercat.createOperationId();
      commitSubmission.current = run ? { method: 'followup', request: { opId, id: run.id, revision: run.revision, text: draft, selection: context.selection } }
        : { method: 'start', request: { opId, jobId: context.jobId, revision: context.revision, track: context.track, purpose: { kind: 'edit', selection: context.selection, text: draft } } };
    }
    const pending = commitSubmission.current;
    const result = pending.method === 'start' ? await window.intercat.startRepair(pending.request) : await window.intercat.followUpRepair(pending.request);
    if (result.status === 'rejected') { commitSubmission.current = null; setSaveError(false); setNotice(result.reason); return; }
    commitSubmission.current = null; setCommitContext(null); setDraft(''); setSaveError(false); setNotice('Instruction sent for this commit.');
    void window.intercat.getStatus().then(observeStatus).catch(() => {});
  }
  async function resendCommitInstruction() {
    if (!commitContext) return;
    setSaving(true);
    try {
      const current = await window.intercat.getStatus();
      const job = current.integration;
      if (!job || job.id !== commitContext.jobId || ['done', 'discarded'].includes(job.status)) { setStatus(current); setNotice('The commit is gone. Choose whether to send this as ordinary work.'); return; }
      const diff = await window.intercat.getIntegrationDiff({ id: job.id, revision: job.revision, track: commitContext.track, path: commitContext.selection.path });
      const updated = reanchor(commitContext, job, diff);
      if (!updated) { setStatus(current); setNotice('Those lines could not be found uniquely. Select them again in the current diff.'); return; }
      setCommitContext(updated); setStatus(current);
      await sendCommitInstruction(updated, current);
    } catch { setSaveError(!!commitSubmission.current); setNotice(commitSubmission.current ? 'Save could not be confirmed. Retry uses the same operation ID.' : 'The current diff could not be loaded. Your draft has been kept.'); }
    finally { setSaving(false); }
  }
  async function attachReference(reselect = false) {
    setAttaching(true); setAttachmentError(false);
    try {
      const reference = await window.intercat.attachReference(reselect);
      if (reference) {
        if (references.length >= 128 || references.reduce((sum, file) => sum + file.byteLen, reference.byteLen) > 100 * 1024 * 1024) { setNotice('Reference files can total up to 100 MiB per request. Remove a file before adding another.'); return; }
        setReferences(files => files.some(file => file.id === reference.id) ? files : [...files, reference]);
      }
    } catch { setAttachmentError(true); setNotice('The reference could not be saved. Retry keeps the same file, or choose another file.'); }
    finally { setAttaching(false); }
  }
  async function saveDraft(newTrack = false, asWork = false) {
    if (attaching || (references.length > 0 && !asWork && (replyTarget || selectedRepair || commitContext))) { setNotice('Send these reference files with an ordinary request, or remove them before replying.'); return; }
    setSaving(true);
    try {
      if (commitContext && !submission.current && !asWork) { await sendCommitInstruction(commitContext, status); return; }
      if (asWork && commitContext) submission.current ??= { opId: await window.intercat.createOperationId(), text: draft, attachmentIds: references.map(file => file.id), target: { mode: 'lock', trackId: commitContext.track }, selectedTrackId: commitContext.track };
      if ((selectedRepair || repairSubmission.current) && !submission.current) {
        if (!repairSubmission.current && (activeRepair?.phase !== 'held' || !!activeRepair.restoring || (!!activeRepair.refresh && !activeRepair.refresh.error))) {
          setNotice('Stop this repair before sending another instruction.');
          return;
        }
        repairSubmission.current ??= { opId: await window.intercat.createOperationId(), id: selectedRepair!, revision: activeRepair!.revision, text: draft };
        const result = await window.intercat.followUpRepair(repairSubmission.current);
        if (result.status === 'rejected') {
          repairSubmission.current = null; setSaveError(false); setNotice(result.reason);
          return;
        }
        repairSubmission.current = null; setDraft(''); setSaveError(false); setNotice('Instruction sent to repair.');
        void window.intercat.getStatus().then(observeStatus).catch(() => {});
        return;
      }
      submission.current ??= { opId: await window.intercat.createOperationId(), text: draft, attachmentIds: references.map(file => file.id), target: { mode: newTrack ? 'new' : 'none' }, selectedTrackId: active?.id ?? null, ...(replyTarget ? { replyTo: { id: replyTarget.id, revision: replyTarget.revision } } : {}) };
      const request = submission.current;
      const saved = request.replyTo ? await window.intercat.respondToReport({ opId: request.opId, questionId: request.replyTo.id, revision: request.replyTo.revision, answer: { action: 'reply', text: request.text } }) : await window.intercat.stageUtterance(request);
      if (!saved) throw new Error('Reply was not saved');
      setStatus(current => ({ ...current, utterances: [...(current.utterances ?? []).filter(item => item.id !== saved.id), saved] }));
      submission.current = null; setDraft(''); setReferences([]); setSaveError(false); setReplyTarget(null); setCommitContext(null);
    } catch { setSaveError(true); setNotice('Save could not be confirmed. Retry uses the same operation ID.'); }
    finally { setSaving(false); }
  }
  async function respondReport(question: ReportQuestion, answer: ReportAnswer) {
    try { await window.intercat.respondToReport({ opId: await window.intercat.createOperationId(), questionId: question.id, revision: question.revision, answer }); }
    catch { setNotice('The question changed. Refresh before answering.'); }
  }
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
  function cycleRegion(event: React.KeyboardEvent) {
    if (event.metaKey && (event.code === 'Period' || event.key === '.')) {
      event.preventDefault();
      if (event.altKey) {
        void pauseDispatch(true);
      } else if (event.shiftKey) {
        const worker = (status.workers ?? []).find(worker => worker.trackId === active?.id) ?? status.workers?.[0];
        if (worker) void holdWorker(worker.trackId);
      } else if (selectedRepair) void stopRepair(selectedRepair);
      else { const worker = (status.workers ?? []).find(worker => worker.trackId === active?.id) ?? status.workers?.[0]; if (worker) void stopWorker(worker.trackId); }
      return;
    }
    if (!event.ctrlKey || event.key !== 'Tab') return;
    event.preventDefault();
    const regions = [...document.querySelectorAll<HTMLElement>('[data-focus-region]')].filter(region => region.getClientRects().length > 0);
    const index = regions.findIndex(region => region.contains(document.activeElement));
    regions[(index + (event.shiftKey ? -1 : 1) + regions.length) % regions.length]?.focus();
  }
  return <div className={commitOpen ? "app has-commit" : "app"} onKeyDownCapture={cycleRegion}>
    <aside className="sidebar" tabIndex={-1} data-focus-region aria-label="Sessions">
      <div className="window-chrome"><span className="app-wordmark">intercat</span><span className="sidebar-icon">◧</span></div>
      <button className="new-session" disabled><span>＋</span> New session</button>
      <div className="nav-label">WORKSPACE</div>
      <div className="project-label">{preview ? 'Sample project' : status.project?.name ?? 'Local workspace'}</div>
      <button className="session active"><span className="session-dot" />{sessionName}<span>↗</span></button>
      <div className="sidebar-note">A place for parallel work.<br />A timeline you can steer.</div>
      <button className="settings-button" disabled={!status.connected} onClick={() => setSettingsOpen(true)}>Settings</button>
      <div className="sidebar-bottom"><span className="local-dot" />{preview ? 'Local preview' : 'Local editor'}<span className="version">0.1</span></div>
    </aside>
    <main className="main">
      <header className="session-header"><div><div className="eyebrow">{preview ? 'SAMPLE SESSION' : 'LOCAL SESSION'}</div><h1>{sessionName}</h1></div><button disabled={!status.connected || preview || !(status.integration || status.workspaces?.some(workspace => workspace.state === 'ready'))} className="commit-button" onClick={() => setCommitOpen(true)}>Commit{preview && <span> · 2 folders</span>}</button></header>
      <div className="session-subtitle">{preview ? <><span className="folder-chip">▱ sample-web</span><span className="folder-chip">▱ sample-api</span><span className="preview-label">Preview data</span></> : <><span className="folder-chip" title={status.project?.root}>{status.project ? `▱ ${status.project.name}` : 'No project folder'}</span>{!status.project && <button className="folder-chip" disabled={!status.connected || choosingProject} onClick={() => { void setupProject(); }}>{choosingProject ? 'Checking folder…' : 'Choose project folder'}</button>}</>}</div>
      {status.projectSecretIssue && <div className="project-secret-notice" role="status"><span>{status.projectSecretIssue}</span><button onClick={() => setSettingsOpen(true)}>Review saved secrets</button></div>}
      <section className="stage" tabIndex={-1} data-focus-region aria-label="Stage">
        <Timeline onViewport={showTimeline} liveNow={timelineNow} advancing={preview ? playing : !!status.timeline} preview={preview} tracks={tracks} marks={timelineMarks} selected={selected} selectedRepair={selectedRepair} view={view} onSelect={id => { if (saving || saveError) return; setSelected(id); setSelectedRepair(null); setClip(null); }} onRepairSelect={(track, id) => { if (saving || saveError) return; setSelected(track.id); if (!commitContext) { setSelectedRepair(id); setReplyTarget(null); } setClip(null); }} onRepairStop={id => { void stopRepair(id); }} onClip={(track, chosen, repairRunId) => setClip({ track, clip: chosen, repairRunId })}
          onCut={id => { void cutFuture(id); }} onInteraction={() => { interaction.current = Date.now(); }} />
        <div className="stage-footer"><div><span className="legend-mark">◇</span> waiting <span className="legend-mark">✓</span> done <span className="legend-edit" /> edit</div><div><span className="key-hint">↑ ↓ select · 1–9 jump</span>{preview && <button className="play-button" aria-pressed={playing} onClick={() => setPlaying(!playing)}>{playing ? 'Ⅱ Pause preview' : '▷ Play preview'}</button>}</div></div>
        {!preview && status.timeline?.summaries.some(summary => summary.trackId === selected && summary.repairRunId === selectedRepair) && <p className="history-summary" role="note">Earlier history was summarized by the worker.</p>}
        {clip ? <section className="clip-inspector"><div className="inspector-heading"><span>{clip.track.label}:{clip.clip.ordinal ?? clip.clip.id}{clip.repairRunId ? ' · for the commit' : ''} / {clip.clip.label}</span><button onClick={() => setClip(null)} aria-label="Close step preview">×</button></div><Suspense fallback={<p>Loading step…</p>}>{preview ? <DocumentPreview /> : <StepInspector clipId={clip.clip.id} sequence={status.sequence} />}</Suspense></section> : null}
      </section>
      <section className="desk" tabIndex={-1} data-focus-region aria-label="Desk">
        <div className="desk-heading"><span>DESK</span><span className="desk-selection" title={deskContext}>{deskContext}</span><span className="shortcut">⌃Tab to move between areas</span></div>
        <div className="desk-scroll">
        {status.connected && status.editor && <EditorStatus value={status.editor} update={editor => setStatus(current => ({ ...current, editor }))} />}
        <ReportQuestions provideSecret={async (question, entry) => { await window.intercat.provideWorkerSecret({ ...entry, owner: { kind: 'work' }, trackId: question.trackId, sessionId: question.sessionId, turnId: question.turnId, question: { kind: 'report', id: question.id, revision: question.revision } }); }} stop={track => { void stopWorker(track); }} status={status} reply={question => { if (saving || saveError) return; setSelectedRepair(null); setCommitContext(null); setReplyTarget(question); document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus(); }} respond={(question, answer) => { void respondReport(question, answer); }} takeBack={word => { void cutWord(word); }} retry={word => { void resolveWord(word, { action: 'retry', target: word.target }); }} />
        {status.mode === 'unavailable' && status.message && <div className="desk-card" role="alert"><div><p>{status.message}</p>{status.fingerprintRecovery && <FingerprintRecovery value={status.fingerprintRecovery} cancel={async request => { setStatus(await window.intercat.cancelBackup(request)); }} apply={async request => { setStatus(await window.intercat.applyBackup(request)); }} rebuild={async request => { setStatus(await window.intercat.rebuildFingerprints(request)); }} />}</div></div>}
        {status.connected && status.project && !status.workerConfigured && <div className="desk-card desk-action-card worker-setup"><span className="card-mark">◇</span><div className="desk-copy"><strong>Connect your Meta worker</strong><p>Workers use your Muse sign-in. Your requests and project files are sent to Meta when work runs.</p>{workerError && <p role="alert">{workerError}</p>}</div><div className="desk-actions"><button disabled={workerSetup} onClick={() => { void setupWorkers(); }}>{workerSetup ? 'Connecting…' : 'Connect workers'}</button></div></div>}
        {status.workers?.filter(worker => worker.detail && ['failed', 'needs-recovery', 'question', 'lost', 'after-connection-loss'].includes(worker.phase)).map(worker => <div key={worker.trackId} className="desk-card desk-action-card" role="alert"><span className="card-mark">◇</span><div className="desk-copy"><strong>{worker.phase === 'lost' ? 'Worker lost its connection' : 'Worker needs attention'}</strong><p>{worker.detail}</p></div><div className="desk-actions">{['failed', 'lost'].includes(worker.phase) && !status.pickups?.some(p => p.trackId === worker.trackId && p.sessionId === worker.sessionId && !['ready', 'cancelled'].includes(p.phase)) && <button onClick={() => { void retryWorker(worker.trackId); }}>{worker.phase === 'lost' ? 'Retry connection' : 'Retry worker'}</button>}{['lost', 'needs-recovery'].includes(worker.phase) && !status.pickups?.some(p => p.trackId === worker.trackId && p.sessionId === worker.sessionId && !['ready', 'cancelled'].includes(p.phase)) && <button onClick={() => { void stopWorker(worker.trackId); }}>Stop here</button>}{['lost', 'needs-recovery', 'failed', 'after-connection-loss'].includes(worker.phase) && worker.sessionId && <PickupButton worker={worker} pending={status.pickups?.filter(p => p.trackId === worker.trackId && p.sessionId === worker.sessionId && p.phase !== 'ready').at(-1)} />}</div></div>)}
        {preparationError && <div className="desk-card desk-action-card" role="alert"><span className="card-mark">◇</span><div className="desk-copy"><strong>Work could not be prepared</strong><p>{preparationError}</p></div><div className="desk-actions"><button onClick={() => { void retryPreparation(); }}>Retry preparation</button></div></div>}
        {projectError && <div className="desk-card desk-action-card" role="alert"><span className="card-mark">◇</span><div className="desk-copy"><strong>Project needs attention</strong><p>{projectError}</p></div><div className="desk-actions"><button onClick={() => setProjectError('')}>Dismiss</button></div></div>}
        {status.outputFlows?.filter(flow => flow.lagging || flow.interrupted).map(flow => {
          const repair = flow.repairRunId ? status.repairRuns?.find(run => run.id === flow.repairRunId) : null;
          const track = status.tracks?.find(track => track.id === (repair?.track ?? flow.trackId));
          return <div className="desk-card desk-action-card" role="status" key={flow.repairRunId ?? flow.trackId}><div className="desk-copy"><strong>{flow.interrupted ? 'output too fast to record' : flow.oldestQueuedMs >= 2000 ? 'New work paused · can’t save to the log' : 'log lagging'} · Worker {track?.label ?? '?'}{flow.repairRunId ? ' · for the commit' : ''}</strong><p>{flow.interrupted ? 'This worker was interrupted. Review its state before restarting.' : `${Math.ceil(flow.queuedBytes / 1024)} KiB waiting to be saved. Stop still works.`}</p></div><div className="desk-actions"><button onClick={() => { if (flow.repairRunId) void stopRepair(flow.repairRunId); else if (flow.trackId) void stopWorker(flow.trackId); }}>Stop this {flow.repairRunId ? 'repair' : 'worker'}</button></div></div>;
        })}
        <Workers unloggedControls={status.unloggedControls} secretInputs={status.secretInputs ?? []} provideSecret={async (worker, prompt, entry) => { await window.intercat.provideWorkerSecret({ ...entry, owner: { kind: 'work' }, trackId: worker.trackId, sessionId: prompt.request.sessionId, turnId: prompt.request.turnId, question: { kind: 'msp', id: prompt.request.userInputId, revision: prompt.revision } }); }} flows={status.outputFlows ?? []} effects={status.effects ?? []} workers={status.workers ?? []} tracks={status.tracks ?? []} start={id => { void startWorker(id); }} stop={id => { void stopWorker(id); }} answer={answerWorker} respond={async (worker, prompt, response) => { await window.intercat.respondToWorker({ opId: await window.intercat.createOperationId(), trackId: worker.trackId, promptId: prompt.request.userInputId, revision: prompt.revision, response }); }} />
        <Requests onStart={word => { void startWord(word); }} redirects={status.redirects ?? []} words={status.utterances ?? []} tracks={status.tracks ?? []} onResolve={(word, resolution) => { void resolveWord(word, resolution); }} onSelect={word => { if (word.decision?.trackId || word.provisionalTrackId) setSelected(word.decision?.trackId ?? word.provisionalTrackId!); }} onCut={word => { void cutWord(word); }} onEdit={word => { void cutWord(word).then(cut => { if (cut) { setDraft(word.text); setReferences(word.attachments); } }); }} />
        </div>
        <div className="composer">{commitContext && <div className="reply-target commit-context" aria-label="Commit context"><span title={commitContext.selection.path}>{commitContext.selection.path}:{commitContext.selection.startLine}{commitContext.selection.endLine !== commitContext.selection.startLine ? `–${commitContext.selection.endLine}` : ''} @ {commitContext.selection.tree.slice(0, 7)}</span><span>fix for this commit · {commitContext.label}</span><button disabled={saving || saveError} onClick={() => setCommitContext(null)}>Clear commit context</button>{commitContextState !== 'current' && <div className="commit-context-changed" role="status"><span>{commitContextState === 'gone' ? 'the commit is gone' : 'the commit changed'}</span>{commitContextState === 'changed' ? <button disabled={saving || saveError || !draft.trim()} onClick={() => { void resendCommitInstruction(); }}>Resend on the new version</button> : <button disabled={saving || saveError || !draft.trim()} onClick={() => { void saveDraft(false, true); }}>Send to {commitContext.label}</button>}<button disabled={saving || saveError} onClick={() => setCommitContext(null)}>Take back</button></div>}</div>}{selectedRepair && <div className="reply-target">{active?.label} · for the commit · {activeRepair?.phase === 'held' ? 'Add an instruction' : activeRepair?.phase ?? 'Recorded repair'}<button disabled={saving || saveError} onClick={() => { setSelectedRepair(null); repairSubmission.current = null; }}>Clear repair target</button></div>}{replyTarget && <div className="reply-target">Reply to Worker {status.tracks?.find(track => track.id === replyTarget.trackId)?.label}<button disabled={saving} onClick={() => { setReplyTarget(null); submission.current = null; setSaveError(false); }}>Cancel reply</button></div>}{(references.length > 0 || attachmentError) && <div className="reference-files">{attachmentError && <button disabled={attaching || saving || saveError} onClick={() => { void attachReference(true); }}>Choose another file</button>}{references.map(file => <span className="reference-chip" key={file.id}><span title={file.name}>📎 {file.name}</span><button disabled={saving || saveError} aria-label={`Remove reference ${file.name}`} onClick={() => setReferences(files => files.filter(item => item.id !== file.id))}>×</button></span>)}</div>}<textarea data-focus-region aria-label="Request draft" placeholder="What would you like to work on?" rows={2} value={draft} disabled={saving || saveError} onChange={event => setDraft(event.target.value)} onKeyDown={event => { if (event.key === 'Escape' && commitContext && !saving && !saveError) { event.preventDefault(); setCommitContext(null); return; } if (event.metaKey && event.key === 'Enter' && draft.trim() && !saving && status.connected) { event.preventDefault(); void saveDraft(event.shiftKey); } }} /><div className="composer-bottom"><div className="composer-tools"><button className="attach-reference" aria-label={attachmentError ? 'Retry reference save' : 'Attach reference file'} title={attaching ? 'Saving reference…' : attachmentError ? 'Retry reference save' : 'Attach reference file'} aria-busy={attaching} disabled={!status.connected || preview || saving || saveError || attaching || !!replyTarget || !!selectedRepair || !!commitContext} onClick={() => { void attachReference(); }}><svg viewBox="0 0 20 20" width="18" height="18" aria-hidden="true"><path d="M10 4v12M4 10h12" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" /></svg></button><span>{commitContext ? `fix for this commit · ${commitContext.label}` : selectedRepair ? `${active?.label} · for the commit` : 'Editor decides'}</span></div><div className="composer-actions"><button disabled={!status.connected || !draft.trim() || saving || attaching || !!replyTarget || !!selectedRepair || !!commitContext} onClick={() => { void saveDraft(true); }} title="New track · ⇧⌘↵">New track</button><button disabled={!status.connected || !draft.trim() || saving || attaching || (!!commitContext && !canSendContext && !saveError) || (!!selectedRepair && (activeRepair?.phase !== 'held' || !!activeRepair.restoring || (!!activeRepair.refresh && !activeRepair.refresh.error)) && !saveError)} onClick={() => { void saveDraft(); }} title="Put request · ⌘↵">{saveError ? 'Retry save' : saving ? 'Saving…' : commitContext ? 'Send to repair' : selectedRepair ? 'Send to repair' : replyTarget ? 'Send reply' : 'Put'} <span aria-hidden="true">↵</span></button></div></div></div>
        <footer className="status-bar">{status.logUnavailable && <span role="status">Can’t save to the log · Stop still works</span>}<DispatchControls status={status} change={paused=>{void pauseDispatch(paused);}} /><span><i />{status.connected ? 'Daemon connected' : 'Daemon not connected'}</span><span>Local editor · {status.editor?.phase === 'ready' ? 'ready' : status.editor?.phase === 'starting' ? 'starting' : 'unavailable'}</span>{status.mode === 'unavailable' && <button onClick={() => { void window.intercat.retryConnection().then(setStatus); }}>Reconnect</button>}<span className="status-end">{status.workers?.length ? `Workers · ${status.workers.filter(worker => worker.phase === 'working' || worker.phase === 'approval').length} active / ${status.workers.filter(worker => worker.phase === 'ready').length} ready` : status.workerConfigured ? 'Workers connecting' : 'No work is being sent'}</span></footer>
      </section>
    </main>
    {commitOpen && <Suspense fallback={<aside className="commit-sheet">Opening review…</aside>}><CommitSheet secretInputs={status.secretInputs ?? []} selectContext={context => { if (saving || saveError) return; setCommitContext(context); setSelectedRepair(null); setReplyTarget(null); }} job={status.integration ?? null} effects={status.effects ?? []} flows={status.outputFlows ?? []} repairs={status.repairRuns ?? []} close={() => setCommitOpen(false)} update={async job => { setStatus(current => ({ ...current, integration: job })); const value = await window.intercat.getStatus(); observeStatus(value); }} /></Suspense>}
    <div className="toast" role="status">{notice}</div>
    <PolicySettings project={status.projectSecrets} tracks={status.tracks ?? []} open={settingsOpen} stops={status.stops ?? 'irreversible-only'} close={() => setSettingsOpen(false)} saved={stops => setStatus(current => ({ ...current, stops }))} />
  </div>;
}

createRoot(document.getElementById('root')!).render(new URLSearchParams(location.search).get('view') === 'diagnostics' ? <Suspense fallback={<p>Loading diagnostics…</p>}><Diagnostics /></Suspense> : <App />);
