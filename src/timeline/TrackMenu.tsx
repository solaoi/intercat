import { useId, useRef, useState } from 'react';
import type { Track } from './model';

export function TrackMenu({ track, disabled, sayTo }: { track: Track; disabled: boolean; sayTo: (id: string) => void }) {
  const id = useId();
  const menu = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false);
  const [position, setPosition] = useState({ left: 0, top: 0 });
  return <>
    <button className="track-menu-button" aria-label={`Actions for Worker ${track.label}`} aria-haspopup="menu" aria-expanded={open}
      popoverTarget={id} onClick={event => {
        event.stopPropagation();
        const bounds = event.currentTarget.getBoundingClientRect();
        setPosition({ left: Math.max(8, Math.min(bounds.right - 180, window.innerWidth - 188)), top: Math.min(bounds.bottom + 4, window.innerHeight - 60) });
      }}>
      <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true"><circle cx="3" cy="8" r="1.2" /><circle cx="8" cy="8" r="1.2" /><circle cx="13" cy="8" r="1.2" /></svg>
    </button>
    <div id={id} ref={menu} popover="auto" role="menu" aria-label={`Worker ${track.label} actions`} className="track-menu" style={position}
      onToggle={event => setOpen(event.newState === 'open')} onClick={event => event.stopPropagation()}>
      <button role="menuitem" disabled={disabled} onClick={() => { menu.current?.hidePopover(); sayTo(track.id); }}>Say to {track.label}</button>
    </div>
  </>;
}
