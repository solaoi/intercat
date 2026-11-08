import { useId, useLayoutEffect, useRef, useState } from 'react';
import type { PlanDependencyLocation } from '../bridge';
import { PrivacyText, usePrivacyLabels } from '../desk/PrivacyText';
import type { Plan, PlanAction, PlanItem } from './types';
import { blockedDependencies } from './dependencies';

export function DependencyActions({ plan, item, plans, locations, tracks, disabled, command, open, change, edit, point, dismiss }: {
  plan: Plan; item: PlanItem; plans: Plan[]; locations: Record<string, PlanDependencyLocation>; disabled: boolean;
  command: (plan: Plan, action: PlanAction) => void; open: (location: PlanDependencyLocation) => void; change: () => void;
  tracks: { id: string; label: string }[]; edit: (plan: Plan, itemId: string) => void;
  point?: { left: number; top: number }; dismiss?: () => void;
}) {
  const id = useId(), menu = useRef<HTMLDivElement>(null), trigger = useRef<HTMLButtonElement>(null);
  const anchor = useRef(point ?? { left: 8, top: 8 });
  const [shown, setShown] = useState(false);
  const [page, setPage] = useState<'main' | 'tracks'>('main');
  const [captured, capture] = useState<{ plan: Plan; parents: PlanDependencyLocation[]; owners: Plan[] } | null>(null);
  const [position, setPosition] = useState(point ?? { left: 8, top: 8 });
  const parents = blockedDependencies(plan, item, plans, locations);
  const label = usePrivacyLabels([item.title, ...parents.map(parent => parent.title)]);
  const remember = () => { setPage('main'); capture({ plan, parents, owners: plans }); };
  const close = () => { menu.current?.hidePopover(); trigger.current?.focus(); };
  useLayoutEffect(() => {
    if (point) { anchor.current = point; remember(); menu.current?.showPopover(); }
  }, [point]);
  useLayoutEffect(() => {
    if (shown && !menu.current?.contains(document.activeElement)) menu.current?.querySelector<HTMLButtonElement>('button:not(:disabled)')?.focus();
  }, [shown, page]);
  useLayoutEffect(() => {
    if (!shown) return;
    const place = () => {
      const box = menu.current?.getBoundingClientRect(); if (!box) return;
      const next = { left: Math.max(8, Math.min(anchor.current.left, innerWidth - box.width - 8)), top: Math.max(8, Math.min(anchor.current.top, innerHeight - box.height - 8)) };
      setPosition(old => old.left === next.left && old.top === next.top ? old : next);
    };
    place(); const observer = new ResizeObserver(place); if (menu.current) observer.observe(menu.current);
    window.addEventListener('resize', place);
    return () => { observer.disconnect(); window.removeEventListener('resize', place); };
  }, [shown, page]);
  const selected = captured?.parents ?? parents;
  const stale = !!captured && (item.state !== 'planned' || captured.plan.rev !== plan.rev || selected.some(parent => {
    const now = locations[parent.id], owner = plans.find(plan => plan.id === parent.planId);
    return !now || now.state !== parent.state || now.clipId !== parent.clipId || now.retryable !== parent.retryable || now.unavailable !== parent.unavailable
      || owner?.rev !== captured.owners.find(plan => plan.id === parent.planId)?.rev;
  }));
  if (!parents.length && !shown && !point) return null;
  return <>
    {!point && <button ref={trigger} className="plan-dependency-action" aria-label={`Resolve dependency for ${label(item.title)}`} aria-haspopup="menu" aria-expanded={shown} popoverTarget={id} onClick={event => {
      remember(); const box = event.currentTarget.getBoundingClientRect(); anchor.current = { left: box.left, top: box.bottom + 4 }; setPosition(anchor.current);
    }}>Resolve dependency…</button>}
    <div ref={menu} id={id} popover="auto" role="menu" aria-label={`Dependency actions for ${label(item.title)}`} className="plan-item-menu plan-dependency-menu"
      style={position}
      onToggle={event => { const open = event.newState === 'open'; setShown(open); if (!open) dismiss?.(); }}
      onKeyDown={event => {
        if (event.key === 'Escape') { event.preventDefault(); close(); }
        if (!['ArrowDown','ArrowUp','Home','End'].includes(event.key)) return;
        event.preventDefault(); const buttons = [...event.currentTarget.querySelectorAll<HTMLButtonElement>('button:not(:disabled)')];
        const index = buttons.indexOf(document.activeElement as HTMLButtonElement);
        buttons[event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : (index + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length]?.focus();
      }}>
      {page === 'main' ? <>{selected.map(parent => {
        const owner = captured?.owners.find(plan => plan.id === parent.planId);
        return <div key={parent.id} role="group" aria-label={label(parent.title)}>
          <p><PrivacyText text={parent.title} /> · {parent.unavailable ? 'unavailable' : parent.state === 'cut' ? 'was cut' : parent.unclear ? 'result unclear' : 'didn’t finish'}</p>
          {parent.state === 'cut' ? <button role="menuitem" disabled={disabled || stale || !owner || !parent.itemId} onClick={() => { close(); if (owner && parent.itemId) command(owner, { kind: 'restore', itemId: parent.itemId }); }}>Restore {label(parent.title)}</button>
            : <><button role="menuitem" disabled={disabled || !parent.clipId} onClick={() => { close(); open(parent); }}>Open {label(parent.title)}</button>
              {owner && parent.itemId ? <button role="menuitem" disabled={disabled || stale || !parent.retryable} onClick={() => { close(); command(owner, { kind: 'retry-item', itemId: parent.itemId! }); }}>Retry {label(parent.title)}’s item</button>
                : <button role="menuitem" disabled={disabled || stale || !parent.retryable || !parent.clipId} title={!parent.retryable ? 'Retry needs the original request and its saved checkpoint.' : undefined} onClick={() => { close(); if (parent.clipId) command(captured?.plan ?? plan, { kind: 'retry-dependency', itemId: item.planItemId, dependencyId: parent.id, clipId: parent.clipId }); }}>Retry {label(parent.title)}’s item</button>}</>}
          <button role="menuitem" disabled={disabled || stale} onClick={() => { close(); command(captured?.plan ?? plan, { kind: 'start-without', itemId: item.planItemId, dependencyId: parent.id }); }}>Start anyway without {label(parent.title)}</button>
        </div>;
      })}
      <button role="menuitem" disabled={disabled || stale} onClick={() => { close(); change(); }}>Change dependency…</button>
      <button role="menuitem" disabled={disabled || stale} onClick={() => { close(); command(captured?.plan ?? plan, { kind: 'move', itemId: item.planItemId, direction: 'earlier' }); }}>Move earlier</button>
      <button role="menuitem" disabled={disabled || stale} onClick={() => { close(); command(captured?.plan ?? plan, { kind: 'move', itemId: item.planItemId, direction: 'later' }); }}>Move later</button>
      <button role="menuitem" disabled={disabled || stale} onClick={() => setPage('tracks')}>Move to track…</button>
      <button role="menuitem" disabled={disabled || stale} onClick={() => { close(); edit(captured?.plan ?? plan, item.planItemId); }}>Revise instructions…</button>
      </> : <><button role="menuitem" onClick={() => setPage('main')}>Back</button>{tracks.filter(track => track.id !== item.trackRef).map(track =>
        <button key={track.id} role="menuitem" disabled={disabled || stale} onClick={() => { close(); command(captured?.plan ?? plan, { kind: 'move-to', itemId: item.planItemId, trackId: track.id }); }}>Move to {track.label}</button>)}</>}
      {stale && <p role="status">The dependency changed. Close and reopen these actions.</p>}
    </div>
  </>;
}
