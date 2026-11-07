import { createContext, useCallback, useContext, useEffect, useId, useLayoutEffect, useRef, useState } from 'react';
import type { ClipboardEvent } from 'react';
import type { PrivacyView } from '../bridge';
import './privacy.css';

export const PrivacyRevision = createContext<string | number>(0);
export const PrivacyEnabled = createContext(true);

type PendingPreview = { text: string; resolve: (view: PrivacyView) => void; reject: (error: unknown) => void };
const pending: PendingPreview[] = [];
let flushing = false;
const encoder = new TextEncoder();
async function flushPreviews() {
  try {
    while (pending.length) {
      const batch: PendingPreview[] = []; let bytes = 0;
      while (pending.length && batch.length < 64) {
        const size = encoder.encode(pending[0].text).byteLength;
        if (batch.length && bytes + size > 2 * 1024 * 1024) break;
        batch.push(pending.shift()!); bytes += size;
      }
      try {
        const views = await window.intercat.previewPrivacy(batch.map(request => request.text));
        if (views.length !== batch.length) throw new Error('Incomplete privacy preview.');
        batch.forEach((request, index) => request.resolve(views[index]));
      } catch (error) { batch.forEach(request => request.reject(error)); }
    }
  } finally { flushing = false; }
}
export function previewText(text: string): Promise<PrivacyView> {
  if (!text) return Promise.resolve({ text: '', parts: [] });
  if (encoder.encode(text).byteLength > 2 * 1024 * 1024) return Promise.reject(new Error('Text is too large.'));
  return new Promise((resolve, reject) => {
    pending.push({ text, resolve, reject });
    if (!flushing) { flushing = true; queueMicrotask(() => { void flushPreviews(); }); }
  });
}

export function privacyLabel(text: string, limit: number) {
  const characters = [...text];
  if (characters.length <= limit) return text;
  let end = characters.slice(0, limit).join('').length;
  const start = text.slice(0, end).lastIndexOf('<');
  if (start >= 0 && (text.startsWith('<redacted:', start) || text.startsWith('<secret:', start))) {
    const close = text.indexOf('>', start);
    if (close >= end && close - start < 192) end = close + 1;
  }
  return text.slice(0, end);
}

export function usePrivacyText(text: string, inspect = true, explicitRevision?: string | number) {
  const inheritedRevision = useContext(PrivacyRevision);
  const enabled = useContext(PrivacyEnabled) && inspect;
  const revision = explicitRevision ?? inheritedRevision;
  const [attempt, setAttempt] = useState(0);
  const [state, setState] = useState<{ text: string; revision: string | number; view?: PrivacyView; error?: string } | null>(null);
  useEffect(() => {
    let disposed = false;
    setState(null);
    if (!enabled) return;
    void previewText(text).then(view => {
      if (!disposed) setState({ text, revision, view });
    }).catch(() => { if (!disposed) setState({ text, revision, error: 'Privacy check unavailable' }); });
    return () => { disposed = true; };
  }, [text, revision, attempt, enabled]);
  const current = state?.text === text && state.revision === revision ? state : null;
  return { view: enabled ? current?.view : { text, parts: [{ kind: 'text' as const, text }] }, error: enabled ? current?.error : undefined, retry: () => setAttempt(value => value + 1) };
}

export function PrivacySummary({ text, limit }: { text: string; limit: number }) {
  const { view, error } = usePrivacyText(text);
  const checked = view?.text ?? error ?? 'Checking privacy…';
  return <span title={checked}>{privacyLabel(checked, limit)}</span>;
}

export function usePrivacyLabels(texts: string[], explicitRevision?: string | number, requested = true) {
  const enabled = useContext(PrivacyEnabled) && requested;
  const inheritedRevision = useContext(PrivacyRevision);
  const revision = explicitRevision ?? inheritedRevision;
  const key = JSON.stringify([...new Set(texts)].sort());
  const [state, setState] = useState<{ revision: string | number; labels: Map<string, string> }>({ revision, labels: new Map() });
  useEffect(() => {
    if (!enabled) return;
    let disposed = false;
    const wanted: string[] = JSON.parse(key);
    setState(previous => ({ revision, labels: new Map(wanted.flatMap(text => previous.revision === revision && previous.labels.has(text) ? [[text, previous.labels.get(text)!] as const] : [])) }));
    for (const text of wanted) {
      if (state.revision === revision && state.labels.has(text)) continue;
      void previewText(text).then(view => view.text, () => 'Privacy check unavailable').then(label => {
        if (!disposed) setState(previous => ({ revision, labels: new Map(previous.labels).set(text, label) }));
      });
    }
    return () => { disposed = true; };
  }, [key, revision, enabled]);
  return useCallback((text: string) => !enabled ? text : state.revision === revision ? state.labels.get(text) ?? 'Checking privacy…' : 'Checking privacy…', [enabled, state, revision]);
}

function PrivateValue({ part }: { part: Extract<PrivacyView['parts'][number], { kind: 'redaction' }> }) {
  const [open, setOpen] = useState(false);
  const [value, setValue] = useState<string | null>(null);
  const [error, setError] = useState(false);
  const button = useRef<HTMLButtonElement>(null);
  const popup = useRef<HTMLSpanElement>(null);
  const id = useId();
  const hideTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  function keepOpen() { if (hideTimer.current) clearTimeout(hideTimer.current); setOpen(true); }
  function leave() {
    if (hideTimer.current) clearTimeout(hideTimer.current);
    hideTimer.current = setTimeout(() => {
      if (document.activeElement !== button.current && !popup.current?.matches(':hover')) setOpen(false);
    }, 150);
  }
  useEffect(() => () => { if (hideTimer.current) clearTimeout(hideTimer.current); }, []);
  useEffect(() => {
    if (!open) { setValue(null); setError(false); return; }
    let disposed = false;
    void window.intercat.readPrivacyOriginal(part.id).then(text => { if (!disposed) setValue(text); })
      .catch(() => { if (!disposed) setError(true); });
    return () => { disposed = true; };
  }, [open, part.id]);
  useLayoutEffect(() => {
    const tooltip = popup.current;
    if (!tooltip || !open) return;
    tooltip.showPopover();
    const place = () => {
      const anchor = button.current?.getBoundingClientRect();
      if (!anchor) return;
      const bounds = tooltip.getBoundingClientRect();
      tooltip.style.left = `${Math.max(8, Math.min(anchor.left, innerWidth - bounds.width - 8))}px`;
      tooltip.style.top = `${Math.max(8, Math.min(anchor.bottom + 6, innerHeight - bounds.height - 8))}px`;
    };
    place();
    const observer = new ResizeObserver(place); observer.observe(tooltip);
    window.addEventListener('resize', place);
    return () => { observer.disconnect(); window.removeEventListener('resize', place); tooltip.hidePopover(); };
  }, [open]);
  return <>
    <button className="privacy-value" ref={button} type="button" aria-label={`Show original ${part.category} value`} aria-describedby={open ? id : undefined} aria-expanded={open}
      onMouseEnter={keepOpen} onMouseLeave={leave}
      onFocus={keepOpen} onBlur={leave}
      onClick={() => setOpen(true)} onKeyDown={event => { if (event.key === 'Escape' && open) { event.preventDefault(); event.stopPropagation(); setOpen(false); } }}>
      {`<redacted:${part.category}#${part.id}>`}
    </button>
    {open && <span ref={popup} id={id} className="privacy-original" popover="manual" role="tooltip" onMouseEnter={keepOpen} onMouseLeave={leave}>{error ? 'Original unavailable' : value ?? 'Loading original…'}</span>}
  </>;
}

export function PrivacyText({ text, interactive = true }: { text: string; interactive?: boolean }) {
  const { view, error, retry } = usePrivacyText(text);
  const [copyStatus, setCopyStatus] = useState('');
  const copyVersion = useRef(0);
  const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; copyVersion.current++; }; }, []);
  useEffect(() => { copyVersion.current++; setCopyStatus(''); }, [text]);
  async function copy(selected: string) {
    const version = ++copyVersion.current;
    try {
      const fresh = await previewText(selected);
      if (!mounted.current || copyVersion.current !== version) return;
      await navigator.clipboard.writeText(fresh.text);
      if (mounted.current && copyVersion.current === version) setCopyStatus('Copied masked text');
    } catch { if (mounted.current && copyVersion.current === version) setCopyStatus('Could not copy safely. Retry the privacy check.'); }
  }
  function onCopy(event: ClipboardEvent) {
    const selected = window.getSelection()?.toString();
    if (!selected) return;
    event.preventDefault(); event.stopPropagation(); void copy(selected);
  }
  if (!view) return <span className="privacy-pending" aria-live="polite" aria-busy={!error}>{error ?? 'Checking privacy…'}{error && interactive && <button type="button" onClick={retry}>Retry privacy check</button>}</span>;
  return <span className="privacy-text" onCopy={onCopy}>
    <span className="privacy-content">{interactive ? view.parts.map((part, index) => part.kind === 'text' ? part.text : <PrivateValue key={`${part.id}/${index}`} part={part} />) : view.text}</span>
    {interactive && view.parts.some(part => part.kind === 'redaction') && <span className="privacy-copy-row"><button type="button" onClick={() => { void copy(text); }}>Copy masked text</button>{copyStatus && <span role="status">{copyStatus}</span>}</span>}
  </span>;
}
