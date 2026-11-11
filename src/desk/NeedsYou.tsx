import { createContext, useEffect, useLayoutEffect, useRef, useState, type ReactNode } from 'react';

import { usePrivacyLabels } from './PrivacyText';
import { useCardCapacity } from './useCardCapacity';

export const AttentionActive = createContext(true);

export interface AttentionCard {
  id: string;
  kind: 'stopped' | 'blocked' | 'question' | 'proposal' | 'assumption';
  title: string;
  version?: string | number;
  collapseAt?: number;
  createdAt?: number;
  destination?: { trackId: string; label: string };
  content: ReactNode;
}

interface Layout {
  order: string[];
  visible: string[];
}

const priority = { assumption: -2, proposal: -1, question: 0, blocked: 1, stopped: 2 };

function arrange(cards: AttentionCard[], expanded: boolean, capacity: number, now: number, returnedAt: number): Layout {
  const ordered = cards.toSorted((left, right) => priority[left.kind] - priority[right.kind]);
  const waiting = ordered.some(card => card.kind === 'blocked' || card.kind === 'stopped');
  const eligible = ordered.filter(card => !(waiting && card.kind === 'question')
    && !(card.kind === 'proposal' && ((card.collapseAt ?? Infinity) <= now || (card.createdAt ?? Infinity) <= returnedAt)));
  const visible = expanded ? ordered : eligible.toSorted((left, right) => priority[right.kind] - priority[left.kind]).slice(0, capacity);
  return {
    order: ordered.map(card => card.id),
    visible: visible.map(card => card.id),
  };
}

export function NeedsYou({ cards, active = true, returnedAt = 0, canDropFiles = false, dropFiles }: { cards: AttentionCard[]; active?: boolean; returnedAt?: number; canDropFiles?: boolean; dropFiles?: (files: File[], trackId: string) => void }) {
  const label = usePrivacyLabels(cards.map(card => card.title));
  const [expanded, setExpanded] = useState(false);
  const [dropCard, setDropCard] = useState<string | null>(null);
  const [held, setHeld] = useState<Layout | null>(null);
  const [now, setNow] = useState(Date.now);
  const region = useRef<HTMLElement>(null);
  const previous = useRef(new Map<string, { card: AttentionCard; height: number }>());
  const pointerInside = useRef(false);
  const dragInside = useRef(false);
  const focusInside = useRef(false);
  const focusedCard = useRef<string | null>(null);
  const releaseTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const capacity = useCardCapacity(region, arrange(cards, false, 2, now, returnedAt).visible,
    cards.map(card => `${card.id}:${card.version ?? ''}`).join('\n'), active);
  const live = new Map(cards.map(card => [card.id, card]));
  const desired = arrange(cards, expanded, capacity, now, returnedAt);
  const layout = held ?? desired;
  const order = [...layout.order, ...desired.order.filter(id => !layout.order.includes(id))];
  const entries = order.map(id => {
    const current = live.get(id);
    const old = previous.current.get(id)?.card;
    if (held && current && old && current.version !== old.version) return old;
    return current ?? old;
  }).filter((card): card is AttentionCard => !!card);
  const currentIds = new Set(entries.filter(card => live.has(card.id) && live.get(card.id)?.version === card.version).map(card => card.id));
  const collapsed = cards.filter(card => !layout.visible.includes(card.id));
  const showMore = expanded || layout.order.length > layout.visible.length;
  const arrivals = held ? cards.filter(card => (!held.order.includes(card.id) || !currentIds.has(card.id)) && (card.kind === 'blocked' || card.kind === 'stopped')).length : 0;
  const visibleOrder = layout.order.filter(id => layout.visible.includes(id)).join('\n');

  useEffect(() => {
    const next = Math.min(...cards.map(card => card.collapseAt ?? Infinity).filter(at => at > now));
    if (!Number.isFinite(next)) return;
    const timer = setTimeout(() => setNow(Date.now()), Math.max(0, next - Date.now()));
    return () => clearTimeout(timer);
  }, [cards, now]);

  useLayoutEffect(() => {
    if (held || !active) return;
    const scroll = region.current?.closest('.desk-scroll, .stage-cards');
    const keepLastCardVisible = () => {
      const focused = document.activeElement;
      if (focused instanceof HTMLElement && scroll?.contains(focused) && !region.current?.contains(focused)) return;
      const last = visibleOrder.split('\n').at(-1);
      const card = [...(region.current?.querySelectorAll<HTMLElement>('[data-attention-card]') ?? [])]
        .find(element => element.dataset.attentionCard === last);
      const actions = card?.querySelector<HTMLElement>('[data-card-actions]');
      (actions?.checkVisibility() ? actions : card)?.scrollIntoView({ block: 'end' });
    };
    keepLastCardVisible();
    const observer = new ResizeObserver(keepLastCardVisible);
    if (region.current) observer.observe(region.current);
    if (scroll) observer.observe(scroll);
    return () => observer.disconnect();
  }, [visibleOrder, held, active]);

  useLayoutEffect(() => {
    const heights = new Map([...(region.current?.querySelectorAll<HTMLElement>('[data-attention-card]') ?? [])]
      .map(element => [element.dataset.attentionCard, element.getBoundingClientRect().height]));
    previous.current = new Map(entries.map(card => [card.id, {
      card,
      height: currentIds.has(card.id) && (heights.get(card.id) ?? 0) > 0
        ? heights.get(card.id)! : previous.current.get(card.id)?.height ?? 0,
    }]));
  });
  useEffect(() => { if (!cards.length && !held) setExpanded(false); }, [cards.length, held]);
  useEffect(() => {
    if (active) return;
    if (releaseTimer.current) clearTimeout(releaseTimer.current);
    pointerInside.current = false;
    dragInside.current = false;
    setDropCard(null);
    focusInside.current = false;
    focusedCard.current = null;
    setHeld(null);
  }, [active]);
  useEffect(() => () => { if (releaseTimer.current) clearTimeout(releaseTimer.current); }, []);
  useEffect(() => {
    // Making the answered card inert can retarget its pointer events before
    // React sees a leave. Observe the next actual pointer target so that this
    // stale hover cannot hold an answered card indefinitely.
    const pointerMoved = (event: PointerEvent) => {
      if (!pointerInside.current || event.target instanceof Node && region.current?.contains(event.target)) return;
      pointerInside.current = false;
      focusInside.current = !!region.current?.contains(document.activeElement);
      release();
    };
    window.addEventListener('pointermove', pointerMoved, true);
    return () => window.removeEventListener('pointermove', pointerMoved, true);
  }, []);
  useEffect(() => {
    const keepFocusedControlVisible = () => {
      const focused = document.activeElement;
      if (focused instanceof HTMLElement && region.current?.contains(focused)) {
        focused.scrollIntoView({ block: 'nearest', inline: 'nearest' });
      }
    };
    window.addEventListener('resize', keepFocusedControlVisible);
    return () => window.removeEventListener('resize', keepFocusedControlVisible);
  }, []);
  useLayoutEffect(() => {
    if (!active || !focusedCard.current || currentIds.has(focusedCard.current)) return;
    const candidates = region.current?.querySelectorAll<HTMLElement>('[data-attention-card]:not([hidden]):not([inert]) button:not(:disabled), [data-attention-card]:not([hidden]):not([inert]) [tabindex="0"]');
    const next = [...(candidates ?? [])].find(element => element.checkVisibility());
    focusedCard.current = null;
    focusInside.current = !!next;
    (next ?? document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]'))?.focus();
    if (!next) release();
  });

  function lock() {
    if (releaseTimer.current) clearTimeout(releaseTimer.current);
    setHeld(current => current ?? desired);
  }
  function release() {
    if (pointerInside.current || dragInside.current || focusInside.current) return;
    if (releaseTimer.current) clearTimeout(releaseTimer.current);
    releaseTimer.current = setTimeout(() => { setHeld(null); }, 1500);
  }
  function toggle() {
    const next = !expanded;
    setExpanded(next);
    setHeld(arrange(cards, next, capacity, now, returnedAt));
  }

  if (!entries.length) return null;
  return <section ref={region} className="needs-you" aria-label="Needs you"
    onPointerEnter={() => { pointerInside.current = true; lock(); }}
    onPointerLeave={() => { pointerInside.current = false; release(); }}
    onFocusCapture={event => {
      focusInside.current = true;
      focusedCard.current = (event.target as HTMLElement).closest<HTMLElement>('[data-attention-card]')?.dataset.attentionCard ?? null;
      lock();
    }}
    onBlurCapture={event => {
      if (event.relatedTarget instanceof Node && event.currentTarget.contains(event.relatedTarget)) return;
      focusInside.current = false;
      focusedCard.current = null;
      release();
    }}>
    <div className="needs-you-heading"><span>Needs you · {cards.length}</span>{arrivals > 0 && <span role="status">+{arrivals} new</span>}</div>
    {showMore && <button className="needs-you-more" aria-expanded={expanded} onClick={toggle}
      disabled={!expanded && !collapsed.length} aria-hidden={!expanded && !collapsed.length || undefined}
      style={!expanded && !collapsed.length ? { visibility: 'hidden' } : undefined}
      title={collapsed.map(card => label(card.title)).join(' · ')}>
      {expanded ? 'Show fewer cards' : `+${collapsed.length} more · ${collapsed[0] ? label(collapsed[0].title) : ''}`}
    </button>}
    {entries.map(card => {
      const stale = !currentIds.has(card.id);
      const destination = active && !stale && canDropFiles && card.destination?.label ? card.destination : undefined;
      return <div key={card.id} data-attention-card={card.id} className={`attention-card${stale ? ' answered' : ''}${destination && dropCard === card.id ? ' file-drop-active' : ''}`}
        hidden={!layout.visible.includes(card.id)} inert={stale} style={stale ? { height: previous.current.get(card.id)?.height } : undefined}
        onDragOver={event => {
          if (!event.dataTransfer.types.includes('Files')) return;
          event.preventDefault(); event.stopPropagation();
          dragInside.current = true; lock();
          event.dataTransfer.dropEffect = destination ? 'copy' : 'none';
          setDropCard(destination ? card.id : null);
        }}
        onDragLeave={event => { if (!(event.relatedTarget instanceof Node) || !event.currentTarget.contains(event.relatedTarget)) { setDropCard(null); dragInside.current = false; release(); } }}
        onDrop={event => {
          if (!event.dataTransfer.types.includes('Files')) return;
          event.preventDefault(); event.stopPropagation(); setDropCard(null);
          dragInside.current = false; release();
          if (destination) dropFiles?.(Array.from(event.dataTransfer.files), destination.trackId);
        }}>
        {destination && dropCard === card.id && <div className="card-file-drop"><span>Drop to attach · next words to Worker {destination.label}</span></div>}
        {stale ? <span className="attention-state" role="status">{live.has(card.id) ? 'changed elsewhere' : 'answered elsewhere'}</span>
          : card.kind === 'assumption' ? <span className="attention-state">assumed</span>
          : card.kind === 'proposal' ? <span className="attention-state">not started</span>
          : card.kind !== 'question' && <span className="attention-state">needs an answer</span>}
        <AttentionActive value={active && !stale}><fieldset disabled={stale} aria-hidden={stale || undefined}>{card.content}</fieldset></AttentionActive>
      </div>;
    })}
  </section>;
}
