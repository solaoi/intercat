import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { PrivacyText, usePrivacyText } from './PrivacyText';
import type { EditHistory, EditorEntry, EditorNotice } from '../bridge';
import { textLength } from './RequestTitle';
import { AwayDetails } from './AwayDetails';

export function entryFallback(scene: string): string {
  if (scene === 'away') return 'While you were away.';
  return ({ fork: 'New track created.', trim: 'Turn stopped. Checkpoint saved.', reconnecting: 'Reconnecting. See the worker card.', lost: 'Worker session lost. See the card.', stopped: 'Approval needed. See the card.', completed: 'Worker turn completed.', commit: 'Committed. Push when you like.', confirmation: 'I’ll ask before placing each interpretation.', automatic: 'Back to placing on my own.', takeback: 'Request taken back.', masked: 'Private details masked.', assumed: 'An assumption was recorded.', 'assumption-change': 'An assumption correction was requested.', reply: 'Reply draft opened.' })[scene] ?? 'Editor update. See the details.';
}
const duration = (line: string) => Math.min(12_000, 4000 + textLength(line) * 50);
function expiry(entry: EditorEntry) {
  return entry.expiresAt === null ? Infinity : Math.min(entry.expiresAt, entry.createdAt + duration(entry.line));
}

export function EditorLine({ entries: speech, waitingTracks, history, undo, disabled, character, hidden, away }: {
  entries: EditorEntry[]; waitingTracks: string[]; history?: EditHistory; undo: () => void; disabled: boolean;
  character: 'speak' | 'mute' | null; hidden: boolean; away: EditorNotice | null;
}) {
  const entries = useMemo(() => [...speech, ...(away ? [away] : [])], [speech, away]);
  const [now, setNow] = useState(Date.now());
  const [opened, setOpened] = useState<EditorNotice | null>(null);
  const [fits, setFits] = useState(false);
  const [displayExpiry, setDisplayExpiry] = useState<Record<number, number>>({});
  const label = useRef<HTMLParagraphElement>(null);
  const popup = useRef<HTMLDivElement>(null);
  const more = useRef<HTMLButtonElement>(null);
  const hadOpened = useRef(false);
  const recent = history?.undo?.label === 'Take back' ? history.undo : undefined;
  const notice = recent && now < recent.at + 5000 ? {
    id: -1, trackId: null, inputId: null, scene: 'takeback', title: 'Request taken back',
    line: `Took back “${recent.text}”`, detail: `Took back “${recent.text}”. Nothing further will be sent for this request.`,
    createdAt: recent.at, expiresAt: recent.at + 5000,
  } satisfies EditorEntry : undefined;
  const live = entries.filter(entry => !hidden && (character === 'speak' || entry.scene === 'away') && entry.scene !== 'interpretation' && (displayExpiry[entry.id] ?? expiry(entry)) > now);
  const transient = [...live.filter(entry => entry.expiresAt !== null), ...(notice ? [notice] : [])]
    .sort((a, b) => b.createdAt - a.createdAt || b.id - a.id)[0];
  const persistent = live.filter(entry => entry.expiresAt === null)
    .sort((a, b) => Number(waitingTracks.includes(b.trackId ?? '')) - Number(waitingTracks.includes(a.trackId ?? '')) || b.id - a.id)[0];
  const current: EditorNotice | undefined = opened ?? (hidden ? undefined : transient ?? persistent);
  const line = current?.line ?? '';
  const privacy = usePrivacyText(line);
  const shownLine = privacy.view?.text ?? '';
  useEffect(() => {
    if (hidden || character !== 'speak' && opened && !['away', 'takeback'].includes(opened.scene)) setOpened(null);
  }, [hidden, character, opened]);
  useEffect(() => { setNow(Date.now()); }, [entries, recent?.entryId, recent?.at, opened, hidden, character]);
  useEffect(() => {
    const deadlines = entries.filter(entry => entry.scene !== 'interpretation').map(entry => displayExpiry[entry.id] ?? expiry(entry));
    if (recent) deadlines.push(recent.at + 5000);
    const next = Math.min(...deadlines.filter(deadline => deadline > Date.now()));
    if (!Number.isFinite(next)) return;
    const timer = setTimeout(() => {
      if (!opened && document.activeElement?.closest('.editor-line')) document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus();
      setNow(Date.now());
    }, Math.max(0, next - Date.now()) + 1);
    return () => clearTimeout(timer);
  }, [entries, recent?.entryId, recent?.at, now, opened, displayExpiry]);
  useLayoutEffect(() => {
    const element = label.current;
    if (!element) return;
    const measure = () => {
      const context = document.createElement('canvas').getContext('2d');
      if (!context) { setFits(false); return; }
      context.font = getComputedStyle(element).font;
      const fits = !!shownLine && textLength(shownLine) <= 72 && !/[\r\n]/.test(shownLine) && context.measureText(shownLine).width <= element.clientWidth;
      setFits(fits);
      if (current && current.expiresAt !== null && current.scene !== 'takeback') {
        const until = Math.min(current.expiresAt, current.createdAt + duration(fits ? shownLine : entryFallback(current.scene)));
        setDisplayExpiry(previous => previous[current.id] === until ? previous : {
          ...Object.fromEntries(Object.entries(previous).filter(([id]) => entries.some(entry => entry.id === Number(id)))),
          [current.id]: until,
        });
      }
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, [shownLine, current, entries]);
  useEffect(() => {
    if (!opened) return;
    const element = popup.current!;
    const trigger = more.current;
    element.showPopover();
    const position = () => {
      const anchor = more.current?.closest('.editor-line')?.getBoundingClientRect();
      if (!anchor) return;
      const bounds = element.getBoundingClientRect();
      element.style.margin = '0';
      element.style.left = `${Math.max(8, Math.min(anchor.left, innerWidth - bounds.width - 8))}px`;
      element.style.top = `${Math.max(8, Math.min(anchor.top - bounds.height - 8, innerHeight - bounds.height - 8))}px`;
    };
    position();
    const observer = new ResizeObserver(position);
    observer.observe(element);
    window.addEventListener('resize', position);
    element.focus();
    const close = (event: Event) => { if ((event as ToggleEvent).newState === 'closed') setOpened(null); };
    element.addEventListener('toggle', close);
    return () => {
      observer.disconnect(); window.removeEventListener('resize', position);
      element.removeEventListener('toggle', close); element.hidePopover();
      if (trigger?.isConnected) trigger.focus();
    };
  }, [opened]);
  useLayoutEffect(() => {
    if (opened) { hadOpened.current = true; return; }
    if (!hadOpened.current) return;
    hadOpened.current = false;
    if (current) more.current?.focus();
    else document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus();
  }, [opened, current]);
  useEffect(() => {
    if (!current && more.current === document.activeElement) document.querySelector<HTMLTextAreaElement>('[aria-label="Request draft"]')?.focus();
  }, [current]);
  return <section className="editor-line" aria-label="Editor explanation" data-speaking={!!current}>
    {current && <><p ref={label} role="status">{fits ? shownLine : entryFallback(current.scene)}</p>
      {current.scene === 'takeback' && notice && <button disabled={disabled} onClick={undo}>Undo take back</button>}
      <button ref={more} onClick={() => setOpened(current)} aria-expanded={!!opened}>more ›</button></>}
    {opened && <div ref={popup} popover="auto" tabIndex={-1} role="dialog" aria-label="Editor explanation details" className="editor-explanation">
      <div className="inspector-heading"><h3><PrivacyText text={opened.title} interactive={false} /></h3><button onClick={() => setOpened(null)} aria-label="Close Editor explanation">×</button></div>
      {opened.away ? <AwayDetails key={opened.away.interval.id} interval={opened.away.interval} page={opened.away.page} /> : <p><PrivacyText text={opened.detail} /></p>}
    </div>}
  </section>;
}
