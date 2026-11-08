import { useEffect, useId, useRef, useState } from 'react';
import { PrivacyText, usePrivacyLabels } from '../desk/PrivacyText';
import type { Plan, PlanAction, PlanItem } from './types';

export function ItemActions({ plan, item, tracks, disabled, command, edit, dependencies, point, dismiss }: {
  plan: Plan; item: PlanItem; tracks: { id: string; label: string }[]; disabled: boolean;
  command: (action: PlanAction) => void; edit: () => void; dependencies: () => void; point?: { left: number; top: number }; dismiss?: () => void;
}) {
  const id = useId();
  const menu = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const [page, setPage] = useState<'main' | 'tracks'>('main');
  const [position, setPosition] = useState(point ?? { left: 8, top: 8 });
  const [open, setOpen] = useState(false);
  const label = usePrivacyLabels([item.title, ...plan.items.map(item => item.title)]);
  const close = () => { menu.current?.hidePopover(); trigger.current?.focus(); };
  const apply = (action: PlanAction) => { close(); command(action); };
  useEffect(() => {
    if (point) { setPosition(point); menu.current?.showPopover(); }
  }, [point]);
  useEffect(() => { if (open) menu.current?.querySelector<HTMLButtonElement>('button:not(:disabled)')?.focus(); }, [open, page]);
  const style = { left: Math.max(8, Math.min(position.left, window.innerWidth - 280)), top: Math.max(8, Math.min(position.top, window.innerHeight - 280)) };
  return <>
    {!point && <button ref={trigger} aria-label={`Actions for ${label(item.title)}`} aria-haspopup="menu" aria-expanded={open} popoverTarget={id} onClick={event => {
      const box = event.currentTarget.getBoundingClientRect(); setPosition({ left: box.left, top: box.bottom + 4 }); setPage('main');
    }}>•••</button>}
    <div ref={menu} id={id} popover="auto" role="menu" aria-label={`Plan item actions for ${label(item.title)}`} className="plan-item-menu" style={style}
      onToggle={event => { const active = event.newState === 'open'; setOpen(active); if (!active) dismiss?.(); }}
      onKeyDown={event => {
        if (event.key === 'Escape') { event.preventDefault(); close(); }
        if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
        event.preventDefault(); const buttons = [...event.currentTarget.querySelectorAll<HTMLButtonElement>('button:not(:disabled)')];
        const index = buttons.indexOf(document.activeElement as HTMLButtonElement);
        const next = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : (index + (event.key === 'ArrowDown' ? 1 : -1) + buttons.length) % buttons.length;
        buttons[next]?.focus();
      }}>
      {page === 'main' ? <>
        <button role="menuitem" disabled={disabled} onClick={() => apply({ kind: 'move', itemId: item.planItemId, direction: 'earlier' })}>Move earlier</button>
        <button role="menuitem" disabled={disabled} onClick={() => apply({ kind: 'move', itemId: item.planItemId, direction: 'later' })}>Move later</button>
        <button role="menuitem" disabled={disabled} onClick={() => setPage('tracks')}>Move to track…</button>
        <button role="menuitem" disabled={disabled} onClick={() => { close(); dependencies(); }}>Change dependency…</button>
        <button role="menuitem" disabled={disabled} onClick={() => { close(); edit(); }}>Revise instructions…</button>
      </> : <>
        <button role="menuitem" onClick={() => setPage('main')}>Back</button>
        {tracks.filter(track => track.id !== item.trackRef).map(track => <button key={track.id} role="menuitem" disabled={disabled} onClick={() => apply({ kind: 'move-to', itemId: item.planItemId, trackId: track.id })}>Move to {track.label}</button>)}
      </>}
    </div>
  </>;
}
