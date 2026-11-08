import { useId, useRef } from 'react';

export function PlanOnlyButton({ disabled, submit }: { disabled: boolean; submit: () => void }) {
  const id = useId();
  const menu = useRef<HTMLDivElement>(null);
  return <>
    <button className="send-options" aria-label="More send options" aria-haspopup="menu" disabled={disabled} onClick={event => {
      const rect = event.currentTarget.getBoundingClientRect();
      const element = menu.current!;
      element.style.left = `${Math.max(8, Math.min(rect.right - 215, innerWidth - 223))}px`;
      element.style.top = `${Math.max(8, rect.top - 57)}px`;
      element.togglePopover();
    }}>▾</button>
    <div ref={menu} id={id} popover="auto" className="send-options-menu" role="menu">
      <button role="menuitem" disabled={disabled} onClick={() => { menu.current?.hidePopover(); submit(); }}>Plan only <kbd>⌘⇧P</kbd></button>
    </div>
  </>;
}
