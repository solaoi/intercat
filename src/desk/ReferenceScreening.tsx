import { useRef, useState } from 'react';
import type { Attachment } from '../bridge';

export function RefreshReference({ id, label, disabled = false }: { id: string; label: string; disabled?: boolean }) {
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState('');
  const operation = useRef<string | null>(null);
  const running = useRef(false);

  async function refresh() {
    if (running.current) return;
    running.current = true;
    setBusy(true);
    setMessage('');
    try {
      operation.current ??= await window.intercat.createOperationId();
      const result = await window.intercat.refreshPastedText({ opId: operation.current, attachmentId: id });
      operation.current = null;
      setMessage(result.changed ? 'Updated the screened copy. Resume the held request when ready.' : 'Saved the screening check.');
    } catch {
      setMessage('Could not save the screening check. Retry keeps the same operation.');
    } finally {
      running.current = false;
      setBusy(false);
    }
  }

  return <div className="reference-screening-action">
    <button disabled={disabled || busy} aria-label={`Recheck ${label}`} onClick={() => { void refresh(); }}>{busy ? 'Screening…' : 'Recheck screening'}</button>
    {message && <span role="status">{message}</span>}
  </div>;
}

type Page = { items: Attachment[]; next: string | null };
export function ReferenceScreening({ track, label }: { track: string; label: string }) {
  const [page, setPage] = useState<Page | null>(null);
  const [trail, setTrail] = useState<(string | null)[]>([null]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const running = useRef(false);
  const requestedTrail = useRef<(string | null)[]>([null]);

  async function load(cursors = requestedTrail.current) {
    if (running.current) return;
    requestedTrail.current = cursors;
    running.current = true;
    setBusy(true);
    setError('');
    try {
      const result = await window.intercat.readPastedReferences({ trackId: track, after: cursors[cursors.length - 1] });
      setPage(result);
      setTrail(cursors);
    } catch {
      setError('Could not load the pasted references.');
    } finally {
      running.current = false;
      setBusy(false);
    }
  }

  return <details className="reference-screening" onToggle={event => { if (event.currentTarget.open) void load(); }}>
    <summary>Reference screening for {label}</summary>
    <p>Recheck pasted references after adding a secret. The saved originals remain available in their requests.</p>
    {error && <p role="status">{error} <button disabled={busy} onClick={() => { void load(); }}>Retry loading references</button></p>}
    {busy && <p role="status">Loading references…</p>}
    {page && <>
      {!page.items.length && <p>No pasted references on this page.</p>}
      <ul>{page.items.map(item => <li key={item.id}><span>{item.name} · {item.id.slice(-8)}</span><RefreshReference id={item.id} label={`reference ${item.id.slice(-8)}`} disabled={busy} /></li>)}</ul>
      <div className="reference-screening-pages">
        {trail.length > 1 && <button disabled={busy} onClick={() => { void load(trail.slice(0, -1)); }}>Previous references</button>}
        {page.next && <button disabled={busy} onClick={() => { void load([...trail, page.next]); }}>More references</button>}
      </div>
    </>}
  </details>;
}
