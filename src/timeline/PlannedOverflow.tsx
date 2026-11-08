import { useId, useLayoutEffect, useRef, useState } from 'react';
import type { PlannedTurn } from './model';

export function PlannedOverflow({ items, track, selected, openItem, control }: {
  items: PlannedTurn[]; track: string; selected: string | null; openItem: (item: PlannedTurn) => void;
  control: (event: React.KeyboardEvent<HTMLButtonElement>, item: PlannedTurn) => void;
}) {
  const id=useId(), popup=useRef<HTMLDivElement>(null), trigger=useRef<HTMLButtonElement>(null);
  const [open,setOpen]=useState(false),[position,setPosition]=useState({left:8,top:8});
  const close=()=>{popup.current?.hidePopover();trigger.current?.focus();};
  useLayoutEffect(()=>{
    if(!open)return;
    const place=()=>{
      const anchor=trigger.current?.getBoundingClientRect(),box=popup.current?.getBoundingClientRect();
      if(anchor&&box)setPosition({left:Math.max(8,Math.min(anchor.right-box.width,innerWidth-box.width-8)),top:Math.max(8,Math.min(anchor.bottom+4,innerHeight-box.height-8))});
    };
    place();
    const buttons=[...(popup.current?.querySelectorAll<HTMLButtonElement>('button')??[])];
    if(!popup.current?.contains(document.activeElement)) (buttons.find(button=>button.dataset.itemId===selected)??buttons[0])?.focus();
    const observer=new ResizeObserver(place);if(popup.current)observer.observe(popup.current);
    window.addEventListener('resize',place);
    return()=>{observer.disconnect();window.removeEventListener('resize',place);};
  },[open]);
  return <>
    <button ref={trigger} className={`planned-more${items.some(item=>item.id===selected)?' selected':''}`} aria-haspopup="menu" aria-expanded={open} popoverTarget={id}
      aria-label={`Show ${items.length} more planned items for ${track}`} title={items.map(item=>item.label).join('\n')}
      onKeyDown={event=>{const item=items.find(item=>item.id===selected)??items[0];if(item)control(event,item);}}>+{items.length}</button>
    <div ref={popup} id={id} className="planned-overflow-menu" popover="auto" role="menu" aria-label={`More planned items for ${track}`} style={position}
      onToggle={event=>setOpen(event.newState==='open')}
      onKeyDown={event=>{
        if(event.key==='Escape'){event.preventDefault();close();return;}
        if(event.altKey||!['ArrowDown','ArrowUp','Home','End'].includes(event.key))return;
        event.preventDefault();const buttons=[...event.currentTarget.querySelectorAll<HTMLButtonElement>('button')],index=buttons.indexOf(document.activeElement as HTMLButtonElement);
        buttons[event.key==='Home'?0:event.key==='End'?buttons.length-1:(index+(event.key==='ArrowDown'?1:-1)+buttons.length)%buttons.length]?.focus();
      }}>
      {items.map(item=><button key={item.id} data-item-id={item.id} role="menuitem" aria-current={item.id===selected?'true':undefined}
        onClick={()=>{close();openItem(item);}} onKeyDown={event=>{control(event,item);if(event.defaultPrevented)close();}}><span>{item.label}</span><small>{item.after}</small></button>)}
    </div>
  </>;
}
