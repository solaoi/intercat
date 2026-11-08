import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import type { DaemonStatus, HostMemoryStatus, LiveTrack, PlanDependencyLocation } from '../bridge';
import { PrivacySummary, PrivacyText, usePrivacyLabels } from '../desk/PrivacyText';
import type { Plan, PlanAction, PlanCommand, PlanItem, PlanRequest } from './types';
import { ItemActions } from './ItemActions';
import { ItemRevision } from './ItemRevision';
import { DependencyEditor } from './DependencyEditor';
import { DependencyActions } from './DependencyActions';
import { blockedDependencies } from './dependencies';
import type { PlanCountdowns } from './usePlanStart';
import { PlanControls, PlanFailureActions, planLabel } from './PlanControls';
import { PlanPicker } from './PlanPicker';
import { PlanMoveConfirmation, type MoveChoice } from './PlanMoveConfirmation';
import './plans.css';

export function Plans({ status, updated, request, moved, edit, continueItem, countdowns, openDependency }: { status: DaemonStatus; updated: (plan: Plan) => void; request: PlanRequest | null; moved: (plan: Plan, itemId: string, focus: boolean) => void; edit: (plan: Plan, itemId: string) => void; continueItem: (plan: Plan, itemId: string) => void; countdowns: PlanCountdowns; openDependency: (location: PlanDependencyLocation) => void }) {
  const plans = status.plans ?? [];
  const moveTracks = (status.tracks ?? []).filter(track => track.status !== 'lost'
    && !(status.workers ?? []).some(worker => worker.trackId === track.id && ['lost', 'needs-recovery'].includes(worker.phase)));
  const [selected, select] = useState<string | null>(null);
  const [view, setView] = useState<Plan | null>(null);
  const [focusItem, setFocusItem] = useState<string | undefined>();
  const [focusRevision, setFocusRevision] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [announcement, announce] = useState('');
  const [moveNotice, setMoveNotice] = useState<{ text: string } | null>(null);
  // Timeline restores focus in its layout effect. Publish the live-region
  // message afterwards so the focus announcement cannot replace the position.
  useEffect(() => { if (moveNotice) announce(moveNotice.text); }, [moveNotice]);
  const [dependency, setDependency] = useState<{ plan: Plan; itemId: string } | null>(null);
  const changeDependencies = (plan: Plan, itemId: string) => { setView(null); setDependency({ plan, itemId }); };
  const [menuRequest, setMenuRequest] = useState<PlanRequest | null>(null);
  const [moveChoice, setMoveChoice] = useState<MoveChoice | null>(null);
  const pending = useRef<PlanCommand | null>(null);
  const inFlight = useRef(false);
  const current = plans.find(plan => plan.id === selected) ?? plans.find(plan => !['done', 'single-job'].includes(plan.state)) ?? plans.at(-1);
  const connected = status.connected && status.mode !== 'preview';
  async function command(plan: Plan, action: PlanAction): Promise<Plan | null> {
    if (!connected || inFlight.current) return null;
    if (pending.current && (pending.current.planId !== plan.id || JSON.stringify(pending.current.action) !== JSON.stringify(action))) return null;
    if (action.kind === 'move-to' && plan.lockedTrack && action.trackId !== plan.lockedTrack) {
      setError(''); setMoveChoice({ plan, itemId: action.itemId, trackId: action.trackId });
      return null;
    }
    inFlight.current = true;
    setBusy(true); setError(''); announce('');
    try {
      pending.current ??= { opId: await window.intercat.createOperationId(), planId: plan.id, revision: plan.rev, action };
      const result = await window.intercat.commandPlan(pending.current);
      pending.current = null; updated(result); setView(old => old?.id === result.id ? result : old);
      if (action.kind === 'move' || action.kind === 'move-to' || action.kind === 'unlock-and-move') {
        if (action.kind === 'unlock-and-move') setMoveChoice(null);
        const item = result.items.find(item => item.planItemId === action.itemId);
        if (item) {
          const previous = [...result.items].filter(other => other.trackRef === item.trackRef && other.state !== 'cut' && other.order < item.order).sort((a, b) => b.order - a.order)[0];
          const track = status.tracks?.find(track => track.id === item.trackRef)?.label ?? 'new track';
          moved(result, item.planItemId, !view);
          if (view) { setFocusItem(item.planItemId); setFocusRevision(value => value + 1); }
          setMoveNotice({ text: `Moved to ${track}${previous ? `, after ${previous.title}` : ', first item'}.` });
        }
      }
      return result;
    } catch (failure) { setError(failure instanceof Error ? failure.message.replace(/^(?:Error: )?Error invoking remote method '[^']+': (?:Error: )?/, '') : 'The plan could not be updated.'); return null; }
    finally { inFlight.current = false; setBusy(false); }
  }
  async function start(plan: Plan) {
    // Review and Start retain the same displayed revision. Another window's
    // edit between these calls is rejected by the journal.
    const seen = await command(plan, { kind: 'review' });
    if (seen && seen.rev === plan.rev) {
      const started = await command(plan, { kind: 'start' });
      if (started) setView(old => old?.id === started.id ? started : old);
    }
  }
  function retryPending() {
    const request = pending.current;
    const plan = plans.find(plan => plan.id === request?.planId);
    if (request && plan) void command(plan, request.action);
  }
  function review(plan: Plan, itemId?: string) { select(plan.id); setView(plan); setFocusItem(itemId); if (!pending.current) { setError(''); void command(plan, { kind: 'review' }); } }
  useEffect(() => {
    if (!request) return;
    const plan = plans.find(plan => plan.id === request.id);
    if (!plan) return;
    const item = plan.items.find(item => item.planItemId === request.itemId);
    if (!request.mode) {
      if (item && request.point && blockedDependencies(plan, item, plans, status.planDependencies ?? {}).length) setMenuRequest(request);
      else review(plan, request.itemId);
      return;
    }
    if (!item) return;
    if (request.mode === 'menu') { setMenuRequest(request); return; }
    if (request.mode === 'cut') { void command(plan, { kind: 'cut', itemId: item.planItemId }); return; }
    if (request.mode === 'earlier' || request.mode === 'later') { void command(plan, { kind: 'move', itemId: item.planItemId, direction: request.mode }); return; }
    const tracks = moveTracks;
    const index = tracks.findIndex(track => track.id === item.trackRef);
    const target = index < 0 ? (request.mode === 'down' ? tracks[0] : tracks.at(-1)) : tracks[index + (request.mode === 'down' ? 1 : -1)];
    if (target) void command(plan, { kind: 'move-to', itemId: item.planItemId, trackId: target.id });
    else { setError('There is no worker track in that direction.'); }
  }, [request]);
  const latest = view ? plans.find(plan => plan.id === view.id) : undefined;
  const menuPlan = plans.find(plan => plan.id === menuRequest?.id);
  const menuItem = menuPlan?.items.find(item => item.planItemId === menuRequest?.itemId);
  const dismissMenu = () => setMenuRequest(current => current?.nonce === menuRequest?.nonce ? null : current);
  return <>
    {!view && <p role="status" aria-atomic="true" className="sr-only"><PrivacyText text={announcement} /></p>}
    {menuPlan && menuItem && menuRequest?.point && (blockedDependencies(menuPlan, menuItem, plans, status.planDependencies ?? {}).length
      ? <DependencyActions key={menuRequest.nonce} plan={menuPlan} item={menuItem} plans={plans} locations={status.planDependencies ?? {}} tracks={moveTracks} edit={edit} disabled={busy || !!pending.current || !connected}
          command={(owner, action) => { void command(owner, action); }} open={openDependency} change={() => changeDependencies(menuPlan, menuItem.planItemId)} point={menuRequest.point} dismiss={dismissMenu} />
      : <ItemActions key={menuRequest.nonce} plan={menuPlan} item={menuItem} tracks={moveTracks} disabled={busy || !!pending.current || !connected || menuItem.state !== 'planned'} dependencies={() => changeDependencies(menuPlan, menuItem.planItemId)} point={menuRequest.point} dismiss={dismissMenu} edit={() => { setView(null); edit(menuPlan, menuItem.planItemId); }} command={action => { void command(menuPlan, action); }} />)}
    {dependency && <DependencyEditor initial={dependency.plan} itemId={dependency.itemId} update={updated} close={(plan, applied) => { updated(plan); setDependency(null); moved(plan, dependency.itemId, true); if (applied) announce('Dependencies updated.'); }} />}
    {current && <section className="plan-strip" aria-label="Plans">
      <PlanPicker hostMemory={status.hostMemory} plans={plans} tracks={status.tracks ?? []} selected={current.id} busy={busy} pending={!!pending.current} connected={connected} paused={!!status.dispatchPaused}
        choose={plan => { select(plan.id); if (!pending.current) setError(''); }} review={plan => review(plan)} start={plan => { void start(plan); }} command={(plan, action) => { void command(plan, action); }} />
      <div className="plan-strip-copy"><span>{countdowns[current.id]?.revision === current.rev ? `starts in ${countdowns[current.id].seconds} s` : planLabel(current, status.hostMemory)}</span>{current.summary && <small><PrivacySummary text={current.summary} limit={90} /></small>}</div>
      <PlanControls plan={current} tracks={status.tracks ?? []} busy={busy} pending={!!pending.current} connected={connected} paused={!!status.dispatchPaused} primary
        review={() => review(current)} start={() => { void start(current); }} command={action => { void command(current, action); }} />
    </section>}
    {error && !view && !moveChoice && <div className="plan-error" role="alert"><span>{error}</span>{pending.current && <button disabled={busy} onClick={retryPending}>Retry plan action</button>}{pending.current && <button disabled={busy} onClick={() => { pending.current = null; setError(''); }}>Reload plan</button>}</div>}
    {view && <PlanReview hostMemory={status.hostMemory} view={latest?.rev === view.rev ? latest : view} focusItem={focusItem} focusRevision={focusRevision} latest={latest} tracks={status.tracks ?? []} moveTracks={moveTracks} source={status.utterances?.find(word => word.id === view.sourceId)?.text}
      dependencyActions={item => <DependencyActions plan={view} item={item} plans={plans} locations={status.planDependencies ?? {}} tracks={moveTracks} edit={(plan, id) => { setView(null); edit(plan, id); }} disabled={busy || !!pending.current || !connected || latest?.rev !== view.rev}
        command={(owner, action) => { void command(owner, action); }} open={location => { setView(null); openDependency(location); }} change={() => changeDependencies(view, item.planItemId)} />}
      canRetry={item => !!status.planDependencies?.[item.planItemId]?.retryable}
      continueItem={(plan, id) => { setView(null); continueItem(plan, id); }} busy={busy} paused={!!status.dispatchPaused} connected={connected} error={moveChoice ? '' : error} pending={!!pending.current} announcement={announcement} dependencies={changeDependencies} edit={(plan, id) => { setView(null); edit(plan, id); }} update={plan => { updated(plan); setView(plan); }}
      refresh={() => { if (latest) { pending.current = null; review(latest); } }} close={() => setView(null)}
      action={action => { void command(view, action).then(result => { if (result) setView(action.kind === 'retry-item' ? null : result); }); }} retry={retryPending} />}
    {moveChoice && <PlanMoveConfirmation choice={moveChoice} latest={plans.find(plan => plan.id === moveChoice.plan.id)} tracks={status.tracks ?? []}
      busy={busy} connected={connected} targetAvailable={moveTracks.some(track => track.id === moveChoice.trackId)} pending={!!pending.current} error={error} keep={() => setMoveChoice(null)} retry={retryPending}
      confirm={() => { void command(moveChoice.plan, { kind: 'unlock-and-move', itemId: moveChoice.itemId, trackId: moveChoice.trackId }); }}
      review={() => { const latest = plans.find(plan => plan.id === moveChoice.plan.id); pending.current = null; setError(''); setMoveChoice(null); if (latest) review(latest, moveChoice.itemId); }} />}
  </>;
}

function PlanReview({ hostMemory, view, focusItem, focusRevision, latest, tracks, moveTracks, source, busy, paused, connected, error, pending, announcement, edit, continueItem, dependencies, update, refresh, close, action, retry, dependencyActions, canRetry }: {
  hostMemory?: HostMemoryStatus; view: Plan; focusItem?: string; focusRevision: number; latest?: Plan; tracks: LiveTrack[]; moveTracks: LiveTrack[]; source?: string; busy: boolean; paused: boolean; connected: boolean; error: string; pending: boolean; announcement: string; edit: (plan: Plan, itemId: string) => void; continueItem: (plan: Plan, itemId: string) => void; update: (plan: Plan) => void; dependencies: (plan: Plan, itemId: string) => void;
  refresh: () => void; close: () => void; action: (action: PlanAction) => void; retry: () => void;
  dependencyActions: (item: PlanItem) => ReactNode; canRetry: (item: PlanItem) => boolean;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const privateLabel = usePrivacyLabels(view.items.map(item => item.title));
  useLayoutEffect(() => {
    const previous = document.activeElement;
    dialog.current?.showModal();
    return () => { if (previous instanceof HTMLElement && previous.isConnected) previous.focus(); };
  }, []);
  useLayoutEffect(() => {
    if (focusItem) dialog.current?.querySelector<HTMLElement>(`[data-plan-item-id="${CSS.escape(focusItem)}"] h3`)?.focus();
  }, [focusItem, focusRevision]);
  const stale = latest?.rev !== view.rev;
  const deleted = view.items.filter(item => item.state === 'cut');
  const destination = (id: string) => tracks.find(track => track.id === id)?.label ?? (id.startsWith('new:') ? `New track ${id.slice(4)}` : id);
  return <dialog ref={dialog} className="plan-review" aria-labelledby="plan-review-title" onCancel={event => { event.preventDefault(); if (!busy) close(); }}>
    <header><h2 id="plan-review-title">Review plan</h2><button aria-label="Close plan review" disabled={busy} onClick={close}>×</button></header>
    <div className="plan-review-body">
      <p role="status" aria-atomic="true" className="sr-only"><PrivacyText text={announcement} /></p>
      {source && <p className="plan-source">Your request: <PrivacySummary text={source} limit={180} /></p>}
      <p><PrivacyText text={view.summary} /></p>
      {stale && <div className="plan-changed" role="status">The plan changed · review again <button disabled={busy} onClick={refresh}>Review current plan</button></div>}
      {view.notices.map((notice, index) => <p key={index}><PrivacyText text={notice} /></p>)}
      {view.rewrite && <ItemRevision plan={view} update={update} edit={edit} />}
      <ol className="plan-items">{view.items.filter(item => item.state !== 'cut').sort((a, b) => a.order - b.order).map(item => <li key={item.planItemId} data-plan-item-id={item.planItemId}>
        <div className="plan-item-heading"><h3 tabIndex={-1}><PrivacyText text={item.title} /></h3><span>{destination(item.trackRef)} · {item.state}</span>{item.state === 'planned' && <button disabled={busy || stale || pending || !connected} aria-label={`Cut ${privateLabel(item.title)}`} onClick={() => action({ kind: 'cut', itemId: item.planItemId })}>Cut</button>}{item.state === 'planned' && <ItemActions plan={view} item={item} tracks={moveTracks} disabled={busy || stale || pending || !connected} edit={() => edit(view, item.planItemId)} dependencies={() => dependencies(view, item.planItemId)} command={action} />}</div>
        <p className="plan-instruction"><PrivacyText text={item.instruction} /></p>
        {!!item.constraints.length && <div><h4>Constraints</h4><ul>{item.constraints.map((text, index) => <li key={index}><PrivacyText text={text} /></li>)}</ul></div>}
        <div><h4>Done when</h4><p><PrivacyText text={item.doneWhen} /></p></div>
        {!!item.after.length && <p className="plan-dependencies">After: {item.after.map((after, index) => <span key={after.id}>{index ? ', ' : ''}<PrivacyText text={view.items.find(other => other.planItemId === after.id)?.title ?? after.id} />{after.inherit ? ' · inherits files' : ' · order only'}</span>)}</p>}
        {item.detail && <p className="plan-item-state"><PrivacyText text={item.detail} /></p>}
        {dependencyActions(item)}
        {item.state === 'failed' && <button disabled={busy || stale || pending || !connected || !canRetry(item)} onClick={() => action({ kind: 'retry-item', itemId: item.planItemId })} aria-label={`Retry ${privateLabel(item.title)}’s item`}>Retry item</button>}
        {["running", "succeeded", "failed"].includes(item.state) && <button disabled={busy || stale || pending || !connected} onClick={() => continueItem(view, item.planItemId)} aria-label={`Add instruction to ${privateLabel(item.title)}`}>Add instruction</button>}
      </li>)}</ol>
      {!!deleted.length && <details className="deleted-plans"><summary>Deleted plans · {deleted.length}</summary>{deleted.map(item => <div key={item.planItemId}><PrivacyText text={item.title} /><button disabled={busy || stale || pending || !connected || view.state === 'single-job'} onClick={() => action({ kind: 'restore', itemId: item.planItemId })}>Restore</button></div>)}</details>}
      {view.generation.detail && <p><PrivacyText text={view.generation.detail} /></p>}
      {error && <div role="alert" className="plan-error"><span>{error}</span>{pending && <button disabled={busy} onClick={retry}>Retry plan action</button>}{pending && <button disabled={busy} onClick={refresh}>Reload plan</button>}</div>}
    </div>
    <footer><span>{planLabel(view, hostMemory)}</span><PlanFailureActions plan={view} tracks={tracks} disabled={busy || stale || pending || !connected} paused={paused} command={action} /><button disabled={busy} onClick={close}>Back to plans</button></footer>
  </dialog>;
}
