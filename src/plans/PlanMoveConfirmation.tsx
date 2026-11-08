import { useId, useLayoutEffect, useRef } from 'react';
import { PrivacyText } from '../desk/PrivacyText';
import type { Plan } from './types';

export interface MoveChoice { plan: Plan; itemId: string; trackId: string }

export function PlanMoveConfirmation({ choice, latest, tracks, busy, connected, targetAvailable, pending, error, confirm, keep, review, retry }: {
  choice: MoveChoice; latest?: Plan; tracks: { id: string; label: string }[];
  busy: boolean; connected: boolean; targetAvailable: boolean; pending: boolean; error: string;
  confirm: () => void; keep: () => void; review: () => void; retry: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null), titleId = useId();
  useLayoutEffect(() => {
    const previous = document.activeElement;
    dialog.current?.showModal();
    return () => { if (previous instanceof HTMLElement && previous.isConnected) previous.focus(); };
  }, []);
  const item = choice.plan.items.find(item => item.planItemId === choice.itemId);
  const currentItem = latest?.items.find(item => item.planItemId === choice.itemId);
  const stale = !latest || latest.rev !== choice.plan.rev || latest.lockedTrack !== choice.plan.lockedTrack
    || currentItem?.state !== 'planned' || !targetAvailable;
  const from = tracks.find(track => track.id === choice.plan.lockedTrack)?.label ?? 'the current track';
  const to = tracks.find(track => track.id === choice.trackId)?.label ?? 'the selected track';
  return <dialog ref={dialog} className="plan-review plan-move-confirmation" aria-labelledby={titleId}
    onCancel={event => { event.preventDefault(); if (!busy && !pending) keep(); }}>
    <header><h2 id={titleId}>Unlock and split?</h2><button aria-label="Keep the current plan" disabled={busy || pending} onClick={keep}>×</button></header>
    <div className="plan-review-body">
      <p>Move <PrivacyText text={item?.title ?? 'this item'} /> from {from} to {to}?</p>
      <p>This removes the plan’s destination lock. Other items keep their current tracks.</p>
      {stale && <p role="status">The plan changed · review again</p>}
      {error && <div className="plan-error" role="alert"><span>{error}</span>{pending && <button disabled={busy} onClick={retry}>Retry plan action</button>}</div>}
    </div>
    <footer>
      {(stale || pending) && <button disabled={busy} onClick={review}>Review current plan</button>}
      <button disabled={busy || pending || stale || !connected} onClick={confirm}>Yes</button>
      <button disabled={busy || pending} onClick={keep}>Keep on {from}</button>
    </footer>
  </dialog>;
}
