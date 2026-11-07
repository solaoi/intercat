import { useId, useLayoutEffect, useRef, useState, type ReactNode } from 'react';
import { NeedsYou, type AttentionCard } from './NeedsYou';

interface Props {
  finish: ReactNode;
  compact: boolean;
  folded: boolean;
  workerLabel: string | null;
  cards: AttentionCard[];
  returnedAt?: number;
  workers: ReactNode;
  expandWorkers: () => void;
  canDropFiles: boolean;
  dropFiles: (files: File[], trackId: string) => void;
}

/** Keep the live cards mounted when the stage takes over their space. */
export function StagePanels({ finish, compact, folded, workerLabel, cards, returnedAt, workers, expandWorkers, canDropFiles, dropFiles }: Props) {
  const id = useId();
  const panel = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const [open, setOpen] = useState(false);
  const place = () => {
    if (!panel.current || !trigger.current) return;
    const anchor = trigger.current.getBoundingClientRect();
    const width = Math.min(760, innerWidth - 32);
    panel.current.style.left = `${Math.max(16, Math.min(anchor.left, innerWidth - width - 16))}px`;
    panel.current.style.width = `${width}px`;
    panel.current.style.bottom = `${Math.max(16, innerHeight - anchor.top + 8)}px`;
    panel.current.style.maxHeight = `${Math.max(80, anchor.top - 24)}px`;
  };
  useLayoutEffect(() => { if (open && compact) place(); });
  useLayoutEffect(() => {
    if (!compact) setOpen(false);
    if (!open || !compact) return;
    const observer = new ResizeObserver(place);
    const desk = panel.current?.closest('.desk');
    if (desk) observer.observe(desk);
    window.addEventListener('resize', place);
    return () => { observer.disconnect(); window.removeEventListener('resize', place); };
  }, [compact, open]);

  function toggle() {
    const element = panel.current!;
    if (element.matches(':popover-open')) element.hidePopover();
    else {
      place();
      element.showPopover();
      element.querySelector<HTMLButtonElement>('button:not(:disabled)')?.focus();
    }
  }

  return <div className="stage-panels">
    {folded && <div className="stage-panel-controls">
      {workerLabel && <button className="stage-worker-summary" onClick={expandWorkers}>▸ {workerLabel}</button>}
      {compact && <button ref={trigger} disabled={!cards.length} className="stage-needs-you" aria-haspopup="dialog" aria-controls={id} aria-expanded={open} onClick={toggle}>Needs you · {cards.length}</button>}
      {compact && <><span className="stage-shortcut" title="Stage mode · ⇧⌘T">⇧⌘T</span><span className="stage-finish">{finish}</span></>}
    </div>}
    <div className="stage-workers" hidden={folded}>{workers}</div>
    <div ref={panel} id={id} className={compact ? 'stage-cards' : 'stage-cards-inline'} popover={compact ? 'auto' : undefined}
      role={compact ? 'dialog' : undefined} aria-label={compact ? 'Needs you cards' : undefined}
      onToggle={event => {
        if (event.target !== event.currentTarget) return;
        const shown = (event.nativeEvent as ToggleEvent).newState === 'open';
        setOpen(shown);
        if (!shown && compact && document.activeElement === document.body) trigger.current?.focus();
      }}>
      <NeedsYou cards={cards} returnedAt={returnedAt} active={!compact || open} canDropFiles={canDropFiles} dropFiles={dropFiles} />
    </div>
  </div>;
}
