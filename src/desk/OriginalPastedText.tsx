import { useState } from 'react';

export function OriginalPastedText({ id, label = 'Read original pasted text' }: { id: string; label?: string }) {
  const [text, setText] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  async function read() {
    if (loading || text !== null) return;
    setLoading(true); setError('');
    try { setText(await window.intercat.readPastedText(id)); }
    catch { setError('The original pasted text could not be loaded.'); }
    finally { setLoading(false); }
  }
  return <details className="original-paste" onToggle={event => { if (event.currentTarget.open) void read(); }}>
    <summary>{label}</summary>
    {loading ? <p>Loading pasted text…</p> : text !== null ? <pre tabIndex={0}>{text}</pre> : <p role="status">{error}<button type="button" disabled={loading} onClick={() => { void read(); }}>Retry reading</button></p>}
  </details>;
}
