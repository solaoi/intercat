import type { Plan } from '../plans/types';
import { waitSourceLabel } from '../waitSource';
import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import type { EditAction, LiveTrack, RequestEdit, Utterance, WorkerState, TimelineClip, WaitTarget } from '../bridge';
import { DecisionTiming } from './DecisionTiming';
import { RequestTurn } from './RequestTurn';
import { PrivacyText, usePrivacyLabels } from './PrivacyText';

type PendingEdit = Extract<EditAction, { kind: 'apply' | 'end' }>;
type Lease = NonNullable<Utterance['editHold']>;
type Turn = { sessionId: string; turnId: string } | null;
interface Props {
  word: Utterance;
  tracks: LiveTrack[];
  workers: WorkerState[];
  clips: TimelineClip[];
  plans: Plan[];
  initialEditing: boolean;
  initialLease?: Lease | null;
  compose: (word: Utterance, lease: Lease | null) => void;
  update: (word: Utterance) => void;
  close: () => void;
}

export function RequestEditor({ word, tracks, workers, clips, plans, initialEditing, initialLease, compose, update, close }: Props) {
  const privateLabel = usePrivacyLabels([...tracks.map(track => track.title), ...clips.map(clip => clip.title ?? clip.label), ...plans.flatMap(plan => plan.items.map(item => item.title))]);
  const [current, setCurrent] = useState(word);
  const [track, setTrack] = useState(word.decision?.trackId ?? (word.target.mode === 'lock' ? word.target.trackId : ''));
  const [boundary, setBoundary] = useState(word.decision?.boundary ?? 'after-turn');
  const [alreadySent, setAlreadySent] = useState(!!word.interruptStarted || ['starting', 'sent', 'landed'].includes(word.state));
  const transferred = useRef(!!initialLease);
  const [lease, setLease] = useState<Lease | null>(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const [position, setPosition] = useState({ left: 8, top: 8 });
  const pending = useRef<PendingEdit | null>(null);
  const pendingMisread = useRef<{ opId: string; utteranceId: string; revision: number } | null>(null);
  const leaseRef = useRef<Lease | null>(null);
  const busyRef = useRef(false);
  const popup = useRef<HTMLDivElement>(null);
  const focusAfterChange = useRef<HTMLElement | null>(null);
  const updateRef = useRef(update);
  const closeRef = useRef(close);
  updateRef.current = update;
  closeRef.current = close;
  const [shownTurn, setShownTurn] = useState<Turn>(() => {
    const worker = workers.find(worker => worker.trackId === track);
    return worker?.activeTurn && worker.sessionId ? { sessionId: worker.sessionId, turnId: worker.activeTurn } : null;
  });

  function accept(saved: Utterance) {
    setCurrent(saved);
    setTrack(saved.decision?.trackId ?? (saved.target.mode === 'lock' ? saved.target.trackId : ''));
    setBoundary(saved.decision?.boundary ?? 'after-turn');
    leaseRef.current = saved.editHold ?? null;
    setLease(leaseRef.current);
    updateRef.current(saved);
  }

  useLayoutEffect(() => {
    const element = popup.current!;
    const previous = document.activeElement as HTMLElement | null;
    const anchor = document.querySelector<HTMLElement>(`[data-utterance-id="${CSS.escape(word.id)}"]`);
    element.showPopover();
    const place = () => {
      const bounds = anchor?.getBoundingClientRect();
      const width = element.offsetWidth;
      const height = element.offsetHeight;
      const left = Math.max(8, Math.min(bounds?.left ?? 8, window.innerWidth - width - 8));
      const top = Math.max(8, Math.min((bounds?.top ?? window.innerHeight) - height - 8, window.innerHeight - height - 8));
      setPosition(old => old.left === left && old.top === top ? old : { left, top });
    };
    place();
    const observer = new ResizeObserver(place);
    observer.observe(element);
    window.addEventListener('resize', place);
    element.focus();
    return () => {
      observer.disconnect(); window.removeEventListener('resize', place);
      element.hidePopover();
      if (previous?.isConnected && element.contains(document.activeElement)) previous.focus();
    };
  }, [word.id]);

  useLayoutEffect(() => {
    const target = focusAfterChange.current;
    if (busy || !target) return;
    focusAfterChange.current = null;
    if (target.isConnected && popup.current?.contains(target)) target.focus();
    else popup.current?.focus();
  });

  function enterComposer(saved = current, held = leaseRef.current) {
    transferred.current = true;
    compose(saved, held);
    closeRef.current();
  }

  useEffect(() => {
    let disposed = false;
    void (async () => {
      try {
        if (alreadySent) { if (initialEditing) enterComposer(word, null); return; }
        const token = initialLease?.token ?? await window.intercat.createOperationId();
        const saved = await window.intercat.editUtterance(initialLease ? { kind: 'renew', token } : { kind: 'begin', token, utteranceId: word.id, revision: word.revision });
        if (disposed) {
          if (saved.editHold && !transferred.current) await window.intercat.editUtterance({ kind: 'end', token, revision: saved.editHold.revision, edit: { kind: 'cancel' } });
        } else { accept(saved); if (initialEditing) enterComposer(saved, saved.editHold ?? null); }
      } catch (error) {
        if (!disposed) {
          setError(error instanceof Error ? error.message : 'The request changed. Reopen it before editing.');
          const status = await window.intercat.getStatus().catch(() => null);
          const latest = status?.utterances?.find(item => item.id === word.id);
          if (latest && (latest.interruptStarted || ['starting', 'sent', 'landed'].includes(latest.state))) {
            setCurrent(latest); setAlreadySent(true);
            if (initialEditing) enterComposer(latest, null);
          }
        }
      }
    })();
    return () => {
      disposed = true;
      const active = leaseRef.current;
      if (active && !transferred.current) void window.intercat.editUtterance({ kind: 'end', token: active.token, revision: active.revision, edit: { kind: 'cancel' } }).catch(() => {});
    };
  }, [word.id, word.revision]);

  useEffect(() => {
    if (!lease) return;
    const timer = setInterval(() => {
      void window.intercat.editUtterance({ kind: 'renew', token: lease.token }).catch(() => {
        setError('Editing was interrupted. Make an explicit change to continue, or close to keep the request held.');
      });
    }, 10_000);
    return () => clearInterval(timer);
  }, [lease]);

  useEffect(() => {
    const outside = (event: PointerEvent) => {
      const element = event.target as Node;
      const anchor = document.querySelector(`[data-utterance-id="${CSS.escape(word.id)}"]`);
      if (!popup.current?.contains(element) && !anchor?.contains(element)) void finish({ kind: 'cancel' });
    };
    document.addEventListener('pointerdown', outside, true);
    return () => document.removeEventListener('pointerdown', outside, true);
  });

  async function submit() {
    const action = pending.current;
    if (!action) return;
    const focused = document.activeElement as HTMLElement | null;
    let failed = false;
    busyRef.current = true; setBusy(true);
    try {
      const saved = await window.intercat.editUtterance(action);
      if (action.kind === 'apply' && saved.editHold?.token !== action.token) throw new Error('Another edit owns this request. Reopen it.');
      accept(saved); pending.current = null; setError('');
      if (action.kind === 'end') closeRef.current();
    } catch (error) {
      failed = true;
      setError(error instanceof Error ? error.message : 'The change could not be confirmed. Retry uses the same operation.');
      popup.current?.focus();
    } finally {
      if (!failed) focusAfterChange.current = focused ?? popup.current;
      busyRef.current = false; setBusy(false);
    }
  }

  async function apply(edit: RequestEdit) {
    if (busyRef.current || pending.current || !leaseRef.current) return;
    busyRef.current = true; setBusy(true);
    try {
      const active = leaseRef.current;
      pending.current = { kind: 'apply', token: active.token, revision: active.revision, opId: await window.intercat.createOperationId(), edit };
      await submit();
    } catch { busyRef.current = false; setBusy(false); setError('The change could not be prepared. Try again.'); }
  }

  async function markMisread() {
    if (busyRef.current || current.correction?.misread) return;
    busyRef.current = true; setBusy(true);
    try {
      pendingMisread.current ??= { opId: await window.intercat.createOperationId(), utteranceId: current.id, revision: current.revision };
      accept(await window.intercat.markMisread(pendingMisread.current));
      pendingMisread.current = null;
      setError('');
      focusAfterChange.current = popup.current;
    } catch (error) {
      setError(error instanceof Error ? error.message : 'The correction could not be confirmed. Retry uses the same operation.');
      popup.current?.focus();
    } finally {
      busyRef.current = false; setBusy(false);
    }
  }

  async function finish(edit: RequestEdit) {
    if (busyRef.current) return;
    if (edit.kind === 'cancel' && transferred.current) { closeRef.current(); return; }
    let active = leaseRef.current;
    if (!active) { closeRef.current(); return; }
    busyRef.current = true; setBusy(true);
    if (pending.current) {
      try {
        const status = await window.intercat.getStatus();
        const saved = status.utterances?.find(candidate => candidate.id === word.id);
        if (saved?.editHold?.token === active.token) { accept(saved); active = saved.editHold; }
      } catch { /* The persisted lease still protects an uncertain edit. */ }
    }
    pending.current = { kind: 'end', token: active.token, revision: active.revision, edit };
    if (edit.kind !== 'cancel') { await submit(); return; }
    try { accept(await window.intercat.editUtterance(pending.current)); }
    catch { /* An unrenewed lease becomes held if its release cannot be saved. */ }
    finally { leaseRef.current = null; busyRef.current = false; closeRef.current(); }
  }

  const label = tracks.find(candidate => candidate.id === track)?.label ?? '?';
  const missingTrack = !!track && !tracks.some(candidate => candidate.id === track);
  const completedCommand = current.state === 'landed' && current.decision?.kind === 'command';
  const started = current.startWaiting?.started;
  const startedRequest = current.startWaiting?.candidates.find(candidate => candidate.kind === 'plan'
    ? started?.kind === 'plan' && candidate.planId === started.planId
    : started?.kind === 'input' && candidate.inputId === started.inputId);
  let interpretation = 'The Editor will read this request.';
  if (current.decision?.kind === 'command') interpretation = `Application command · ${current.decision.command}`;
  else if (current.decision?.trackId) {
    const timing = current.startCondition?.source ? `after ${waitSourceLabel(current.startCondition.source)}`
      : boundary === 'when-told' ? 'when I say' : boundary === 'now' ? 'now' : 'after this turn';
    interpretation = `${current.decision.kind === 'trim' ? 'Redirect' : 'Send to'} Worker ${label} · ${timing}`;
  } else if (current.decision?.kind === 'park') interpretation = 'Parked · choose a track to place it';
  else if (current.state === 'asked') interpretation = 'Waiting for clarification';
  const selectedWait = current.startCondition?.source;
  const selectedWaitKey = selectedWait?.kind === 'clip' ? `clip:${selectedWait.value.id}` : selectedWait?.kind === 'plan-item' ? `plan:${selectedWait.value.itemId}` : selectedWait?.kind === 'turn' && (selectedWait.value.trackId !== track || selectedWait.value.sessionId !== shownTurn?.sessionId || selectedWait.value.turnId !== shownTurn?.turnId) ? `turn:${selectedWait.value.sessionId}/${selectedWait.value.turnId}` : '';
  const disabled = !lease || busy || pending.current !== null;
  const waits: { key: string; label: string; source: WaitTarget }[] = [
    ...clips.filter(clip => clip.running && !clip.repairRunId && clip.turnId).map(clip => ({ key: `clip:${clip.id}`, label: `Turn containing ${tracks.find(track => track.id === clip.trackId)?.label ?? '?'}:${clip.ordinal} · ${privateLabel(clip.title ?? clip.label)}`, source: { kind: 'clip' as const, clipId: clip.id } })),
    ...plans.filter(plan => plan.generation.state === 'ready').flatMap(plan => plan.items.filter(item => item.state === 'planned' || item.state === 'editing').map(item => ({ key: `plan:${item.planItemId}`, label: `${tracks.find(track => track.id === item.trackRef)?.label ?? 'New track'} · ${privateLabel(item.title)}`, source: { kind: 'plan-item' as const, planId: plan.id, itemId: item.planItemId, revision: plan.rev } }))),
  ];

  return <div popover="manual" className="request-editor" role="dialog" aria-modal="false" aria-labelledby="request-editor-title" tabIndex={-1} ref={popup} style={position} onKeyDown={event => {
    if (event.nativeEvent.isComposing) return;
    if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); void finish({ kind: 'cancel' }); }
  }}>
    <header><h2 id="request-editor-title">Request</h2><button aria-label="Close request details" disabled={busy} onClick={() => { void finish({ kind: 'cancel' }); }}>×</button></header>
    <div className="request-editor-body">
      <section aria-labelledby="request-you-said"><h3 id="request-you-said">You said</h3>
        <p className="request-original" tabIndex={0}><PrivacyText text={current.text} /></p>
        {alreadySent && <p>{completedCommand ? 'Command completed' : current.interruptStarted ? 'Stop already sent' : 'Already sent'} · edit this as a new instruction</p>}
        <button disabled={busy || (!lease && !alreadySent)} onClick={() => enterComposer()}>Edit text</button>
      </section>
      <section aria-labelledby="request-read-as"><h3 id="request-read-as">Read as</h3>
        <p className="request-interpretation"><PrivacyText text={interpretation} interactive={false} /></p>
        {current.startCondition?.detail && <p>{current.startCondition.detail} · change the wait target or start time to continue.</p>}
        {current.reasonReview && <p className="request-reason-review">{alreadySent ? 'Sent' : 'Ready to check'} with {current.reasonReview.label}’s reasons</p>}
        {completedCommand && startedRequest && <p className="request-started">Start requested{startedRequest.label ? ` · Worker ${startedRequest.label}` : ''}<span><PrivacyText text={startedRequest.text} interactive={false} /></span></p>}
        <label>Send to<select aria-label="Edited request destination" value={track} disabled={disabled} onChange={event => {
          const id = event.target.value;
          const worker = workers.find(worker => worker.trackId === id);
          const turn = worker?.activeTurn && worker.sessionId ? { sessionId: worker.sessionId, turnId: worker.activeTurn } : null;
          setTrack(id);
          setShownTurn(turn);
          void apply({ kind: 'readAs', trackId: id, boundary: boundary === 'now' || boundary === 'when-told' ? boundary : 'after-turn', turn });
        }}>
          {!track && <option value="">Choose a track</option>}
          {missingTrack && <option value={track}>Unavailable track</option>}
          {tracks.map(track => <option key={track.id} value={track.id}>Worker {track.label} · {privateLabel(track.title)}</option>)}
        </select></label>
        <fieldset disabled={disabled || !track || missingTrack}><legend>When</legend>
          <label><input type="radio" name="request-boundary" checked={boundary === 'now'} onChange={() => { setBoundary('now'); void apply({ kind: 'readAs', trackId: track, boundary: 'now', turn: shownTurn }); }} />{shownTurn ? `Stop ${label} and send` : 'Now'}<small>{shownTurn ? 'stops the current turn, then sends this (⌘.)' : 'Starts when the worker is ready'}</small></label>
          <label><input type="radio" name="request-boundary" checked={boundary === 'after-turn' && !selectedWaitKey} onChange={() => { setBoundary('after-turn'); void apply({ kind: 'readAs', trackId: track, boundary: 'after-turn', turn: shownTurn }); }} />After this turn</label>
          <label><input type="radio" name="request-boundary" checked={boundary === 'when-told'} onChange={() => { setBoundary('when-told'); void apply({ kind: 'readAs', trackId: track, boundary: 'when-told', turn: shownTurn }); }} />When I say</label>
        </fieldset>
        <label className="request-wait-target">Wait for work<select aria-label="Wait for work" value={selectedWaitKey} disabled={disabled || !track || missingTrack} onChange={event => {
          const choice = waits.find(choice => choice.key === event.target.value);
          if (choice) void apply({ kind: 'waitFor', source: choice.source });
        }}><option value="">Choose a running clip or planned item</option>{selectedWaitKey && !waits.some(choice => choice.key === selectedWaitKey) && <option value={selectedWaitKey} disabled>{waitSourceLabel(selectedWait!)}</option>}{waits.map(choice => <option key={choice.key} value={choice.key}>{choice.label}</option>)}</select></label>
        <p className="request-wait-hint">Waits for the selected turn to end, or the planned item to succeed. Files are not inherited.</p>
        {current.decision && <details className="decision-details"><summary>Decision details</summary><dl>
          <dt>Operation</dt><dd>{current.decision.kind}</dd>
          <dt>Track</dt><dd>{current.decision.trackId ?? 'None'}</dd>
          <dt>Boundary</dt><dd>{current.decision.boundary ?? 'None'}</dd>
          {current.decision.command && <><dt>Command</dt><dd>{current.decision.command}</dd></>}
        </dl></details>}
        <DecisionTiming decision={current.decision} />
        {current.decision?.description && <section aria-label="Recorded Editor explanation"><h4>Editor explanation</h4><p><PrivacyText text={current.decision.description.detail} /></p></section>}
      </section>
      <p role="status">{busy ? 'Applying change…' : error || (current.correction?.misread ? 'Marked as misread.' : completedCommand ? 'Application command completed.' : alreadySent ? 'This request has already been sent.' : lease ? 'This request is held while you edit.' : 'Holding the request…')}</p>
      {current.state === 'landed' && !completedCommand && <RequestTurn input={current.id} />}
      {pending.current && !busy && <button onClick={() => { void submit(); }}>Retry change</button>}
    </div>
    <footer className="request-editor-actions">{current.state === 'landed' && <button disabled={busy || current.correction?.misread} onClick={() => { void markMisread(); }}>{pendingMisread.current ? 'Retry correction' : 'Mark as misread'}</button>}<button disabled={disabled} onClick={() => { void finish({ kind: 'takeBack' }); }}>Take back</button></footer>
  </div>;
}
