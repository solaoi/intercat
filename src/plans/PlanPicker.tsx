import { useId, useLayoutEffect, useRef, useState } from 'react';
import { PrivacyText } from '../desk/PrivacyText';
import { PlanControls, planLabel } from './PlanControls';
import type { Plan, PlanAction } from './types';
import type { HostMemoryStatus, LiveTrack } from '../bridge';

export function PlanPicker({ hostMemory, plans, tracks, selected, busy, pending, connected, paused, choose, review, start, command }: {
  hostMemory?: HostMemoryStatus; plans: Plan[]; selected: string; busy: boolean; pending: boolean; connected: boolean; paused: boolean;
  tracks: Pick<LiveTrack, 'id' | 'label'>[];
  choose: (plan: Plan) => void; review: (plan: Plan) => void; start: (plan: Plan) => void; command: (plan: Plan, action: PlanAction) => void;
}) {
  const id = useId();
  const popup = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const [open, setOpen] = useState(false);
  const [shown, setShown] = useState<Plan[]>([]);
  const [position, setPosition] = useState({ left: 12, top: 12 });
  function close() { popup.current?.hidePopover(); trigger.current?.focus(); }
  function chooseAndRun(plan: Plan, action: () => void) { close(); choose(plan); action(); }
  useLayoutEffect(() => {
    if (!open) return;
    function place() {
      if (!trigger.current || !popup.current) return;
      const anchor = trigger.current.getBoundingClientRect(), box = popup.current.getBoundingClientRect();
      setPosition({ left: Math.max(12, Math.min(anchor.left, innerWidth - box.width - 12)), top: Math.max(12, Math.min(anchor.bottom + 8, innerHeight - box.height - 12)) });
    }
    place();
    const observer = new ResizeObserver(place);
    if (popup.current) observer.observe(popup.current);
    if (!popup.current?.contains(document.activeElement)) {
      popup.current?.querySelector<HTMLButtonElement>(`button[data-select-plan="${CSS.escape(selected)}"]`)?.focus();
    }
    window.addEventListener('resize', place);
    return () => { observer.disconnect(); window.removeEventListener('resize', place); };
  }, [open, shown]);
  const changed = shown.length !== plans.length || shown.some(old => plans.find(plan => plan.id === old.id)?.rev !== old.rev);
  return <>
    <button ref={trigger} className="plans-picker" aria-label="Plans" aria-haspopup="dialog" aria-expanded={open} aria-controls={id} popoverTarget={id}
      onClick={() => { if (!open) setShown(plans); }}>Plans <span aria-hidden="true">▾</span><small>{plans.length}</small></button>
    <div ref={popup} id={id} className="plans-menu" popover="auto" role="dialog" aria-label="All plans" style={position}
      onToggle={event => setOpen(event.newState === 'open')}
      onKeyDown={event => { if (event.key === 'Escape') { event.preventDefault(); close(); } }}>
      <header><h2>Plans</h2><span className="sr-only" role="status">{changed ? 'Plans changed' : ''}</span>
        <button className="plans-menu-refresh" disabled={!changed} onClick={() => setShown(plans)}>Refresh plans</button><button onClick={close} aria-label="Close plans">×</button></header>
      <ul>{shown.map((plan, index) => {
        const current = plans.find(value => value.id === plan.id);
        const stale = !current || current.rev !== plan.rev;
        return <li key={plan.id} data-plan-choice={plan.id}>
          <button className="plan-choice" data-select-plan={plan.id} aria-pressed={selected === plan.id} disabled={busy || pending || !current}
            onClick={() => chooseAndRun(current ?? plan, () => {})}>
            <span className="plan-choice-number">{index + 1}</span><span><PrivacyText text={plan.summary || 'Planning your request'} interactive={false} /></span>
          </button>
          <p>{planLabel(plan, hostMemory)}{stale && <span> · changed elsewhere</span>}</p>
          <PlanControls plan={plan} tracks={tracks} busy={busy} pending={pending} connected={connected} paused={paused} stale={stale}
            review={() => { if (current) chooseAndRun(current, () => review(current)); }} start={() => chooseAndRun(plan, () => start(plan))}
            command={action => chooseAndRun(plan, () => command(plan, action))} />
        </li>;
      })}</ul>
    </div>
  </>;
}
