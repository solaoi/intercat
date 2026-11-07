import { useEffect, useRef, useState } from 'react';
import type { EditorLogPage } from '../bridge';
import { PrivacyText } from './PrivacyText';

export function EditorLog({ trackId, label, sequence }: { trackId: string; label: string; sequence?: number }) {
  const [open, setOpen] = useState(false);
  const [page, setPage] = useState<EditorLogPage | null>(null);
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const generation = useRef(0);
  const paged = useRef(false);
  useEffect(() => {
    if (!open || paged.current) return;
    let disposed = false;
    void window.intercat.getEditorLog({ trackId, before: null }).then(value => {
      if (!disposed && !paged.current) { setPage(value); setError(''); }
    }).catch(() => { if (!disposed && !paged.current) setError('The Editor log could not be loaded.'); });
    return () => { disposed = true; };
  }, [open, sequence, trackId]);
  useEffect(() => () => { generation.current++; }, []);
  async function earlier() {
    if (!page?.before || busy) return;
    const id = ++generation.current;
    setBusy(true); setError(''); paged.current = true;
    try {
      const next = await window.intercat.getEditorLog({ trackId, before: page.before });
      if (generation.current === id) setPage({ entries: [...page.entries, ...next.entries], before: next.before });
    } catch { if (generation.current === id) setError('Earlier explanations could not be loaded.'); }
    finally { if (generation.current === id) setBusy(false); }
  }
  return <details className="editor-log" onToggle={event => {
    if (event.target !== event.currentTarget) return;
    setOpen(event.currentTarget.open);
    if (!event.currentTarget.open) { generation.current++; paged.current = false; setBusy(false); }
  }}>
    <summary>Worker {label} · Editor log</summary>
    {open && <div className="editor-log-body" role="region" aria-label={`Editor log for Worker ${label}`}>
      {error && <p role="alert">{error}</p>}
      {!page && !error && <p>Loading explanations…</p>}
      {page?.entries.length === 0 && <p>No Editor explanations yet.</p>}
      {page?.entries.map(entry => <article key={entry.id}>
        <header><strong><PrivacyText text={entry.title} interactive={false} /></strong><time dateTime={new Date(entry.createdAt).toISOString()}>{new Date(entry.createdAt).toLocaleTimeString()}</time></header>
        <p><PrivacyText text={entry.detail} /></p>
      </article>)}
      {page?.before && <button disabled={busy} onClick={() => { void earlier(); }}>{busy ? 'Loading earlier…' : 'Earlier explanations'}</button>}
      {error && !page && <button onClick={() => { setOpen(false); requestAnimationFrame(() => setOpen(true)); }}>Retry loading explanations</button>}
    </div>}
  </details>;
}
