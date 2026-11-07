import { useId, useLayoutEffect, useRef, useState } from 'react';
import type { TimelineMark } from '../bridge';
import type { Track } from './model';
import { markLabel, type MarkGroup } from './inputMarkLayout';

interface Props { groups: MarkGroup[]; tracks: Track[]; open: (mark: TimelineMark) => void }
export function InputMarks({ groups, tracks, open }: Props) {
  const id = useId();
  const [expandedId, setExpandedId] = useState<string | null>(null);
  const expanded = groups.find(group => group.id === expandedId) ?? null;
  const menu = useRef<HTMLDivElement>(null);
  const buttons = useRef(new Map<string, HTMLButtonElement>());
  const [position, setPosition] = useState({ left: 8, top: 8 });
  useLayoutEffect(() => {
    if (!expanded || !menu.current) return;
    const element = menu.current;
    element.showPopover();
    const place = () => {
      const anchor = buttons.current.get(expanded.id)?.getBoundingClientRect();
      const next = {
        left: Math.max(8, Math.min(anchor?.left ?? 8, innerWidth - element.offsetWidth - 8)),
        top: Math.max(8, Math.min(anchor?.bottom ?? 8, innerHeight - element.offsetHeight - 8)),
      };
      setPosition(previous => previous.left === next.left && previous.top === next.top ? previous : next);
    };
    place();
    const observer = new ResizeObserver(place);
    observer.observe(element);
    window.addEventListener('resize', place);
    element.querySelector<HTMLButtonElement>('button')?.focus();
    return () => {
      observer.disconnect(); window.removeEventListener('resize', place);
      if (element.matches(':popover-open')) element.hidePopover();
    };
  }, [expanded?.id]);
  const describe = (mark: TimelineMark) => `${mark.text} · ${markLabel(mark, tracks)}`;
  return <>
    <div className="input-mark-actions" aria-label="Timeline requests">
      {groups.map((group, index) => <button key={group.id} ref={element => { if (element) buttons.current.set(group.id, element); else buttons.current.delete(group.id); }}
        style={{ left: group.x - 12 }} data-utterance-id={group.marks.length === 1 ? group.marks[0].inputId ?? undefined : undefined}
        title={group.marks.length === 1 ? describe(group.marks[0]) : `${group.marks.length} requests at this time`}
        aria-label={group.marks.length === 1 ? describe(group.marks[0]) : `${group.marks.length} requests at this time`}
        aria-haspopup={group.marks.length > 1 || group.marks[0].inputId ? 'dialog' : undefined} aria-controls={group.marks.length > 1 ? id : undefined}
        onClick={() => group.marks.length === 1 ? open(group.marks[0]) : setExpandedId(group.id)}
        onKeyDown={event => {
          const next = event.key === 'ArrowRight' ? Math.min(index + 1, groups.length - 1)
            : event.key === 'ArrowLeft' ? Math.max(index - 1, 0) : event.key === 'Home' ? 0 : event.key === 'End' ? groups.length - 1 : null;
          if (next !== null) { event.preventDefault(); event.stopPropagation(); buttons.current.get(groups[next].id)?.focus(); }
          if (event.ctrlKey && event.key === 'Enter') { event.preventDefault(); event.stopPropagation(); setExpandedId(group.id); }
        }} />)}
    </div>
    {expanded && <div id={id} ref={menu} popover="auto" className="input-mark-menu" role="dialog" aria-label="Requests at this time" style={position}
      onToggle={event => { if ((event.nativeEvent as ToggleEvent).newState === 'closed') setExpandedId(null); }}>
      <strong>{expanded.marks.length === 1 ? 'Request' : `${expanded.marks.length} requests`}</strong>
      {expanded.marks.map(mark => <button key={mark.id} onClick={() => { setExpandedId(null); open(mark); }}>
        <span>{mark.text}</span><small>{markLabel(mark, tracks)}</small>
      </button>)}
    </div>}
  </>;
}
